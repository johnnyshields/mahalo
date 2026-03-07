# Mahalo Quick Start Guide

## Prerequisites

- Rust (edition 2024)
- The [rebar](https://github.com/johnnyshields/rebar) runtime cloned at `../rebar` relative to this repo

## Installation

Add `mahalo` to your `Cargo.toml`:

```toml
[dependencies]
mahalo = { path = "../mahalo/crates/mahalo" }
rebar-core = { git = "https://github.com/johnnyshields/rebar.git", branch = "rebar-v4" }
```

## Hello World

```rust
use std::net::SocketAddr;

use mahalo::{Conn, MahaloEndpoint, MahaloRouter, plug_fn};
use http::StatusCode;

fn main() {
    let addr: SocketAddr = "127.0.0.1:4000".parse().unwrap();
    let endpoint = MahaloEndpoint::new(
        || {
            MahaloRouter::new()
                .get("/", plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK)
                        .put_resp_header("content-type", "text/plain")
                        .put_resp_body("Aloha from Mahalo!")
                }))
        },
        addr,
    );
    endpoint.start().unwrap();
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
            s.resources("/rooms", RoomController);
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
use mahalo::{Channel, ChannelError, ChannelSocket, BoxFuture, Reply};
use serde_json::Value;

struct RoomChannel;

impl Channel for RoomChannel {
    fn join<'a>(
        &'a self,
        topic: &'a str,
        _payload: &'a Value,
        _socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Value, ChannelError>> {
        Box::pin(async move {
            println!("Client joined {topic}");
            Ok(serde_json::json!({"joined": topic}))
        })
    }

    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
        Box::pin(async move {
            match event {
                "new_msg" => {
                    // Broadcast to all subscribers
                    socket.broadcast("new_msg", payload.clone());
                    Ok(Some(Reply::ok(serde_json::json!({"sent": true}))))
                }
                _ => Ok(None),
            }
        })
    }
}
```

### Register Channels with the Endpoint

```rust
use mahalo::{ChannelRouter, PubSub, WsConfig};

// Channels are registered via the endpoint's factory pattern:
let endpoint = MahaloEndpoint::new(|| build_router(), addr)
    .channels(|| WsConfig {
        channel_router: ChannelRouter::new()
            .channel("room:*", RoomChannel),
        pubsub: PubSub::start(),
    });
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

// Subscribe (synchronous, returns Option<crossbeam::Receiver>)
if let Some(rx) = pubsub.subscribe("notifications") {
    // Receive in a loop (blocking recv on the crossbeam channel)
    std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
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

// Attach a handler (synchronous)
telemetry.attach(&["mahalo", "endpoint"], |event| {
    println!("{:?}: {:?}", event.name, event.measurements);
});

// Emit an event (synchronous)
telemetry.execute(
    &["mahalo", "endpoint", "stop"],
    HashMap::from([("duration_ms".into(), 42.0)]),
    HashMap::new(),
);

// Or use span() for automatic start/stop + duration
let result = telemetry.span(
    &["mahalo", "endpoint"],
    HashMap::new(),
    || { do_work() },
);
```

## Supervision with rebar

Mahalo integrates with rebar for OTP-style supervision:

```rust
use mahalo::{MahaloEndpoint, MahaloRouter, PubSub};

let addr = "127.0.0.1:4000".parse().unwrap();
let endpoint = MahaloEndpoint::new(
    || MahaloRouter::new(), // ... configure routes
    addr,
);

// The endpoint manages its own worker threads.
// PubSub runs on a dedicated std::thread.
endpoint.start().unwrap();
```

## Next Steps

- [Architecture Guide](./architecture.md) - Deep dive into the crate structure
- [Conn Reference](./conn.md) - Full Conn API documentation
- [Channels Guide](./channels.md) - WebSocket channels in depth
- [Telemetry Guide](./telemetry.md) - Observability and metrics
