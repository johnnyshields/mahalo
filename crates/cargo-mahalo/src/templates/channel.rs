pub fn channel_file(pascal: &str, snake: &str) -> String {
    format!(
        r#"use async_trait::async_trait;
use mahalo::{{Channel, ChannelError, ChannelSocket, Reply}};
use serde_json::Value;

pub struct {pascal}Channel;

#[async_trait]
impl Channel for {pascal}Channel {{
    async fn join(
        &self,
        topic: &str,
        _payload: &Value,
        _socket: &mut ChannelSocket,
    ) -> Result<Value, ChannelError> {{
        tracing::info!("{pascal}Channel joined: {{topic}}");
        Ok(serde_json::json!({{"{snake}": "joined"}}))
    }}

    async fn handle_in(
        &self,
        event: &str,
        payload: &Value,
        _socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError> {{
        tracing::debug!("{pascal}Channel received {{event}}: {{payload}}");
        Ok(Some(Reply::ok(payload.clone())))
    }}

    async fn handle_info(
        &self,
        _msg: &mahalo::PubSubMessage,
        _socket: &mut ChannelSocket,
    ) -> Result<(), ChannelError> {{
        Ok(())
    }}

    async fn terminate(&self, reason: &str, _socket: &mut ChannelSocket) {{
        tracing::info!("{pascal}Channel terminated: {{reason}}");
    }}
}}
"#
    )
}
