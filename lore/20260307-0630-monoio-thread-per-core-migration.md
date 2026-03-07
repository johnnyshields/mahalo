# Monoio Thread-Per-Core Migration

**Date**: 2026-03-07
**Branch**: `worktree-refactor-monoio` (mahalo), `rebar-v4` (rebar)
**Scope**: Full runtime migration — rebar-core + all mahalo crates
**Stats**: 39 files changed, +1,394 / -4,292 lines (net -2,898). 297 mahalo tests, 285 rebar tests.

## Overview

Replaced the raw io_uring + tokio `block_on` bridge architecture with monoio, a native io_uring async runtime with cooperative scheduling and `!Send` tasks. This eliminates sequential request stalling, removes all atomic reference counting from the hot path, and unifies the Linux/non-Linux code paths into a single `FusionDriver` implementation.

## The Problem

The original endpoint stalled the io_uring event loop during handler execution via `tokio_rt.block_on()`. While a handler ran, the ring was blind to completions. Requests in the same submission batch executed sequentially. All shared state required `Arc` wrapping and `Send + Sync` bounds solely to satisfy a tokio runtime that ran everything on one thread anyway. A separate 453-line `tcp_server.rs` provided a completely different code path for non-Linux platforms.

## Key Architectural Decisions

### 1. Factory Pattern for MahaloEndpoint

**Decision**: `MahaloEndpoint::new()` takes factory closures (`impl Fn() -> MahaloRouter + Send`) instead of owned values.

**Rationale**: Factories are `Send` (they cross thread boundaries during worker spawn), but the values they produce are `!Send` (thread-local, `Rc`-based). Each worker thread calls factories once at startup, then owns its state exclusively. No sharing, no contention.

### 2. PubSub Stays Cross-Thread (crossbeam + std::thread)

**Decision**: PubSub server runs on a dedicated `std::thread` with blocking `crossbeam_channel::recv()`, not on monoio.

**Rationale**: PubSub is the intentionally cross-thread system — its handle must be `Clone + Send + Sync`. If both server and caller ran on the same monoio thread, the blocking crossbeam recv for subscribe replies would deadlock. A dedicated thread avoids this and is simpler than async polling.

### 3. Telemetry Becomes Synchronous

**Decision**: `execute()` and `attach()` are now sync functions. Handlers are `Rc<dyn Fn(&TelemetryEvent)>` called inline.

**Rationale**: Handlers were already synchronous closures. The broadcast channel and RwLock existed only because tokio demanded Send. With single-threaded execution, `Rc<RefCell<Vec<...>>>` and direct iteration is simpler and faster.

### 4. FusionDriver for Cross-Platform

**Decision**: Single code path using `monoio::RuntimeBuilder::<FusionDriver>::new()` instead of `#[cfg(target_os = "linux")]` branching.

**Rationale**: FusionDriver uses io_uring on Linux and epoll/kqueue elsewhere. One server implementation, one test matrix. Deleted 1,790 lines of platform-specific code.

### 5. Rebar Cross-Thread via eventfd + crossbeam

**Decision**: `ThreadBridgeRouter` checks `pid.thread_id()` — local delivery is direct, cross-thread uses crossbeam channel + eventfd wake.

**Rationale**: Processes on different threads can't share Rc-based state. The bridge uses lock-free crossbeam channels for the data path and eventfd (Linux) / self-pipe (elsewhere) to wake the target thread's monoio runtime without polling.

## Files Created

| File | Purpose |
|------|---------|
| `crates/mahalo-endpoint/src/server.rs` | Monoio accept loop + connection handler with ownership-based I/O |
| `crates/rebar-core/src/multi.rs` | Multi-thread bootstrap (`start_threaded`, `ThreadedHandle`) |

## Files Deleted

| File | Lines | Reason |
|------|-------|--------|
| `crates/mahalo-endpoint/src/uring.rs` | 1,337 | Replaced by monoio's native io_uring runtime |
| `crates/mahalo-endpoint/src/tcp_server.rs` | 453 | Unified into server.rs via FusionDriver |

## Major Files Modified

