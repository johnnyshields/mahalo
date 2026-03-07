# Rebar + SQLite: Embedded Document Database Architecture

**Date:** 2026-03-06
**Status:** Exploration / Strategic Analysis

## Context

Analysis of combining rebar (OTP-inspired Rust runtime) with SQLite to build an
embedded document database — "the SQLite of document databases" — as a potential
MongoDB disruptor.

## Market Gap

- **SQLite** proved embedded relational is a massive market (most deployed DB in the world)
- **DuckDB** proved the same for analytics (embedded OLAP)
- **Nobody has done it well for documents** — that's the gap

Existing attempts:
- **PoloDB** — Rust embedded document DB, minimal and unmaintained
- **SurrealDB** — multi-model but went the server route (opposite of embedded)
- **FerretDB** — MongoDB protocol on Postgres, still requires a server
- **Turso/libSQL** — SQLite fork with replication, shows the business model works

## Why Rust

- Compiles to a single static library with C ABI — embed anywhere (Python, Node, Go, Java, mobile)
- No GC pauses — predictable latency like SQLite
- Memory safety without runtime cost
- Cross-compiles to every platform including WASM (runs in the browser)

## Core Library Architecture

```
libmahalo-db (core library, Rust)
├── Storage engine (LSM-tree or B-tree, memory-mapped)
├── Document model (BSON-like or just JSON with binary extensions)
├── Query engine (find/aggregate/index)
├── ACID transactions (MVCC)
└── WAL for durability

Optional layers:
├── Sync/replication (CRDTs or OT for conflict resolution)
├── WASM build (runs in browser)
└── MongoDB wire protocol (drop-in migration path)
```

## Rebar + SQLite Integration

### Architecture

```
RebarSupervisor
├── DbPool (GenServer - round-robin or checkout-based)
│   ├── DbWorker 1 (GenServer - owns one sqlite3 connection)
│   ├── DbWorker 2 (GenServer - read-only replica)
│   └── DbWorker 3 (GenServer - read-only replica)
├── WalCheckpointer (GenServer - periodic WAL checkpoint)
└── BackupWorker (GenServer - scheduled backups)
```

### Why GenServer + SQLite Pairs Well

- SQLite is single-writer — a GenServer serializes writes naturally
- Connection pooling maps to a supervision tree of GenServer workers
- Background tasks (WAL checkpointing, vacuum, backups) become supervised processes
- Read/write splitting: N readers, 1 writer
- Backpressure via GenServer mailbox
- Clean shutdown ordering
- PubSub notifications on writes (trigger -> GenServer -> rebar PubSub -> subscribers)

### Rust Libraries

- **`rusqlite`** — mature, synchronous SQLite bindings (good for GenServer since each worker is its own task)
- **`sqlite-vfs`** — custom VFS for exotic use cases (encryption, remote storage)

### Key Design Decision: `!Send` Connections

`rusqlite::Connection` is `!Send` (can't move between threads). Options:

1. **`spawn_blocking`** (recommended) — create the connection inside a `spawn_blocking`
   closure that runs the GenServer's message loop on a dedicated OS thread. Cleanest
   and most Erlang-like — each DbWorker lives on its own thread, like an Erlang
   process with a port driver.
2. **Raw SQLite FFI** — the C API is thread-safe with `SQLITE_OPEN_FULLMUTEX`.
3. **Existing pool patterns** — reference `deadpool-sqlite` or `r2d2-sqlite`.

### Pseudocode Sketch

```rust
struct DbWorker {
    conn: rusqlite::Connection,
}

enum DbMsg {
    Query { sql: String, reply: oneshot::Sender<Vec<Row>> },
    Execute { sql: String, reply: oneshot::Sender<usize> },
}

impl GenServer for DbWorker {
    type Message = DbMsg;

    async fn handle_cast(&mut self, msg: DbMsg) {
        match msg {
            DbMsg::Query { sql, reply } => {
                let rows = self.conn.prepare(&sql)
                    .and_then(|mut stmt| /* collect rows */);
                let _ = reply.send(rows);
            }
            DbMsg::Execute { sql, reply } => {
                let changed = self.conn.execute(&sql, []);
                let _ = reply.send(changed);
            }
        }
    }
}
```

## Where Rebar Fits (and Doesn't)

- **Doesn't fit in the core embedded library** — the engine should be minimal, no async
  runtime, no actor system. Just like SQLite is a single C file.
- **Fits in the optional server mode** — if offering a MongoDB-compatible network server
  on top, rebar's supervision/GenServer model works well for connection management and
  background tasks.
- **Fits as the coordination layer** — connection pooling, background maintenance,
  PubSub change notifications, supervised workers.

This is essentially what Elixir's `Ecto.Adapters.SQLite3` + `db_connection` does, but
in Rust with rebar.

## MongoDB Disruption Strategy

### Where MongoDB Is Vulnerable

1. **Cost** — Atlas pricing is a major pain point at scale
2. **Document model limitations** — no real joins, weak transactions, schema validation is an afterthought
3. **Operational complexity** — sharding is painful, replica set management is non-trivial
4. **Performance cliffs** — large working sets, poor index coverage, write-heavy workloads
5. **Vendor lock-in** — Atlas-specific features, SSPL licensing alienated open-source community

### Go-to-Market

1. Ship a Rust crate + C library + Python/Node bindings
2. MongoDB query syntax compatibility (low switching cost)
3. "Replace your MongoDB Atlas with one line of code"
4. Free/open-source core (not SSPL like MongoDB)
5. Paid sync/cloud layer later (the SQLite -> Turso playbook)

### Best Disruption Angle

Don't build "better MongoDB." Build for a use case MongoDB serves poorly, then expand.
The strongest angle is **local-first/embedded documents with automatic sync** — that's
where the industry is heading (edge computing, offline-capable apps) and MongoDB has no
good answer.
