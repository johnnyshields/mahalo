# WebSocket Implementation Hardening

Post-implementation review of the io_uring WebSocket support (Steps 1-6).

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Consolidate submit_read/submit_ws_read and submit_write/submit_ws_write into parameterized helpers | Quick | Medium | Auto-fix |
| 2 | `ChannelConnectionState` should be `pub(crate)` not `pub` (not exported in lib.rs anyway) | Quick | Low | Auto-fix |
| 3 | Add reserved opcode rejection (3-7, 11-15) in `ws_parse::try_parse_frame` | Quick | Medium | Auto-fix |
| 4 | Unknown WS opcodes in uring.rs silently ignored (line 741 `_ => {}`) — should protocol-error close | Quick | Medium | Auto-fix |
| 5 | Remove `ws_active_slots` entry in write-handler close path (line 845-851) to avoid iterating dead entries | Quick | Low | Auto-fix |
| 6 | Dead code: outbox flush close path (line 942-946) is unreachable — write handler always closes first | Quick | Low | Auto-fix |
| 7 | Pair `channel_router` + `pubsub` into `Option<WsConfig>` struct instead of two separate Options | Easy | Medium | Ask first |
| 8 | Add `Sec-WebSocket-Version: 13` validation and GET method check in `http_parse.rs` upgrade detection | Easy | Medium | Auto-fix |
| 9 | tcp_server.rs `handle_ws_upgraded`: no graceful GenServer shutdown (kills + aborts immediately) | Easy | Medium | Ask first |
| 10 | BufferPool `as_ptr()`/`total_len()` marked `#[allow(dead_code)]` — move to `#[cfg(test)]` | Quick | Low | Auto-fix |
| 11 | Test coverage: `ChannelConnectionServer` GenServer cast/info dispatch has zero tests | Moderate | High | Ask first |
| 12 | Test coverage: `ws_parse` missing reserved opcode and close-without-code edge case tests | Easy | Medium | Auto-fix |

## Opportunity Details

### 1. Consolidate submit_read/submit_ws_read, submit_write/submit_ws_write
- **What**: `submit_read` and `submit_ws_read` are identical except for the state constant. Same for write variants.
- **Where**: `uring.rs:241-322`
- **Why**: Removes 30 lines of duplication, single maintenance point.

### 2. ChannelConnectionState visibility
- **What**: Change `pub struct ChannelConnectionState` to `pub(crate) struct ChannelConnectionState`.
- **Where**: `socket.rs:427`
- **Why**: It's the GenServer `State` type — internal detail. Not exported in `lib.rs`. Leaking it as public creates accidental API surface.

### 3. Reserved opcode rejection in ws_parse
- **What**: Reject opcodes 3-7 and 11-15 with `WsError::ProtocolError` in `try_parse_frame`.
- **Where**: `ws_parse.rs:~61`
- **Why**: RFC 6455 §5.2 — reserved opcodes MUST cause failure. Currently silently accepted.

### 4. Unknown opcode handling in uring.rs
- **What**: The `_ => {}` arm at line 741 silently ignores unknown opcodes. Should set `ws.closing = true` and queue a 1002 close frame (same as the `Err` branch at line 747).
- **Where**: `uring.rs:741`
- **Why**: Complementary to #3. Even if the parser rejects reserved opcodes, defense in depth.

### 5. Clean up ws_active_slots on write-handler close
- **What**: When the write completion handler closes a WS connection (line 845-851), also remove the entry from `ws_active_slots`.
- **Where**: `uring.rs:845-851`
- **Why**: Without this, the outbox flush loop iterates over dead slots (harmlessly, since `ws` is None, but wasteful).

### 6. Remove dead outbox-flush close path
- **What**: Lines 942-946 (`else if ws.closing && ws.outbox.is_empty() && !ws.writing`) are unreachable — when a close frame is queued, it's always flushed via a write, and the write completion handler (line 845) always handles closing first.
- **Where**: `uring.rs:942-946` and `950-957`
- **Why**: Dead code removal. The `slots_to_close` vector and loop are also unnecessary.

### 7. Bundle channel_router + pubsub into WsConfig
- **What**: Replace `channel_router: Option<Arc<ChannelRouter>>` + `pubsub: Option<PubSub>` with `ws_config: Option<WsConfig>` where `WsConfig { channel_router: Arc<ChannelRouter>, pubsub: PubSub }`.
- **Where**: `endpoint.rs`, `worker.rs`, `uring.rs`, `tcp_server.rs`, `application.rs`
- **Why**: These two are always used together. A single Option enforces the invariant at the type level and eliminates repeated `(Some(_), Some(_))` destructuring.
- **Trade-offs**: Touches 5 files, moderate churn.

### 8. WebSocket handshake validation
- **What**: In `try_parse_request`, only set `ws_key` if method is GET and add `Sec-WebSocket-Version: 13` check.
- **Where**: `http_parse.rs:79-90`
- **Why**: RFC 6455 §4.2.1 requires GET method. Without version check, clients sending unsupported versions are silently accepted. Add test for POST-with-upgrade being rejected.

### 9. Graceful GenServer shutdown in tcp_server.rs
- **What**: Replace `runtime.kill(pid)` with `gen_server::stop(runtime, pid)` (or equivalent) and `send_task.abort()` with a channel-based shutdown signal.
- **Where**: `tcp_server.rs:319-321`
- **Why**: `kill` bypasses `stopping()` lifecycle hook. The GenServer may have cleanup (unsubscribe from PubSub, leave channels). Same concern exists in `uring.rs:849,946` but less fixable there due to sync context.
- **Trade-offs**: Need to verify rebar-core has a graceful stop API.

### 10. BufferPool dead_code to cfg(test)
- **What**: Move `as_ptr()` and `total_len()` behind `#[cfg(test)]`.
- **Where**: `uring.rs:211-219`
- **Why**: Only used in tests. Removes `#[allow(dead_code)]` lint suppression.

### 11. ChannelConnectionServer tests
- **What**: Add tests for the GenServer `handle_cast` (receiving JSON text, dispatching to channels) and `handle_info` (timer-triggered messages).
- **Where**: `socket.rs` test module
- **Why**: This is the core message dispatch path for both io_uring and tcp_server WebSocket connections. Zero test coverage currently.
- **Trade-offs**: Requires constructing a GenServer test harness with rebar runtime.

### 12. ws_parse edge case tests
- **What**: Add tests for: reserved opcode rejection (after #3), close frame with empty payload (no status code), close frame with 1-byte payload (invalid per RFC).
- **Where**: `ws_parse.rs` test module
- **Why**: Covers the fixes from #3 and validates RFC edge cases.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
