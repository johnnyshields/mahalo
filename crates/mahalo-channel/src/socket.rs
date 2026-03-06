use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Notify};

/// The type carried over the internal channel from ChannelSocket → WebSocket sink.
/// Decoupled from any specific WebSocket library (axum, tungstenite, etc.).
pub type WsSendItem = String;

use async_trait::async_trait;
use rebar_core::gen_server::{self, CastReply, CallReply, GenServer, GenServerContext, InfoReply, From as GsFrom};
use rebar_core::process::ProcessId;
use rebar_core::runtime::Runtime;

use mahalo_core::conn::AssignKey;
use mahalo_pubsub::PubSub;

use crate::channel::{Channel, Reply};

/// Phoenix join event name.
pub const PHX_JOIN: &str = "phx_join";
/// Phoenix leave event name.
pub const PHX_LEAVE: &str = "phx_leave";
/// Phoenix reply event name.
pub const PHX_REPLY: &str = "phx_reply";
/// Phoenix heartbeat event name.
pub const HEARTBEAT: &str = "heartbeat";

/// Phoenix-compatible wire format.
#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixMessage {
    pub topic: String,
    pub event: String,
    pub payload: Value,
    #[serde(rename = "ref")]
    pub msg_ref: Option<String>,
}

/// A typed handle to a channel's GenServer process that can be used for
/// inter-channel messaging. Actix equivalent: `Addr<A>`.
#[derive(Clone)]
pub struct ChannelAddr {
    pid: ProcessId,
    runtime: Arc<Runtime>,
}

impl ChannelAddr {
    /// Send a synthetic event to the channel as if it came from a PubSub message.
    pub async fn send_event(&self, topic: &str, event: &str, payload: Value) {
        let msg = PhoenixMessage {
            topic: topic.to_string(),
            event: event.to_string(),
            payload,
            msg_ref: None,
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let cast_val = rmpv::Value::String(json.into());
            let _ = gen_server::cast_from_runtime(&self.runtime, self.pid, cast_val).await;
        }
    }
}

/// Prefix for timer messages routed through the GenServer mailbox.
const TIMER_PREFIX: &str = "__timer:";
/// Separator between topic and event in timer keys. Uses null byte to avoid
/// conflicts with colons in Phoenix topic names (e.g., "room:lobby").
const TIMER_SEP: char = '\0';

/// State for a single channel connection.
pub struct ChannelSocket {
    pub topic: String,
    assigns: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    sender: mpsc::UnboundedSender<WsSendItem>,
    pubsub: PubSub,
    /// Set when running inside a GenServer process — enables timer scheduling.
    self_pid: Option<ProcessId>,
    runtime: Option<Arc<Runtime>>,
}

impl ChannelSocket {
    pub fn new(topic: String, sender: mpsc::UnboundedSender<WsSendItem>, pubsub: PubSub) -> Self {
        Self {
            topic,
            assigns: HashMap::new(),
            sender,
            pubsub,
            self_pid: None,
            runtime: None,
        }
    }

    /// Create a ChannelSocket with GenServer context for timer scheduling.
    pub fn new_with_pid(
        topic: String,
        sender: mpsc::UnboundedSender<WsSendItem>,
        pubsub: PubSub,
        self_pid: ProcessId,
        runtime: Arc<Runtime>,
    ) -> Self {
        Self {
            topic,
            assigns: HashMap::new(),
            sender,
            pubsub,
            self_pid: Some(self_pid),
            runtime: Some(runtime),
        }
    }

