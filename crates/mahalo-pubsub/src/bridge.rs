//! Distributed PubSub bridge.
//!
//! Provides a [`DistributedPubSub`] wrapper that forwards broadcasts to peer
//! nodes. The bridge maintains a list of peer handles; when a broadcast
//! happens locally, it is also forwarded to each peer so they can relay
//! it to their local subscribers.
//!
//! This is an additive layer — the local [`PubSub`] API is unchanged.

use std::sync::{Arc, RwLock};

use crossbeam_channel as crossbeam;
use serde_json::Value;

use crate::pubsub::{PubSub, PubSubMessage};

/// A `PubSub` wrapper that also fans out broadcasts to peer nodes, enabling
/// cross-node message delivery.
///
/// The local [`PubSub`] and its API are unchanged — `subscribe()` returns the
/// same `Receiver` type. Only `broadcast()` is intercepted.
///
/// # Distributed Wiring
///
/// Nodes exchange their `DistributedPubSub` handles (or a crossbeam sender
/// wrapped around their local PubSub) out-of-band (e.g., via the cluster
/// node_up event). Call [`add_peer()`] to register each peer's broadcast sender.
///
/// When Phase 5 (mahalo-cluster) is active, the cluster integration subscribes
/// to `"mahalo:cluster"` PubSub events and calls `add_peer` / `remove_peer`
/// automatically.
#[derive(Clone)]
pub struct DistributedPubSub {
    local: PubSub,
    /// Peer broadcast channels. Each peer's channel accepts `PubSubMessage`
    /// values that the peer forwards to its own local PubSub.
    peers: Arc<RwLock<Vec<crossbeam::Sender<PubSubMessage>>>>,
}

impl DistributedPubSub {
    /// Create a `DistributedPubSub` wrapping the given local `PubSub`.
    pub fn new(local: PubSub) -> Self {
        Self {
            local,
            peers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Broadcast a message to local subscribers AND all peer nodes.
    pub fn broadcast(&self, topic: &str, event: &str, payload: Value) {
        let msg = PubSubMessage {
            topic: topic.to_string(),
            event: event.to_string(),
            payload: Arc::new(payload),
        };
        // Deliver locally — use broadcast_arc to avoid deep-cloning the Arc'd payload.
        self.local
            .broadcast_arc(topic, &msg.event, Arc::clone(&msg.payload));
        // Fan out to peers (stale senders are pruned)
        let mut peers = self.peers.write().unwrap();
        peers.retain(|tx| tx.send(msg.clone()).is_ok());
    }

    /// Subscribe to a topic. Delegates to the local PubSub.
    pub fn subscribe(&self, topic: &str) -> Option<crossbeam::Receiver<PubSubMessage>> {
        self.local.subscribe(topic)
    }

    /// Unsubscribe from a topic.
    pub fn unsubscribe(&self, topic: &str) {
        self.local.unsubscribe(topic);
    }

    /// Create a peer channel pair. Messages sent to the sender will be received
    /// by the receiver on the peer side.
    ///
    /// Returns the corresponding receiver that the peer should poll.
    pub fn peer_channel() -> (
        crossbeam::Sender<PubSubMessage>,
        crossbeam::Receiver<PubSubMessage>,
    ) {
        crossbeam::unbounded()
    }

    /// Register a peer's inbound sender so this node will forward broadcasts to it.
    pub fn add_peer(&self, sender: crossbeam::Sender<PubSubMessage>) {
        self.peers.write().unwrap().push(sender);
    }

    /// Spawn a relay thread that reads from `peer_rx` and calls `local.broadcast()`.
    ///
    /// Use this on the receiving side to integrate a peer channel with the
    /// local PubSub. Returns a `JoinHandle` that can be used to wait for
    /// the relay to complete (it runs until the sender is dropped).
    pub fn spawn_relay(
        local: PubSub,
        peer_rx: crossbeam::Receiver<PubSubMessage>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Ok(msg) = peer_rx.recv() {
                local.broadcast_arc(&msg.topic, msg.event, Arc::clone(&msg.payload));
            }
        })
    }

    /// Access the underlying local PubSub handle.
    #[allow(dead_code)]
    pub(crate) fn local(&self) -> &PubSub {
        &self.local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn local_broadcast_delivers_to_local_subscribers() {
        let local = PubSub::start();
        let distributed = DistributedPubSub::new(local.clone());
        let rx = local.subscribe("test:topic").unwrap();

        distributed.broadcast("test:topic", "hello", serde_json::json!({"ok": true}));

        let msg = rx.recv_timeout(Duration::from_secs(1)).expect("recv error");

        assert_eq!(msg.event, "hello");
        assert_eq!(msg.topic, "test:topic");
        local.shutdown();
    }

    #[test]
    fn subscribe_delegates_to_local() {
        let local = PubSub::start();
        let distributed = DistributedPubSub::new(local.clone());
        let rx = distributed.subscribe("room:test").unwrap();

        distributed.broadcast("room:test", "ping", serde_json::json!(null));

        let msg = rx.recv_timeout(Duration::from_secs(1)).expect("recv error");

        assert_eq!(msg.event, "ping");
        local.shutdown();
    }

    #[test]
    fn broadcast_forwards_to_peer() {
        // Simulate two nodes: node A and node B
        let local_a = PubSub::start();
        let local_b = PubSub::start();

        let dist_a = DistributedPubSub::new(local_a.clone());
        let _dist_b = DistributedPubSub::new(local_b.clone());

        // Wire A -> B: A's broadcasts are forwarded to B
        let (peer_tx_b, peer_rx_b) = DistributedPubSub::peer_channel();
        dist_a.add_peer(peer_tx_b);
        let _relay = DistributedPubSub::spawn_relay(local_b.clone(), peer_rx_b);

        // Subscribe on B
        let rx_b = local_b.subscribe("room:lobby").unwrap();

        // Broadcast from A
        dist_a.broadcast("room:lobby", "from_a", serde_json::json!({"x": 1}));

        // B should receive it
        let msg = rx_b.recv_timeout(Duration::from_secs(1)).expect("recv error");

        assert_eq!(msg.event, "from_a");
        assert_eq!(msg.payload["x"], 1);

        local_a.shutdown();
        local_b.shutdown();
        drop(dist_a);
        drop(_dist_b);
    }
}
