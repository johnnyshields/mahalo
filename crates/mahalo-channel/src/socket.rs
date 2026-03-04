use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use mahalo_pubsub::PubSub;

use crate::channel::{Channel, Reply};

/// Phoenix-compatible wire format.
#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixMessage {
    pub topic: String,
    pub event: String,
    pub payload: Value,
    #[serde(rename = "ref")]
    pub msg_ref: Option<String>,
}

/// State for a single channel connection.
pub struct ChannelSocket {
    pub topic: String,
    assigns: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    sender: mpsc::UnboundedSender<WsMessage>,
    pubsub: PubSub,
}

impl ChannelSocket {
    pub fn new(topic: String, sender: mpsc::UnboundedSender<WsMessage>, pubsub: PubSub) -> Self {
        Self {
            topic,
            assigns: HashMap::new(),
            sender,
            pubsub,
        }
    }

    /// Push a message to this client.
    pub async fn push(&self, event: &str, payload: &Value) {
        let msg = PhoenixMessage {
            topic: self.topic.clone(),
            event: event.to_string(),
            payload: payload.clone(),
            msg_ref: None,
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = self.sender.send(WsMessage::Text(json.into()));
        }
    }

    /// Broadcast to all subscribers on this topic via PubSub.
    pub fn broadcast(&self, event: impl Into<String>, payload: Value) {
        self.pubsub.broadcast(&self.topic, event, payload);
    }

    /// Reply to a specific message ref.
    pub async fn reply(&self, msg_ref: &str, reply: &Reply) {
        let msg = PhoenixMessage {
            topic: self.topic.clone(),
            event: "phx_reply".to_string(),
            payload: serde_json::json!({
                "status": reply.status,
                "response": reply.payload,
            }),
            msg_ref: Some(msg_ref.to_string()),
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = self.sender.send(WsMessage::Text(json.into()));
        }
    }

    pub fn assign<T: Send + Sync + 'static>(&mut self, value: T) {
        self.assigns.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn get_assign<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.assigns
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }
}

/// Channel router mapping topic patterns to channel implementations.
pub struct ChannelRouter {
    routes: Vec<(String, Arc<dyn Channel>)>,
}

impl ChannelRouter {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Register a channel for a topic pattern (e.g., "room:*").
    pub fn channel(mut self, topic_pattern: &str, handler: Arc<dyn Channel>) -> Self {
        self.routes.push((topic_pattern.to_string(), handler));
        self
    }

    /// Find a channel matching the given topic.
    pub fn find(&self, topic: &str) -> Option<&Arc<dyn Channel>> {
        for (pattern, channel) in &self.routes {
            if topic_matches(pattern, topic) {
                return Some(channel);
            }
        }
        None
    }
}

impl Default for ChannelRouter {
    fn default() -> Self {
        Self::new()
    }
}

fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern.ends_with(":*") {
        let prefix = &pattern[..pattern.len() - 1]; // "room:"
        topic.starts_with(prefix)
    } else {
        pattern == topic
    }
}

/// Handle a WebSocket connection. This should be called from an Axum handler.
/// Each connection becomes its own async task (would be a rebar process in full integration).
pub async fn handle_websocket(
    ws: WebSocket,
    channel_router: Arc<ChannelRouter>,
    pubsub: PubSub,
) {
    let (ws_sender, mut ws_receiver) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();

    // Spawn a task to forward messages from the channel to the WebSocket
    let send_task = tokio::spawn(async move {
        let mut ws_sender = ws_sender;
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Track joined channels: topic -> ChannelSocket
    let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();

    // Main receive loop
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            WsMessage::Text(ref text) => {
                let phoenix_msg: PhoenixMessage = match serde_json::from_str(text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                match phoenix_msg.event.as_str() {
                    "phx_join" => {
                        if let Some(channel) = channel_router.find(&phoenix_msg.topic) {
                            let mut socket = ChannelSocket::new(
                                phoenix_msg.topic.clone(),
                                tx.clone(),
                                pubsub.clone(),
                            );
                            match channel
                                .join(&phoenix_msg.topic, &phoenix_msg.payload, &mut socket)
                                .await
                            {
                                Ok(resp) => {
                                    let reply = Reply::ok(resp);
                                    if let Some(ref r) = phoenix_msg.msg_ref {
                                        socket.reply(r, &reply).await;
                                    }
                                    // Subscribe to PubSub for this topic
                                    let mut pubsub_rx =
                                        pubsub.subscribe(&phoenix_msg.topic).await;
                                    let sender_clone = tx.clone();
                                    let topic_clone = phoenix_msg.topic.clone();

                                    // Spawn task to forward PubSub messages
                                    tokio::spawn(async move {
                                        while let Ok(pubsub_msg) = pubsub_rx.recv().await {
                                            let out = PhoenixMessage {
                                                topic: topic_clone.clone(),
                                                event: pubsub_msg.event.clone(),
                                                payload: pubsub_msg.payload.clone(),
                                                msg_ref: None,
                                            };
                                            if let Ok(json) = serde_json::to_string(&out) {
                                                if sender_clone
                                                    .send(WsMessage::Text(json.into()))
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        }
                                    });

                                    joined_channels.insert(
                                        phoenix_msg.topic.clone(),
                                        (Arc::clone(channel), socket),
                                    );
                                }
                                Err(_) => {
                                    let reply = Reply::error(
                                        serde_json::json!({"reason": "join failed"}),
                                    );
                                    if let Some(ref r) = phoenix_msg.msg_ref {
                                        socket.reply(r, &reply).await;
                                    }
                                }
                            }
                        }
                    }
                    "phx_leave" => {
                        if let Some((channel, mut socket)) =
                            joined_channels.remove(&phoenix_msg.topic)
                        {
                            channel.terminate("leave", &mut socket).await;
                            if let Some(ref r) = phoenix_msg.msg_ref {
                                let reply = Reply::ok(serde_json::json!({}));
                                socket.reply(r, &reply).await;
                            }
                        }
                    }
                    "heartbeat" => {
                        let reply_msg = PhoenixMessage {
                            topic: "phoenix".to_string(),
                            event: "phx_reply".to_string(),
                            payload: serde_json::json!({"status": "ok", "response": {}}),
                            msg_ref: phoenix_msg.msg_ref,
                        };
                        if let Ok(json) = serde_json::to_string(&reply_msg) {
                            let _ = tx.send(WsMessage::Text(json.into()));
                        }
                    }
                    _ => {
                        // Regular event - dispatch to joined channel
                        if let Some((channel, socket)) =
                            joined_channels.get_mut(&phoenix_msg.topic)
                        {
                            match channel
                                .handle_in(&phoenix_msg.event, &phoenix_msg.payload, socket)
                                .await
                            {
                                Ok(Some(reply)) => {
                                    if let Some(ref r) = phoenix_msg.msg_ref {
                                        socket.reply(r, &reply).await;
                                    }
                                }
                                Ok(None) => {}
                                Err(_) => {
                                    let reply = Reply::error(
                                        serde_json::json!({"reason": "error"}),
                                    );
                                    if let Some(ref r) = phoenix_msg.msg_ref {
                                        socket.reply(r, &reply).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    // Terminate all joined channels
    for (_, (channel, mut socket)) in joined_channels {
        channel.terminate("disconnect", &mut socket).await;
    }

    send_task.abort();
}
