use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, trace};

/// A single presence entry representing one connected entity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PresenceEntry {
    pub key: String,
    pub meta: Value,
    pub node_id: u64,
}

/// A diff representing changes to presence state for a topic.
#[derive(Debug, Clone)]
pub struct PresenceDiff {
    pub topic: String,
    pub joins: Vec<PresenceEntry>,
    pub leaves: Vec<PresenceEntry>,
}

/// Internal command for the PresenceTracker.
pub(crate) enum TrackerCommand {
    Track {
        topic: String,
        entry: PresenceEntry,
    },
    Untrack {
        topic: String,
        key: String,
    },
    List {
        topic: String,
        reply: tokio::sync::oneshot::Sender<Vec<PresenceEntry>>,
    },
    Subscribe {
        topic: String,
        reply: tokio::sync::oneshot::Sender<mpsc::UnboundedReceiver<PresenceDiff>>,
    },
    Shutdown,
}

/// Manages presence state across topics with diff broadcasting.
///
/// Runs as a background tokio task. Use [`Presence`](crate::presence::Presence)
/// to interact with it.
pub struct PresenceTracker {
    cmd_tx: mpsc::UnboundedSender<TrackerCommand>,
}

impl PresenceTracker {
    /// Start the tracker.
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::run(cmd_rx));
        Self { cmd_tx }
    }

    async fn run(mut cmd_rx: mpsc::UnboundedReceiver<TrackerCommand>) {
        // topic -> list of entries
        let mut state: HashMap<String, Vec<PresenceEntry>> = HashMap::new();
        // topic -> list of subscriber senders
        let mut subscribers: HashMap<String, Vec<mpsc::UnboundedSender<PresenceDiff>>> =
            HashMap::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TrackerCommand::Track { topic, entry } => {
                    let entries = state.entry(topic.clone()).or_default();
                    // Upsert by key
                    if let Some(existing) = entries.iter_mut().find(|e| e.key == entry.key) {
                        *existing = entry.clone();
                    } else {
                        entries.push(entry.clone());
                    }
                    let diff = PresenceDiff {
                        topic: topic.clone(),
                        joins: vec![entry],
                        leaves: vec![],
                    };
                    Self::notify_subscribers(&mut subscribers, &topic, diff);
                    trace!(topic = %topic, "tracked presence entry");
                }
                TrackerCommand::Untrack { topic, key } => {
                    let mut diff_leaves = vec![];
                    if let Some(entries) = state.get_mut(&topic) {
                        let pos = entries.iter().position(|e| e.key == key);
                        if let Some(idx) = pos {
                            diff_leaves.push(entries.remove(idx));
                        }
                    }
                    if !diff_leaves.is_empty() {
                        let diff = PresenceDiff {
                            topic: topic.clone(),
                            joins: vec![],
                            leaves: diff_leaves,
                        };
                        Self::notify_subscribers(&mut subscribers, &topic, diff);
                    }
                    trace!(topic = %topic, key = %key, "untracked presence entry");
                }
                TrackerCommand::List { topic, reply } => {
                    let entries = state.get(&topic).cloned().unwrap_or_default();
                    let _ = reply.send(entries);
                }
                TrackerCommand::Subscribe { topic, reply } => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    subscribers.entry(topic).or_default().push(tx);
                    let _ = reply.send(rx);
                }
                TrackerCommand::Shutdown => break,
            }
        }
        debug!("PresenceTracker shutting down");
    }

    fn notify_subscribers(
        subscribers: &mut HashMap<String, Vec<mpsc::UnboundedSender<PresenceDiff>>>,
        topic: &str,
        diff: PresenceDiff,
    ) {
        if let Some(subs) = subscribers.get_mut(topic) {
            subs.retain(|tx| tx.send(diff.clone()).is_ok());
        }
    }

    pub(crate) fn cmd_tx(&self) -> &mpsc::UnboundedSender<TrackerCommand> {
        &self.cmd_tx
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(TrackerCommand::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    fn entry(key: &str, meta: Value) -> PresenceEntry {
        PresenceEntry {
            key: key.into(),
            meta,
            node_id: 1,
        }
    }

    #[tokio::test]
    async fn track_and_list() {
        let tracker = PresenceTracker::start();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tracker
            .cmd_tx()
            .send(TrackerCommand::Track {
                topic: "room:lobby".into(),
                entry: entry("user:1", serde_json::json!({"name": "Alice"})),
            })
            .unwrap();
        tracker
            .cmd_tx()
            .send(TrackerCommand::List {
                topic: "room:lobby".into(),
                reply: tx,
            })
            .unwrap();
        let entries = timeout(Duration::from_secs(1), rx).await.unwrap().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "user:1");
        tracker.shutdown();
    }

    #[tokio::test]
    async fn untrack_removes_entry() {
        let tracker = PresenceTracker::start();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tracker
            .cmd_tx()
            .send(TrackerCommand::Track {
                topic: "room:lobby".into(),
                entry: entry("user:1", serde_json::json!({})),
            })
            .unwrap();
        tracker
            .cmd_tx()
            .send(TrackerCommand::Untrack {
                topic: "room:lobby".into(),
                key: "user:1".into(),
            })
            .unwrap();
        tracker
            .cmd_tx()
            .send(TrackerCommand::List {
                topic: "room:lobby".into(),
                reply: tx,
            })
            .unwrap();
        let entries = timeout(Duration::from_secs(1), rx).await.unwrap().unwrap();
        assert_eq!(entries.len(), 0);
        tracker.shutdown();
    }
}
