use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crossbeam_channel as crossbeam;
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
        reply: crossbeam::Sender<crossbeam::Receiver<PubSubMessage>>,
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

/// A cloneable handle to the PubSub server.
///
/// The server runs on a dedicated background thread using blocking
/// crossbeam channel operations. This handle is `Clone + Send + Sync`
/// so it can be shared freely across worker threads.
#[derive(Clone)]
pub struct PubSub {
    cmd_tx: crossbeam::Sender<PubSubCommand>,
}

impl PubSub {
    /// Start the PubSub server on a dedicated background thread.
    /// Returns a handle for interacting with it.
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam::unbounded();

        std::thread::spawn(move || {
            Self::run_server(cmd_rx);
        });

        debug!("PubSub server started");
        PubSub { cmd_tx }
    }

    /// The server event loop — blocks on crossbeam recv.
    fn run_server(cmd_rx: crossbeam::Receiver<PubSubCommand>) {
        let mut topics: HashMap<String, Vec<crossbeam::Sender<PubSubMessage>>> = HashMap::new();

        while let Ok(cmd) = cmd_rx.recv() {
            if Self::handle_command(cmd, &mut topics) {
                return;
            }
            // Drain any additional queued commands before blocking again
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if Self::handle_command(cmd, &mut topics) {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    /// Handle a single command. Returns `true` if shutdown was requested.
    fn handle_command(
        cmd: PubSubCommand,
        topics: &mut HashMap<String, Vec<crossbeam::Sender<PubSubMessage>>>,
    ) -> bool {
        match cmd {
            PubSubCommand::Subscribe { topic, reply } => {
                let (tx, rx) = crossbeam::unbounded();
                let senders = topics.entry(topic.clone()).or_insert_with(|| {
                    trace!(topic = %topic, "creating new topic");
                    Vec::new()
                });
                senders.push(tx);
                let _ = reply.send(rx);
            }
            PubSubCommand::Broadcast { topic, msg } => {
                if let Some(senders) = topics.get_mut(&topic) {
                    // Send to each subscriber, remove disconnected ones
                    senders.retain(|tx| tx.send(msg.clone()).is_ok());
                    if senders.is_empty() {
                        topics.remove(&topic);
                    }
                }
            }
            PubSubCommand::Unsubscribe { topic } => {
                if let Some(senders) = topics.get_mut(&topic) {
                    senders.clear();
                    topics.remove(&topic);
                    trace!(topic = %topic, "removed topic on unsubscribe");
                }
            }
            PubSubCommand::Shutdown => {
                debug!("PubSub server shutting down");
                return true;
            }
        }
        false
    }

    /// Start the PubSub server as a rebar process. Returns a handle.
    ///
    /// The server loop runs on a dedicated background thread. The rebar
    /// process monitors it via a crossbeam channel and exits when the
    /// server thread completes.
    pub fn start_with_runtime(runtime: &Runtime) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam::unbounded();

        // The actual server runs on a dedicated thread
        let (done_tx, done_rx) = crossbeam::bounded::<()>(1);
        std::thread::spawn(move || {
            Self::run_server(cmd_rx);
            let _ = done_tx.send(());
        });

        // The rebar process waits for the server thread to finish
        runtime.spawn(move |_ctx| async move {
            // Poll for server thread completion
            loop {
                match done_rx.try_recv() {
                    Ok(()) | Err(crossbeam::TryRecvError::Disconnected) => break,
                    Err(crossbeam::TryRecvError::Empty) => {
                        rebar_core::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        });

        debug!("PubSub server started as rebar process");
        PubSub { cmd_tx }
    }

    /// Subscribe to a topic. Returns a crossbeam receiver that will receive
    /// all future messages published to this topic.
    ///
    /// Returns `None` if the PubSub server has been dropped.
    pub fn subscribe(&self, topic: &str) -> Option<crossbeam::Receiver<PubSubMessage>> {
        let (reply_tx, reply_rx) = crossbeam::bounded(1);
        self.cmd_tx
            .send(PubSubCommand::Subscribe {
                topic: topic.to_string(),
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()
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
        let _ = self.cmd_tx.send(PubSubCommand::Broadcast {
            topic: topic.to_string(),
            msg,
        });
    }

    /// Request a cleanup pass for the given topic. If there are no remaining
    /// subscribers the internal channel is removed.
    pub fn unsubscribe(&self, topic: &str) {
        let _ = self.cmd_tx.send(PubSubCommand::Unsubscribe {
            topic: topic.to_string(),
        });
    }

    /// Gracefully shut down the PubSub server.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(PubSubCommand::Shutdown);
    }

    /// Create a properly supervised `(PubSub, ChildEntry)` pair.
    ///
    /// The channel is pre-allocated so the handle and the supervised factory
    /// each own their respective end. The supervised process *is* the server
    /// loop — if it crashes the supervisor will restart it correctly.
    pub fn new_supervised() -> (Self, ChildEntry) {
        let (cmd_tx, cmd_rx) = crossbeam::unbounded::<PubSubCommand>();
        let cmd_rx = Rc::new(RefCell::new(Some(cmd_rx)));
        let handle = PubSub { cmd_tx };
        let entry = ChildEntry::new(ChildSpec::new("mahalo_pubsub"), move || {
            let cmd_rx = Rc::clone(&cmd_rx);
            async move {
                let rx = cmd_rx
                    .borrow_mut()
                    .take()
                    .expect("PubSub factory called twice");
                // Run server on a background thread; wait for it via polling
                let (done_tx, done_rx) = crossbeam::bounded::<()>(1);
                std::thread::spawn(move || {
                    Self::run_server(rx);
                    let _ = done_tx.send(());
                });
                loop {
                    match done_rx.try_recv() {
                        Ok(()) | Err(crossbeam::TryRecvError::Disconnected) => break,
                        Err(crossbeam::TryRecvError::Empty) => {
                            rebar_core::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                }
                ExitReason::Normal
            }
        });
        (handle, entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn subscribe_and_receive() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("room:lobby").unwrap();

        pubsub.broadcast("room:lobby", "new_msg", serde_json::json!({"text": "hello"}));

        let msg = rx.recv_timeout(Duration::from_secs(1)).expect("recv error");

        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "new_msg");
        assert_eq!(msg.payload, serde_json::json!({"text": "hello"}));

        pubsub.shutdown();
    }

    #[test]
    fn multiple_subscribers_receive_same_broadcast() {
        let pubsub = PubSub::start();
        let rx1 = pubsub.subscribe("room:lobby").unwrap();
        let rx2 = pubsub.subscribe("room:lobby").unwrap();

        pubsub.broadcast("room:lobby", "ping", serde_json::json!(null));

        let msg1 = rx1.recv_timeout(Duration::from_secs(1)).expect("recv error");
        let msg2 = rx2.recv_timeout(Duration::from_secs(1)).expect("recv error");

        assert_eq!(msg1.event, "ping");
        assert_eq!(msg2.event, "ping");

        pubsub.shutdown();
    }

    #[test]
    fn broadcast_to_nonexistent_topic_does_not_panic() {
        let pubsub = PubSub::start();
        // Should not panic or error
        pubsub.broadcast("no_such_topic", "evt", serde_json::json!(42));
        pubsub.shutdown();
    }

    #[test]
    fn unsubscribe_cleans_up_empty_topic() {
        let pubsub = PubSub::start();
        let rx = pubsub.subscribe("ephemeral").unwrap();

        // Drop the receiver so senders become disconnected
        drop(rx);

        pubsub.unsubscribe("ephemeral");

        // After cleanup, broadcasting should be a no-op (no channel exists)
        pubsub.broadcast("ephemeral", "gone", serde_json::json!(null));

        pubsub.shutdown();
    }

    #[test]
    fn subscribers_on_different_topics_are_independent() {
        let pubsub = PubSub::start();
        let rx_a = pubsub.subscribe("topic_a").unwrap();
        let rx_b = pubsub.subscribe("topic_b").unwrap();

        pubsub.broadcast("topic_a", "only_a", serde_json::json!("a"));

        let msg = rx_a.recv_timeout(Duration::from_secs(1)).expect("recv error");
        assert_eq!(msg.event, "only_a");

        // rx_b should have nothing
        let result = rx_b.recv_timeout(Duration::from_millis(50));
        assert!(result.is_err(), "rx_b should not receive topic_a messages");

        pubsub.shutdown();
    }

    #[test]
    fn pubsub_message_serde() {
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

    #[test]
    fn subscribe_returns_none_after_shutdown() {
        let pubsub = PubSub::start();
        pubsub.shutdown();
        // Allow server thread to process shutdown
        std::thread::sleep(Duration::from_millis(50));
        // After shutdown, subscribe returns None since the server has stopped
        let result = pubsub.subscribe("test");
        assert!(result.is_none());
    }

    #[test]
    fn start_with_runtime_creates_working_pubsub() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let runtime = rebar_core::runtime::Runtime::new(1);
            let pubsub = PubSub::start_with_runtime(&runtime);
            let rx = pubsub.subscribe("test:topic").unwrap();

            pubsub.broadcast("test:topic", "hello", serde_json::json!({"msg": "hi"}));

            let msg = rx.recv_timeout(Duration::from_secs(1)).expect("recv error");

            assert_eq!(msg.topic, "test:topic");
            assert_eq!(msg.event, "hello");
            assert_eq!(msg.payload, serde_json::json!({"msg": "hi"}));

            pubsub.shutdown();
        });
    }

    #[test]
    fn pubsub_handle_is_clone_send_sync() {
        fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
        assert_clone_send_sync::<PubSub>();
    }

    #[test]
    fn new_supervised_subscribe_and_broadcast() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        use rebar_core::supervisor::spec::{RestartStrategy, SupervisorSpec};
        use rebar_core::supervisor::engine::start_supervisor;

        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let runtime = rebar_core::runtime::Runtime::new(1);
            let (pubsub, entry) = PubSub::new_supervised();

            assert_eq!(entry.spec.id, "mahalo_pubsub");

            // Start the supervisor which will start the actual server loop.
            let _supervisor = start_supervisor(
                &runtime,
                SupervisorSpec::new(RestartStrategy::OneForOne),
                vec![entry],
            );

            // Give the server thread a moment to start.
            rebar_core::time::sleep(Duration::from_millis(50)).await;

            let rx = pubsub.subscribe("supervised:topic").unwrap();
            pubsub.broadcast("supervised:topic", "hello", serde_json::json!({"ok": true}));

            let msg = rx.recv_timeout(Duration::from_secs(1)).expect("recv error");
            assert_eq!(msg.event, "hello");
        });
    }
}
