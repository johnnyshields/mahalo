# Harden: Phase 3 Turbine Zero-Copy I/O

## Context

Phase 3 added turbine zero-copy leased reads to the HTTP hot path, `Arc<Value>` to PubSub, and buffer pool accessors to rebar. This hardening pass addresses code quality issues, missing safety docs, redundant APIs, avoidable allocations, and missing test coverage introduced by those changes.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add SAFETY comments to LeaseIoBuf unsafe blocks | Quick | Medium | Auto-fix |
| 2 | Remove redundant `ProcessContext::with_buffer_pool` | Quick | Medium | Auto-fix |
| 3 | Extract `DEFAULT_BUF_SIZE` constant in server.rs | Quick | Low | Auto-fix |
| 4 | Fix double-copy on pipelining leftover in lease path | Easy | Medium | Auto-fix |
| 5 | Accept `Arc<Value>` in `PubSub::broadcast` to avoid deep-clone in bridge | Easy | High | Ask first |
| 6 | Fix `write_all` per-iteration `.to_vec()` allocation (rebar) | Easy | High | Ask first |
| 7 | Add integration test for lease read path in server.rs | Moderate | High | Ask first |
| 8 | Update CLAUDE.md with turbine/zero-copy conventions | Easy | Medium | Ask first |

## Opportunity Details

### 1. Add SAFETY comments to LeaseIoBuf unsafe blocks
- **What**: Add `// SAFETY:` comments to `as_uninit()` and `set_len()` impls
- **Where**: `rebar/crates/rebar-core/src/io.rs` lines 63-79
- **Why**: Unsafe code without documented invariants is a maintenance hazard

### 2. Remove redundant `ProcessContext::with_buffer_pool`
- **What**: Delete `ProcessContext::with_buffer_pool()` — takes `&self` but doesn't use any ProcessContext state, just delegates to global `executor::with_buffer_pool`. No call sites exist.
- **Where**: `rebar/crates/rebar-core/src/runtime.rs` lines 101-112

### 3. Extract `DEFAULT_BUF_SIZE` constant in server.rs
- **What**: Replace magic number `8192` (4 occurrences) with `const DEFAULT_BUF_SIZE: usize = 8192`
- **Where**: `mahalo-endpoint/src/server.rs` lines 102, 140, 148, 165

### 4. Fix double-copy on pipelining leftover in lease path
- **What**: Currently leftover bytes are copied twice: `to_vec()` then `copy_from_slice` into a fresh zeroed Vec. Instead, use the `to_vec()` result directly and grow capacity.
- **Where**: `mahalo-endpoint/src/server.rs` lines 122-143

### 5. Accept `Arc<Value>` in PubSub to avoid deep-clone in bridge
- **What**: Add `broadcast_arc()` that takes `Arc<Value>` directly. Bridge currently does `(*msg.payload).clone()` — full JSON deep-clone defeating the Arc purpose.
- **Where**: `mahalo-pubsub/src/pubsub.rs`, `mahalo-pubsub/src/bridge.rs`
- **Why**: The bridge calls `self.local.broadcast(topic, &msg.event, (*msg.payload).clone())` — this deep-clones the entire JSON tree, which is exactly what Arc was supposed to prevent.

### 6. Fix `write_all` per-iteration `.to_vec()` allocation (rebar)
- **What**: `write_all` calls `buf[written..].to_vec()` every loop iteration. Pre-existing issue but on the hot path we're optimizing.
- **Where**: `rebar/crates/rebar-core/src/io.rs` lines 277-296
- **Why**: Under partial writes (common under load), allocates O(n) copies.

### 7. Add integration test for lease read path in server.rs
- **What**: Test with `pool_config: Some(PoolConfig::default())`, start accept loop, send HTTP request via raw TcpStream, verify response.
- **Where**: `mahalo-endpoint/src/server.rs` (new test module)
- **Why**: The entire hybrid read path has zero test coverage. Existing endpoint tests use default config (no pool).

### 8. Update CLAUDE.md with turbine/zero-copy conventions
- **What**: Document turbine buffer pool lifecycle, `with_buffer_pool` accessor, `LeaseIoBuf`, hybrid read strategy, `Arc<Value>` in PubSubMessage.
- **Where**: `CLAUDE.md`

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
