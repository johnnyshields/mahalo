use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use mahalo_core::conn::AssignKey;
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
            if self.sender.send(WsMessage::Text(json.into())).is_err() {
                tracing::warn!(topic = %self.topic, event = %event, "push failed, client disconnected");
            }
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
            if self.sender.send(WsMessage::Text(json.into())).is_err() {
                tracing::warn!(topic = %self.topic, msg_ref = %msg_ref, "reply failed, client disconnected");
            }
        }
    }

    pub fn assign<K: AssignKey>(&mut self, value: K::Value) {
        self.assigns.insert(TypeId::of::<K>(), Box::new(value));
    }

    pub fn get_assign<K: AssignKey>(&self) -> Option<&K::Value> {
        self.assigns
            .get(&TypeId::of::<K>())
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

/// Handle a `phx_join` event: create a socket, call channel.join, subscribe to PubSub.
async fn handle_join(
    phoenix_msg: &PhoenixMessage,
    channel: &Arc<dyn Channel>,
    tx: &mpsc::UnboundedSender<WsMessage>,
    pubsub: &PubSub,
    joined_channels: &mut HashMap<String, (Arc<dyn Channel>, ChannelSocket)>,
) {
    let mut socket = ChannelSocket::new(phoenix_msg.topic.clone(), tx.clone(), pubsub.clone());
    match channel
        .join(&phoenix_msg.topic, &phoenix_msg.payload, &mut socket)
        .await
    {
        Ok(resp) => {
            let reply = Reply::ok(resp);
            if let Some(ref r) = phoenix_msg.msg_ref {
                socket.reply(r, &reply).await;
            }

            // Subscribe to PubSub and spawn forwarder task
            if let Some(mut pubsub_rx) = pubsub.subscribe(&phoenix_msg.topic).await {
                let sender_clone = tx.clone();
                let topic_clone = phoenix_msg.topic.clone();
                tokio::spawn(async move {
                    while let Ok(pubsub_msg) = pubsub_rx.recv().await {
                        let out = PhoenixMessage {
                            topic: topic_clone.clone(),
                            event: pubsub_msg.event.clone(),
                            payload: pubsub_msg.payload.clone(),
                            msg_ref: None,
                        };
                        if let Ok(json) = serde_json::to_string(&out) {
                            if sender_clone.send(WsMessage::Text(json.into())).is_err() {
                                break;
                            }
                        }
                    }
                });
            } else {
                tracing::warn!(
                    topic = %phoenix_msg.topic,
                    "PubSub subscribe failed, channel will not receive broadcasts"
                );
            }

            joined_channels.insert(phoenix_msg.topic.clone(), (Arc::clone(channel), socket));
        }
        Err(_) => {
            let reply = Reply::error(serde_json::json!({"reason": "join failed"}));
            if let Some(ref r) = phoenix_msg.msg_ref {
                socket.reply(r, &reply).await;
            }
        }
    }
}

/// Handle a `phx_leave` event: terminate the channel and send a reply.
async fn handle_leave(
    phoenix_msg: &PhoenixMessage,
    joined_channels: &mut HashMap<String, (Arc<dyn Channel>, ChannelSocket)>,
) {
    if let Some((channel, mut socket)) = joined_channels.remove(&phoenix_msg.topic) {
        channel.terminate("leave", &mut socket).await;
        if let Some(ref r) = phoenix_msg.msg_ref {
            let reply = Reply::ok(serde_json::json!({}));
            socket.reply(r, &reply).await;
        }
    }
}

/// Handle a heartbeat event: reply with an ok status.
fn handle_heartbeat(phoenix_msg: &PhoenixMessage, tx: &mpsc::UnboundedSender<WsMessage>) {
    let reply_msg = PhoenixMessage {
        topic: "phoenix".to_string(),
        event: "phx_reply".to_string(),
        payload: serde_json::json!({"status": "ok", "response": {}}),
        msg_ref: phoenix_msg.msg_ref.clone(),
    };
    if let Ok(json) = serde_json::to_string(&reply_msg) {
        if tx.send(WsMessage::Text(json.into())).is_err() {
            tracing::warn!("failed to send heartbeat reply, client disconnected");
        }
    }
}

/// Dispatch a regular event to the appropriate joined channel.
async fn dispatch_event(
    phoenix_msg: &PhoenixMessage,
    joined_channels: &mut HashMap<String, (Arc<dyn Channel>, ChannelSocket)>,
) {
    if let Some((channel, socket)) = joined_channels.get_mut(&phoenix_msg.topic) {
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
                let reply = Reply::error(serde_json::json!({"reason": "error"}));
                if let Some(ref r) = phoenix_msg.msg_ref {
                    socket.reply(r, &reply).await;
                }
            }
        }
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
                            handle_join(
                                &phoenix_msg,
                                channel,
                                &tx,
                                &pubsub,
                                &mut joined_channels,
                            )
                            .await;
                        }
                    }
                    "phx_leave" => {
                        handle_leave(&phoenix_msg, &mut joined_channels).await;
                    }
                    "heartbeat" => {
                        handle_heartbeat(&phoenix_msg, &tx);
                    }
                    _ => {
                        dispatch_event(&phoenix_msg, &mut joined_channels).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    // -- topic_matches ---------------------------------------------------------

    #[test]
    fn topic_matches_exact() {
        assert!(topic_matches("room:lobby", "room:lobby"));
    }

    #[test]
    fn topic_matches_wildcard() {
        assert!(topic_matches("room:*", "room:lobby"));
        assert!(topic_matches("room:*", "room:123"));
    }

    #[test]
    fn topic_matches_wildcard_no_match() {
        assert!(!topic_matches("room:*", "chat:lobby"));
    }

    #[test]
    fn topic_matches_exact_no_match() {
        assert!(!topic_matches("room:lobby", "room:other"));
    }

    // -- ChannelRouter ---------------------------------------------------------

    struct DummyChannel;

    #[async_trait]
    impl Channel for DummyChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Ok(serde_json::json!({}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Ok(None)
        }
    }

    #[test]
    fn channel_router_find_exact() {
        let router = ChannelRouter::new()
            .channel("room:lobby", Arc::new(DummyChannel));
        assert!(router.find("room:lobby").is_some());
        assert!(router.find("room:other").is_none());
    }

    #[test]
    fn channel_router_find_wildcard() {
        let router = ChannelRouter::new()
            .channel("room:*", Arc::new(DummyChannel));
        assert!(router.find("room:lobby").is_some());
        assert!(router.find("room:123").is_some());
        assert!(router.find("chat:lobby").is_none());
    }

    #[test]
    fn channel_router_empty_returns_none() {
        let router = ChannelRouter::new();
        assert!(router.find("anything").is_none());
    }

    // -- ChannelSocket assigns -------------------------------------------------

    struct UserId;
    impl AssignKey for UserId {
        type Value = u64;
    }

    struct UserName;
    impl AssignKey for UserName {
        type Value = String;
    }

    struct IsAdmin;
    impl AssignKey for IsAdmin {
        type Value = bool;
    }

    #[tokio::test]
    async fn channel_socket_assigns() {
        let pubsub = PubSub::start();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        socket.assign::<UserId>(42);
        socket.assign::<UserName>("hello".to_string());

        assert_eq!(socket.get_assign::<UserId>(), Some(&42));
        assert_eq!(socket.get_assign::<UserName>(), Some(&"hello".to_string()));
        assert_eq!(socket.get_assign::<IsAdmin>(), None);

        pubsub.shutdown();
    }

    // -- PhoenixMessage serde --------------------------------------------------

    #[test]
    fn phoenix_message_serialize() {
        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "new_msg".into(),
            payload: serde_json::json!({"text": "hi"}),
            msg_ref: Some("1".into()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["topic"], "room:lobby");
        assert_eq!(json["event"], "new_msg");
        assert_eq!(json["ref"], "1");
        assert_eq!(json["payload"]["text"], "hi");
    }

    #[test]
    fn phoenix_message_deserialize() {
        let json = r#"{"topic":"room:lobby","event":"phx_join","payload":{},"ref":"1"}"#;
        let msg: PhoenixMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "phx_join");
        assert_eq!(msg.msg_ref, Some("1".into()));
    }

    #[test]
    fn phoenix_message_deserialize_null_ref() {
        let json = r#"{"topic":"t","event":"e","payload":null,"ref":null}"#;
        let msg: PhoenixMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_ref, None);
    }

    #[test]
    fn phoenix_message_roundtrip() {
        let original = PhoenixMessage {
            topic: "room:1".into(),
            event: "update".into(),
            payload: serde_json::json!({"count": 5}),
            msg_ref: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PhoenixMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.topic, original.topic);
        assert_eq!(decoded.event, original.event);
        assert_eq!(decoded.payload, original.payload);
        assert_eq!(decoded.msg_ref, original.msg_ref);
    }

    // -- ChannelSocket push/reply/broadcast ------------------------------------

    #[tokio::test]
    async fn channel_socket_push() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        socket.push("event", &serde_json::json!({"a": 1})).await;

        let msg = rx.try_recv().expect("should receive a message");
        match msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "event");
                assert_eq!(parsed["payload"]["a"], 1);
                assert_eq!(parsed["topic"], "test:topic");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_push_closed_sender() {
        let pubsub = PubSub::start();
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        drop(rx); // close the receiver
        // should not panic, just logs a warning
        socket.push("event", &serde_json::json!({})).await;

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_broadcast() {
        let pubsub = PubSub::start();
        let (tx, _rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        // Subscribe before broadcasting
        let mut sub_rx = pubsub.subscribe("test:topic").await.expect("subscribe should succeed");

        socket.broadcast("evt", serde_json::json!({"key": "val"}));

        let msg = sub_rx.recv().await.expect("should receive broadcast");
        assert_eq!(msg.event, "evt");
        assert_eq!(msg.payload, serde_json::json!({"key": "val"}));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_reply() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let reply = Reply::ok(serde_json::json!({"data": "ok"}));
        socket.reply("ref1", &reply).await;

        let msg = rx.try_recv().expect("should receive reply");
        match msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["ref"], "ref1");
                assert_eq!(parsed["payload"]["status"], "ok");
                assert_eq!(parsed["payload"]["response"]["data"], "ok");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_reply_closed() {
        let pubsub = PubSub::start();
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        drop(rx);
        // should not panic
        socket.reply("ref1", &Reply::ok(serde_json::json!({}))).await;

        pubsub.shutdown();
    }

    // -- ChannelRouter default -------------------------------------------------

    #[test]
    fn channel_router_default() {
        let router = ChannelRouter::default();
        assert!(router.find("anything").is_none());
    }

    // -- handle_heartbeat ------------------------------------------------------

    #[test]
    fn handle_heartbeat_sends_reply() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let msg = PhoenixMessage {
            topic: "phoenix".into(),
            event: "heartbeat".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("hb-1".into()),
        };

        handle_heartbeat(&msg, &tx);

        let ws_msg = rx.try_recv().expect("should receive heartbeat reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["topic"], "phoenix");
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "ok");
                assert_eq!(parsed["ref"], "hb-1");
            }
            other => panic!("expected Text, got {:?}", other),
        }
    }

    // -- handle_join -----------------------------------------------------------

    struct OkChannel;

    #[async_trait]
    impl Channel for OkChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Ok(serde_json::json!({"joined": true}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Ok(None)
        }
    }

    struct FailChannel;

    #[async_trait]
    impl Channel for FailChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Err(crate::channel::ChannelError::NotAuthorized)
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn handle_join_success() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(OkChannel);
        let mut joined_channels = HashMap::new();

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "phx_join".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("join-1".into()),
        };

        handle_join(&msg, &channel, &tx, &pubsub, &mut joined_channels).await;

        // Should have sent an ok reply
        let ws_msg = rx.try_recv().expect("should receive join reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "ok");
                assert_eq!(parsed["payload"]["response"]["joined"], true);
                assert_eq!(parsed["ref"], "join-1");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        // Channel should be in joined_channels
        assert!(joined_channels.contains_key("room:lobby"));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn handle_join_failure() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(FailChannel);
        let mut joined_channels = HashMap::new();

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "phx_join".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("join-2".into()),
        };

        handle_join(&msg, &channel, &tx, &pubsub, &mut joined_channels).await;

        // Should have sent an error reply
        let ws_msg = rx.try_recv().expect("should receive error reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "error");
                assert_eq!(parsed["ref"], "join-2");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        // Channel should NOT be in joined_channels
        assert!(!joined_channels.contains_key("room:lobby"));

        pubsub.shutdown();
    }

    // -- handle_leave ----------------------------------------------------------

    #[tokio::test]
    async fn handle_leave_known_topic() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(DummyChannel);
        let socket = ChannelSocket::new("room:lobby".into(), tx.clone(), pubsub.clone());

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "phx_leave".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("leave-1".into()),
        };

        handle_leave(&msg, &mut joined_channels).await;

        assert!(!joined_channels.contains_key("room:lobby"));

        // Should receive ok reply
        let ws_msg = rx.try_recv().expect("should receive leave reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "ok");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn handle_leave_unknown_topic() {
        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();

        let msg = PhoenixMessage {
            topic: "room:unknown".into(),
            event: "phx_leave".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("leave-2".into()),
        };

        handle_leave(&msg, &mut joined_channels).await;
        // no panic = success
    }

    // -- dispatch_event --------------------------------------------------------

    struct ReplyChannel;

    #[async_trait]
    impl Channel for ReplyChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Ok(serde_json::json!({}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Ok(Some(Reply::ok(serde_json::json!({"echo": true}))))
        }
    }

    struct ErrorChannel;

    #[async_trait]
    impl Channel for ErrorChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Ok(serde_json::json!({}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Err(crate::channel::ChannelError::Internal("test error".into()))
        }
    }

    #[tokio::test]
    async fn dispatch_event_with_reply() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(ReplyChannel);
        let socket = ChannelSocket::new("room:lobby".into(), tx.clone(), pubsub.clone());

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "custom_event".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("ev-1".into()),
        };

        dispatch_event(&msg, &mut joined_channels).await;

        let ws_msg = rx.try_recv().expect("should receive reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "ok");
                assert_eq!(parsed["payload"]["response"]["echo"], true);
                assert_eq!(parsed["ref"], "ev-1");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn dispatch_event_no_reply() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(DummyChannel); // returns Ok(None)
        let socket = ChannelSocket::new("room:lobby".into(), tx.clone(), pubsub.clone());

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "silent_event".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("ev-2".into()),
        };

        dispatch_event(&msg, &mut joined_channels).await;

        // No message should be sent
        assert!(rx.try_recv().is_err());

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn dispatch_event_error() {
        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(ErrorChannel);
        let socket = ChannelSocket::new("room:lobby".into(), tx.clone(), pubsub.clone());

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "bad_event".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("ev-3".into()),
        };

        dispatch_event(&msg, &mut joined_channels).await;

        let ws_msg = rx.try_recv().expect("should receive error reply");
        match ws_msg {
            WsMessage::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "phx_reply");
                assert_eq!(parsed["payload"]["status"], "error");
                assert_eq!(parsed["ref"], "ev-3");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    // -- handle_websocket integration test ------------------------------------

    /// A channel that echoes back events for testing handle_websocket end-to-end.
    struct EchoChannel;

    #[async_trait]
    impl Channel for EchoChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, crate::channel::ChannelError> {
            Ok(serde_json::json!({"status": "connected"}))
        }

        async fn handle_in(
            &self,
            event: &str,
            payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, crate::channel::ChannelError> {
            Ok(Some(Reply::ok(serde_json::json!({
                "echo_event": event,
                "echo_payload": payload,
            }))))
        }
    }

    #[tokio::test]
    async fn handle_websocket_full_lifecycle() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;

        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("room:*", Arc::new(EchoChannel)),
        );

        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                async move { ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps)) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Connect via tokio-tungstenite
        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        // 1. Send heartbeat
        let heartbeat = serde_json::json!({
            "topic": "phoenix",
            "event": "heartbeat",
            "payload": {},
            "ref": "hb-1"
        });
        ws_stream
            .send(TungsteniteMsg::Text(heartbeat.to_string().into()))
            .await
            .unwrap();

        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["ref"], "hb-1");

        // 2. Join a channel
        let join_msg = serde_json::json!({
            "topic": "room:lobby",
            "event": "phx_join",
            "payload": {},
            "ref": "join-1"
        });
        ws_stream
            .send(TungsteniteMsg::Text(join_msg.to_string().into()))
            .await
            .unwrap();

        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["payload"]["response"]["status"], "connected");
        assert_eq!(parsed["ref"], "join-1");

        // 3. Send a custom event
        let custom_msg = serde_json::json!({
            "topic": "room:lobby",
            "event": "new_msg",
            "payload": {"text": "hello"},
            "ref": "msg-1"
        });
        ws_stream
            .send(TungsteniteMsg::Text(custom_msg.to_string().into()))
            .await
            .unwrap();

        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["payload"]["response"]["echo_event"], "new_msg");
        assert_eq!(parsed["payload"]["response"]["echo_payload"]["text"], "hello");
        assert_eq!(parsed["ref"], "msg-1");

        // 4. Send invalid JSON (should be ignored, no crash)
        ws_stream
            .send(TungsteniteMsg::Text("not json".to_string().into()))
            .await
            .unwrap();

        // 5. Leave the channel
        let leave_msg = serde_json::json!({
            "topic": "room:lobby",
            "event": "phx_leave",
            "payload": {},
            "ref": "leave-1"
        });
        ws_stream
            .send(TungsteniteMsg::Text(leave_msg.to_string().into()))
            .await
            .unwrap();

        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["ref"], "leave-1");

        // 6. Send event to unmatched topic (no crash)
        let unmatched = serde_json::json!({
            "topic": "nonexistent:topic",
            "event": "phx_join",
            "payload": {},
            "ref": "x"
        });
        ws_stream
            .send(TungsteniteMsg::Text(unmatched.to_string().into()))
            .await
            .unwrap();

        // 7. Close
        ws_stream.close(None).await.unwrap();

        server.abort();
        pubsub.shutdown();
    }
}
