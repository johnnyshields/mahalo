use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;
use tracing::{debug, info};

use mahalo_pubsub::PubSub;

use crate::topology::TopologyStrategy;

pub struct MahaloCluster {
    node_id: u64,
    _handle: tokio::task::JoinHandle<()>,
}

impl MahaloCluster {
    /// Start the cluster, polling the topology and broadcasting node events.
    pub async fn start(
        node_id: u64,
        topology: impl TopologyStrategy,
        pubsub: PubSub,
    ) -> Self {
        let topology = Arc::new(topology);
        let pubsub_clone = pubsub.clone();

        // Get initial peers
        let initial_peers: HashSet<SocketAddr> =
            topology.initial_peers().await.into_iter().collect();
        for peer in &initial_peers {
            info!(node_id, %peer, "initial peer discovered");
            pubsub_clone.broadcast(
                "mahalo:cluster",
                "node_up",
                json!({ "node_id": node_id, "addr": peer.to_string() }),
            );
        }

        let interval = topology.poll_interval();
        let handle = tokio::spawn(async move {
            let mut known_peers = initial_peers;
            let mut interval = tokio::time::interval(interval);
            interval.tick().await; // skip immediate first tick

            loop {
                interval.tick().await;
                let current: HashSet<SocketAddr> =
                    topology.poll_peers().await.into_iter().collect();

                for peer in current.difference(&known_peers) {
                    info!(%peer, "node_up");
                    pubsub_clone.broadcast(
                        "mahalo:cluster",
                        "node_up",
                        json!({ "node_id": node_id, "addr": peer.to_string() }),
                    );
                }

                for peer in known_peers.difference(&current) {
                    info!(%peer, "node_down");
                    pubsub_clone.broadcast(
                        "mahalo:cluster",
                        "node_down",
                        json!({ "node_id": node_id, "addr": peer.to_string() }),
                    );
                }

                known_peers = current;
                debug!(peer_count = known_peers.len(), "topology polled");
            }
        });

        Self {
            node_id,
            _handle: handle,
        }
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    struct MockTopology {
        initial: Vec<SocketAddr>,
        poll: Arc<Mutex<Vec<SocketAddr>>>,
        interval: Duration,
    }

    #[async_trait]
    impl TopologyStrategy for MockTopology {
        async fn initial_peers(&self) -> Vec<SocketAddr> {
            self.initial.clone()
        }

        async fn poll_peers(&self) -> Vec<SocketAddr> {
            self.poll.lock().unwrap().clone()
        }

        fn poll_interval(&self) -> Duration {
            self.interval
        }
    }

    #[tokio::test]
    async fn start_returns_correct_node_id() {
        let pubsub = PubSub::start();
        let topo = MockTopology {
            initial: vec![],
            poll: Arc::new(Mutex::new(vec![])),
            interval: Duration::from_millis(50),
        };

        let cluster = MahaloCluster::start(42, topo, pubsub.clone()).await;
        assert_eq!(cluster.node_id(), 42);

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn initial_peers_broadcast_node_up() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("mahalo:cluster").unwrap();

        let peer1: SocketAddr = "127.0.0.1:4001".parse().unwrap();
        let peer2: SocketAddr = "127.0.0.1:4002".parse().unwrap();
        let initial = vec![peer1, peer2];

        let topo = MockTopology {
            initial: initial.clone(),
            poll: Arc::new(Mutex::new(initial.clone())),
            interval: Duration::from_millis(50),
        };

        let _cluster = MahaloCluster::start(1, topo, pubsub.clone()).await;

        // Collect the two node_up messages from initial peers
        let mut seen_addrs: HashSet<String> = HashSet::new();
        for _ in 0..2 {
            let msg = rx.recv_timeout(Duration::from_secs(2))
                .expect("timed out waiting for node_up");
            assert_eq!(msg.event, "node_up");
            let addr = msg.payload["addr"].as_str().unwrap().to_string();
            seen_addrs.insert(addr);
        }

        assert!(seen_addrs.contains(&peer1.to_string()));
        assert!(seen_addrs.contains(&peer2.to_string()));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn poll_detects_new_peer() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("mahalo:cluster").unwrap();

        let poll_peers: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(vec![]));

        let topo = MockTopology {
            initial: vec![],
            poll: poll_peers.clone(),
            interval: Duration::from_millis(50),
        };

        let _cluster = MahaloCluster::start(10, topo, pubsub.clone()).await;

        // Inject a new peer for the next poll cycle
        let new_peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        *poll_peers.lock().unwrap() = vec![new_peer];

        // Poll with tokio yield to let the interval task run
        let msg = tokio::task::spawn_blocking(move || {
            rx.recv_timeout(Duration::from_secs(2))
                .expect("timed out waiting for node_up from poll")
        }).await.unwrap();

        assert_eq!(msg.event, "node_up");
        assert_eq!(msg.payload["addr"].as_str().unwrap(), new_peer.to_string());

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn poll_detects_removed_peer() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("mahalo:cluster").unwrap();

        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let poll_peers: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(vec![peer]));

        let topo = MockTopology {
            initial: vec![peer],
            poll: poll_peers.clone(),
            interval: Duration::from_millis(50),
        };

        let _cluster = MahaloCluster::start(20, topo, pubsub.clone()).await;

        // Drain the initial node_up message via spawn_blocking to not block tokio
        let rx2 = {
            let msg = tokio::task::spawn_blocking({
                let rx = rx.clone();
                move || rx.recv_timeout(Duration::from_secs(2))
                    .expect("timed out waiting for initial node_up")
            }).await.unwrap();
            assert_eq!(msg.event, "node_up");
            rx
        };

        // Remove the peer so next poll detects it as gone
        *poll_peers.lock().unwrap() = vec![];

        let msg = tokio::task::spawn_blocking(move || {
            rx2.recv_timeout(Duration::from_secs(2))
                .expect("timed out waiting for node_down from poll")
        }).await.unwrap();

        assert_eq!(msg.event, "node_down");
        assert_eq!(msg.payload["addr"].as_str().unwrap(), peer.to_string());

        pubsub.shutdown();
    }
}
