use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The type carried over the internal channel from ChannelSocket → WebSocket sink.
/// Decoupled from any specific WebSocket library.
pub type WsSendItem = String;

use local_sync::mpsc::unbounded;
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
    runtime: Rc<Runtime>,
}

impl ChannelAddr {
    /// Send a synthetic event to the channel as if it came from a PubSub message.
    pub fn send_event(&self, topic: &str, event: &str, payload: Value) {
        let msg = PhoenixMessage {
            topic: topic.to_string(),
            event: event.to_string(),
            payload,
            msg_ref: None,
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let cast_val = rmpv::Value::String(json.into());
            let _ = gen_server::cast_from_runtime(&self.runtime, self.pid, cast_val);
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
    assigns: HashMap<TypeId, Box<dyn Any>>,
    sender: unbounded::Tx<WsSendItem>,
    pubsub: PubSub,
    /// Set when running inside a GenServer process — enables timer scheduling.
    self_pid: Option<ProcessId>,
    runtime: Option<Rc<Runtime>>,
}

impl ChannelSocket {
    pub fn new(topic: String, sender: unbounded::Tx<WsSendItem>, pubsub: PubSub) -> Self {
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
        sender: unbounded::Tx<WsSendItem>,
        pubsub: PubSub,
        self_pid: ProcessId,
        runtime: Rc<Runtime>,
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
            let runtime = Rc::clone(runtime);
            let key = format!("{TIMER_PREFIX}{}{TIMER_SEP}{}", self.topic, event.into());
            rebar_core::executor::spawn(async move {
                rebar_core::time::sleep(delay).await;
                let msg = rmpv::Value::String(rmpv::Utf8String::from(key));
                let _ = runtime.send(pid, msg);
            }).detach();
        }
    }

    /// Schedule a recurring synthetic `handle_info` with the given event every `interval`.
    /// Actix equivalent: `Context::run_interval()`.
    ///
    /// Continues until the GenServer is stopped or the runtime is dropped. No-op if
    /// the socket is not backed by a GenServer process.
    pub fn run_interval(&self, interval: Duration, event: impl Into<String>) {
        if let (Some(pid), Some(runtime)) = (self.self_pid, &self.runtime) {
            let runtime = Rc::clone(runtime);
            let key = format!("{TIMER_PREFIX}{}{TIMER_SEP}{}", self.topic, event.into());
            rebar_core::executor::spawn(async move {
                loop {
                    rebar_core::time::sleep(interval).await;
                    let msg = rmpv::Value::String(rmpv::Utf8String::from(key.clone()));
                    if runtime.send(pid, msg).is_err() {
                        break;
                    }
                }
            }).detach();
        }
    }

    /// Returns a typed handle to this channel's GenServer process.
    /// Available for inter-channel messaging from within channel callbacks.
    /// Returns `None` when not backed by a GenServer process.
    pub fn addr(&self) -> Option<ChannelAddr> {
        match (self.self_pid, &self.runtime) {
            (Some(pid), Some(runtime)) => Some(ChannelAddr {
                pid,
                runtime: Rc::clone(runtime),
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
    routes: Vec<(String, Rc<dyn Channel>)>,
}

impl ChannelRouter {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Register a channel for a topic pattern (e.g., "room:*").
    pub fn channel(mut self, topic_pattern: &str, handler: Rc<dyn Channel>) -> Self {
        self.routes.push((topic_pattern.to_string(), handler));
        self
    }

    /// Find a channel matching the given topic.
    pub fn find(&self, topic: &str) -> Option<&Rc<dyn Channel>> {
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
    channel: &Rc<dyn Channel>,
    tx: &unbounded::Tx<WsSendItem>,
    pubsub: &PubSub,
    joined_channels: &mut HashMap<String, (Rc<dyn Channel>, ChannelSocket)>,
    self_pid: ProcessId,
    runtime: &Rc<Runtime>,
) {
    let mut socket = ChannelSocket::new_with_pid(
        phoenix_msg.topic.clone(),
        tx.clone(),
        pubsub.clone(),
        self_pid,
        Rc::clone(runtime),
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
            if let Some(pubsub_rx) = pubsub.subscribe(&phoenix_msg.topic) {
                let runtime = Rc::clone(runtime);
                let pid = self_pid;
                rebar_core::executor::spawn(async move {
                    loop {
                        match pubsub_rx.try_recv() {
                            Ok(pubsub_msg) => {
                                if let Ok(json) = serde_json::to_string(&pubsub_msg) {
                                    let msg = rmpv::Value::String(rmpv::Utf8String::from(json));
                                    if runtime.send(pid, msg).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => {
                                rebar_core::time::sleep(Duration::from_millis(5)).await;
                            }
                            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                        }
                    }
                }).detach();
            } else {
                tracing::warn!(
                    topic = %phoenix_msg.topic,
                    "PubSub subscribe failed, channel will not receive broadcasts"
                );
            }

            joined_channels.insert(phoenix_msg.topic.clone(), (Rc::clone(channel), socket));
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
    joined_channels: &mut HashMap<String, (Rc<dyn Channel>, ChannelSocket)>,
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
fn handle_heartbeat(phoenix_msg: &PhoenixMessage, tx: &unbounded::Tx<WsSendItem>) {
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
    joined_channels: &mut HashMap<String, (Rc<dyn Channel>, ChannelSocket)>,
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

// ---------------------------------------------------------------------------
// GenServer-based WebSocket connection
// ---------------------------------------------------------------------------

/// GenServer implementation for a single WebSocket channel connection.
///
/// Public so that alternative transports (e.g. io_uring raw TCP) can start
/// the same GenServer without going through a specific WebSocket library.
pub struct ChannelConnectionServer {
    channel_router: Rc<ChannelRouter>,
    pubsub: PubSub,
    ws_tx: unbounded::Tx<WsSendItem>,
    runtime: Rc<Runtime>,
}

impl ChannelConnectionServer {
    /// Create a new ChannelConnectionServer.
    pub fn new(
        channel_router: Rc<ChannelRouter>,
        pubsub: PubSub,
        ws_tx: unbounded::Tx<WsSendItem>,
        runtime: Rc<Runtime>,
    ) -> Self {
        Self {
            channel_router,
            pubsub,
            ws_tx,
            runtime,
        }
    }
}

/// Internal state for the ChannelConnectionServer GenServer.
///
/// This type is public because it must satisfy the `GenServer::State` associated type,
/// but it is not re-exported from `lib.rs` and should be treated as an internal detail.
pub struct ChannelConnectionState {
    joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)>,
    self_pid: rebar_core::process::ProcessId,
    runtime: Rc<Runtime>,
}

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
            runtime: Rc::clone(&self.runtime),
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
/// Takes a sender/receiver pair for the WebSocket transport. The caller is
/// responsible for bridging their specific WebSocket implementation to these
/// channels.
pub async fn handle_websocket(
    mut ws_rx: unbounded::Rx<String>,
    ws_tx_out: unbounded::Tx<WsSendItem>,
    channel_router: Rc<ChannelRouter>,
    pubsub: PubSub,
    runtime: Rc<Runtime>,
) {
    // Start GenServer for this connection
    let server = ChannelConnectionServer::new(
        channel_router,
        pubsub,
        ws_tx_out,
        Rc::clone(&runtime),
    );
    let pid = gen_server::start(&runtime, server, rmpv::Value::Nil);

    // Read WebSocket messages and cast them to the GenServer
    while let Some(text) = ws_rx.recv().await {
        let cast_val = rmpv::Value::String(rmpv::Utf8String::from(text));
        if gen_server::cast_from_runtime(&runtime, pid, cast_val).is_err() {
            break;
        }
    }

    // Kill the GenServer process on disconnect
    runtime.kill(pid);
}

/// Handle a WebSocket connection via a supervised `ChannelSupervisor`.
///
/// The connection GenServer is started as a `Temporary` child of the supervisor,
/// so crashes are isolated and don't affect other connections.
pub async fn handle_websocket_supervised(
    ws_rx: unbounded::Rx<String>,
    ws_tx_out: unbounded::Tx<WsSendItem>,
    channel_router: Rc<ChannelRouter>,
    pubsub: PubSub,
    runtime: Rc<Runtime>,
    supervisor: &crate::supervisor::ChannelSupervisor,
) {
    use crate::supervisor::connection_child_entry;
    use rebar_core::process::ExitReason;

    let entry = connection_child_entry(move || {
        Box::pin(async move {
            handle_websocket(ws_rx, ws_tx_out, channel_router, pubsub, runtime).await;
            ExitReason::Normal
        })
    });

    if let Err(e) = supervisor.add_connection(entry).await {
        tracing::warn!("failed to add WebSocket connection to supervisor: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mahalo_core::plug::BoxFuture;

    /// Parse a JSON string, panicking on invalid JSON.
    fn parse_ws_json(msg: WsSendItem) -> Value {
        serde_json::from_str(&msg).unwrap()
    }

    /// Create a ChannelSocket with a fresh PubSub and unbounded channel.
    fn test_socket(topic: &str) -> (ChannelSocket, unbounded::Rx<WsSendItem>, PubSub) {
        let pubsub = PubSub::start();
        let (tx, rx) = unbounded::channel();
        let socket = ChannelSocket::new(topic.into(), tx, pubsub.clone());
        (socket, rx, pubsub)
    }

    /// Like `test_socket` but also returns the sender for tests that need it.
    fn test_socket_with_tx(
        topic: &str,
    ) -> (
        ChannelSocket,
        unbounded::Tx<WsSendItem>,
        unbounded::Rx<WsSendItem>,
        PubSub,
    ) {
        let pubsub = PubSub::start();
        let (tx, rx) = unbounded::channel();
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

    impl Channel for DummyChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }

        fn handle_in<'a>(
            &'a self,
            _event: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[test]
    fn channel_router_find_exact() {
        let router = ChannelRouter::new()
            .channel("room:lobby", Rc::new(DummyChannel));
        assert!(router.find("room:lobby").is_some());
        assert!(router.find("room:other").is_none());
    }

    #[test]
    fn channel_router_find_wildcard() {
        let router = ChannelRouter::new()
            .channel("room:*", Rc::new(DummyChannel));
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
        let sub_rx = pubsub.subscribe("test:topic").expect("subscribe should succeed");

        socket.broadcast("evt", serde_json::json!({"key": "val"}));

        let msg = sub_rx.recv().expect("should receive broadcast");
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
        let (tx, mut rx) = unbounded::channel();
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

    impl Channel for FailChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
            Box::pin(async { Err(crate::channel::ChannelError::NotAuthorized) })
        }

        fn handle_in<'a>(
            &'a self,
            _event: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[test]
    fn handle_join_success() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let pubsub = PubSub::start();
            let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));
            let (tx, mut rx) = unbounded::channel();
            let channel: Rc<dyn Channel> = Rc::new(DummyChannel);
            let mut joined_channels = HashMap::new();

            let pid = rebar_core::process::ProcessId::new(1, 0, 1);
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
        });
    }

    #[tokio::test]
    async fn handle_join_failure() {
        let pubsub = PubSub::start();
        let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));
        let (tx, mut rx) = unbounded::channel();
        let channel: Rc<dyn Channel> = Rc::new(FailChannel);
        let mut joined_channels = HashMap::new();

        let pid = rebar_core::process::ProcessId::new(1, 0, 1);
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
        let channel: Rc<dyn Channel> = Rc::new(DummyChannel);

        let mut joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)> = HashMap::new();
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
        let mut joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)> = HashMap::new();

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

    impl Channel for ReplyChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }

        fn handle_in<'a>(
            &'a self,
            _event: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
            Box::pin(async { Ok(Some(Reply::ok(serde_json::json!({"echo": true})))) })
        }
    }

    struct ErrorChannel;

    impl Channel for ErrorChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }

