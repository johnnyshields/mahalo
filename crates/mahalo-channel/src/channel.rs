use async_trait::async_trait;
use serde_json::Value;

use crate::socket::ChannelSocket;

#[derive(Debug)]
pub enum ChannelError {
    NotAuthorized,
    InvalidTopic(String),
    Internal(String),
}

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
