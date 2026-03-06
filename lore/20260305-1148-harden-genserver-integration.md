# Harden GenServer Integration: Make Mahalo Behave Like Phoenix

## Context

GenServer was recently added to rebar-core and partially integrated into mahalo's PubSub, Channel, and Endpoint. But the integration is shallow — the GenServer exists but doesn't actually participate in the message flow the way Phoenix Channel.Server does. Specifically:

- **PubSub broadcasts bypass the GenServer** — forwarder tasks send directly to WebSocket, never hitting `Channel::handle_info`
- **`child_entry` loses exit reasons** — always returns `ExitReason::Normal`, breaking supervisor restart logic
- **`terminate` is a no-op** — channels aren't cleaned up on disconnect
- **`handle_info` is stubbed** — PubSub messages are silently dropped
- **No tests** for the GenServer WebSocket path

This plan hardens the integration so message flow matches Phoenix: PubSub → process mailbox → `handle_info` → client.

---

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Fix `child_entry` exit reason (use oneshot instead of polling) | Quick | High | Auto-fix |
| 2 | Change `GenServer::terminate` to take owned state | Quick | Medium | Auto-fix |
| 3 | Add test for malformed envelope / ref_id=0 edge case | Quick | Medium | Auto-fix |
| 4 | Implement `ChannelConnectionServer::terminate` (cleanup channels) | Easy | High | Auto-fix |
| 5 | Implement `ChannelConnectionServer::handle_info` (PubSub dispatch) | Easy | High | Auto-fix |
| 6 | PubSub forwarder sends to GenServer mailbox (not directly to WS) | Moderate | High | Auto-fix |
| 7 | Add test for child_entry exit reason under supervisor | Easy | Medium | Auto-fix |
| 8 | Add integration test for GenServer WebSocket lifecycle | Moderate | High | Auto-fix |
| 9 | Add test for terminate called on disconnect | Easy | Medium | Auto-fix |
| 10 | Extract shared WS send-forwarder helper | Easy | Low | Ask first |

---

## Opportunity Details

### #1 Fix `child_entry` exit reason propagation [Quick/High]
- **What**: Replace 100ms polling loop with a `oneshot::channel<ExitReason>` that carries the exit reason from the spawned GenServer process back to the supervisor factory closure
- **Where**: `rebar/crates/rebar-core/src/gen_server.rs` lines 352-374
- **Why**: Currently always returns `ExitReason::Normal` regardless of actual exit — breaks Transient/Permanent restart logic. A Permanent GenServer that crashes won't be restarted because the supervisor thinks it exited normally.
- **Fix**: Create oneshot in factory, send ExitReason from inside spawned process, await receiver in factory

### #2 Change `GenServer::terminate` to take owned state [Quick/Medium]
- **What**: Change signature from `terminate(&self, reason: &str, state: &Self::State)` to `terminate(&self, reason: &str, state: Self::State)` — state is consumed on termination anyway
- **Where**: `rebar/crates/rebar-core/src/gen_server.rs` — trait def (line 112), gen_server_loop call sites (lines 184, 196, 205, 215, 222), all test GenServer impls
- **Why**: Enables mahalo's terminate to call `Channel::terminate(&mut socket)` on each joined channel. With `&Self::State` you can't get mutable access to channel sockets.

### #3 Test malformed envelope / ref_id=0 edge case [Quick/Medium]
- **What**: Add test that sends a call with missing `ref` field, verify it doesn't crash and the default ref_id=0 doesn't cause reply confusion
- **Where**: `rebar/crates/rebar-core/src/gen_server.rs` tests module
- **Why**: `ref_id` defaults to 0 on malformed calls (line 169). Two malformed calls could receive each other's replies.

