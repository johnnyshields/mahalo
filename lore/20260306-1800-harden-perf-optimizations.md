# Hardening: Performance Optimizations (sync plug, io_uring, conn slim)

## Context

Four recent commits landed performance optimizations across the stack:
- Sync plug fast path (`call_sync` / `SyncPlugFn`) to avoid BoxFuture heap allocation
- io_uring multishot accept, registered buffers (ReadFixed), TCP_CORK
- Lazy query_params, `put_resp_body_static`, conn struct slimming
- Worker split: `spawn_uring_workers` (non-blocking) vs `start_uring_server` (blocking)

This review identifies code quality issues, missing tests, and duplication.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add unit tests for `SyncPlugFn` / `sync_plug_fn` | Quick | High | Auto-fix |
| 2 | Add unit test for `put_resp_body_static` | Quick | Medium | Auto-fix |
| 3 | Add unit tests for `encode_user_data` / `decode_user_data` round-trip | Quick | High | Auto-fix |
| 4 | Add unit test for `BufferPool::iovecs()` | Quick | Medium | Auto-fix |
| 5 | Deduplicate `spawn_uring_workers` / `start_uring_server` in worker.rs | Easy | Medium | Ask first |
| 6 | Add doc comment on `Result<Conn, Conn>` pattern for `call_sync` | Quick | Low | Auto-fix |

## Opportunity Details

### #1: Unit tests for `SyncPlugFn` / `sync_plug_fn`
- **What**: Add tests verifying `call_sync()` returns `Ok(conn)`, `call()` async path works, and `SyncPlugFn` is Send+Sync
- **Where**: `crates/mahalo-core/src/plug.rs` (test module, after line 98)
- **Why**: Core optimization feature has zero test coverage. The `call_sync` → `Ok(conn)` path and the fallback `call()` → `std::future::ready()` path are both untested.

### #2: Unit test for `put_resp_body_static`
- **What**: Add test that `put_resp_body_static(b"hello")` sets `resp_body` to expected bytes
- **Where**: `crates/mahalo-core/src/conn.rs` (test module)
- **Why**: New API with different semantics than `put_resp_body` (uses `Bytes::from_static` for zero-copy)

### #3: Unit tests for `encode_user_data` / `decode_user_data`
- **What**: Round-trip tests for the bit-packing used in io_uring CQE user_data. Test boundary values (max slot, max generation, all states).
- **Where**: `crates/mahalo-endpoint/src/uring.rs` (test module)
- **Why**: Bit manipulation is error-prone. These encode every CQE in the event loop — a bug here corrupts all I/O.

### #4: Unit test for `BufferPool::iovecs()`
- **What**: Test that `iovecs()` returns correct count and each iovec has correct `iov_len`
- **Where**: `crates/mahalo-endpoint/src/uring.rs` (test module)
- **Why**: Used for `register_buffers()` — incorrect iovecs could cause kernel panics or silent data corruption

### #5: Deduplicate `spawn_uring_workers` / `start_uring_server`
- **What**: Extract shared worker-spawning loop into a helper that returns `Vec<JoinHandle<()>>`. `spawn_uring_workers` drops the handles, `start_uring_server` joins them.
- **Where**: `crates/mahalo-endpoint/src/worker.rs` (lines 18-94)
- **Why**: 30+ lines of identical code (Arc cloning, thread building, spawning). Single maintenance point.
- **Trade-offs**: Minor — adds one function but removes substantial duplication

### #6: Doc comment on `Result<Conn, Conn>` pattern
- **What**: Add a brief explanation on the `call_sync` trait method about why `Result<Conn, Conn>` is used (ownership transfer pattern, not success/failure)
- **Where**: `crates/mahalo-core/src/plug.rs` (lines 11-17)
- **Why**: `Err(conn)` doesn't mean "error" — it means "I can't handle this synchronously, here's the conn back". Developers will be confused without documentation.

## Items NOT pursued
- **`submit_read` vs `submit_ws_read` duplication**: Intentional thin wrappers over `submit_read_impl`, serving semantic clarity. Keep as-is.
- **`SyncPlugFn` vs `PlugFn` duplication**: Different trait impls for different function signatures. Keep as-is.
- **`set_tcp_cork` / `set_tcp_nodelay` tests**: Best-effort socket options with no return value. Testing would require platform-specific socket introspection. Skip.
- **`bufs_registered: bool` threading**: Clean design, single computation point. No improvement needed.

## Resolution

All 6 items implemented in commit on `feat-perf-sync-plug-uring-conn-slim`:

| # | Status | Notes |
|---|--------|-------|
| 1 | Done | 4 tests: `sync_plug_fn_call_sync_returns_ok`, `sync_plug_fn_call_async_fallback`, `async_plug_fn_call_sync_returns_err`, `sync_plug_fn_is_send_sync` |
| 2 | Done | `put_resp_body_static_zero_copy` test |
| 3 | Already existed | 4 round-trip tests were already present in uring.rs |
| 4 | Done | `buffer_pool_iovecs_count_and_layout` test |
| 5 | Done | Extracted `spawn_workers() -> Vec<JoinHandle>`, both functions now delegate to it |
| 6 | Done | Expanded doc comment on `call_sync` explaining `Ok`/`Err` semantics |

All 327 workspace tests pass.
