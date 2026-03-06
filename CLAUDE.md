# Mahalo - Development Guide for Claude

## Project Overview

Mahalo is a Phoenix-like web framework for Rust, built on top of the [rebar](../rebar) OTP-inspired runtime. It provides Plug-based middleware, RESTful routing, Phoenix-compatible WebSocket channels, PubSub, telemetry, and built-in plugs for security, logging, static files, and more.

## Workspace Structure

```
crates/
  mahalo/           # Umbrella crate - re-exports everything
  mahalo-core/      # Conn, Plug, Pipeline, Controller, AssignKey
  mahalo-router/    # MahaloRouter, scopes, resources, named routes, path matching
  mahalo-endpoint/  # io_uring HTTP server, error handlers, after-plugs, rebar supervision
  mahalo-pubsub/    # Topic-based PubSub with broadcast channels
  mahalo-channel/   # Phoenix-compatible WebSocket channels
  mahalo-telemetry/ # Telemetry events, handlers, spans
  mahalo-plug/      # Built-in plugs: CSRF, Request ID, Logger, Secure Headers, Static Files, ETag
```

## Key Abstractions

- **Conn** (`mahalo-core`): Request + response struct flowing through the pipeline. Builder-style API. Has typed assigns via `AssignKey` trait. Convenience methods: `Conn::test()` for test construction, `get_resp_header()` for reading response headers.
- **Plug** (`mahalo-core`): Middleware trait. Use `plug_fn()` to wrap async closures.
- **Pipeline** (`mahalo-core`): Ordered sequence of Plugs, halts early if `conn.halted`.
- **Controller** (`mahalo-core`): RESTful trait with index/show/create/update/delete.
- **MahaloRouter** (`mahalo-router`): Routes with scopes, named pipelines, named routes, `resources()` for CRUD, and `path_for()` reverse routing.
- **MahaloEndpoint** (`mahalo-endpoint`): Bridges MahaloRouter to io_uring HTTP server. Body limit: 2MB default. Supports custom error handlers and after-plugs (post-handler pipeline).
- **ErrorHandler** (`mahalo-endpoint`): `Arc<dyn Fn(StatusCode, Conn) -> Conn + Send + Sync>`. Built-in: `json_error_handler()`, `text_error_handler()`.
- **PubSub** (`mahalo-pubsub`): Background tokio task managing topic -> broadcast channel map.
- **Channel** (`mahalo-channel`): Phoenix-compatible WebSocket channels with join/handle_in/handle_info/terminate.
- **ChannelSocket** (`mahalo-channel`): Per-connection state with typed assigns (uses `AssignKey`, same as Conn).
- **ChannelRouter** (`mahalo-channel`): Maps topic patterns (e.g. `"room:*"`) to Channel impls.
- **Telemetry** (`mahalo-telemetry`): Event system with `execute()`, `attach()` handlers, and `span()` for timing.

### Built-in Plugs (`mahalo-plug`)

- **RequestIdPlug**: Reads `x-request-id` from request headers or generates UUID v4. Stores in assigns (`RequestId` key) and sets response header.
- **SecureHeaders**: Sets security response headers (CSP, HSTS, X-Frame-Options, etc.). Builder with `new()` (defaults), `put()`, `remove()`.
- **CsrfProtection**: CSRF token generation (safe methods) and HMAC-SHA256 validation (unsafe methods). Uses `x-csrf-token` header. Returns 403 on failure.
- **request_logger()**: Returns `(start_plug, finish_plug)` pair. Start stores `Instant::now()` in assigns, finish logs method/path/status/duration via `tracing::info!`.
- **StaticFiles**: Serves static files from a directory under a URL prefix. Path traversal protection, content-type detection, cache-control, HEAD support.
- **ETag**: Computes weak ETag (`W/"<sha256>"`) for 200 responses. Returns 304 Not Modified when `if-none-match` matches. Best used as an after-plug.

## Build & Test

```bash
cargo check --workspace
cargo test --workspace
```

There are no features flags. Edition is 2024. All crates use `workspace = true` for shared deps.

## Dependencies

