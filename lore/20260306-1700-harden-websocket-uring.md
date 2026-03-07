# Harden: WebSocket io_uring Integration

**Date:** 2026-03-06
**Scope:** Recent commit `1c02701` — "Fix WebSocket support in io_uring server"

## Context

Commit `1c02701` fixed two issues in the io_uring WebSocket path:
1. WebSocket upgrade detection was skipped when recycled Conns were available in the pool
2. GenServer replies weren't ready before the outbound drain phase

The fixes work but introduced two heuristics that need hardening:
- A substring scan (`buf.windows(9)`) to detect WebSocket upgrades
- A `yield_now` loop (4 iterations) to let GenServers process messages

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add `ws_key` to `ParsedIntoResult`, eliminate `maybe_ws` heuristic | Easy | High | Ask first |
| 2 | Replace yield_now hack with proper synchronization | Moderate | High | Ask first |
| 3 | Add unit tests for WS upgrade path selection in uring.rs | Easy | Medium | Ask first |
| 4 | Add integration test for WS frame round-trip | Moderate | Medium | Ask first |

## Opportunity Details

### #1 — Add `ws_key` to `ParsedIntoResult`, eliminate `maybe_ws` heuristic

- **What**: Add `ws_key: Option<String>` field to `ParsedIntoResult` and detect WebSocket upgrade in `try_parse_into_conn` (same logic as `try_parse_request`). Remove the `maybe_ws` heuristic in `uring.rs` — instead, always try the recycled path and check `ws_key` on the result.
- **Where**: `crates/mahalo-endpoint/src/http_parse.rs` (lines 238-272), `crates/mahalo-endpoint/src/uring.rs` (lines 564-569)
- **Why**: The current `buf.windows(9).any(|w| w.eq_ignore_ascii_case(b"websocket"))` can false-positive on POST bodies containing "websocket". The proper fix is to detect ws_key at parse time in both paths, eliminating the heuristic entirely.
- **Trade-offs**: Slightly more work in the recycle path for WS requests, but eliminates a code smell and makes behavior deterministic.

### #2 — Replace yield_now hack with proper synchronization

- **What**: Replace the 4x `tokio::task::yield_now()` loop with a proper mechanism. After dispatching casts to GenServers, use `tokio::sync::Notify` (or a counter + condvar) so the GenServer signals when it has queued replies. The drain phase then waits on this signal (with a timeout).
- **Where**: `crates/mahalo-endpoint/src/uring.rs` (lines 1024-1034), `crates/mahalo-channel/src/socket.rs` (ChannelConnectionServer)
- **Why**: The 4-yield count is arbitrary and undocumented. If the GenServer processing chain takes more yields (e.g., nested spawns, PubSub subscribe), replies could be missed until the next event loop iteration. A proper signal eliminates timing dependence.
- **Trade-offs**: Adds coupling between the event loop and GenServer signaling. Need to ensure the signal doesn't block the event loop indefinitely (use timeout).

### #3 — Add unit tests for WS upgrade path selection in uring.rs

- **What**: Add tests in `uring.rs` that verify WebSocket upgrade detection works correctly when recycled Conns are available. Test cases: (a) GET with WS headers → takes WS path, (b) POST with "websocket" in body → takes HTTP path, (c) regular GET → takes recycled path.
- **Where**: `crates/mahalo-endpoint/src/uring.rs` (tests module) and `crates/mahalo-endpoint/src/http_parse.rs` (tests)
- **Why**: The WS upgrade path in the io_uring event loop has zero test coverage. These tests validate the fix from #1.
- **Trade-offs**: None.

### #4 — Add integration test for WS frame round-trip

- **What**: Add a test that connects via WebSocket, sends a phx_join message, and verifies the reply comes back. Uses the tcp_server path (not io_uring) since io_uring requires a full kernel event loop.
- **Where**: `crates/mahalo-endpoint/src/tcp_server.rs` or `crates/mahalo-endpoint/src/endpoint.rs` (tests module)
- **Why**: No end-to-end test exists for WebSocket message flow through the channel system. This would catch regressions in the upgrade → join → reply pipeline.
- **Trade-offs**: Integration tests are slower; need to bind a port and do async I/O.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
