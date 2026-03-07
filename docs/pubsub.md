# PubSub Guide

Mahalo PubSub provides topic-based publish/subscribe messaging. It enables real-time communication between channels, request handlers, and background tasks.

## Starting PubSub

```rust
use mahalo::PubSub;

let pubsub = PubSub::start();
```

`PubSub::start()` spawns a dedicated `std::thread` that manages topic-to-channel mappings via blocking `crossbeam_channel::recv()`. The returned `PubSub` handle is `Clone + Send + Sync`.

## Subscribing

```rust
if let Some(rx) = pubsub.subscribe("room:lobby") {
    std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            println!("{}: {}", msg.event, msg.payload);
        }
    });
}
```

`subscribe()` is synchronous and returns `Option<crossbeam::Receiver<PubSubMessage>>`:
- `Some(rx)` on success
- `None` if the PubSub server thread has stopped

Each topic maps to a set of crossbeam senders. New topics are created lazily on first subscribe.

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

Gracefully stops the PubSub server thread. After shutdown, `subscribe()` will return `None` and `broadcast()` will be a no-op.

## Supervision

PubSub can be supervised under a rebar supervisor tree:

```rust
let (pubsub, child) = PubSub::new_supervised();
// Add child to supervisor...
```

The supervised process holds the PubSub handle alive and manages its lifecycle.

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
- **Channel full**: Uses crossbeam unbounded channels (no message loss under normal conditions)
