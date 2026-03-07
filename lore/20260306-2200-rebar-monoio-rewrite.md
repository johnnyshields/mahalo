# Rebar on Monoio: Thread-Per-Core OTP Runtime

**Date:** 2026-03-06
**Scope:** Architecture design for rewriting rebar-core on ByteDance's monoio runtime
**Status:** Proposal

## Motivation

Rebar currently runs GenServers as tokio tasks on a multi-threaded work-stealing executor. Mahalo's io_uring event loop runs on a dedicated OS thread and must `block_on()` + synchronize (via `Notify`) to interact with rebar GenServers on the tokio runtime. This cross-thread hop is the primary bottleneck in the WebSocket path.

Monoio's thread-per-core model eliminates this: each thread runs its own monoio runtime with its own io_uring instance. GenServers, network I/O, and WS frame processing all share the same event loop — zero cross-thread synchronization for the common case.

## Core Insight: Thread = Erlang Scheduler

In Erlang/OTP, each scheduler thread runs a set of processes. Monoio maps to this naturally:

```
Thread 0 (monoio + io_uring)          Thread 1 (monoio + io_uring)
├── ProcessTable (HashMap, no locks)   ├── ProcessTable (HashMap, no locks)
├── GenServer A (Rc<State>!)           ├── GenServer C
├── GenServer B                        ├── GenServer D
├── TcpListener (SO_REUSEPORT)         ├── TcpListener (SO_REUSEPORT)
└── WS connections (same thread!)      └── WS connections (same thread!)
         ↕ crossbeam channel ↕
       (only for cross-thread routing)
```

## Key Changes from Current Rebar

### 1. Mailbox: tokio::sync::mpsc → local_sync::mpsc

```rust
// BEFORE (rebar today): requires Send, uses atomics
pub struct MailboxTx(tokio::sync::mpsc::UnboundedSender<Message>);

// AFTER (monoio-rebar): thread-local, no Send, no atomics
pub struct MailboxTx(local_sync::mpsc::UnboundedSender<Message>);
```

The `local-sync` crate (by the monoio team) provides single-thread mpsc/oneshot with zero atomic operations and zero locks.

### 2. ProcessTable: DashMap → HashMap

```rust
// BEFORE: lock-free concurrent map (multi-threaded tokio)
processes: DashMap<ProcessId, ProcessHandle>,
next_id: AtomicU64,

// AFTER: plain HashMap (single thread owns it)
processes: HashMap<ProcessId, ProcessHandle>,
next_id: Cell<u64>,  // Cell, not Atomic
```

### 3. GenServer State: Drop Send + Sync Bounds

```rust
// BEFORE
pub trait GenServer: Send + Sync + 'static {
    type State: Send + 'static;
}

// AFTER — the biggest ergonomic win
pub trait GenServer: 'static {
    type State: 'static;  // No Send! Rc, RefCell, thread-local caches OK
}
```

GenServer state can hold `Rc<Vec<...>>`, `RefCell<HashMap<...>>`, thread-local connection pools — things impossible with tokio's Send requirement.

### 4. Runtime::spawn: tokio::spawn → monoio::spawn

```rust
// BEFORE
tokio::spawn(gen_server_loop(server, args, ctx));  // must be Send

// AFTER
monoio::spawn(gen_server_loop(server, args, ctx));  // non-Send OK
```

### 5. Cross-Thread Router (the one hard part)

Within a thread, routing is `HashMap::get` + channel send (zero-cost). Across threads, a crossbeam bridge:

```rust
pub enum MessageRouter {
    /// Same-thread: direct mailbox send (zero-cost)
    Local(Rc<ProcessTable>),
    /// Cross-thread: serialized over crossbeam channel
    Remote {
        senders: Vec<crossbeam::channel::Sender<RoutedMessage>>,
    },
}
```

Each thread runs a background task draining its crossbeam receiver into local mailboxes. Cross-thread messages pay a serialization + channel cost; same-thread messages are free.

### 6. Timers and Cancellation

