//! Distributed PubSub bridge.
//!
//! Provides a [`DistributedPubSub`] wrapper that forwards broadcasts to peer
//! nodes. The bridge maintains a list of peer handles; when a broadcast
//! happens locally, it is also forwarded to each peer so they can relay
//! it to their local subscribers.
//!
//! This is an additive layer — the local [`PubSub`] API is unchanged.

use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::broadcast;

use crate::pubsub::{PubSub, PubSubMessage};

/// A `PubSub` wrapper that also fans out broadcasts to peer nodes, enabling
/// cross-node message delivery.
///
/// The local [`PubSub`] and its API are unchanged — `subscribe()` returns the
/// same `Receiver` type. Only `broadcast()` is intercepted.
///
/// # Distributed Wiring
///
/// Nodes exchange their `DistributedPubSub` handles (or an mpsc sender wrapped
/// around their local PubSub) out-of-band (e.g., via the cluster node_up event).
/// Call [`add_peer()`] to register each peer's broadcast sender.
///
/// When Phase 5 (mahalo-cluster) is active, the cluster integration subscribes
/// to `"mahalo:cluster"` PubSub events and calls `add_peer` / `remove_peer`
/// automatically.
#[derive(Clone)]
pub struct DistributedPubSub {
    local: PubSub,
    /// Peer broadcast channels. Each peer's channel accepts `PubSubMessage`
    /// values that the peer forwards to its own local PubSub.
    peers: Arc<RwLock<Vec<tokio::sync::mpsc::UnboundedSender<PubSubMessage>>>>,
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
            payload,
        };
        // Deliver locally
        self.local.broadcast(topic, &msg.event, msg.payload.clone());
        // Fan out to peers (stale senders are pruned)
        let mut peers = self.peers.write().unwrap();
        peers.retain(|tx| tx.send(msg.clone()).is_ok());
    }

    /// Subscribe to a topic. Delegates to the local PubSub.
    pub async fn subscribe(&self, topic: &str) -> Option<broadcast::Receiver<PubSubMessage>> {
        self.local.subscribe(topic).await
    }

    /// Unsubscribe from a topic.
    pub fn unsubscribe(&self, topic: &str) {
        self.local.unsubscribe(topic);
    }

    /// Add a peer node's inbound channel. Messages broadcast here will be
    /// sent to the peer, which should call its own local PubSub.
    ///
    /// Returns the corresponding receiver that the peer should poll.
    pub fn peer_channel() -> (
        tokio::sync::mpsc::UnboundedSender<PubSubMessage>,
        tokio::sync::mpsc::UnboundedReceiver<PubSubMessage>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    /// Register a peer's inbound sender so this node will forward broadcasts to it.
    pub fn add_peer(&self, sender: tokio::sync::mpsc::UnboundedSender<PubSubMessage>) {
        self.peers.write().unwrap().push(sender);
    }

    /// Spawn a relay task that reads from `peer_rx` and calls `local.broadcast()`.
    ///
    /// Use this on the receiving side to integrate a peer channel with the
    /// local PubSub. The returned `JoinHandle` can be aborted on disconnect.
    pub fn spawn_relay(
        local: PubSub,
        mut peer_rx: tokio::sync::mpsc::UnboundedReceiver<PubSubMessage>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(msg) = peer_rx.recv().await {
                local.broadcast(&msg.topic, msg.event, msg.payload);
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
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn local_broadcast_delivers_to_local_subscribers() {
        let local = PubSub::start();
        let distributed = DistributedPubSub::new(local.clone());
        let mut rx = local.subscribe("test:topic").await.unwrap();

        distributed.broadcast("test:topic", "hello", serde_json::json!({"ok": true}));

        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg.event, "hello");
        assert_eq!(msg.topic, "test:topic");
        local.shutdown();
    }

    #[tokio::test]
    async fn subscribe_delegates_to_local() {
        let local = PubSub::start();
        let distributed = DistributedPubSub::new(local.clone());
        let mut rx = distributed.subscribe("room:test").await.unwrap();

        distributed.broadcast("room:test", "ping", serde_json::json!(null));

        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg.event, "ping");
        local.shutdown();
    }

    #[tokio::test]
    async fn broadcast_forwards_to_peer() {
        // Simulate two nodes: node A and node B
        let local_a = PubSub::start();
        let local_b = PubSub::start();

        let dist_a = DistributedPubSub::new(local_a.clone());
        let dist_b = DistributedPubSub::new(local_b.clone());

        // Wire A -> B: A's broadcasts are forwarded to B
        let (peer_tx_b, peer_rx_b) = DistributedPubSub::peer_channel();
        dist_a.add_peer(peer_tx_b);
        let _relay = DistributedPubSub::spawn_relay(local_b.clone(), peer_rx_b);

        // Subscribe on B
        let mut rx_b = local_b.subscribe("room:lobby").await.unwrap();

        // Broadcast from A
        dist_a.broadcast("room:lobby", "from_a", serde_json::json!({"x": 1}));

        // B should receive it
        let msg = timeout(Duration::from_secs(1), rx_b.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg.event, "from_a");
        assert_eq!(msg.payload["x"], 1);

        local_a.shutdown();
        local_b.shutdown();
        drop(dist_a);
        drop(dist_b);
    }
}