### #4 Implement `ChannelConnectionServer::terminate` [Easy/High]
- **What**: Iterate over all `state.joined_channels` and call `channel.terminate("disconnect", &mut socket)` on each
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs` lines 402-405
- **Why**: Currently channels are never cleaned up on GenServer disconnect — `terminate` is a no-op. Phoenix always calls `Channel.terminate/2` on disconnect.
- **Depends on**: #2 (owned state in terminate)

### #5 Implement `ChannelConnectionServer::handle_info` [Easy/High]
- **What**: Deserialize incoming `rmpv::Value` as PubSub message (JSON-encoded `PubSubMessage`), look up channel+socket by topic, call `channel.handle_info(&msg, &mut socket)`
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs` lines 393-400
- **Why**: Currently stubbed — PubSub broadcasts that arrive via the GenServer mailbox are silently dropped. This is the core of the Phoenix pattern.
- **Depends on**: #6 (PubSub must actually send to the mailbox)

### #6 PubSub forwarder sends to GenServer mailbox [Moderate/High]
- **What**: In the GenServer join path, change the PubSub forwarder task (lines 162-176) to send messages to the GenServer's PID via `runtime.send()` instead of directly to the WebSocket. The GenServer's `handle_info` then dispatches to `Channel::handle_info`, which pushes to the client.
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs` — `handle_join` and `ChannelConnectionServer`
- **Why**: This is the entire point of the GenServer integration. Without it, PubSub bypasses the process model entirely.
- **Approach**: Store `self_pid` and `Arc<Runtime>` in `ChannelConnectionState` (set during init). When `handle_cast` processes a `phx_join`, spawn a forwarder task that calls `runtime.send(self_pid, serialized_pubsub_msg)` for each broadcast. The message arrives in `handle_info` (#5).

### #7 Test child_entry exit reason under supervisor [Easy/Medium]
- **What**: Create a GenServer that stops with `ExitReason::Abnormal`, wrap in `child_entry`, run under supervisor, verify supervisor sees the correct reason and restarts
- **Where**: `rebar/crates/rebar-core/src/gen_server.rs` tests module
- **Why**: Existing test only checks ChildEntry creation, not actual supervisor behavior

### #8 Integration test for GenServer WebSocket lifecycle [Moderate/High]
- **What**: Start Runtime + Endpoint with GenServer WS handler, connect WebSocket, join channel, send events, verify PubSub broadcasts reach `handle_info`, disconnect, verify `terminate` called
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs` tests module
- **Why**: Zero test coverage for the entire GenServer WebSocket path

### #9 Test terminate called on disconnect [Easy/Medium]
- **What**: Custom Channel with `Arc<AtomicBool>` that records terminate was called; connect, join, disconnect, assert flag
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs` tests module
- **Why**: Verifies #4 works correctly

### #10 Extract shared WS send-forwarder helper [Easy/Low]
- **What**: Extract the common pattern of spawning a task that reads from `UnboundedReceiver<WsMessage>` and writes to the WebSocket sink
- **Where**: `mahalo/crates/mahalo-channel/src/socket.rs`
- **Why**: Minor dedup between `handle_websocket` and `handle_websocket_with_runtime`
- **Trade-offs**: The two paths have genuinely different semantics; may not be worth coupling them

---

## Implementation Order

**Phase 1: Rebar fixes** (no mahalo dependency)
- #1 Fix child_entry exit reason
- #2 Change terminate to owned state
- #3 Test malformed envelope
- #7 Test child_entry under supervisor

**Phase 2: Mahalo Channel GenServer** (depends on Phase 1)
- #4 Implement terminate cleanup
- #5 Implement handle_info dispatch
- #6 PubSub forwarder → mailbox

**Phase 3: Tests** (depends on Phase 2)
- #8 GenServer WS lifecycle test
- #9 Terminate on disconnect test
- #10 Extract send-forwarder helper (ask first)

## Verification

```bash
cd /mnt/c/workspace/rebar && cargo test --workspace
cd /mnt/c/workspace/mahalo && cargo test --workspace
```

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