```rust
// BEFORE
tokio::time::timeout(duration, future)
tokio_util::sync::CancellationToken

// AFTER
monoio::time::sleep(duration)  // requires enable_timer()
// CancellationToken: replace with local_sync::oneshot or Rc<Cell<bool>>
```

Monoio timer uses io_uring's timer facility on Linux — kernel-level, not userspace wheel.

## Mahalo Endpoint Payoff

The biggest win is eliminating the entire synchronization layer in `uring.rs`:

```rust
// TODAY (uring.rs): io_uring thread ←block_on→ tokio thread
tokio_rt.block_on(async {
    gen_server::cast_from_runtime(runtime, ws.gen_pid, val).await;
});
// ... wait for Arc<Notify> with 50ms timeout ...
// ... then drain mpsc ...

// MONOIO-REBAR: everything on the same thread, same event loop
let mailbox = process_table.get(ws.gen_pid).unwrap();
mailbox.send(cast_msg);  // local_sync send — instant, no atomics
// GenServer processes message in the next poll iteration
// WS drain happens naturally — no Notify, no block_on, no timeout
```

io_uring completions, GenServer mailbox processing, and WS frame writes all happen in the **same event loop iteration** on the **same thread**.

## Trade-offs

| Trade-off | Impact | Mitigation |
|-----------|--------|------------|
| No work-stealing | Unbalanced load stays on one core | SO_REUSEPORT distributes accepts; most WS workloads are balanced |
| Cross-thread GenServer calls slower | Need crossbeam bridge + waker | Most calls are same-thread; design channels to be thread-local |
| PubSub across threads | Broadcast must fan out via crossbeam | Each thread subscribes; cross-thread broadcast is the exception path |
| Process migration impossible | Can't move a GenServer to another thread | Erlang rarely migrates in practice; acceptable |
| Ecosystem compatibility | Can't use tokio-native libraries directly | monoio-compat bridges AsyncRead/AsyncWrite; local-sync for channels |
| Supervision spanning threads | Coordinator on each thread | Top-level supervisor uses cross-thread messaging |

## Estimated Effort

| Component | Effort | Notes |
|-----------|--------|-------|
| Mailbox (local-sync) | Small | Drop-in replacement |
| ProcessTable (HashMap) | Small | Simpler than today |
| GenServer loop | Small | Same logic, different spawn |
| Router (local + remote) | Medium | New cross-thread bridge |
| Supervisor | Medium | Thread-aware child placement |
| CancellationToken replacement | Small | local_sync oneshot or Rc<Cell<bool>> |
| mahalo-endpoint (uring.rs) | Large refactor, large win | Eliminate entire block_on/Notify sync layer |
| PubSub cross-thread | Medium | Crossbeam broadcast fan-out |

## Performance Expectations

Based on monoio benchmarks (ByteDance):
- **Single core**: Slightly better than tokio
- **4 cores**: ~2x throughput vs tokio
- **16 cores**: ~3x throughput vs tokio
- **Real-world**: ByteDance gateway saw +20% vs NGINX; RPC saw +26% vs tokio version

For mahalo specifically, the biggest gain isn't raw I/O throughput (already using io_uring directly) — it's eliminating the **cross-thread synchronization overhead** in the GenServer ↔ io_uring interaction path.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `monoio` | Thread-per-core async runtime with io_uring |
| `local-sync` | Single-thread mpsc/oneshot (monoio team) |
| `crossbeam-channel` | Cross-thread message bridge |
| `monoio-tungstenite` | WebSocket support (optional, for non-io_uring path) |

## Open Questions

1. **ProcessId encoding**: Should thread ID be part of ProcessId? e.g., `(thread_id, local_id)` for fast local-vs-remote routing decisions.
2. **Supervisor thread affinity**: Should a supervisor and all its children live on the same thread? (Simpler, matches Erlang scheduler affinity.)
3. **GenServer call across threads**: Use crossbeam + local_sync oneshot bridge? Or expose a `call_remote` that returns a Future backed by flume?
4. **Graceful shutdown**: How to coordinate shutdown across N independent monoio runtimes? Shared AtomicBool + per-thread cancellation signal?
5. **Testing**: monoio's `#[monoio::test]` macro vs custom test harness for multi-thread scenarios.
