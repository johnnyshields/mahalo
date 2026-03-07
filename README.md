# 🌺 Mahalo

A Phoenix-like web framework for Rust, built on top of [rebar](https://github.com/johnnyshields/rebar) — an OTP-inspired runtime.

Mahalo brings the ergonomics of Elixir's Phoenix Framework to Rust: Plug-based middleware pipelines, RESTful routing with scopes and resources, Phoenix-compatible WebSocket channels, PubSub, telemetry, and batteries-included plugs for security, logging, static files, and more.

## Features

- **Plug Pipeline** — Composable middleware with early halt support
- **Router** — Scoped routes, named routes, `resources()` for CRUD, reverse routing with `path_for()`
- **Controllers** — RESTful trait with index/show/create/update/delete
- **WebSocket Channels** — Phoenix-compatible protocol with topic patterns, join/leave, push/reply/broadcast
- **PubSub** — Topic-based broadcast channels for real-time messaging
- **Telemetry** — Event system with handlers and timing spans
- **Built-in Plugs** — CSRF protection, request ID, secure headers, request logger, static files, ETag
- **Typed Assigns** — Type-safe per-request and per-socket state via `AssignKey`

## Quick Start

```bash
# Run the demo ice cream store app
cargo run -p mahalo-test-app

# Then visit http://127.0.0.1:4000
```

## Workspace Structure

```
crates/
  mahalo/             Umbrella crate — re-exports everything
  mahalo-core/        Conn, Plug, Pipeline, Controller, AssignKey
  mahalo-router/      Router with scopes, resources, named routes
  mahalo-endpoint/    Thread-per-core HTTP server (monoio), error handlers
  mahalo-pubsub/      Topic-based PubSub
  mahalo-channel/     Phoenix-compatible WebSocket channels
  mahalo-telemetry/   Telemetry events, handlers, spans
  mahalo-plug/        Built-in plugs (CSRF, Logger, Static, ETag, etc.)
  mahalo-test-app/    Ice cream store demo app
  cargo-mahalo/       CLI tool for project scaffolding
bench/                Cross-framework benchmark suite
```

## Example

```rust
use mahalo::*;
use http::StatusCode;

fn main() {
    let api_pipeline = Pipeline::new("api")
        .plug(plug_fn(|conn: Conn| async {
            conn.put_resp_header("content-type", "application/json")
        }));

    let addr = "127.0.0.1:4000".parse().unwrap();
    let endpoint = MahaloEndpoint::new(
        move || {
            MahaloRouter::new()
                .pipeline(api_pipeline.clone())
                .scope("/api", &["api"], |s| {
                    s.get("/hello", plug_fn(|conn: Conn| async {
                        conn.put_status(StatusCode::OK)
                            .put_resp_body(r#"{"message": "Aloha!"}"#)
                    }));
                })
        },
        addr,
    );

    endpoint.start().unwrap();
}
```

## Build & Test

```bash
cargo check --workspace
cargo test --workspace
```

## Dependencies

Built on [monoio](https://github.com/bytedance/monoio) (thread-per-core async runtime) and the [rebar](https://github.com/johnnyshields/rebar) OTP-style runtime. No feature flags required — edition 2024.

## License

MIT
