use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, trace};

use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{ChildEntry, ChildSpec};

/// A message delivered through the PubSub system.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PubSubMessage {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
}

/// Internal commands sent to the PubSub server task.
enum PubSubCommand {
    Subscribe {
        topic: String,
        reply: oneshot::Sender<broadcast::Receiver<PubSubMessage>>,
    },
    Broadcast {
        topic: String,
        msg: PubSubMessage,
    },
    Unsubscribe {
        topic: String,
    },
    Shutdown,
}

/// Capacity of each per-topic broadcast channel.
const CHANNEL_CAPACITY: usize = 256;

/// A cloneable handle to the PubSub server.
///
/// The server itself runs as a background tokio task that manages
/// topic -> broadcast channel mappings. This handle is `Clone + Send + Sync`
/// so it can be shared freely across request handlers and channels.
#[derive(Clone)]
pub struct PubSub {
    cmd_tx: mpsc::UnboundedSender<PubSubCommand>,
}

impl PubSub {
    /// Start the PubSub server. Returns a handle for interacting with it.
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        tokio::spawn(Self::run_server(cmd_rx));

        debug!("PubSub server started");
        PubSub { cmd_tx }
    }

    /// The server event loop.
    async fn run_server(mut cmd_rx: mpsc::UnboundedReceiver<PubSubCommand>) {
        let mut topics: HashMap<String, broadcast::Sender<PubSubMessage>> = HashMap::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                PubSubCommand::Subscribe { topic, reply } => {
                    let tx = topics.entry(topic.clone()).or_insert_with(|| {
                        trace!(topic = %topic, "creating new broadcast channel");
                        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
                        tx
                    });
                    let _ = reply.send(tx.subscribe());
                }
                PubSubCommand::Broadcast { topic, msg } => {
                    if let Some(tx) = topics.get(&topic) {
                        // send returns Err only when there are no receivers; that is fine.
                        let _ = tx.send(msg);
                    }
                }
                PubSubCommand::Unsubscribe { topic } => {
                    if let Some(tx) = topics.get(&topic)
                        && tx.receiver_count() == 0
                    {
                        topics.remove(&topic);
                        trace!(topic = %topic, "removed empty topic");
                    }
                }
                PubSubCommand::Shutdown => {
                    debug!("PubSub server shutting down");
                    break;
                }
            }
        }
    }

    /// Start the PubSub server as a rebar process. Returns a handle for interacting with it.
    pub async fn start_with_runtime(runtime: &Runtime) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        runtime
            .spawn(move |_ctx| async move {
                Self::run_server(cmd_rx).await;
            })
            .await;

        debug!("PubSub server started as rebar process");
        PubSub { cmd_tx }
    }

    /// Subscribe to a topic. Returns a broadcast receiver that will receive
    /// all future messages published to this topic.
    ///
    /// Returns `None` if the PubSub server has been dropped.
    pub async fn subscribe(&self, topic: &str) -> Option<broadcast::Receiver<PubSubMessage>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(PubSubCommand::Subscribe {
                topic: topic.to_string(),
                reply: reply_tx,
            })
            .ok();
        match reply_rx.await {
            Ok(rx) => Some(rx),
            Err(_) => {
                tracing::warn!(topic = %topic, "PubSub server dropped before subscribe completed");
                None
            }
        }
    }

    /// Broadcast a message to all current subscribers of the given topic.
    ///
    /// This is a fire-and-forget operation: if the topic has no subscribers
    /// the message is silently dropped.
    pub fn broadcast(&self, topic: &str, event: impl Into<String>, payload: serde_json::Value) {
        let msg = PubSubMessage {
            topic: topic.to_string(),
            event: event.into(),
            payload,
        };
        self.cmd_tx
            .send(PubSubCommand::Broadcast {
                topic: topic.to_string(),
                msg,
            })
            .ok();
    }

    /// Request a cleanup pass for the given topic. If there are no remaining
    /// subscribers the internal channel is removed.
    pub fn unsubscribe(&self, topic: &str) {
        self.cmd_tx
            .send(PubSubCommand::Unsubscribe {
                topic: topic.to_string(),
            })
            .ok();
    }

    /// Gracefully shut down the PubSub server.
    pub fn shutdown(&self) {
        self.cmd_tx.send(PubSubCommand::Shutdown).ok();
    }

    /// Create a properly supervised `(PubSub, ChildEntry)` pair.
    ///
    /// The channel is pre-allocated so the handle and the supervised factory
    /// each own their respective end. The supervised process *is* the server
    /// loop — if it crashes the supervisor will restart it correctly.
    pub fn new_supervised() -> (Self, ChildEntry) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<PubSubCommand>();
        let cmd_rx = Arc::new(Mutex::new(Some(cmd_rx)));
        let handle = PubSub { cmd_tx };
        let entry = ChildEntry::new(ChildSpec::new("mahalo_pubsub"), move || {
            let cmd_rx = Arc::clone(&cmd_rx);
            async move {
                let rx = cmd_rx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("PubSub factory called twice");
                Self::run_server(rx).await;
                ExitReason::Normal
            }
        });
        (handle, entry)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn subscribe_and_receive() {
        let pubsub = PubSub::start();
        let mut rx = pubsub.subscribe("room:lobby").await.unwrap();

        pubsub.broadcast("room:lobby", "new_msg", serde_json::json!({"text": "hello"}));

        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "new_msg");
        assert_eq!(msg.payload, serde_json::json!({"text": "hello"}));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_broadcast() {
        let pubsub = PubSub::start();
        let mut rx1 = pubsub.subscribe("room:lobby").await.unwrap();
        let mut rx2 = pubsub.subscribe("room:lobby").await.unwrap();

        pubsub.broadcast("room:lobby", "ping", serde_json::json!(null));

        let msg1 = timeout(Duration::from_secs(1), rx1.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        let msg2 = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg1.event, "ping");
        assert_eq!(msg2.event, "ping");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn broadcast_to_nonexistent_topic_does_not_panic() {
        let pubsub = PubSub::start();
        // Should not panic or error
        pubsub.broadcast("no_such_topic", "evt", serde_json::json!(42));
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn unsubscribe_cleans_up_empty_topic() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("ephemeral").await.unwrap();

        // Drop the receiver so receiver_count goes to 0
        drop(rx);

        pubsub.unsubscribe("ephemeral");

        // After cleanup, broadcasting should be a no-op (no channel exists)
        pubsub.broadcast("ephemeral", "gone", serde_json::json!(null));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn subscribers_on_different_topics_are_independent() {
        let pubsub = PubSub::start();
        let mut rx_a = pubsub.subscribe("topic_a").await.unwrap();
        let mut rx_b = pubsub.subscribe("topic_b").await.unwrap();

        pubsub.broadcast("topic_a", "only_a", serde_json::json!("a"));

        let msg = timeout(Duration::from_secs(1), rx_a.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(msg.event, "only_a");

        // rx_b should have nothing
        let result = timeout(Duration::from_millis(50), rx_b.recv()).await;
        assert!(result.is_err(), "rx_b should not receive topic_a messages");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn pubsub_message_serde() {
        let msg = PubSubMessage {
            topic: "room:lobby".to_string(),
            event: "new_msg".to_string(),
            payload: serde_json::json!({"text": "hello"}),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: PubSubMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.topic, "room:lobby");
        assert_eq!(deserialized.event, "new_msg");
        assert_eq!(deserialized.payload, serde_json::json!({"text": "hello"}));
    }

    #[tokio::test]
    async fn subscribe_returns_none_after_shutdown() {
        let pubsub = PubSub::start();
        pubsub.shutdown();
        // Allow server loop to process shutdown
        tokio::time::sleep(Duration::from_millis(50)).await;
        // After shutdown, subscribe may return None since the server has stopped
        let result = pubsub.subscribe("test").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn start_with_runtime_creates_working_pubsub() {
        let runtime = rebar_core::runtime::Runtime::new(1);
        let pubsub = PubSub::start_with_runtime(&runtime).await;
        let mut rx = pubsub.subscribe("test:topic").await.unwrap();

        pubsub.broadcast("test:topic", "hello", serde_json::json!({"msg": "hi"}));

        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(msg.topic, "test:topic");
        assert_eq!(msg.event, "hello");
        assert_eq!(msg.payload, serde_json::json!({"msg": "hi"}));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn pubsub_handle_is_clone_send_sync() {
        fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
        assert_clone_send_sync::<PubSub>();
    }

    #[tokio::test]
    async fn new_supervised_subscribe_and_broadcast() {
        use rebar_core::runtime::Runtime;
        use rebar_core::supervisor::spec::{RestartStrategy, SupervisorSpec};
        use rebar_core::supervisor::engine::start_supervisor;

        let runtime = Arc::new(Runtime::new(1));
        let (pubsub, entry) = PubSub::new_supervised();

        assert_eq!(entry.spec.id, "mahalo_pubsub");

        // Start the supervisor which will start the actual server loop.
        let _supervisor = start_supervisor(
            Arc::clone(&runtime),
            SupervisorSpec::new(RestartStrategy::OneForOne),
            vec![entry],
        )
        .await;

        // Give the server loop a moment to start.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut rx = pubsub.subscribe("supervised:topic").await.unwrap();
        pubsub.broadcast("supervised:topic", "hello", serde_json::json!({"ok": true}));

        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(msg.event, "hello");
    }
}
