# Mahalo - Development Guide for Claude

## Project Overview

Mahalo is a Phoenix-like web framework for Rust, built on top of the [rebar](../rebar) OTP-inspired runtime. It provides Plug-based middleware, RESTful routing, Phoenix-compatible WebSocket channels, PubSub, and telemetry.

## Workspace Structure

```
crates/
  mahalo/           # Umbrella crate - re-exports everything
  mahalo-core/      # Conn, Plug, Pipeline, Controller, AssignKey
  mahalo-router/    # MahaloRouter, scopes, resources, path matching
  mahalo-endpoint/  # Axum bridge, HTTP server, rebar supervision
  mahalo-pubsub/    # Topic-based PubSub with broadcast channels
  mahalo-channel/   # Phoenix-compatible WebSocket channels
  mahalo-telemetry/ # Telemetry events, handlers, spans
```

## Key Abstractions

- **Conn** (`mahalo-core`): Request + response struct flowing through the pipeline. Builder-style API. Has typed assigns via `AssignKey` trait.
- **Plug** (`mahalo-core`): Middleware trait. Use `plug_fn()` to wrap async closures.
- **Pipeline** (`mahalo-core`): Ordered sequence of Plugs, halts early if `conn.halted`.
- **Controller** (`mahalo-core`): RESTful trait with index/show/create/update/delete.
- **MahaloRouter** (`mahalo-router`): Routes with scopes, named pipelines, and `resources()` for CRUD.
- **MahaloEndpoint** (`mahalo-endpoint`): Bridges MahaloRouter to Axum. Body limit: 2MB default.
- **PubSub** (`mahalo-pubsub`): Background tokio task managing topic -> broadcast channel map.
- **Channel** (`mahalo-channel`): Phoenix-compatible WebSocket channels with join/handle_in/handle_info/terminate.
- **ChannelSocket** (`mahalo-channel`): Per-connection state with typed assigns (uses `AssignKey`, same as Conn).
- **ChannelRouter** (`mahalo-channel`): Maps topic patterns (e.g. `"room:*"`) to Channel impls.
- **Telemetry** (`mahalo-telemetry`): Event system with `execute()`, `attach()` handlers, and `span()` for timing.

## Build & Test

```bash
cargo check --workspace
cargo test --workspace
```

There are no features flags. Edition is 2024. All crates use `workspace = true` for shared deps.

## Dependencies

- **rebar-core**: OTP-style runtime, supervision, processes (path dep at `../rebar`)
- **axum 0.8**: HTTP server (with `ws` feature for WebSockets)
- **tokio**: Async runtime (full features)
- **serde/serde_json**: Serialization
- **http**: HTTP types (Method, StatusCode, Uri, HeaderMap)
- **bytes**: Efficient byte buffers
- **tracing**: Structured logging
- **async-trait**: Async trait methods (Channel, Controller)
- **percent-encoding**: URL decoding for query params
- **futures**: Stream/Sink for WebSocket handling

## Patterns & Conventions

- **Builder pattern**: Conn methods return `self` for chaining: `conn.put_status(OK).put_resp_body("hi")`
- **Typed assigns**: Both Conn and ChannelSocket use the `AssignKey` trait for type-safe state storage. Define a zero-sized struct, impl `AssignKey` with an associated `Value` type.
- **Phoenix wire format**: WebSocket messages use `PhoenixMessage` with `topic`, `event`, `payload`, `ref` fields. Events: `phx_join`, `phx_leave`, `heartbeat`, custom events.
- **Topic patterns**: ChannelRouter supports exact match (`"room:lobby"`) and wildcard (`"room:*"`).
- **PubSub subscribe returns Option**: `subscribe()` returns `Option<Receiver>` (not panic). Always handle the `None` case.
- **Error handling**: `ChannelError` implements `Display` + `Error`. Prefer `Result` over panics.
- **Send failures are logged**: `tracing::warn` on WebSocket send failures in push/reply/heartbeat.
- **No unwrap in production paths**: Only use `.unwrap()` in tests.

## Testing Conventions

- Each crate has `#[cfg(test)] mod tests` in source files (not separate test files).
- Integration tests live alongside unit tests (see `endpoint.rs` integration test).
- Use `tokio::test` for async tests, plain `#[test]` for sync.
- PubSub tests should call `pubsub.shutdown()` at the end.
- Use `"127.0.0.1:0"` for random port binding in integration tests.

## File Layout within Crates

Each crate follows: `src/lib.rs` (re-exports) + `src/<module>.rs` (implementation + tests).
No nested module directories. Keep it flat.

## Common Tasks

- **Add a new plug**: Write an async fn taking and returning `Conn`, wrap with `plug_fn()`.
- **Add a new route**: Use `router.get("/path", plug)` or `scope()` + `ScopeBuilder` methods.
- **Add a new channel**: Implement the `Channel` trait, register with `ChannelRouter::channel("topic:*", handler)`.
- **Add telemetry**: Use `telemetry.execute()` to emit, `telemetry.attach()` to listen, `telemetry.span()` for timing.
- **Store per-request state**: Define `struct MyKey;` + `impl AssignKey for MyKey { type Value = T; }`, then `conn.assign::<MyKey>(value)`.
