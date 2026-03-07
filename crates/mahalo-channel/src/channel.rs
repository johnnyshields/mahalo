use mahalo_core::plug::BoxFuture;
use serde_json::Value;

use crate::socket::ChannelSocket;

#[derive(Debug)]
pub enum ChannelError {
    NotAuthorized,
    InvalidTopic(String),
    Internal(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::NotAuthorized => write!(f, "not authorized"),
            ChannelError::InvalidTopic(t) => write!(f, "invalid topic: {t}"),
            ChannelError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for ChannelError {}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Reply {
    pub status: String,
    pub payload: Value,
}

impl Reply {
    pub fn ok(payload: Value) -> Self {
        Reply {
            status: "ok".to_string(),
            payload,
        }
    }

    pub fn error(payload: Value) -> Self {
        Reply {
            status: "error".to_string(),
            payload,
        }
    }
}

/// Return value from [`Channel::stopping()`] that controls whether the channel
/// should proceed with shutdown or stay alive to flush pending work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShouldStop {
    /// Proceed with normal shutdown. (default)
    Yes,
    /// Keep the channel alive (e.g., to flush pending messages).
    No,
}

pub trait Channel: 'static {
    /// Called when a client joins a topic.
    fn join<'a>(
        &'a self,
        topic: &'a str,
        payload: &'a Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Value, ChannelError>>;

    /// Called when a client pushes an event.
    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>>;

    /// Called for PubSub messages from other processes.
    fn handle_info<'a>(
        &'a self,
        msg: &'a mahalo_pubsub::PubSubMessage,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            // Default: push to client
            socket.push(&msg.event, &msg.payload).await;
            Ok(())
        })
    }

    /// Called after a successful join reply is sent. Use this to schedule
    /// timers, fetch initial state, or subscribe to additional topics.
    ///
    /// Actix equivalent: `Actor::started()`.
    fn started<'a>(&'a self, _socket: &'a mut ChannelSocket) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Called before channel cleanup begins. Returning [`ShouldStop::No`]
    /// keeps the channel alive (e.g., to flush pending messages). The default
    /// is [`ShouldStop::Yes`] — proceed with shutdown.
    ///
    /// Actix equivalent: `Actor::stopping()`.
    fn stopping<'a>(&'a self, _socket: &'a mut ChannelSocket) -> BoxFuture<'a, ShouldStop> {
        Box::pin(async { ShouldStop::Yes })
    }

    /// Called when the channel process terminates.
    fn terminate<'a>(
        &'a self,
        _reason: &'a str,
        _socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_ok_has_ok_status() {
        let reply = Reply::ok(serde_json::json!({"msg": "hi"}));
        assert_eq!(reply.status, "ok");
        assert_eq!(reply.payload, serde_json::json!({"msg": "hi"}));
    }

    #[test]
    fn reply_error_has_error_status() {
        let reply = Reply::error(serde_json::json!({"reason": "bad"}));
        assert_eq!(reply.status, "error");
        assert_eq!(reply.payload, serde_json::json!({"reason": "bad"}));
    }

    #[test]
    fn channel_error_display() {
        assert_eq!(ChannelError::NotAuthorized.to_string(), "not authorized");
        assert_eq!(
            ChannelError::InvalidTopic("foo".into()).to_string(),
            "invalid topic: foo"
        );
        assert_eq!(
            ChannelError::Internal("boom".into()).to_string(),
            "internal error: boom"
        );
    }

    #[test]
    fn channel_error_is_std_error() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<ChannelError>();
    }

    #[test]
    fn should_stop_default_is_yes() {
        assert_eq!(ShouldStop::Yes, ShouldStop::Yes);
        assert_ne!(ShouldStop::Yes, ShouldStop::No);
    }

    // -- Channel default trait methods -----------------------------------------

    struct MinimalChannel;

    impl Channel for MinimalChannel {
        fn join<'a>(
            &'a self,
            _topic: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Value, ChannelError>> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }

        fn handle_in<'a>(
            &'a self,
            _event: &'a str,
            _payload: &'a Value,
            _socket: &'a mut ChannelSocket,
        ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn handle_info_default_pushes_to_client() {
        use mahalo_pubsub::{PubSub, PubSubMessage};
        use local_sync::mpsc::unbounded;

        let pubsub = PubSub::start();
        let (tx, mut rx) = unbounded::channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let msg = PubSubMessage {
            topic: "test:topic".into(),
            event: "greeting".into(),
            payload: serde_json::json!({"hello": "world"}),
        };

        let ch = MinimalChannel;
        ch.handle_info(&msg, &mut socket).await.unwrap();

        let json = rx.try_recv().expect("should have received a message");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"], "greeting");
        assert_eq!(parsed["payload"]["hello"], "world");

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn terminate_default_does_nothing() {
        use mahalo_pubsub::PubSub;
        use local_sync::mpsc::unbounded;

        let pubsub = PubSub::start();
        let (tx, _rx) = unbounded::channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let ch = MinimalChannel;
        ch.terminate("test", &mut socket).await;
        // no panic = success

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn started_default_does_nothing() {
        use mahalo_pubsub::PubSub;
        use local_sync::mpsc::unbounded;

        let pubsub = PubSub::start();
        let (tx, _rx) = unbounded::channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let ch = MinimalChannel;
        ch.started(&mut socket).await;
        // no panic = success

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn stopping_default_returns_yes() {
        use mahalo_pubsub::PubSub;
        use local_sync::mpsc::unbounded;

        let pubsub = PubSub::start();
        let (tx, _rx) = unbounded::channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let ch = MinimalChannel;
        let result = ch.stopping(&mut socket).await;
        assert_eq!(result, ShouldStop::Yes);

        pubsub.shutdown();
    }
}