        fn handle_in<'a>(
            &'a self,
            _event: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
            Box::pin(async { Err(crate::channel::ChannelError::Internal("test error".into())) })
        }
    }

    #[tokio::test]
    async fn dispatch_event_with_reply() {
        let (socket, _tx, mut rx, pubsub) = test_socket_with_tx("room:lobby");
        let channel: Rc<dyn Channel> = Rc::new(ReplyChannel);

        let mut joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)> = HashMap::new();
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
        let channel: Rc<dyn Channel> = Rc::new(DummyChannel); // returns Ok(None)

        let mut joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)> = HashMap::new();
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
        let channel: Rc<dyn Channel> = Rc::new(ErrorChannel);

        let mut joined_channels: HashMap<String, (Rc<dyn Channel>, ChannelSocket)> = HashMap::new();
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

    // -- EchoChannel for integration tests ------------------------------------

    struct EchoChannel;

    impl Channel for EchoChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
            Box::pin(async { Ok(serde_json::json!({"status": "connected"})) })
        }

        fn handle_in<'a>(
            &'a self,
            event: &'a str,
            payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
            Box::pin(async move {
                Ok(Some(Reply::ok(serde_json::json!({
                    "echo_event": event,
                    "echo_payload": payload,
                }))))
            })
        }
    }

    // -- GenServer unit tests (no WebSocket needed) ----------------------------

    /// Helper: start a ChannelConnectionServer GenServer and return (pid, rx, runtime, pubsub).
    async fn start_genserver_with_channel(
        channel: Rc<dyn Channel>,
        topic_pattern: &str,
    ) -> (
        rebar_core::process::ProcessId,
        unbounded::Rx<WsSendItem>,
        Rc<Runtime>,
        PubSub,
    ) {
        let pubsub = PubSub::start();
        let runtime = Rc::new(Runtime::new(1));
        let channel_router = Rc::new(ChannelRouter::new().channel(topic_pattern, channel));
        let (tx, rx) = unbounded::channel();
        let server = ChannelConnectionServer::new(
            channel_router,
            pubsub.clone(),
            tx,
            Rc::clone(&runtime),
        );
        let pid = gen_server::start(&runtime, server, rmpv::Value::Nil);
        (pid, rx, runtime, pubsub)
    }

    /// Cast a Phoenix JSON message to the GenServer.
    async fn cast_phoenix_msg(runtime: &Rc<Runtime>, pid: rebar_core::process::ProcessId, msg: &Value) {
        let json = serde_json::to_string(msg).unwrap();
        let cast_val = rmpv::Value::String(rmpv::Utf8String::from(json));
        gen_server::cast_from_runtime(runtime, pid, cast_val).unwrap();
    }

    /// Read all available messages from the receiver (non-blocking).
    fn drain_rx(rx: &mut unbounded::Rx<WsSendItem>) -> Vec<Value> {
        let mut msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            msgs.push(serde_json::from_str(&msg).unwrap());
        }
        msgs
    }

    #[test]
    fn genserver_handle_cast_join_and_event() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let (pid, mut rx, runtime, pubsub) =
                start_genserver_with_channel(Rc::new(EchoChannel), "room:*").await;

            // Join
            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "room:lobby", "event": "phx_join", "payload": {}, "ref": "j1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;

            let msgs = drain_rx(&mut rx);
            assert!(!msgs.is_empty(), "should receive join reply");
            assert_eq!(msgs[0]["event"], "phx_reply");
            assert_eq!(msgs[0]["payload"]["status"], "ok");
            assert_eq!(msgs[0]["ref"], "j1");

            // Custom event
            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "room:lobby", "event": "new_msg", "payload": {"text": "hi"}, "ref": "m1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;

            let msgs = drain_rx(&mut rx);
            assert!(!msgs.is_empty(), "should receive event reply");
            assert_eq!(msgs[0]["payload"]["response"]["echo_event"], "new_msg");
            assert_eq!(msgs[0]["ref"], "m1");

            runtime.kill(pid);
            pubsub.shutdown();
        });
    }

    #[test]
    fn genserver_handle_cast_heartbeat() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let (pid, mut rx, runtime, pubsub) =
                start_genserver_with_channel(Rc::new(DummyChannel), "room:*").await;

            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": "hb-1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;

            let msgs = drain_rx(&mut rx);
            assert!(!msgs.is_empty());
            assert_eq!(msgs[0]["event"], "phx_reply");
            assert_eq!(msgs[0]["payload"]["status"], "ok");
            assert_eq!(msgs[0]["ref"], "hb-1");

            runtime.kill(pid);
            pubsub.shutdown();
        });
    }

    #[test]
    fn genserver_handle_cast_leave() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let (pid, mut rx, runtime, pubsub) =
                start_genserver_with_channel(Rc::new(DummyChannel), "room:*").await;

            // Join first
            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "room:test", "event": "phx_join", "payload": {}, "ref": "j1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;
            drain_rx(&mut rx); // consume join reply

            // Leave
            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "room:test", "event": "phx_leave", "payload": {}, "ref": "l1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;

            let msgs = drain_rx(&mut rx);
            assert!(!msgs.is_empty());
            assert_eq!(msgs[0]["event"], "phx_reply");
            assert_eq!(msgs[0]["payload"]["status"], "ok");
            assert_eq!(msgs[0]["ref"], "l1");

            runtime.kill(pid);
            pubsub.shutdown();
        });
    }

    #[test]
    fn genserver_handle_cast_invalid_json_ignored() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            let (pid, mut rx, runtime, pubsub) =
                start_genserver_with_channel(Rc::new(DummyChannel), "room:*").await;

            // Send non-JSON text — should be silently ignored.
            let cast_val = rmpv::Value::String(rmpv::Utf8String::from("not valid json"));
            gen_server::cast_from_runtime(&runtime, pid, cast_val).unwrap();
            rebar_core::time::sleep(std::time::Duration::from_millis(50)).await;

            assert!(drain_rx(&mut rx).is_empty(), "invalid JSON should produce no output");

            runtime.kill(pid);
            pubsub.shutdown();
        });
    }

    #[test]
    fn genserver_handle_info_pubsub_broadcast() {
        use rebar_core::executor::{RebarExecutor, ExecutorConfig};
        RebarExecutor::new(ExecutorConfig::default()).unwrap().block_on(async {
            // Use a channel that pushes on handle_info
            struct InfoPushChannel;

            impl Channel for InfoPushChannel {
                fn join<'a>(
                    &'a self, _topic: &'a str, _payload: &'a Value, _socket: &'a mut ChannelSocket,
                ) -> BoxFuture<'a, Result<Value, crate::channel::ChannelError>> {
                    Box::pin(async { Ok(serde_json::json!({})) })
                }
                fn handle_in<'a>(
                    &'a self, _event: &'a str, _payload: &'a Value, _socket: &'a mut ChannelSocket,
                ) -> BoxFuture<'a, Result<Option<Reply>, crate::channel::ChannelError>> {
                    Box::pin(async { Ok(None) })
                }
                fn handle_info<'a>(
                    &'a self, msg: &'a mahalo_pubsub::PubSubMessage, socket: &'a mut ChannelSocket,
                ) -> BoxFuture<'a, Result<(), crate::channel::ChannelError>> {
                    Box::pin(async move {
                        socket.push(&msg.event, &msg.payload).await;
                        Ok(())
                    })
                }
            }

            let (pid, mut rx, runtime, pubsub) =
                start_genserver_with_channel(Rc::new(InfoPushChannel), "room:*").await;

            // Join to establish PubSub subscription
            cast_phoenix_msg(&runtime, pid, &serde_json::json!({
                "topic": "room:lobby", "event": "phx_join", "payload": {}, "ref": "j1"
            })).await;
            rebar_core::time::sleep(std::time::Duration::from_millis(100)).await;
            drain_rx(&mut rx); // consume join reply

            // Broadcast via PubSub — should arrive via handle_info
            pubsub.broadcast("room:lobby", "server_push", serde_json::json!({"data": 42}));
            rebar_core::time::sleep(std::time::Duration::from_millis(200)).await;

            let msgs = drain_rx(&mut rx);
            assert!(!msgs.is_empty(), "should receive handle_info push from PubSub broadcast");
            assert_eq!(msgs[0]["event"], "server_push");
            assert_eq!(msgs[0]["payload"]["data"], 42);

            runtime.kill(pid);
            pubsub.shutdown();
        });
    }
}
