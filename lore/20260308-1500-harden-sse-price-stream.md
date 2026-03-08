# Harden SSE Price Stream Implementation

**Date:** 2026-03-08
**Scope:** `mahalo-test-app` — SSE price stream endpoint hardening

## Context

The SSE price stream feature (commit 59f6f22) replaced WebSocket-based price updates with Server-Sent Events at a 3s interval. This hardening pass addresses missing error handling, dead code, code duplication, and test coverage.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add `Duration` import to clean up verbose FQN paths | Quick | Low | Auto-fix |
| 2 | Add module docstring mention of SSE | Quick | Low | Auto-fix |
| 3 | Add warning log when PubSub subscribe fails in SSE endpoint | Quick | Medium | Auto-fix |
| 4 | Add debug log when SSE send fails (client disconnect) | Quick | Low | Auto-fix |
| 5 | Extract price update payload helper to eliminate duplication | Easy | Medium | Ask first |
| 6 | Remove `pubsub` field from StoreChannel (dead code path) | Easy | Medium | Ask first |
| 7 | Add unit test for price payload builder | Moderate | Medium | Ask first |

## Opportunity Details

### #1 — Import `Duration`
- **What**: Add `use std::time::Duration;` and replace 3 verbose `std::time::Duration::from_*` calls
- **Where**: `main.rs` imports, lines 1075, 1334, 1352
- **Why**: Consistency with Rust style conventions; reduces line length

### #2 — Update module docstring
- **What**: Add `//! - Server-Sent Events (SSE) for price streaming` to the module doc header
- **Where**: `main.rs` lines 1-11
- **Why**: The docstring lists all demonstrated features but doesn't mention SSE

### #3 — Warn on subscribe failure
- **What**: Add else branch with `tracing::warn!` in the SSE endpoint when `pubsub.subscribe` returns None
- **Where**: `main.rs` SSE endpoint, the `if let Some(rx)` block
- **Why**: Matches canonical pattern in `mahalo-channel/src/socket.rs:309-312`

### #4 — Log SSE send failure
- **What**: Add `tracing::debug!("SSE price client disconnected")` before break on send error
- **Where**: `main.rs` SSE endpoint
- **Why**: Observability for connection lifecycle

### #5 — Extract price update payload helper
- **What**: Create `fn price_update_payload(id, name, price_cents) -> Value` used by both StoreChannel and background thread
- **Where**: `main.rs` near `format_price()`
- **Why**: Same JSON built in 2+ places; could diverge

### #6 — Remove `pubsub` from StoreChannel
- **What**: Remove dead PubSub broadcast from StoreChannel's update_price handler — no JS client joins `store:lobby` anymore
- **Where**: `main.rs` StoreChannel struct, handle_in, instantiation
- **Why**: Dead code since SSE replaced WebSocket for prices

### #7 — Unit test for price payload builder
- **What**: Test the extracted helper to lock down the JSON contract between server and clients
- **Where**: `main.rs` tests section

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test -p mahalo-test-app`
