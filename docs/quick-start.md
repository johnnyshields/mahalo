# Mahalo Quick Start Guide

## Prerequisites

- Rust (edition 2024)
- The [rebar](https://github.com/johnnyshields/rebar) runtime cloned at `../rebar` relative to this repo

## Installation

Add `mahalo` to your `Cargo.toml`:

```toml
[dependencies]
mahalo = { path = "../mahalo/crates/mahalo" }
tokio = { version = "1", features = ["full"] }
rebar-core = { path = "../rebar/crates/rebar-core" }
```

## Hello World

```rust
use std::net::SocketAddr;
use std::sync::Arc;

use mahalo::{Conn, MahaloEndpoint, MahaloRouter, plug_fn};
use http::StatusCode;
use rebar_core::runtime::Runtime;

#[tokio::main]
async fn main() {
    let runtime = Arc::new(Runtime::new(4));

    let router = MahaloRouter::new()
        .get("/", plug_fn(|conn: Conn| async {
            conn.put_status(StatusCode::OK)
                .put_resp_header("content-type", "text/plain")
                .put_resp_body("Aloha from Mahalo!")
        }));

    let addr: SocketAddr = "127.0.0.1:4000".parse().unwrap();
    let endpoint = MahaloEndpoint::new(router, addr, runtime);
    endpoint.start().await.unwrap();
}
```

Run it:

```bash
cargo run
# => Mahalo endpoint listening on 127.0.0.1:4000
```

Test it:

```bash
curl http://127.0.0.1:4000/
# => Aloha from Mahalo!
```

## Adding Routes with Scopes and Pipelines

Pipelines are ordered sequences of plugs that run before your handler. Scopes group routes under a path prefix and apply pipelines.

```rust
use mahalo::{Conn, MahaloRouter, Pipeline, plug_fn};
use http::StatusCode;

fn build_router() -> MahaloRouter {
    // A pipeline that adds a response header
    let api_pipeline = Pipeline::new("api")
        .plug(plug_fn(|conn: Conn| async {
            conn.put_resp_header("content-type", "application/json")
        }));

    MahaloRouter::new()
        .pipeline(api_pipeline)
        .scope("/api", &["api"], |s| {
            s.get("/health", plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body(r#"{"status":"ok"}"#)
            }));
            s.post("/echo", plug_fn(|conn: Conn| async {
                let body = conn.body.clone();
                conn.put_status(StatusCode::OK).put_resp_body(body)
            }));
        })
}
```

## RESTful Controllers

For CRUD resources, implement the `Controller` trait and use `resources()`:

```rust
use std::sync::Arc;
use mahalo::{Conn, Controller, BoxFuture, MahaloRouter};
use http::StatusCode;

struct RoomController;

impl Controller for RoomController {
    fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            conn.put_status(StatusCode::OK)
                .put_resp_body(r#"[{"id":1,"name":"lobby"}]"#)
        })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = conn.path_params.get("id").cloned().unwrap_or_default();
            conn.put_status(StatusCode::OK)
                .put_resp_body(format!(r#"{{"id":{id}}}"#))
        })
    }

    fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            conn.put_status(StatusCode::CREATED)
                .put_resp_body(r#"{"created":true}"#)
        })
    }
}

fn build_router() -> MahaloRouter {
    MahaloRouter::new()
        .scope("/api", &[], |s| {
            s.resources("/rooms", Arc::new(RoomController));
        })
}
// Generates: GET /api/rooms, GET /api/rooms/:id, POST /api/rooms,
//            PUT /api/rooms/:id, DELETE /api/rooms/:id
```

## Typed Assigns (Per-Request State)

Store typed state on `Conn` using the `AssignKey` pattern:

```rust
use mahalo::{AssignKey, Conn, plug_fn};

// 1. Define a key type
struct CurrentUserId;
impl AssignKey for CurrentUserId {
    type Value = u64;
}

// 2. Set it in a plug
let auth_plug = plug_fn(|conn: Conn| async {
    conn.assign::<CurrentUserId>(42)
});

// 3. Read it later
let handler = plug_fn(|conn: Conn| async {
    if let Some(user_id) = conn.get_assign::<CurrentUserId>() {
        println!("User: {user_id}");
    }
    conn
});
```

## WebSocket Channels

Mahalo provides Phoenix-compatible real-time channels.

### Define a Channel

```rust
use async_trait::async_trait;
use mahalo::{Channel, ChannelError, ChannelSocket, Reply};
use serde_json::Value;

struct RoomChannel;

#[async_trait]
impl Channel for RoomChannel {
    async fn join(
        &self,
        topic: &str,
        _payload: &Value,
        _socket: &mut ChannelSocket,
    ) -> Result<Value, ChannelError> {
        println!("Client joined {topic}");
        Ok(serde_json::json!({"joined": topic}))
    }

    async fn handle_in(
        &self,
        event: &str,
        payload: &Value,
        socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError> {
        match event {
            "new_msg" => {
                // Broadcast to all subscribers
                socket.broadcast("new_msg", payload.clone());
                Ok(Some(Reply::ok(serde_json::json!({"sent": true}))))
            }
            _ => Ok(None),
        }
    }
}
```

### Register and Handle WebSocket Upgrades

```rust
use std::sync::Arc;
use mahalo::{ChannelRouter, PubSub, handle_websocket};

let pubsub = PubSub::start();
let channel_router = Arc::new(
    ChannelRouter::new()
        .channel("room:*", Arc::new(RoomChannel))
);

// In your Axum WebSocket handler:
// handle_websocket(ws, channel_router, pubsub).await;
```

### Client Wire Format

Channels use the Phoenix wire format over WebSocket:

```json
{"topic": "room:lobby", "event": "phx_join", "payload": {}, "ref": "1"}
{"topic": "room:lobby", "event": "new_msg", "payload": {"text": "hello"}, "ref": "2"}
{"topic": "room:lobby", "event": "phx_leave", "payload": {}, "ref": "3"}
{"topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": "4"}
```

## PubSub

Broadcast messages across processes:

```rust
use mahalo::PubSub;

let pubsub = PubSub::start();

// Subscribe (returns Option<Receiver>)
if let Some(mut rx) = pubsub.subscribe("notifications").await {
    tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            println!("Got: {} - {}", msg.event, msg.payload);
        }
    });
}

// Broadcast (fire-and-forget)
pubsub.broadcast("notifications", "alert", serde_json::json!({"level": "warn"}));

// Cleanup
pubsub.shutdown();
```

## Telemetry

Emit and observe structured events:

```rust
use std::collections::HashMap;
use mahalo::Telemetry;

let telemetry = Telemetry::new(512);

// Attach a handler
telemetry.attach(&["mahalo", "endpoint"], |event| {
    println!("{:?}: {:?}", event.name, event.measurements);
}).await;

// Emit an event
telemetry.execute(
    &["mahalo", "endpoint", "stop"],
    HashMap::from([("duration_ms".into(), 42.0)]),
    HashMap::new(),
).await;

// Or use span() for automatic start/stop + duration
let result = telemetry.span(
    &["mahalo", "endpoint"],
    HashMap::new(),
    || async { do_work().await },
).await;
```

## Supervision with rebar

Mahalo integrates with rebar for OTP-style supervision:

```rust
use std::sync::Arc;
use mahalo::{MahaloEndpoint, MahaloRouter, PubSub};
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::Supervisor;

let runtime = Arc::new(Runtime::new(4));

let pubsub = Arc::new(PubSub::start());
let router = MahaloRouter::new(); // ... configure routes
let addr = "127.0.0.1:4000".parse().unwrap();
let endpoint = MahaloEndpoint::new(router, addr, Arc::clone(&runtime));

let supervisor = Supervisor::new()
    .child(PubSub::child_entry(pubsub))
    .child(endpoint.child_entry());

// Start under supervision...
```

## Next Steps

- [Architecture Guide](./architecture.md) - Deep dive into the crate structure
- [Conn Reference](./conn.md) - Full Conn API documentation
- [Channels Guide](./channels.md) - WebSocket channels in depth
- [Telemetry Guide](./telemetry.md) - Observability and metrics
