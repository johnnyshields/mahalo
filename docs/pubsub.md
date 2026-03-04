# PubSub Guide

Mahalo PubSub provides topic-based publish/subscribe messaging. It enables real-time communication between channels, request handlers, and background tasks.

## Starting PubSub

```rust
use mahalo::PubSub;

let pubsub = PubSub::start();
```

`PubSub::start()` spawns a background tokio task that manages topic-to-channel mappings. The returned `PubSub` handle is `Clone + Send + Sync`.

## Subscribing

```rust
if let Some(mut rx) = pubsub.subscribe("room:lobby").await {
    tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            println!("{}: {}", msg.event, msg.payload);
        }
    });
}
```

`subscribe()` returns `Option<broadcast::Receiver<PubSubMessage>>`:
- `Some(rx)` on success
- `None` if the PubSub server task has been dropped

Each topic gets a broadcast channel with capacity 256. New topics are created lazily on first subscribe.

## Broadcasting

```rust
pubsub.broadcast("room:lobby", "new_msg", serde_json::json!({"text": "hello"}));
```

Broadcasting is fire-and-forget:
- If the topic has subscribers, they all receive the message
- If the topic has no subscribers, the message is silently dropped
- If the PubSub server is down, the message is silently dropped

## PubSubMessage

```rust
pub struct PubSubMessage {
    pub topic: String,    // e.g. "room:lobby"
    pub event: String,    // e.g. "new_msg"
    pub payload: serde_json::Value,
}
```

## Unsubscribing

```rust
pubsub.unsubscribe("room:lobby");
```

This triggers a cleanup pass. If there are no remaining receivers for the topic, the internal channel is removed. Call this after dropping a receiver to free memory.

## Shutdown

```rust
pubsub.shutdown();
```

Gracefully stops the PubSub server task. After shutdown, `subscribe()` will return `None` and `broadcast()` will be a no-op.

## Supervision

PubSub can be supervised under a rebar supervisor tree:

```rust
use std::sync::Arc;

let pubsub = Arc::new(PubSub::start());
let child = PubSub::child_entry(Arc::clone(&pubsub));
// Add to supervisor...
```

The supervised process holds the PubSub handle alive and waits for ctrl-c, then calls `shutdown()`.

## With Channels

When a client joins a channel topic, `handle_websocket()` automatically subscribes to PubSub for that topic. Messages broadcast via `ChannelSocket::broadcast()` go through PubSub and are forwarded to all clients on the same topic.

```rust
// Inside a Channel's handle_in:
socket.broadcast("new_msg", payload.clone());
// This calls pubsub.broadcast() on the socket's topic
```

## Error Handling

PubSub is designed to be resilient:

- **Subscribe fails**: Returns `None` with a `tracing::warn` log
- **Broadcast to empty topic**: Silently dropped (by design)
- **Server dropped**: All operations become no-ops
- **Channel full**: Uses tokio broadcast semantics (lagged receivers skip messages)
