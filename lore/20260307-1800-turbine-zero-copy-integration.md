# Turbine Zero-Copy Integration: From Monoio to Custom Executor with Epoch-Arena I/O

**Date**: 2026-03-07
**Branches**: `rebar-v5` (rebar), `feat-add-turbine-support` (mahalo), `master` (turbine)
**Scope**: Custom cooperative executor, zero-copy HTTP reads, PubSub Arc optimization
**Stats**: 281 mahalo tests, ~170 rebar tests passing. Zero-copy read path live.

## Overview

Replaced monoio with a custom cooperative executor (`RebarExecutor`) built on `compio-driver` + `turbine-core`. The executor owns the event loop, enabling deterministic epoch-based buffer rotation. Wired turbine's epoch-arena buffers into mahalo's HTTP read hot path, eliminating per-request `Vec<u8>` allocation. Added `Arc<Value>` to PubSub for O(1) broadcast fan-out.

## The Problem

The monoio migration (rebar-v4, mahalo `worktree-refactor-monoio`) solved the tokio bridge stalling problem and removed `Send+Sync` bounds. But the I/O path still allocated a `Vec<u8>` per connection read. On every HTTP request:

1. `vec![0u8; 8192]` — allocator call, possible `mmap` on cold path
2. Kernel copies data into the Vec via io_uring CQE
3. HTTP parser reads from the Vec
4. Vec dropped after parsing — deallocator call

The allocation/deallocation cycle is where p99 latency jitter lives. The average cost of `malloc` is fine — the occasional 10us spike when the allocator hits a slow path, does a `mmap`, or contends on a lock is not.

Turbine (`/mnt/c/workspace/turbine/`) already existed as an epoch-based buffer pool for io_uring — 38 tests, ~19ns constant-time lease, mmap'd arenas with bump allocation. But it was a pure buffer management library with no integration into the I/O path.

The original plan proposed a `turbine-executor` crate as an intermediate layer between turbine-core and rebar. This was rejected during design review — rebar already *is* a runtime with its own scheduler (`multi.rs`), process table, and thread bridge. Adding a second executor underneath it would create two layers of task queues and two notions of "spawn." The executor was folded directly into rebar-core instead.

## Why compio-driver (Not Monoio, Not Raw io-uring)

Monoio couples its executor to the driver — you can't control when epochs rotate relative to task polling. The whole point of integrating turbine is deterministic rotation timing: epoch boundaries must happen at a known point in the event loop, after task polling and before the next I/O batch.

compio's disaggregated design provides the driver (io_uring/IOCP/kqueue submission/completion) without the executor. We build our own event loop around it.

Cross-platform benefit: io_uring on Linux, IOCP on Windows, kqueue on macOS. Develop on laptops, deploy to Linux. On non-Linux, epoch rotation still works — just backed by a polling driver instead of io_uring. Buffers still benefit from arena allocation even without kernel registration.

## Key Architectural Decisions

### 1. Executor in Rebar, Not Turbine

**Decision**: Build `RebarExecutor` in `rebar-core/src/executor.rs` instead of a separate `turbine-executor` crate.

