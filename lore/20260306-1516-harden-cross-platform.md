# Harden: Cross-Platform Support Changes

## Summary

Hardened the cross-platform support added to mahalo-endpoint. All 5 opportunities implemented, all tests passing (46 endpoint tests, up from 33).

## Changes Made

### #1 — Extracted shared `bind_socket` helper
- Moved `bind_socket(addr, reuse_port)` from `tcp_server.rs` into `handler.rs` as `pub(crate)`
- `worker.rs` now calls `crate::handler::bind_socket(addr, true)` instead of inline socket creation
- `tcp_server.rs` uses `crate::handler::bind_socket` via import
- Single place to change socket options for both backends

### #2 — Reused response buffer in `tcp_server.rs`
- `handle_connection` now uses `serialize_response_into(&conn, keep_alive, &mut resp_buf)` with a buffer allocated once per connection
- Eliminates per-response allocation for keep-alive connections

### #3 — Test coverage for `tcp_server.rs`
- Made `tcp_server` module always compiled (removed `#[cfg(not(target_os = "linux"))]` gate from `lib.rs`) so tests run on Linux too
- Added 6 tests: basic request, keep-alive pipelining, 404 routing, 400 invalid request, 413 body too large, connection close

### #4 — Test coverage for `handler.rs`
- Added 7 tests: route match, default 404, custom error handler 404, after-plug execution, halt stops after-plugs, bind_socket ipv4, bind_socket with reuse_port

### #5 — Updated CLAUDE.md
- Workspace Structure: `mahalo-endpoint` description updated from "Axum bridge" to "Platform-adaptive HTTP server" with sub-module listing
- Key Abstractions: `MahaloEndpoint` description reflects io_uring/tokio TCP duality, mentions `handler.rs`
