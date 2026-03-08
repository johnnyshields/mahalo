# TLS Architecture Decision: rebar-core + mahalo-endpoint Split

**Date:** 2026-03-08
**Branch:** tls-support
**Status:** Implemented, hardened

## Decision

TLS primitives live in rebar-core (behind a `tls` feature gate). The HTTP-level TLS integration lives in mahalo-endpoint.

## What rebar-core Provides

- **`TlsAcceptor`** (~20 lines): `Clone + Send + Sync` wrapper around `compio_tls::TlsAcceptor`. Constructor takes `Arc<rustls::ServerConfig>`, exposes `async fn accept(TcpStream) -> io::Result<TlsStream<TcpStream>>`.
- **`AsyncRead`/`AsyncWrite` impls on `TcpStream`**: Required for compio-tls handshake to work with rebar's stream type. These are compio-io trait impls — rebar owns `TcpStream`, so only rebar can provide them (Rust orphan rules).
- **Re-exports**: `compio_tls::TlsStream` and `rustls` crate, so downstream doesn't need direct deps for basic usage.
- **Feature gate**: `tls = ["dep:compio-tls", "dep:rustls"]`. Projects that don't need TLS don't pay the rustls compile cost. The `compio-io` dependency is always-on (lightweight trait crate, useful beyond TLS).

## What mahalo-endpoint Provides

- **`MahaloStream` enum**: `Plain(TcpStream)` | `Tls(UnsafeCell<TlsStream<TcpStream>>)`. Abstracts read/write dispatch. `UnsafeCell` is safe because RebarExecutor is single-threaded and cooperative — documented in SAFETY comments.
- **Handshake-in-spawned-task pattern**: TLS handshake runs inside the spawned connection task, not blocking the accept loop.
- **Lease path skip for TLS**: TLS decryption necessarily copies data, so the turbine zero-copy lease path is bypassed (`is_tls()` guard). Falls back to Vec path.
- **Error handling policy**: Handshake failures logged at `warn!` (security-relevant), connection closed, accept loop continues. No retry.
- **Builder API**: `MahaloEndpoint::new(...).tls(Arc<ServerConfig>)`.

## Why This Split

1. **Orphan rules**: mahalo-endpoint cannot impl `compio_io::AsyncRead` on `rebar_core::TcpStream` — both are foreign. rebar-core must own those impls.
2. **Reusability**: Any rebar-based project (not just mahalo) can use TLS without duplicating trait impls.
3. **Separation of concerns**: rebar-core = "TLS-capable streams" (runtime concern). mahalo-endpoint = "how to use TLS in an HTTP server" (application concern).
4. **Compile-time opt-in**: Feature gate keeps rustls out of the dependency tree for non-TLS users.

## Alternatives Considered

- **TLS entirely in mahalo**: Blocked by orphan rules. Would also force every rebar consumer to duplicate compio-io trait impls.
- **Separate `rebar-tls` crate**: Over-engineering for ~20 lines of wrapper + trait impls that are tightly coupled to `TcpStream`.
- **No wrapper, just trait impls**: Would work, but `TlsAcceptor` wrapper provides cleaner ergonomics and a natural place for future enhancements (e.g., ALPN configuration, client cert extraction).

## Key Files

- `rebar-core/src/tls.rs` — TlsAcceptor wrapper + re-exports (~234 lines incl. tests)
- `rebar-core/src/io.rs` — AsyncRead/AsyncWrite impls on TcpStream (~142 lines added)
- `mahalo-endpoint/src/server.rs` — MahaloStream enum + accept loop TLS integration
- `mahalo-endpoint/src/endpoint.rs` — `.tls()` builder method
- `mahalo-endpoint/src/worker.rs` — TlsAcceptor creation per worker thread

## Test Coverage

- **rebar-core**: 3 tests (handshake, echo, data integrity) using self-signed certs via rcgen
- **mahalo-endpoint**: 4 TLS tests (HTTPS request, SSE-over-TLS, handshake failure resilience) + existing plain TCP tests serve as regression