    /// Schedule a one-shot synthetic `handle_info` with the given event after `delay`.
    /// Actix equivalent: `Context::run_later()`.
    ///
    /// The event is delivered to the channel as a timer info message. No-op if
    /// the socket is not backed by a GenServer process.
    pub fn run_later(&self, delay: Duration, event: impl Into<String>) {
        if let (Some(pid), Some(runtime)) = (self.self_pid, &self.runtime) {
            let runtime = Arc::clone(runtime);
            let key = format!("{TIMER_PREFIX}{}{TIMER_SEP}{}", self.topic, event.into());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let msg = rmpv::Value::String(rmpv::Utf8String::from(key));
                let _ = runtime.send(pid, msg).await;
            });
        }
    }

    /// Schedule a recurring synthetic `handle_info` with the given event every `interval`.
    /// Actix equivalent: `Context::run_interval()`.
    ///
    /// Continues until the GenServer is stopped or the runtime is dropped. No-op if
    /// the socket is not backed by a GenServer process.
    pub fn run_interval(&self, interval: Duration, event: impl Into<String>) {
        if let (Some(pid), Some(runtime)) = (self.self_pid, &self.runtime) {
            let runtime = Arc::clone(runtime);
            let key = format!("{TIMER_PREFIX}{}{TIMER_SEP}{}", self.topic, event.into());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let msg = rmpv::Value::String(rmpv::Utf8String::from(key.clone()));
                    if runtime.send(pid, msg).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    /// Returns a typed handle to this channel's GenServer process.
    /// Available for inter-channel messaging from within channel callbacks.
    /// Returns `None` when not backed by a GenServer process.
    pub fn addr(&self) -> Option<ChannelAddr> {
        match (self.self_pid, &self.runtime) {
            (Some(pid), Some(runtime)) => Some(ChannelAddr {
                pid,
                runtime: Arc::clone(runtime),
            }),
            _ => None,
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
        if let Ok(json) = serde_json::to_string(&msg)
            && self.sender.send(json).is_err()
        {
            tracing::warn!(topic = %self.topic, event = %event, "push failed, client disconnected");
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
            event: PHX_REPLY.to_string(),
            payload: serde_json::json!({
                "status": reply.status,
                "response": reply.payload,
            }),
            msg_ref: Some(msg_ref.to_string()),
        };
        if let Ok(json) = serde_json::to_string(&msg)
            && self.sender.send(json).is_err()
        {
            tracing::warn!(topic = %self.topic, msg_ref = %msg_ref, "reply failed, client disconnected");
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

/// Handle a `phx_join` event: create a socket with GenServer context,
/// call channel.join, subscribe to PubSub (forwarding to GenServer mailbox).
async fn handle_join(
    phoenix_msg: &PhoenixMessage,
    channel: &Arc<dyn Channel>,
    tx: &mpsc::UnboundedSender<WsSendItem>,
    pubsub: &PubSub,
    joined_channels: &mut HashMap<String, (Arc<dyn Channel>, ChannelSocket)>,
    self_pid: ProcessId,
    runtime: &Arc<Runtime>,
) {
    let mut socket = ChannelSocket::new_with_pid(
        phoenix_msg.topic.clone(),
        tx.clone(),
        pubsub.clone(),
        self_pid,
        Arc::clone(runtime),
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

            // Call started() lifecycle hook after join reply is sent.
            channel.started(&mut socket).await;

            // Subscribe to PubSub and spawn forwarder that sends to GenServer mailbox
            if let Some(mut pubsub_rx) = pubsub.subscribe(&phoenix_msg.topic).await {
                let runtime = Arc::clone(runtime);
                let pid = self_pid;
                tokio::spawn(async move {
                    while let Ok(pubsub_msg) = pubsub_rx.recv().await {
                        if let Ok(json) = serde_json::to_string(&pubsub_msg) {
                            let msg = rmpv::Value::String(rmpv::Utf8String::from(json));
                            if runtime.send(pid, msg).await.is_err() {
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
fn handle_heartbeat(phoenix_msg: &PhoenixMessage, tx: &mpsc::UnboundedSender<WsSendItem>) {
    let reply_msg = PhoenixMessage {
        topic: "phoenix".to_string(),
        event: PHX_REPLY.to_string(),
        payload: serde_json::json!({"status": "ok", "response": {}}),
        msg_ref: phoenix_msg.msg_ref.clone(),
    };
    if let Ok(json) = serde_json::to_string(&reply_msg)
        && tx.send(json).is_err()
    {
        tracing::warn!("failed to send heartbeat reply, client disconnected");
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

/// Spawn a task that forwards JSON strings from an unbounded channel to a WebSocket sink.
/// Wraps each `String` as `WsMessage::Text` before sending.
/// Returns a JoinHandle that can be aborted on disconnect.
fn spawn_ws_forwarder(
    ws_sender: futures::stream::SplitSink<WebSocket, WsMessage>,
    rx: mpsc::UnboundedReceiver<WsSendItem>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ws_sender = ws_sender;
        let mut rx = rx;
        while let Some(json) = rx.recv().await {
            if ws_sender.send(WsMessage::Text(json.into())).await.is_err() {
                break;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// GenServer-based WebSocket connection
// ---------------------------------------------------------------------------

/// GenServer implementation for a single WebSocket channel connection.
///
/// Public so that alternative transports (e.g. io_uring raw TCP) can start
/// the same GenServer without going through the axum WebSocket path.
pub struct ChannelConnectionServer {
    channel_router: Arc<ChannelRouter>,
    pubsub: PubSub,
    ws_tx: mpsc::UnboundedSender<WsSendItem>,
    runtime: Arc<Runtime>,
    /// Signaled after each `handle_cast` completes, so the io_uring event loop
    /// can drain replies without arbitrary yield loops.
    cast_notify: Arc<Notify>,
}

impl ChannelConnectionServer {
    /// Create a new ChannelConnectionServer.
    pub fn new(
        channel_router: Arc<ChannelRouter>,
        pubsub: PubSub,
        ws_tx: mpsc::UnboundedSender<WsSendItem>,
        runtime: Arc<Runtime>,
    ) -> Self {
        Self::with_notify(channel_router, pubsub, ws_tx, runtime, Arc::new(Notify::new()))
    }

    /// Create a ChannelConnectionServer with a shared `Notify` for cast completion signaling.
    ///
    /// The io_uring event loop passes its own `Arc<Notify>` here so it can
    /// wait for cast processing to finish instead of using arbitrary yield loops.
    pub fn with_notify(
        channel_router: Arc<ChannelRouter>,
        pubsub: PubSub,
        ws_tx: mpsc::UnboundedSender<WsSendItem>,
        runtime: Arc<Runtime>,
        cast_notify: Arc<Notify>,
    ) -> Self {
        Self {
            channel_router,
            pubsub,
            ws_tx,
            runtime,
            cast_notify,
        }
    }
}

/// Internal state for the ChannelConnectionServer GenServer.
/// Internal state for the ChannelConnectionServer GenServer.
///
/// This type is public because it must satisfy the `GenServer::State` associated type,
/// but it is not re-exported from `lib.rs` and should be treated as an internal detail.
pub struct ChannelConnectionState {
    joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)>,
    self_pid: rebar_core::process::ProcessId,
    runtime: Arc<Runtime>,
}

#[async_trait]
impl GenServer for ChannelConnectionServer {
    type State = ChannelConnectionState;

    async fn init(
        &self,
        _args: rmpv::Value,
        ctx: &GenServerContext,
    ) -> Result<Self::State, String> {
        Ok(ChannelConnectionState {
            joined_channels: HashMap::new(),
            self_pid: ctx.self_pid(),
            runtime: Arc::clone(&self.runtime),
        })
    }

    async fn handle_call(
        &self,
        _request: rmpv::Value,
        _from: GsFrom,
        state: Self::State,
        _ctx: &GenServerContext,
    ) -> CallReply<Self::State> {
        CallReply::Reply(rmpv::Value::Nil, state)
    }

    async fn handle_cast(
        &self,
        request: rmpv::Value,
        mut state: Self::State,
        _ctx: &GenServerContext,
    ) -> CastReply<Self::State> {
        if let Some(text) = request.as_str() {
            if let Ok(phoenix_msg) = serde_json::from_str::<PhoenixMessage>(text) {
                match phoenix_msg.event.as_str() {
                    PHX_JOIN => {
                        if let Some(channel) = self.channel_router.find(&phoenix_msg.topic) {
                            handle_join(
                                &phoenix_msg,
                                channel,
                                &self.ws_tx,
                                &self.pubsub,
                                &mut state.joined_channels,
                                state.self_pid,
                                &state.runtime,
                            )
                            .await;
                        }
                    }
                    PHX_LEAVE => {
                        handle_leave(&phoenix_msg, &mut state.joined_channels).await;
                    }
                    HEARTBEAT => {
                        handle_heartbeat(&phoenix_msg, &self.ws_tx);
                    }
                    _ => {
                        dispatch_event(&phoenix_msg, &mut state.joined_channels).await;
                    }
                }
            }
        }
        self.cast_notify.notify_one();
        CastReply::NoReply(state)
    }

    async fn handle_info(
        &self,
        msg: rmpv::Value,
        mut state: Self::State,
        _ctx: &GenServerContext,
    ) -> InfoReply<Self::State> {
        if let Some(json_str) = msg.as_str() {
            if let Some(rest) = json_str.strip_prefix(TIMER_PREFIX) {
                // Timer message: __timer:{topic}\0{event}
                if let Some(sep_pos) = rest.find(TIMER_SEP) {
                    let topic = &rest[..sep_pos];
                    let event = &rest[sep_pos + TIMER_SEP.len_utf8()..];
                    if let Some((channel, socket)) = state.joined_channels.get_mut(topic) {
                        let synthetic = mahalo_pubsub::PubSubMessage {
                            topic: topic.to_string(),
                            event: event.to_string(),
                            payload: Value::Null,
                        };
                        let _ = channel.handle_info(&synthetic, socket).await;
                    }
                }
            } else if let Ok(pubsub_msg) =
                serde_json::from_str::<mahalo_pubsub::PubSubMessage>(json_str)
            {
                // PubSub broadcast message
                if let Some((channel, socket)) = state.joined_channels.get_mut(&pubsub_msg.topic) {
                    let _ = channel.handle_info(&pubsub_msg, socket).await;
                }
            }
        }
        InfoReply::NoReply(state)
    }

    async fn terminate(&self, reason: &str, mut state: Self::State) {
        use crate::channel::ShouldStop;
        for (_, (channel, mut socket)) in state.joined_channels.drain() {
            // Two-pass shutdown: stopping() veto, then unconditional terminate().
            if channel.stopping(&mut socket).await == ShouldStop::Yes {
                channel.terminate(reason, &mut socket).await;
            }
        }
    }
}

/// Handle a WebSocket connection as a GenServer process.
///
/// Each incoming WebSocket message is cast to a GenServer that manages the
/// per-connection channel state. The send-forwarder task remains a plain
/// tokio task since it is purely I/O.
pub async fn handle_websocket(
    ws: WebSocket,
    channel_router: Arc<ChannelRouter>,
    pubsub: PubSub,
    runtime: Arc<Runtime>,
) {
    let (ws_sender, mut ws_receiver) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel::<WsSendItem>();

    let send_task = spawn_ws_forwarder(ws_sender, rx);

    // Start GenServer for this connection
    let server = ChannelConnectionServer::new(
        channel_router,
        pubsub,
        tx,
        Arc::clone(&runtime),
    );
    let pid = gen_server::start(&runtime, server, rmpv::Value::Nil).await;

    // Read WebSocket messages and cast them to the GenServer
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            WsMessage::Text(ref text) => {
                let cast_val = rmpv::Value::String(rmpv::Utf8String::from(text.to_string()));
                if gen_server::cast_from_runtime(&runtime, pid, cast_val).await.is_err() {
                    break;
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    // Kill the GenServer process on disconnect
    runtime.kill(pid);
    send_task.abort();
}

/// Handle a WebSocket connection via a supervised `ChannelSupervisor`.
///
/// The connection GenServer is started as a `Temporary` child of the supervisor,
/// so crashes are isolated and don't affect other connections.
pub async fn handle_websocket_supervised(
    ws: WebSocket,
    channel_router: Arc<ChannelRouter>,
    pubsub: PubSub,
    runtime: Arc<Runtime>,
    supervisor: &crate::supervisor::ChannelSupervisor,
) {
    use crate::supervisor::connection_child_entry;
    use rebar_core::process::ExitReason;

    let entry = connection_child_entry({
        let channel_router = Arc::clone(&channel_router);
        let pubsub = pubsub.clone();
        let runtime = Arc::clone(&runtime);
        move || {
            Box::pin(async move {
                handle_websocket(ws, channel_router, pubsub, runtime).await;
                ExitReason::Normal
            })
        }
    });

    if let Err(e) = supervisor.add_connection(entry).await {
        tracing::warn!("failed to add WebSocket connection to supervisor: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Parse a JSON string, panicking on invalid JSON.
    fn parse_ws_json(msg: WsSendItem) -> Value {
        serde_json::from_str(&msg).unwrap()
    }

    /// Create a ChannelSocket with a fresh PubSub and unbounded channel.
    fn test_socket(topic: &str) -> (ChannelSocket, mpsc::UnboundedReceiver<WsSendItem>, PubSub) {
        let pubsub = PubSub::start();
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new(topic.into(), tx, pubsub.clone());
        (socket, rx, pubsub)
    }

    /// Like `test_socket` but also returns the sender for tests that need it.
    fn test_socket_with_tx(
        topic: &str,
    ) -> (
        ChannelSocket,
        mpsc::UnboundedSender<WsSendItem>,
        mpsc::UnboundedReceiver<WsSendItem>,
        PubSub,
    ) {
        let pubsub = PubSub::start();
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = ChannelSocket::new(topic.into(), tx.clone(), pubsub.clone());
        (socket, tx, rx, pubsub)
    }

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
        let (mut socket, _rx, pubsub) = test_socket("test:topic");

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
        let (socket, mut rx, pubsub) = test_socket("test:topic");

        socket.push("event", &serde_json::json!({"a": 1})).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive a message"));
        assert_eq!(parsed["event"], "event");
        assert_eq!(parsed["payload"]["a"], 1);
        assert_eq!(parsed["topic"], "test:topic");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_push_closed_sender() {
        let (socket, rx, pubsub) = test_socket("test:topic");

        drop(rx); // close the receiver
        // should not panic, just logs a warning
        socket.push("event", &serde_json::json!({})).await;

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_broadcast() {
        let (socket, _rx, pubsub) = test_socket("test:topic");

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
        let (socket, mut rx, pubsub) = test_socket("test:topic");

        let reply = Reply::ok(serde_json::json!({"data": "ok"}));
        socket.reply("ref1", &reply).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["ref"], "ref1");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["payload"]["response"]["data"], "ok");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn channel_socket_reply_closed() {
        let (socket, rx, pubsub) = test_socket("test:topic");

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

        let parsed = parse_ws_json(rx.try_recv().expect("should receive heartbeat reply"));
        assert_eq!(parsed["topic"], "phoenix");
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["ref"], "hb-1");
    }

    // -- handle_join -----------------------------------------------------------

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
        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(DummyChannel);
        let mut joined_channels = HashMap::new();

        let pid = rebar_core::process::ProcessId::new(1, 1);
        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "phx_join".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("join-1".into()),
        };

        handle_join(&msg, &channel, &tx, &pubsub, &mut joined_channels, pid, &runtime).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive join reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["ref"], "join-1");

        assert!(joined_channels.contains_key("room:lobby"));

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn handle_join_failure() {
        let pubsub = PubSub::start();
        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel: Arc<dyn Channel> = Arc::new(FailChannel);
        let mut joined_channels = HashMap::new();

        let pid = rebar_core::process::ProcessId::new(1, 1);
        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "phx_join".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("join-2".into()),
        };

        handle_join(&msg, &channel, &tx, &pubsub, &mut joined_channels, pid, &runtime).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive error reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "error");
        assert_eq!(parsed["ref"], "join-2");

        assert!(!joined_channels.contains_key("room:lobby"));

        pubsub.shutdown();
    }

    // -- handle_leave ----------------------------------------------------------

    #[tokio::test]
    async fn handle_leave_known_topic() {
        let (socket, tx, mut rx, pubsub) = test_socket_with_tx("room:lobby");
        let channel: Arc<dyn Channel> = Arc::new(DummyChannel);

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

        let parsed = parse_ws_json(rx.try_recv().expect("should receive leave reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");

        drop(tx);
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
        let (socket, _tx, mut rx, pubsub) = test_socket_with_tx("room:lobby");
        let channel: Arc<dyn Channel> = Arc::new(ReplyChannel);

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "custom_event".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("ev-1".into()),
        };

        dispatch_event(&msg, &mut joined_channels).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["payload"]["response"]["echo"], true);
        assert_eq!(parsed["ref"], "ev-1");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn dispatch_event_no_reply() {
        let (socket, _tx, mut rx, pubsub) = test_socket_with_tx("room:lobby");
        let channel: Arc<dyn Channel> = Arc::new(DummyChannel); // returns Ok(None)

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
        let (socket, _tx, mut rx, pubsub) = test_socket_with_tx("room:lobby");
        let channel: Arc<dyn Channel> = Arc::new(ErrorChannel);

        let mut joined_channels: HashMap<String, (Arc<dyn Channel>, ChannelSocket)> = HashMap::new();
        joined_channels.insert("room:lobby".into(), (channel, socket));

        let msg = PhoenixMessage {
            topic: "room:lobby".into(),
            event: "bad_event".into(),
            payload: serde_json::json!({}),
            msg_ref: Some("ev-3".into()),
        };

        dispatch_event(&msg, &mut joined_channels).await;

        let parsed = parse_ws_json(rx.try_recv().expect("should receive error reply"));
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "error");
        assert_eq!(parsed["ref"], "ev-3");

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

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move { ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt)) }
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

    // -- GenServer WebSocket lifecycle tests -----------------------------------

    #[tokio::test]
    async fn handle_websocket_genserver_lifecycle() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct LifecycleChannel {
            terminated: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Channel for LifecycleChannel {
            async fn join(
                &self,
                _topic: &str,
                _payload: &Value,
                socket: &mut ChannelSocket,
            ) -> Result<Value, crate::channel::ChannelError> {
                socket.broadcast("joined_broadcast", serde_json::json!({"from": "join"}));
                Ok(serde_json::json!({"status": "joined"}))
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

            async fn handle_info(
                &self,
                msg: &mahalo_pubsub::PubSubMessage,
                socket: &mut ChannelSocket,
            ) -> Result<(), crate::channel::ChannelError> {
                socket.push(&msg.event, &msg.payload).await;
                Ok(())
            }

            async fn terminate(&self, _reason: &str, _socket: &mut ChannelSocket) {
                self.terminated.store(true, Ordering::SeqCst);
            }
        }

        let terminated = Arc::new(AtomicBool::new(false));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("room:*", Arc::new(LifecycleChannel {
                terminated: terminated.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        // 1. Join a channel
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

        // Should get join reply
        let reply = ws_stream.next().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(parsed["event"], "phx_reply");
        assert_eq!(parsed["payload"]["status"], "ok");
        assert_eq!(parsed["payload"]["response"]["status"], "joined");

        // 2. The channel broadcast on join should arrive via handle_info
        let broadcast_reply = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ws_stream.next(),
        ).await;
        if let Ok(Some(Ok(msg))) = broadcast_reply {
            let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(parsed["event"], "joined_broadcast");
            assert_eq!(parsed["payload"]["from"], "join");
        }
        // Note: broadcast may or may not arrive depending on timing

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
        assert_eq!(parsed["payload"]["response"]["echo_event"], "new_msg");

        // 4. Broadcast from server side and verify it arrives via handle_info
        pubsub.broadcast("room:lobby", "server_event", serde_json::json!({"data": "test"}));

        let broadcast_reply = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ws_stream.next(),
        ).await;
        if let Ok(Some(Ok(msg))) = broadcast_reply {
            let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(parsed["event"], "server_event");
            assert_eq!(parsed["payload"]["data"], "test");
        } else {
            panic!("Expected to receive broadcast via handle_info");
        }

        // 5. Close connection
        ws_stream.close(None).await.unwrap();

        // 6. Wait for terminate to be called
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        server.abort();
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn terminate_called_on_disconnect() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TermTrackChannel {
            terminated: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Channel for TermTrackChannel {
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

            async fn terminate(&self, _reason: &str, _socket: &mut ChannelSocket) {
                self.terminated.store(true, Ordering::SeqCst);
            }
        }

        let terminated = Arc::new(AtomicBool::new(false));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("room:*", Arc::new(TermTrackChannel {
                terminated: terminated.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        // Join a channel
        let join_msg = serde_json::json!({
            "topic": "room:test",
            "event": "phx_join",
            "payload": {},
            "ref": "1"
        });
        ws_stream.send(TungsteniteMsg::Text(join_msg.to_string().into())).await.unwrap();

        // Consume join reply
        let _ = ws_stream.next().await;

        // Disconnect
        ws_stream.close(None).await.unwrap();

        // Wait for terminate to propagate
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(terminated.load(Ordering::SeqCst), "terminate should have been called on disconnect");

        server.abort();
        pubsub.shutdown();
    }

    // -- Timer scheduling tests (#7) ------------------------------------------

    #[tokio::test]
    async fn run_later_fires_after_delay() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicU32, Ordering};

        /// Channel that schedules a run_later on join, pushes on handle_info.
        struct TimerChannel {
            timer_count: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Channel for TimerChannel {
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

            async fn started(&self, socket: &mut ChannelSocket) {
                socket.run_later(Duration::from_millis(50), "tick");
            }

            async fn handle_info(
                &self,
                msg: &mahalo_pubsub::PubSubMessage,
                socket: &mut ChannelSocket,
            ) -> Result<(), crate::channel::ChannelError> {
                if msg.event == "tick" {
                    self.timer_count.fetch_add(1, Ordering::SeqCst);
                    socket.push("timer_fired", &serde_json::json!({"count": self.timer_count.load(Ordering::SeqCst)})).await;
                }
                Ok(())
            }
        }

        let timer_count = Arc::new(AtomicU32::new(0));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("timer:*", Arc::new(TimerChannel {
                timer_count: timer_count.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        // Join to trigger run_later
        let join_msg = serde_json::json!({
            "topic": "timer:test",
            "event": "phx_join",
            "payload": {},
            "ref": "1"
        });
        ws_stream.send(TungsteniteMsg::Text(join_msg.to_string().into())).await.unwrap();
        let _ = ws_stream.next().await; // consume join reply

        // Wait for timer to fire (50ms delay + some margin)
        let timer_msg = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ws_stream.next(),
        ).await;

        match timer_msg {
            Ok(Some(Ok(msg))) => {
                let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                assert_eq!(parsed["event"], "timer_fired");
                assert_eq!(parsed["payload"]["count"], 1);
            }
            other => panic!("Expected timer message, got {:?}", other),
        }

        // Verify it only fired once (run_later is one-shot)
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            ws_stream.next(),
        ).await;
        // Should timeout — no second fire
        assert!(second.is_err(), "run_later should only fire once");

        ws_stream.close(None).await.unwrap();
        server.abort();
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn run_interval_fires_repeatedly() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct IntervalChannel {
            tick_count: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Channel for IntervalChannel {
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

            async fn started(&self, socket: &mut ChannelSocket) {
                socket.run_interval(Duration::from_millis(50), "tick");
            }

            async fn handle_info(
                &self,
                msg: &mahalo_pubsub::PubSubMessage,
                socket: &mut ChannelSocket,
            ) -> Result<(), crate::channel::ChannelError> {
                if msg.event == "tick" {
                    self.tick_count.fetch_add(1, Ordering::SeqCst);
                    socket.push("interval_tick", &serde_json::json!({"n": self.tick_count.load(Ordering::SeqCst)})).await;
                }
                Ok(())
            }
        }

        let tick_count = Arc::new(AtomicU32::new(0));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("interval:*", Arc::new(IntervalChannel {
                tick_count: tick_count.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        let join_msg = serde_json::json!({
            "topic": "interval:test",
            "event": "phx_join",
            "payload": {},
            "ref": "1"
        });
        ws_stream.send(TungsteniteMsg::Text(join_msg.to_string().into())).await.unwrap();
        let _ = ws_stream.next().await; // join reply

        // Collect at least 2 interval ticks
        let mut received = 0u32;
        for _ in 0..2 {
            let tick = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                ws_stream.next(),
            ).await;
            if let Ok(Some(Ok(msg))) = tick {
                let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                if parsed["event"] == "interval_tick" {
                    received += 1;
                }
            }
        }

        assert!(received >= 2, "expected at least 2 interval ticks, got {received}");

        ws_stream.close(None).await.unwrap();
        server.abort();
        pubsub.shutdown();
    }

    // -- Stopping veto tests (#8) ---------------------------------------------

    #[tokio::test]
    async fn stopping_veto_prevents_terminate() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicBool, Ordering};
        use crate::channel::ShouldStop;

        struct VetoChannel {
            terminated: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Channel for VetoChannel {
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

            async fn stopping(&self, _socket: &mut ChannelSocket) -> ShouldStop {
                ShouldStop::No
            }

            async fn terminate(&self, _reason: &str, _socket: &mut ChannelSocket) {
                self.terminated.store(true, Ordering::SeqCst);
            }
        }

        let terminated = Arc::new(AtomicBool::new(false));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("veto:*", Arc::new(VetoChannel {
                terminated: terminated.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        let join_msg = serde_json::json!({
            "topic": "veto:test",
            "event": "phx_join",
            "payload": {},
            "ref": "1"
        });
        ws_stream.send(TungsteniteMsg::Text(join_msg.to_string().into())).await.unwrap();
        let _ = ws_stream.next().await; // join reply

        // Disconnect — triggers GenServer terminate
        ws_stream.close(None).await.unwrap();

        // Wait for terminate path to execute
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // stopping() returned ShouldStop::No, so terminate() should NOT have been called
        assert!(!terminated.load(Ordering::SeqCst), "terminate should NOT be called when stopping() returns ShouldStop::No");

        server.abort();
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn stopping_yes_allows_terminate() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use axum::Router;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
        use std::sync::atomic::{AtomicBool, Ordering};
        use crate::channel::ShouldStop;

        struct AllowStopChannel {
            terminated: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Channel for AllowStopChannel {
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

            async fn stopping(&self, _socket: &mut ChannelSocket) -> ShouldStop {
                ShouldStop::Yes
            }

            async fn terminate(&self, _reason: &str, _socket: &mut ChannelSocket) {
                self.terminated.store(true, Ordering::SeqCst);
            }
        }

        let terminated = Arc::new(AtomicBool::new(false));
        let pubsub = PubSub::start();
        let channel_router = Arc::new(
            ChannelRouter::new().channel("allow:*", Arc::new(AllowStopChannel {
                terminated: terminated.clone(),
            })),
        );

        let runtime = Arc::new(rebar_core::runtime::Runtime::new(1));
        let pubsub_clone = pubsub.clone();
        let router_clone = channel_router.clone();
        let runtime_clone = Arc::clone(&runtime);

        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let cr = router_clone.clone();
                let ps = pubsub_clone.clone();
                let rt = runtime_clone.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_websocket(socket, cr, ps, rt))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws_stream, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .expect("WebSocket connect failed");

        use futures::{SinkExt, StreamExt};

        let join_msg = serde_json::json!({
            "topic": "allow:test",
            "event": "phx_join",
            "payload": {},
            "ref": "1"
        });
        ws_stream.send(TungsteniteMsg::Text(join_msg.to_string().into())).await.unwrap();
        let _ = ws_stream.next().await; // join reply

        ws_stream.close(None).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // stopping() returned ShouldStop::Yes, so terminate() SHOULD have been called
        assert!(terminated.load(Ordering::SeqCst), "terminate should be called when stopping() returns ShouldStop::Yes");

        server.abort();
        pubsub.shutdown();
    }

    // -- ChannelConnectionServer GenServer unit tests --------------------------

    /// Helper: start a ChannelConnectionServer GenServer and return (pid, rx, runtime, pubsub).
    async fn start_genserver_with_channel(
        channel: Arc<dyn Channel>,
        topic_pattern: &str,
    ) -> (
        rebar_core::process::ProcessId,
        mpsc::UnboundedReceiver<WsSendItem>,
        Arc<Runtime>,
        PubSub,
    ) {
        let pubsub = PubSub::start();
        let runtime = Arc::new(Runtime::new(1));
        let channel_router = Arc::new(ChannelRouter::new().channel(topic_pattern, channel));
        let (tx, rx) = mpsc::unbounded_channel();
        let server = ChannelConnectionServer::new(
            channel_router,
            pubsub.clone(),
            tx,
            Arc::clone(&runtime),
        );
        let pid = gen_server::start(&runtime, server, rmpv::Value::Nil).await;
        (pid, rx, runtime, pubsub)
    }

    /// Cast a Phoenix JSON message to the GenServer.
    async fn cast_phoenix_msg(runtime: &Arc<Runtime>, pid: rebar_core::process::ProcessId, msg: &Value) {
        let json = serde_json::to_string(msg).unwrap();
        let cast_val = rmpv::Value::String(rmpv::Utf8String::from(json));
        gen_server::cast_from_runtime(runtime, pid, cast_val).await.unwrap();
    }

    /// Read all available messages from the receiver (non-blocking).
    fn drain_rx(rx: &mut mpsc::UnboundedReceiver<WsSendItem>) -> Vec<Value> {
        let mut msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            msgs.push(serde_json::from_str(&msg).unwrap());
        }
        msgs
    }

    #[tokio::test]
    async fn genserver_handle_cast_join_and_event() {
        let (pid, mut rx, runtime, pubsub) =
            start_genserver_with_channel(Arc::new(EchoChannel), "room:*").await;

        // Join
        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "room:lobby", "event": "phx_join", "payload": {}, "ref": "j1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let msgs = drain_rx(&mut rx);
        assert!(!msgs.is_empty(), "should receive join reply");
        assert_eq!(msgs[0]["event"], "phx_reply");
        assert_eq!(msgs[0]["payload"]["status"], "ok");
        assert_eq!(msgs[0]["ref"], "j1");

        // Custom event
        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "room:lobby", "event": "new_msg", "payload": {"text": "hi"}, "ref": "m1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let msgs = drain_rx(&mut rx);
        assert!(!msgs.is_empty(), "should receive event reply");
        assert_eq!(msgs[0]["payload"]["response"]["echo_event"], "new_msg");
        assert_eq!(msgs[0]["ref"], "m1");

        runtime.kill(pid);
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn genserver_handle_cast_heartbeat() {
        let (pid, mut rx, runtime, pubsub) =
            start_genserver_with_channel(Arc::new(DummyChannel), "room:*").await;

        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": "hb-1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let msgs = drain_rx(&mut rx);
        assert!(!msgs.is_empty());
        assert_eq!(msgs[0]["event"], "phx_reply");
        assert_eq!(msgs[0]["payload"]["status"], "ok");
        assert_eq!(msgs[0]["ref"], "hb-1");

        runtime.kill(pid);
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn genserver_handle_cast_leave() {
        let (pid, mut rx, runtime, pubsub) =
            start_genserver_with_channel(Arc::new(DummyChannel), "room:*").await;

        // Join first
        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "room:test", "event": "phx_join", "payload": {}, "ref": "j1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drain_rx(&mut rx); // consume join reply

        // Leave
        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "room:test", "event": "phx_leave", "payload": {}, "ref": "l1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let msgs = drain_rx(&mut rx);
        assert!(!msgs.is_empty());
        assert_eq!(msgs[0]["event"], "phx_reply");
        assert_eq!(msgs[0]["payload"]["status"], "ok");
        assert_eq!(msgs[0]["ref"], "l1");

        runtime.kill(pid);
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn genserver_handle_cast_invalid_json_ignored() {
        let (pid, mut rx, runtime, pubsub) =
            start_genserver_with_channel(Arc::new(DummyChannel), "room:*").await;

        // Send non-JSON text — should be silently ignored.
        let cast_val = rmpv::Value::String(rmpv::Utf8String::from("not valid json"));
        gen_server::cast_from_runtime(&runtime, pid, cast_val).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(drain_rx(&mut rx).is_empty(), "invalid JSON should produce no output");

        runtime.kill(pid);
        pubsub.shutdown();
    }

    #[tokio::test]
    async fn genserver_handle_info_pubsub_broadcast() {
        // Use a channel that pushes on handle_info
        struct InfoPushChannel;

        #[async_trait]
        impl Channel for InfoPushChannel {
            async fn join(
                &self, _topic: &str, _payload: &Value, _socket: &mut ChannelSocket,
            ) -> Result<Value, crate::channel::ChannelError> {
                Ok(serde_json::json!({}))
            }
            async fn handle_in(
                &self, _event: &str, _payload: &Value, _socket: &mut ChannelSocket,
            ) -> Result<Option<Reply>, crate::channel::ChannelError> {
                Ok(None)
            }
            async fn handle_info(
                &self, msg: &mahalo_pubsub::PubSubMessage, socket: &mut ChannelSocket,
            ) -> Result<(), crate::channel::ChannelError> {
                socket.push(&msg.event, &msg.payload).await;
                Ok(())
            }
        }

        let (pid, mut rx, runtime, pubsub) =
            start_genserver_with_channel(Arc::new(InfoPushChannel), "room:*").await;

        // Join to establish PubSub subscription
        cast_phoenix_msg(&runtime, pid, &serde_json::json!({
            "topic": "room:lobby", "event": "phx_join", "payload": {}, "ref": "j1"
        })).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drain_rx(&mut rx); // consume join reply

        // Broadcast via PubSub — should arrive via handle_info
        pubsub.broadcast("room:lobby", "server_push", serde_json::json!({"data": 42}));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let msgs = drain_rx(&mut rx);
        assert!(!msgs.is_empty(), "should receive handle_info push from PubSub broadcast");
        assert_eq!(msgs[0]["event"], "server_push");
        assert_eq!(msgs[0]["payload"]["data"], 42);

        runtime.kill(pid);
        pubsub.shutdown();
    }
}