| File | Changes |
|------|---------|
| `crates/mahalo-core/src/plug.rs` | `BoxFuture` loses `+Send`, `Plug` trait drops `Send+Sync` |
| `crates/mahalo-core/src/conn.rs` | `Arc<Runtime>` -> `Rc<Runtime>`, assigns drop `Send+Sync` |
| `crates/mahalo-endpoint/src/endpoint.rs` | Factory pattern, `ErrorHandler` is `Rc`-based |
| `crates/mahalo-endpoint/src/worker.rs` | Monoio runtime per thread, CPU affinity, SO_REUSEPORT |
| `crates/mahalo-channel/src/socket.rs` | RPITIT GenServer, local_sync channels, Rc throughout |
| `crates/mahalo-pubsub/src/pubsub.rs` | crossbeam channels, std::thread server, sync subscribe |
| `crates/mahalo-telemetry/src/telemetry.rs` | Sync handlers, Rc<RefCell>, no broadcast channel |
| `crates/rebar-core/src/runtime.rs` | Waker-based cancellation, shutdown coordination |
| `crates/rebar-core/src/bridge.rs` | ThreadBridgeRouter with eventfd + crossbeam |
| `crates/rebar-core/src/process/table.rs` | Global named registry (Arc<RwLock<HashMap>>) |

## Dependencies Changed

### Added
- `monoio = { version = "0.2", features = ["sync", "utils", "macros"] }` — async runtime
- `local-sync = "0.1"` — !Send channel primitives
- `crossbeam-channel = "0.5"` — cross-thread lock-free channels

### Removed from workspace
- `tokio` (individual crates add directly where still needed)
- `async-trait` (replaced by RPITIT)
- `io-uring` (monoio provides this)
- `axum` (was only used for WS types)
- `tokio-tungstenite` / `tungstenite` (custom WS framer used instead)

### Rebar dependency
- `rebar-core` now points to `rebar-v4` branch (monoio thread-per-core)

## Security Considerations

- No new attack surface — monoio handles the same TCP/HTTP parsing as before
- `http_parse.rs` (zero-alloc parser) and `ws_parse.rs` (frame parser) are unchanged pure logic
- Static file serving (`StaticFiles`) switched from `tokio::fs` to `std::fs` — sync I/O is acceptable for local file reads and avoids async file descriptor leaks
- Path traversal protection unchanged

## Testing Approach

- All `#[tokio::test]` converted to `#[monoio::test(enable_timer = true)]` across library crates
- `tokio::time::sleep/spawn` -> `monoio::time::sleep` / `monoio::spawn`
- PubSub tests use plain `#[test]` (subscribe is now sync, crossbeam recv is blocking)
- Cluster tests use `tokio::task::spawn_blocking` for crossbeam recv to avoid blocking the tokio runtime (cluster still uses tokio internally)
- 297 tests pass across all mahalo crates, 285 across all rebar crates

## Architecture Diagram

```
MahaloEndpoint (factory closures, Send+Sync)
  |
  +-- worker::spawn_workers()
       |
       +-- N OS threads (one per CPU core)
            |
            +-- monoio::RuntimeBuilder<FusionDriver>
            |     (io_uring on Linux, epoll elsewhere)
            |
            +-- Rc<Runtime> (rebar, thread-local, !Send)
            |
            +-- SO_REUSEPORT TcpListener
            |
            +-- Thread-local state (no Arc, no atomics):
            |     Router, ErrorHandler(Rc), AfterPlugs, WsConfig
            |
            +-- server::run_accept_loop()
                  |
                  +-- monoio::spawn per connection
                       |
                       +-- handle_connection (ownership-based I/O)
                            |
                            +-- read(buf).await  -- cooperative, io_uring backed
                            +-- http_parse::parse_request()
                            +-- handler::execute_request()  -- Plug pipeline
                            +-- write(resp).await
                            +-- keep-alive loop
```

## Future Enhancement Ideas

- **Integration tests**: Re-add HTTP round-trip and WebSocket join tests using monoio test runtime + reqwest (bundles its own tokio)
- **Benchmark validation**: Run TechEmpower scenarios to validate performance vs old io_uring implementation
- **Cluster migration**: Migrate `mahalo-cluster` from tokio to monoio (currently uses tokio::spawn for topology polling)
- **io_uring registered buffers**: monoio supports registered buffer pools — could reduce syscall overhead further
- **Zero-copy responses**: monoio's ownership-based write could enable splice/sendfile for static files
- **Multishot accept**: monoio may support multishot accept in future versions for even lower accept latency
