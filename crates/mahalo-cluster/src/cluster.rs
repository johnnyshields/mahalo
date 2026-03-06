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
