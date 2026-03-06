use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::tracker::{PresenceDiff, PresenceEntry, PresenceTracker, TrackerCommand};

/// User-facing handle to the presence tracker.
#[derive(Clone)]
pub struct Presence {
    tracker: Arc<PresenceTracker>,
    node_id: u64,
}

impl Presence {
    /// Create a new Presence handle wrapping a running PresenceTracker.
    pub fn new(node_id: u64) -> Self {
        Self {
            tracker: Arc::new(PresenceTracker::start()),
            node_id,
        }
    }

    /// Track a presence entry for the given topic and key.
    pub fn track(&self, topic: &str, key: &str, meta: Value) {
        let _ = self.tracker.cmd_tx().send(TrackerCommand::Track {
            topic: topic.to_string(),
            entry: PresenceEntry {
                key: key.to_string(),
                meta,
                node_id: self.node_id,
            },
        });
    }

    /// Remove a presence entry.
    pub fn untrack(&self, topic: &str, key: &str) {
        let _ = self.tracker.cmd_tx().send(TrackerCommand::Untrack {
            topic: topic.to_string(),
            key: key.to_string(),
        });
    }

    /// List all current presence entries for a topic.
    pub async fn list(&self, topic: &str) -> Vec<PresenceEntry> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tracker.cmd_tx().send(TrackerCommand::List {
            topic: topic.to_string(),
            reply: reply_tx,
        });
        reply_rx.await.unwrap_or_default()
    }

    /// Subscribe to presence diffs for a topic.
    ///
    /// Returns a receiver that yields a `PresenceDiff` whenever entries join or leave.
    pub async fn subscribe_diff(&self, topic: &str) -> mpsc::UnboundedReceiver<PresenceDiff> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tracker.cmd_tx().send(TrackerCommand::Subscribe {
            topic: topic.to_string(),
            reply: reply_tx,
        });
        reply_rx
            .await
            .unwrap_or_else(|_| mpsc::unbounded_channel().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn track_and_list() {
        let presence = Presence::new(1);
        presence.track(
            "room:lobby",
            "user:1",
            serde_json::json!({"name": "Alice"}),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        let entries = presence.list("room:lobby").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "user:1");
        assert_eq!(entries[0].node_id, 1);
    }

    #[tokio::test]
    async fn untrack_and_list() {
        let presence = Presence::new(1);
        presence.track("room:1", "user:a", serde_json::json!({}));
        tokio::time::sleep(Duration::from_millis(10)).await;
        presence.untrack("room:1", "user:a");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let entries = presence.list("room:1").await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn subscribe_diff_receives_join() {
        let presence = Presence::new(1);
        let mut diff_rx = presence.subscribe_diff("room:lobby").await;

        presence.track("room:lobby", "user:2", serde_json::json!({"x": 1}));

        let diff = tokio::time::timeout(Duration::from_secs(1), diff_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(diff.topic, "room:lobby");
        assert_eq!(diff.joins.len(), 1);
        assert_eq!(diff.joins[0].key, "user:2");
        assert!(diff.leaves.is_empty());
    }

    #[tokio::test]
    async fn subscribe_diff_receives_leave() {
        let presence = Presence::new(1);
        // Track first so there is something to untrack.
        presence.track("room:lobby", "user:5", serde_json::json!({"name": "Eve"}));
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Subscribe after the initial track so we don't receive the join diff.
        let mut diff_rx = presence.subscribe_diff("room:lobby").await;

        presence.untrack("room:lobby", "user:5");

        let diff = tokio::time::timeout(Duration::from_secs(1), diff_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(diff.topic, "room:lobby");
        assert!(diff.joins.is_empty());
        assert_eq!(diff.leaves.len(), 1);
        assert_eq!(diff.leaves[0].key, "user:5");
    }

    #[tokio::test]
    async fn multiple_topics_are_isolated() {
        let presence = Presence::new(1);
        presence.track("room:a", "user:1", serde_json::json!({"room": "a"}));
        presence.track("room:b", "user:2", serde_json::json!({"room": "b"}));
        tokio::time::sleep(Duration::from_millis(10)).await;

        let entries_a = presence.list("room:a").await;
        let entries_b = presence.list("room:b").await;

        assert_eq!(entries_a.len(), 1);
        assert_eq!(entries_a[0].key, "user:1");

        assert_eq!(entries_b.len(), 1);
        assert_eq!(entries_b[0].key, "user:2");
    }

    #[tokio::test]
    async fn untrack_nonexistent_key_is_silent() {
        let presence = Presence::new(1);
        // Untrack a key that was never tracked — should not panic.
        presence.untrack("room:ghost", "user:999");
        tokio::time::sleep(Duration::from_millis(10)).await;

        let entries = presence.list("room:ghost").await;
        assert!(entries.is_empty());
    }
}