**Rationale**: The `drain_hook` pattern from the original plan (a callback so the executor could drain rebar's ThreadBridge) was a code smell. It existed solely because the executor didn't know about rebar internals. If the executor *is* rebar, the bridge drain is just a step in the event loop, not a callback. Also avoids two layers of task scheduling and two notions of `spawn()`.

### 2. Callback-Based Buffer Pool Access (`with_buffer_pool`)

**Decision**: Expose the pool via `with_buffer_pool<R>(f: impl FnOnce(&IouringBufferPool) -> R) -> Option<R>` instead of returning `&'static IouringBufferPool`.

**Rationale**: The pool lives inside `RebarExecutor` which lives on the stack of `block_on()`. Returning `&'static` lies about the lifetime — if someone stashes that reference in an `Rc<RefCell>` outliving `block_on()`, it's UB. The callback pattern borrows the pool for a bounded scope. Safe, no lifetime extension.

### 3. Hybrid Lease/Vec Read Path

**Decision**: Try leased buffer first, fall back to `Vec<u8>` if pool unavailable or arena exhausted. Lazy allocation of fallback buffer.

**Rationale**: The fast path (single-read complete request) is zero-alloc: lease -> read -> parse -> execute -> drop lease. The uncommon paths (partial request, pipelining leftover, no pool configured) fall back to Vec without penalty. The fallback `accum` Vec is allocated lazily — only when actually needed, not eagerly per connection.

### 4. Arc<Value> for PubSub (Not SendableBuffer)

**Decision**: Wrap `PubSubMessage.payload` in `Arc<serde_json::Value>` instead of the full `SendableBuffer` path.

**Rationale**: `Value::clone()` walks the entire JSON tree recursively — O(n) per subscriber. `Arc::clone()` is one atomic increment. For typical Phoenix-style channel broadcasts (small JSON events), Arc is the 80/20 solution. The full SendableBuffer path (serialize once into arena, fan out arena references) would matter for multi-megabyte binary blobs at high frequency — not the common case. Can upgrade later if profiling shows serialization as a bottleneck.

### 5. Deferred: io_uring Fixed-Buffer Registration

**Decision**: Skip for this phase. compio-driver doesn't expose the underlying io_uring ring, so `pool.register(&ring)` can't be called without patching compio or bypassing it.

**Rationale**: The leased-buffer reads already eliminate per-read allocation. Fixed-buffer registration shaves ~200-500ns per I/O op by avoiding kernel page-table walks — real but incremental. Not worth a compio fork before benchmarking the non-registered path.

### 6. Deferred: Adaptive Epoch Tuning

**Decision**: Use fixed epoch interval. Don't add utilization monitoring yet.

**Rationale**: No benchmark data exists. You don't know what the right fixed interval is, let alone whether adaptive tuning matters. Optimize after measuring.

## Event Loop Design

Each tick of the `RebarExecutor`:

```
1. Poll ready tasks (cooperative — yield on .await)
2. Submit pending I/O ops to compio-driver Proactor
3. Drain completions from Proactor, wake waiting tasks
4. Check epoch timer:
   - If elapsed: rotate() arena, try_collect() old epochs
   - drain_returns() for cross-thread buffer returns
5. Drain cross-thread bridge messages (rebar ThreadBridge)
6. Loop
```

Epoch rotation happens at step 4 — after task polling, before the next batch of I/O submissions. This is the key advantage over layering on top of monoio or tokio: the rotation point is deterministic.

## The Zero-Copy Read Path

```
Before (monoio):
  Vec::with_capacity(8192)       ← allocator call
  monoio::TcpStream::read(vec)   ← kernel copies into vec
  http_parse(&vec[..n])           ← parser reads from vec
  drop(vec)                       ← deallocator call

After (turbine):
  pool.lease(8192)                ← ~19ns bump pointer in epoch arena
  stream.read_lease(lease)        ← kernel writes into arena via CQE
  http_parse(&lease[..n])         ← parser reads from arena slice
  drop(lease)                     ← decrement lease count (no dealloc)
  // Arena reclaimed in bulk when epoch's lease count hits zero
```

Zero copies from kernel completion to parser. Zero allocator calls in steady state. Arena reclamation is amortized — one `madvise(MADV_FREE)` per epoch, not one `free()` per request.

## Implementation: Rebar (rebar-v5)

### New Types

| Type | File | Purpose |
|------|------|---------|
| `RebarExecutor` | `executor.rs` | Cooperative executor: Proactor + epoch rotation + timer queue + task scheduler |
| `ExecutorConfig` | `executor.rs` | Config: tick_budget, epoch_interval, pool_config |
| `with_buffer_pool()` | `executor.rs` | Thread-local callback accessor for the pool |
| `LeaseIoBuf` | `io.rs` | Newtype wrapping `LeasedBuffer` to implement compio's `IoBuf`/`IoBufMut`/`SetLen` |
| `TcpStream::read_lease()` | `io.rs` | Zero-copy read into leased arena buffer |
| `RawTask` / `JoinHandle<T>` | `task.rs` | Task with drop-cancellation and detach |
| `sleep()` / `timeout()` | `time.rs` | Timer futures using executor's BTreeMap timer queue |
| `TcpListener` / `TcpStream` | `io.rs` | Ownership-based I/O via compio Proactor |

### Files Modified

| File | Change |
|------|--------|
| `Cargo.toml` | Replace monoio with compio-driver 0.11, compio-buf 0.8, turbine-core (git) |
| `src/executor.rs` | New: full cooperative executor with integrated epoch management |
| `src/task.rs` | New: Rc-based waker vtable, JoinHandle with drop-cancel |
| `src/time.rs` | New: sleep/timeout using executor timer queue |
| `src/io.rs` | New: TcpListener, TcpStream, LeaseIoBuf, OpFuture state machine |
| `src/runtime.rs` | Waker-based shutdown, ProcessContext with buffer pool access |
| `src/multi.rs` | RebarExecutor per thread instead of monoio RuntimeBuilder |
| `src/bridge.rs` | Unchanged — crossbeam + eventfd, wired into executor drain step |
| `src/process/mailbox.rs` | Unchanged — local_sync mpsc channels |

### Unsafe Code Audit

10 unsafe blocks in rebar-core, all sound:

- **Thread-local executor pointer** (`executor.rs`): Valid for `block_on()` lifetime, single-threaded, `!Send` enforced
- **Flag waker Rc vtable** (`executor.rs`, `task.rs`): Standard Rc raw pointer protocol, proper clone/forget/drop
- **`LeaseIoBuf::as_uninit()`** (`io.rs`): Returns full arena capacity as `MaybeUninit` — kernel writes into this region
- **`set_len()` after read** (`io.rs`): Only called on `Ok(n)` — kernel initialized those bytes
- **`OwnedFd::from_raw_fd`** (`io.rs`): FD just returned by accept syscall
- **Pin projections** (`time.rs`): Structural pinning for Timeout future
- **Pipe/eventfd syscalls** (`bridge.rs`): Direct kernel calls

### Dependencies

```toml
compio-buf = "0.8"           # Zero-copy I/O buffer traits
compio-driver = "0.11"       # Cross-platform async I/O (io_uring/IOCP/kqueue)
turbine-core = { git }       # Epoch-based buffer pool
socket2 = "0.5"              # Low-level socket control
crossbeam-channel = "0.5"    # Cross-thread messaging (bridge, PubSub)
local-sync = "0.1"           # !Send channel primitives (mailboxes)
```

Removed: `monoio`, `io-uring` (direct).

## Implementation: Mahalo (feat-add-turbine-support)

### Files Modified

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | rebar-core branch `rebar-v5`, add turbine-core |
| `mahalo-endpoint/Cargo.toml` | Add turbine-core dependency |
| `mahalo-endpoint/src/server.rs` | Hybrid lease/Vec read path, zero-copy fast path |
| `mahalo-endpoint/src/worker.rs` | `PoolConfig::default()` in ExecutorConfig |
| `mahalo-pubsub/src/pubsub.rs` | `payload: Arc<serde_json::Value>`, add `broadcast_arc()` |
| `mahalo-pubsub/src/bridge.rs` | Use `broadcast_arc()` with `Arc::clone` |
| `mahalo-channel/src/channel.rs` | Wrap payload construction in `Arc::new()` |
| `mahalo-channel/src/socket.rs` | Same Arc wrapping, dereference in assertions |
| `bench/bench.sh` | New benchmark runner script (oha) |

### Server Read Path (server.rs)

```rust
loop {
    // If accumulated partial data exists, use Vec path
    if let Some((ref mut buf, ref mut filled)) = accum {
        // ... existing Vec-based read + parse ...
        continue;
    }

    // Fast path: lease from epoch arena
    let lease = rebar_core::executor::with_buffer_pool(|pool| pool.lease(8192)).flatten();

    match lease {
        Some(lease) => {
            let BufResult(result, lease) = stream.read_lease(lease).await;
            // Parse directly from arena memory
            match http_parse::try_parse_request(&lease.as_slice()[..n], ...) {
                Ok(Some(parsed)) => {
                    // Complete request — execute, respond, lease drops (zero-alloc)
                    if leftover > 0 {
                        // Pipelining: copy leftover to lazy-allocated accum
                        accum = Some((leftover_vec, leftover_len));
                    }
                }
                Ok(None) => {
                    // Incomplete — copy to lazy-allocated accum, switch to Vec path
                    accum = Some((buf, n));
                }
            }
        }
        None => {
            // No pool or arena full — Vec fallback for this connection
            accum = Some((vec![0u8; 8192], 0));
        }
    }
}
```

### PubSub Arc Change

```rust
// Before: O(n) deep clone per subscriber
pub struct PubSubMessage {
    pub payload: serde_json::Value,
}

// After: O(1) atomic increment per subscriber
pub struct PubSubMessage {
    pub payload: Arc<serde_json::Value>,
}
```

`broadcast_arc()` added for callers that already hold an `Arc<Value>` (e.g., bridge relay), avoiding the clone entirely.

## Architecture Diagram

```
MahaloEndpoint (factory closures, Send+Sync)
  |
  +-- worker::spawn_workers()
       |
       +-- N OS threads (one per CPU core)
            |
            +-- RebarExecutor::new(ExecutorConfig {
            |     pool_config: Some(PoolConfig::default()),
            |   })
            |
            +-- compio_driver::Proactor
            |     (io_uring on Linux, IOCP on Windows, kqueue on macOS)
            |
            +-- IouringBufferPool (turbine-core)
            |     Epoch rotation every 100ms in event loop step 4
            |     ~19ns lease via bump pointer
            |
            +-- Rc<Runtime> (rebar, thread-local, !Send)
            |
            +-- server::run_accept_loop()
                  |
                  +-- rebar::spawn per connection
                       |
                       +-- handle_connection (hybrid read path)
                            |
                            +-- with_buffer_pool(|p| p.lease(8192))
                            +-- stream.read_lease(lease).await
                            +-- http_parse::try_parse_request(&lease[..n])
                            +-- handler::execute_request()  — Plug pipeline
                            +-- stream.write_all(resp).await
                            +-- drop(lease)  — lease count--, no dealloc
                            +-- keep-alive loop
```

## What Stays Unchanged

- **Conn, Plug, Pipeline** (`mahalo-core`) — no runtime dependency, pure middleware
- **MahaloRouter** — pure routing logic, no I/O
- **http_parse.rs** — zero-alloc HTTP/1.1 parser, works on `&[u8]` slices
- **ws_parse.rs** — WebSocket frame parser, pure logic
- **Telemetry** — synchronous `Rc<RefCell>` handlers, no broadcast
- **Factory pattern** — closures produce per-thread `!Send` state
- **`!Send` / `Rc` model** — correct for thread-per-core, not a monoio artifact
- **`local-sync`** — kept for process mailboxes, decoupled from monoio runtime
- **PubSub threading model** — crossbeam + dedicated std::thread, `Clone + Send + Sync`

## Known Issues / Hardening Items

1. **Missing SAFETY comments** on `LeaseIoBuf` unsafe blocks (`io.rs`)
2. **Redundant `ProcessContext::with_buffer_pool`** — delegates to global, no call sites, should be removed
3. **Magic number `8192`** appears 4 times in server.rs — extract to `DEFAULT_BUF_SIZE`
4. **Double-copy on pipelining leftover** — `to_vec()` then `copy_from_slice` into fresh Vec
5. **Bridge deep-clones through Arc** — `(*msg.payload).clone()` in bridge.rs defeats `Arc<Value>` purpose. Should use `broadcast_arc()` with `Arc::clone()`
6. **`write_all` per-iteration `.to_vec()`** in rebar's `io.rs` — allocates on partial writes under load
7. **No integration test for lease read path** — existing endpoint tests use default config (no pool)

## Future Work

### Phase 4: Deeper Optimizations
- **io_uring fixed-buffer registration** — requires compio upstream PR or fork to expose the ring
- **Adaptive epoch tuning** — monitor arena utilization, adjust rotation interval dynamically
- **CPU pinning / NUMA awareness** — `multi.rs` spawns threads but doesn't pin to cores
- **SendableBuffer for PubSub** — serialize once into arena, fan out references (if Arc<Value> proves insufficient)

### Remaining Feature Work
- **WebSocket handler** — `ws_parse.rs` exists but server-side upgrade + frame loop needs wiring on the new executor
- **HTTP round-trip integration tests** — need re-adding with compio/rebar test runtime
- **`mahalo-cluster`** — still uses tokio internally for topology polling (legitimate, cross-node)
- **Benchmark validation** — bench.sh exists but no comparison data yet. Need `oha`/`wrk` numbers vs monoio branch

### Architectural Considerations
- **Slowloris protection** — slow clients holding `LeasedBuffer` pin the epoch's arena. Under many slow connections, arenas accumulate up to `max_total_arenas`. May need forced collection with lease promotion (generational GC pattern)
- **CPU-bound GenServer starvation** — cooperative executor yields on `.await`. A CPU-bound GenServer can starve the event loop (and epoch rotation). Consider a `yield_now()` escape hatch or tick budget
- **Mailbox vs CQE priority** — ThreadBridge drain is step 5 (after epoch rotation). Under high cross-thread message volume, mailbox latency could spike relative to I/O completions

## The Principle

The turbine integration is not about replacing one I/O library with another. It's about owning the entire path from kernel completion to application logic. When you control the executor, you control when buffers rotate. When you control the buffer pool, you control where data lives. When you control both, you eliminate the allocation/deallocation cycle that causes p99 jitter — not by making `malloc` faster, but by not calling it.
