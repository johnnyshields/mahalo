use async_trait::async_trait;
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

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Called when a client joins a topic.
    async fn join(
        &self,
        topic: &str,
        payload: &Value,
        socket: &mut ChannelSocket,
    ) -> Result<Value, ChannelError>;

    /// Called when a client pushes an event.
    async fn handle_in(
        &self,
        event: &str,
        payload: &Value,
        socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError>;

    /// Called for PubSub messages from other processes.
    async fn handle_info(
        &self,
        msg: &mahalo_pubsub::PubSubMessage,
        socket: &mut ChannelSocket,
    ) -> Result<(), ChannelError> {
        // Default: push to client
        socket.push(&msg.event, &msg.payload).await;
        Ok(())
    }

    /// Called when the channel process terminates.
    async fn terminate(&self, _reason: &str, _socket: &mut ChannelSocket) {}
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

    // -- Channel default trait methods -----------------------------------------

    struct MinimalChannel;

    #[async_trait]
    impl Channel for MinimalChannel {
        async fn join(
            &self,
            _topic: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Value, ChannelError> {
            Ok(serde_json::json!({}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: &Value,
            _socket: &mut ChannelSocket,
        ) -> Result<Option<Reply>, ChannelError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn handle_info_default_pushes_to_client() {
        use mahalo_pubsub::{PubSub, PubSubMessage};
        use tokio::sync::mpsc;

        let pubsub = PubSub::start();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let msg = PubSubMessage {
            topic: "test:topic".into(),
            event: "greeting".into(),
            payload: serde_json::json!({"hello": "world"}),
        };

        let ch = MinimalChannel;
        ch.handle_info(&msg, &mut socket).await.unwrap();

        let ws_msg = rx.try_recv().expect("should have received a message");
        match ws_msg {
            axum::extract::ws::Message::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["event"], "greeting");
                assert_eq!(parsed["payload"]["hello"], "world");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        pubsub.shutdown();
    }

    #[tokio::test]
    async fn terminate_default_does_nothing() {
        use mahalo_pubsub::PubSub;
        use tokio::sync::mpsc;

        let pubsub = PubSub::start();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut socket = ChannelSocket::new("test:topic".into(), tx, pubsub.clone());

        let ch = MinimalChannel;
        ch.terminate("test", &mut socket).await;
        // no panic = success

        pubsub.shutdown();
    }
}
