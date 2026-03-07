# Mahalo Architecture

## Crate Dependency Graph

```
mahalo (umbrella re-export)
  ├── mahalo-endpoint   → mahalo-router, mahalo-core, rebar-core, monoio
  ├── mahalo-router     → mahalo-core
  ├── mahalo-channel    → mahalo-core, mahalo-pubsub, local-sync
  ├── mahalo-pubsub     → rebar-core, crossbeam-channel
  ├── mahalo-telemetry  → rebar-core
  └── mahalo-core       → rebar-core, http, bytes, percent-encoding
```

All crates share workspace dependencies defined in the root `Cargo.toml`.

## Design Philosophy

Mahalo follows the same layered architecture as [Phoenix Framework](https://www.phoenixframework.org/):

1. **Conn** is the central data structure - a single struct carrying request, response, and state through the entire pipeline.
2. **Plugs** are composable middleware - every transformation is a plug, from authentication to response encoding.
3. **Pipelines** chain plugs and short-circuit on `halt()`.
4. **Routers** match HTTP method + path to handlers, with scoping and pipeline assignment.
5. **Endpoints** bridge the framework to the HTTP server (monoio thread-per-core).
6. **Channels** provide real-time communication over WebSockets using the Phoenix wire protocol.
7. **PubSub** enables inter-process messaging across channels and other components.
8. **Telemetry** provides structured observability events.

Unlike Phoenix (which runs on the BEAM), Mahalo runs on **rebar**, a Rust OTP-inspired runtime using a thread-per-core architecture with monoio. This gives it supervision trees, named processes, and lifecycle management — all with `!Send` thread-local state (no `Arc` on the hot path).

## Request Lifecycle

```
HTTP Request
    │
    ▼
MahaloEndpoint (monoio accept loop, per-thread)
    │
    ├── parse_request()        ← Parse raw HTTP/1.1 → Conn
    │                              (body limited to 2MB, query params URL-decoded)
    │
    ├── MahaloRouter::resolve() ← Match method + path → ResolvedRoute
    │       │
    │       ├── Run Pipelines    ← Each pipeline runs its plugs in order
    │       │   (halt short-circuits)
    │       │
    │       └── Run Handler      ← Plug or Controller action
    │
    └── serialize_response()    ← Serialize Conn → raw HTTP/1.1 response
```

## WebSocket Channel Lifecycle

```
WebSocket Upgrade
    │
    ▼
handle_websocket()
    │
    ├── Spawn send task (forwards local channel → WebSocket)
    │
    └── Receive loop:
        │
        ├── "phx_join"   → handle_join()
        │   ├── ChannelRouter::find() → Channel impl
        │   ├── channel.join() → Ok(resp) / Err
        │   ├── PubSub::subscribe() → spawn forwarder task
        │   └── Track in joined_channels map
        │
        ├── "phx_leave"  → handle_leave()
        │   ├── channel.terminate("leave")
        │   └── Remove from joined_channels
        │
        ├── "heartbeat"  → handle_heartbeat()
        │   └── Reply with {"status": "ok"}
        │
        └── custom event → dispatch_event()
            ├── channel.handle_in()
            └── Reply with result or error
```

## Core Types

### Conn (mahalo-core)

The central request/response struct. Public fields:

| Field | Type | Description |
|-------|------|-------------|
| `method` | `Method` | HTTP method |
| `uri` | `Uri` | Request URI |
| `headers` | `HeaderMap` | Request headers |
| `path_params` | `HashMap<String, String>` | URL path parameters (`:id` → `"42"`) |
| `query_params` | `HashMap<String, String>` | Parsed + URL-decoded query string |
| `remote_addr` | `Option<SocketAddr>` | Client address |
| `body` | `Bytes` | Request body |
| `status` | `StatusCode` | Response status |
| `resp_headers` | `HeaderMap` | Response headers |
| `resp_body` | `Bytes` | Response body |
| `halted` | `bool` | If true, pipeline stops |
| `runtime` | `Option<Rc<Runtime>>` | rebar runtime reference |

Private `assigns` map stores typed state via `AssignKey`.

### AssignKey Pattern

Both `Conn` and `ChannelSocket` use the same `AssignKey` trait for typed state:

```rust
pub trait AssignKey: 'static {
    type Value: 'static;
}
```

This pattern uses `TypeId` internally but provides compile-time type safety at the API surface. Define a zero-sized key type, associate a value type, then use `assign::<Key>(value)` / `get_assign::<Key>()`.

### MahaloRouter

Routes are matched in registration order. Each route stores:
- HTTP method
- Path segments (with `:param` placeholders)
- Handler (Plug or Controller)
- Pipeline names to run before the handler

`scope()` adds a path prefix and pipeline list to all routes registered inside it. `resources()` generates standard CRUD routes for a Controller.

### PubSub

Runs on a dedicated `std::thread` with a blocking `crossbeam_channel::recv()` loop for commands. Topics are lazily created on first subscribe. Each topic maps to a set of crossbeam senders.

`subscribe()` is synchronous and returns `Option<crossbeam::Receiver<PubSubMessage>>` (returns `None` if the PubSub thread has stopped). `broadcast()` is fire-and-forget. `unsubscribe()` triggers cleanup of empty topics.

### Channel

The `Channel` trait has four lifecycle callbacks (using RPITIT, no `async_trait`):

| Method | When Called | Return |
|--------|-----------|--------|
| `join` | Client sends `phx_join` | `Result<Value, ChannelError>` |
| `handle_in` | Client sends custom event | `Result<Option<Reply>, ChannelError>` |
| `handle_info` | PubSub message arrives | `Result<(), ChannelError>` (default: push to client) |
| `terminate` | Leave or disconnect | `()` |

### Telemetry

Events have a hierarchical name (e.g. `["mahalo", "endpoint", "stop"]`), measurements (`HashMap<String, f64>`), and metadata (`HashMap<String, Value>`). Handlers are matched by name prefix. `span()` automatically emits start/stop events with `duration_ms`.

## Error Types

- **ChannelError**: `NotAuthorized`, `InvalidTopic(String)`, `Internal(String)`. Implements `Display` + `Error`.

## Safety & Limits

- Request body is limited to 2MB by default (`DEFAULT_BODY_LIMIT` in endpoint.rs)
- PubSub subscribe does not panic on server drop (returns `Option`)
- WebSocket send failures are logged with `tracing::warn`
- Query params are percent-decoded