- **rebar-core**: OTP-style runtime, supervision, processes (path dep at `../rebar`)
- **io-uring 0.7**: Linux io_uring HTTP server
- **tokio**: Async runtime (full features)
- **serde/serde_json**: Serialization
- **http**: HTTP types (Method, StatusCode, Uri, HeaderMap)
- **bytes**: Efficient byte buffers
- **tracing**: Structured logging
- **async-trait**: Async trait methods (Channel, Controller)
- **percent-encoding**: URL decoding for query params
- **futures**: Stream/Sink for WebSocket handling
- **sha2**: SHA-256 hashing (ETag, CSRF)
- **hmac**: HMAC computation (CSRF)
- **uuid**: UUID v4 generation (Request ID)
- **rand**: Random byte generation (CSRF)
- **base64**: Base64 encoding/decoding (CSRF tokens)

## Patterns & Conventions

- **Builder pattern**: Conn methods return `self` for chaining: `conn.put_status(OK).put_resp_body("hi")`
- **Typed assigns**: Both Conn and ChannelSocket use the `AssignKey` trait for type-safe state storage. Define a zero-sized struct, impl `AssignKey` with an associated `Value` type.
- **Phoenix wire format**: WebSocket messages use `PhoenixMessage` with `topic`, `event`, `payload`, `ref` fields. Events: `phx_join`, `phx_leave`, `heartbeat`, custom events.
- **Topic patterns**: ChannelRouter supports exact match (`"room:lobby"`) and wildcard (`"room:*"`).
- **PubSub subscribe returns Option**: `subscribe()` returns `Option<Receiver>` (not panic). Always handle the `None` case.
- **Error handling**: `ChannelError` implements `Display` + `Error`. Prefer `Result` over panics.
- **Send failures are logged**: `tracing::warn` on WebSocket send failures in push/reply/heartbeat.
- **No unwrap in production paths**: Only use `.unwrap()` in tests.
- **Named routes**: Use `get_named()`, `post_named()`, etc. for reverse routing. `resources()` auto-generates names like `rooms_index`, `rooms_show`.
- **After-plugs**: Use `endpoint.after(plug)` for plugs that run after the handler (e.g., ETag, logger finish).

## Testing Conventions

- Each crate has `#[cfg(test)] mod tests` in source files (not separate test files).
- Integration tests live alongside unit tests (see `endpoint.rs` integration test).
- Use `tokio::test` for async tests, plain `#[test]` for sync.
- PubSub tests should call `pubsub.shutdown()` at the end.
- Use `"127.0.0.1:0"` for random port binding in integration tests.
- Use `Conn::test()` for quick test Conn construction (GET /).

## File Layout within Crates

Each crate follows: `src/lib.rs` (re-exports) + `src/<module>.rs` (implementation + tests).
No nested module directories. Keep it flat.

## Common Tasks

- **Add a new plug**: Write an async fn taking and returning `Conn`, wrap with `plug_fn()`. Or implement `Plug` trait directly for structs with state.
- **Add a new route**: Use `router.get("/path", plug)` or `scope()` + `ScopeBuilder` methods.
- **Add a named route**: Use `router.get_named("/path", "name", plug)` or `scope_builder.get_named()`. Retrieve URL with `router.path_for("name", &[("param", "value")])`.
- **Add a new channel**: Implement the `Channel` trait, register with `ChannelRouter::channel("topic:*", handler)`.
- **Add telemetry**: Use `telemetry.execute()` to emit, `telemetry.attach()` to listen, `telemetry.span()` for timing.
- **Store per-request state**: Define `struct MyKey;` + `impl AssignKey for MyKey { type Value = T; }`, then `conn.assign::<MyKey>(value)`.
- **Custom error handling**: Use `endpoint.error_handler(json_error_handler())` or provide a custom `Fn(StatusCode, Conn) -> Conn`.
- **Add after-plugs**: Use `endpoint.after(ETag::new())` to run plugs after the handler sets the response.
- **Serve static files**: Use `StaticFiles::new("/static", "./public").cache_control("public, max-age=86400")` as a plug.
- **Add security headers**: Add `SecureHeaders::new()` to a pipeline. Customize with `.put("header", "value")` or `.remove("header")`.
- **CSRF protection**: Add `CsrfProtection::new("secret-key")` to a pipeline. Safe methods get tokens, unsafe methods require valid `x-csrf-token` header.
- **Request logging**: Use `let (start, finish) = request_logger();` — add `start` to a pipeline and `finish` as an after-plug.
