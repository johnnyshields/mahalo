# WebSocket Channels Guide

Mahalo channels provide Phoenix-compatible real-time communication over WebSockets. Each channel handles a topic pattern and has lifecycle callbacks for join, events, PubSub messages, and disconnect.

## Overview

```
Client (JavaScript/Phoenix.js)
    │
    │ WebSocket (Phoenix wire format)
    ▼
handle_websocket()
    │
    ├── ChannelRouter::find(topic) → Channel impl
    │
    ├── phx_join   → channel.join()
    ├── custom evt → channel.handle_in()
    ├── PubSub msg → channel.handle_info()
    ├── phx_leave  → channel.terminate()
    └── heartbeat  → automatic reply
```

## Defining a Channel

Implement the `Channel` trait:

```rust
use mahalo::{Channel, ChannelError, ChannelSocket, BoxFuture, Reply};
use serde_json::Value;

struct ChatChannel;

impl Channel for ChatChannel {
    /// Called when a client joins this channel's topic.
    /// Return Ok(payload) to accept, Err to reject.
    fn join<'a>(
        &'a self,
        topic: &'a str,
        payload: &'a Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Value, ChannelError>> {
        Box::pin(async move {
            // Validate authorization
            let token = payload.get("token").and_then(|t| t.as_str());
            if token != Some("valid") {
                return Err(ChannelError::NotAuthorized);
            }

            // Store state on the socket
            socket.assign::<UserName>("anonymous".to_string());

            Ok(serde_json::json!({"status": "joined", "topic": topic}))
        })
    }

    /// Called when the client sends a custom event.
    /// Return Ok(Some(reply)) to reply, Ok(None) for no reply.
    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
        Box::pin(async move {
            match event {
                "new_msg" => {
                    // Broadcast to all subscribers on this topic
                    socket.broadcast("new_msg", payload.clone());
                    Ok(Some(Reply::ok(serde_json::json!({"sent": true}))))
                }
                "ping" => {
                    Ok(Some(Reply::ok(serde_json::json!({"pong": true}))))
                }
                _ => Ok(None)
            }
        })
    }

    /// Called when a PubSub message arrives for this topic.
    /// Default implementation pushes the message to the client.
    fn handle_info<'a>(
        &'a self,
        msg: &'a mahalo_pubsub::PubSubMessage,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            // Custom filtering or transformation
            socket.push(&msg.event, &msg.payload).await;
            Ok(())
        })
    }

    /// Called on leave or disconnect. Cleanup resources here.
    fn terminate<'a>(
        &'a self,
        reason: &'a str,
        _socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            println!("Channel terminated: {reason}");
        })
    }
}
```

## Channel Socket

`ChannelSocket` holds per-connection state for a single topic:

```rust
// Push a message to this specific client
socket.push("event_name", &payload).await;

// Broadcast to ALL subscribers on the topic via PubSub
socket.broadcast("event_name", payload);

// Reply to a specific message ref
socket.reply("msg_ref", &Reply::ok(json!({})));

// Typed assigns (same AssignKey pattern as Conn)
struct UserName;
impl AssignKey for UserName { type Value = String; }

socket.assign::<UserName>("alice".to_string());
let name = socket.get_assign::<UserName>(); // Some(&"alice")
```

## Channel Router

Map topic patterns to channel implementations:

```rust
use mahalo::ChannelRouter;

let channel_router = ChannelRouter::new()
    .channel("room:*", ChatChannel)        // wildcard: room:lobby, room:123, etc.
    .channel("notifications", NotifyChannel); // exact match
```

**Topic patterns:**
- `"room:*"` - matches any topic starting with `room:` (e.g. `room:lobby`, `room:42`)
- `"room:lobby"` - exact match only

Routes are matched in registration order; first match wins.

## Wiring It Up

Channels are registered via the endpoint's factory pattern using `WsConfig`:

```rust
use mahalo::{MahaloEndpoint, ChannelRouter, PubSub, WsConfig};

let endpoint = MahaloEndpoint::new(|| build_router(), addr)
    .channels(|| WsConfig {
        channel_router: ChannelRouter::new()
            .channel("room:*", ChatChannel),
        pubsub: PubSub::start(),
    });

endpoint.start().unwrap();
```

Each worker thread creates its own `WsConfig` via the factory closure, keeping channel state thread-local.

## Phoenix Wire Protocol

All WebSocket messages use JSON with four fields:

```json
{
  "topic": "room:lobby",
  "event": "new_msg",
  "payload": {"text": "hello"},
  "ref": "1"
}
```

### Built-in Events

| Event | Direction | Description |
|-------|-----------|-------------|
| `phx_join` | Client -> Server | Join a topic. Calls `channel.join()` |
| `phx_leave` | Client -> Server | Leave a topic. Calls `channel.terminate("leave")` |
| `heartbeat` | Client -> Server | Keep-alive. Auto-replied with `{"status":"ok"}` |
| `phx_reply` | Server -> Client | Reply to a client message (same `ref`) |

Custom events (any string not matching the above) are dispatched to `channel.handle_in()`.

### Reply Format

Replies use the `phx_reply` event with a nested status/response:

```json
{
  "topic": "room:lobby",
  "event": "phx_reply",
  "payload": {
    "status": "ok",
    "response": {"joined": true}
  },
  "ref": "1"
}
```

## Error Handling

`ChannelError` has three variants:

```rust
ChannelError::NotAuthorized           // Client not allowed
ChannelError::InvalidTopic(String)    // Bad topic format
ChannelError::Internal(String)        // Server-side error
```

When `join()` returns `Err`, the client receives:
```json
{"status": "error", "response": {"reason": "join failed"}}
```

When `handle_in()` returns `Err`, the client receives:
```json
{"status": "error", "response": {"reason": "error"}}
```

## PubSub Integration

When a client joins a topic, `handle_websocket` automatically subscribes to PubSub for that topic. Messages broadcast via `socket.broadcast()` or `pubsub.broadcast()` are forwarded to all connected clients on the topic.

```
Client A joins "room:lobby"
    → PubSub::subscribe("room:lobby")

Client A calls socket.broadcast("new_msg", payload)
    → PubSub::broadcast("room:lobby", "new_msg", payload)
    → All subscribers receive it (including Client B, C, etc.)
    → Each client's PubSub forwarder task pushes it to their WebSocket
```

If PubSub subscribe fails (server dropped), a warning is logged and the channel continues to work for direct push/reply but won't receive broadcasts.

## Client Compatibility

Mahalo channels are compatible with the [Phoenix.js](https://hexdocs.pm/phoenix/js/) JavaScript client. Connect with:

```javascript
import { Socket } from "phoenix";

let socket = new Socket("/socket", { params: { token: "valid" } });
socket.connect();

let channel = socket.channel("room:lobby", {});
channel.join()
  .receive("ok", resp => console.log("Joined!", resp))
  .receive("error", resp => console.log("Failed", resp));

channel.push("new_msg", { text: "hello" })
  .receive("ok", resp => console.log("Sent!", resp));

channel.on("new_msg", msg => console.log("Got:", msg));
```
