# Harden: io_uring Migration Review

**Date**: 2026-03-06
**Scope**: All changes from commits `485a1ca..e44455b` (6 commits, ~1600 LOC added)
**Context**: Replaced Axum HTTP server with raw io_uring event loop in mahalo-endpoint. Added matchit-based router compilation. Optimized for throughput (now beats Actix on JSON, parity on plaintext).

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Remove dead `Conn::from_parts` method | Quick | Low | Auto-fix |
| 2 | Remove unused `axum` dep from mahalo-test-app/Cargo.toml | Quick | Low | Auto-fix |
| 3 | Remove suppressed `_channel_router` / `_pubsub_ref` lines in test-app main.rs | Quick | Low | Auto-fix |
| 4 | Update CLAUDE.md: replace Axum references with io_uring | Quick | Medium | Auto-fix |
| 5 | Extract duplicate `libc::write(503) + libc::close` in uring.rs accept path | Easy | Medium | Auto-fix |
| 6 | Add tests for `colon_to_brace` edge cases (empty, trailing slash, consecutive params) | Easy | Medium | Auto-fix |
| 7 | Add tests for `normalize_prefix` and `build_matchit_path` | Easy | Medium | Auto-fix |
| 8 | Add test for `serialize_response_into` buffer reuse behavior | Easy | Medium | Auto-fix |

## Opportunity Details

### #1 — Remove dead `Conn::from_parts`
- **What**: Delete `Conn::from_parts()` method (conn.rs lines 59-75)
- **Where**: `crates/mahalo-core/src/conn.rs`
- **Why**: Added during migration but never used. HTTP parsing uses `Conn::new()` + field assignment instead.

### #2 — Remove unused `axum` dep from test-app
- **What**: Delete `axum = { workspace = true }` from mahalo-test-app/Cargo.toml line 19
- **Where**: `crates/mahalo-test-app/Cargo.toml`
- **Why**: Test app doesn't import axum directly. The dependency was needed when endpoint bridged through axum, but no longer.

### #3 — Remove suppressed channel_router/pubsub lines
- **What**: Remove `let _channel_router = channel_router;` and `let _pubsub_ref = pubsub.clone();` (lines 1182-1183)
- **Where**: `crates/mahalo-test-app/src/main.rs`
- **Why**: These suppress warnings for values that should be passed to endpoint but can't since WS support was deferred. Should either be fully removed (along with the channel_router/pubsub setup above), or left as-is with a TODO comment. Since WS is deferred, a TODO comment is appropriate.

### #4 — Update CLAUDE.md Axum references
- **What**: Update 3 lines in CLAUDE.md that still reference Axum:
  - Line 14: `mahalo-endpoint/  # Axum bridge...` → `# io_uring HTTP server...`
  - Line 28: `Bridges MahaloRouter to Axum` → `Bridges MahaloRouter to io_uring HTTP server`
  - Line 57: `axum 0.8: HTTP server` → `io-uring 0.7: Linux io_uring HTTP server`
- **Where**: `CLAUDE.md`
- **Why**: Misleads developers and Claude about the actual architecture

### #5 — Extract duplicate 503-and-close in accept path
- **What**: Extract the repeated `libc::write(503) + libc::close(fd) + submit_accept + continue` pattern (uring.rs lines 387-396 and 404-415) into a helper function like `reject_and_close(fd, listen_fd, ring)`
- **Where**: `crates/mahalo-endpoint/src/uring.rs`
- **Why**: Two identical unsafe blocks doing the same thing — DRY and reduces surface area for bugs
- **Trade-offs**: Minor — the helper needs to handle the `conn_pool.free(slot_idx)` call that only appears in the second path, so the caller still does that before calling the helper

### #6 — Add `colon_to_brace` edge case tests
- **What**: Add tests for: empty string, trailing slash, consecutive params (`/:a/:b`), path with no params
- **Where**: `crates/mahalo-router/src/router.rs` tests module
- **Why**: Current tests only cover 3 basic cases. Edge cases like trailing slashes could cause matchit registration issues.

### #7 — Add `normalize_prefix` and `build_matchit_path` tests
- **What**: Add dedicated tests for both helper functions covering: trailing slash stripping, leading slash addition, empty path with prefix, prefix+path concatenation
- **Where**: `crates/mahalo-router/src/router.rs` tests module
- **Why**: These functions are critical to correct route compilation but have zero direct test coverage

### #8 — Add `serialize_response_into` buffer reuse test
- **What**: Test that calling `serialize_response_into` twice on the same Vec reuses capacity (no reallocation for same-size responses)
- **Where**: `crates/mahalo-endpoint/src/http_parse.rs` tests module
- **Why**: Buffer reuse is the key optimization; should be explicitly tested to prevent regressions

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
