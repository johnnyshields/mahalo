# io_uring Enhancement Analysis for Rebar

**Date:** 2026-03-06

## Summary

Analysis of enhancing the rebar OTP-inspired runtime to leverage Linux io_uring for async I/O, covering feasibility, architectural challenges, and a pragmatic migration path.

## How It Could Work

- **tokio-uring or monoio** — Rust runtimes built on io_uring exist. Rebar currently sits on stock tokio, so the most practical path is swapping in `tokio-uring` for the I/O layer while keeping the GenServer/supervision abstractions intact.
- **Targeted use** — io_uring shines for disk I/O and network accept/read/write. Rebar's mailbox/channel/supervision logic is pure userspace and wouldn't benefit. The wins would be in the endpoint layer (socket accept, TLS, static file serving) and any file-based persistence.

## Key Challenges

### 1. Completion-Based vs Readiness-Based

tokio is readiness-based (epoll). io_uring is completion-based, meaning buffers must live until the kernel is done with them. This fundamentally changes ownership semantics — you can't use `&[u8]` borrows freely. Libraries like `tokio-uring` use owned buffers (`Vec<u8>`, `BufRing`) to handle this.

### 2. Axum Compatibility

mahalo-endpoint bridges to Axum, which is built on hyper/tokio (epoll). There's no drop-in io_uring Axum. Options:

- **Hybrid approach**: io_uring for file I/O, epoll for networking (easiest)
- **Full replacement**: Replace Axum with an io_uring-native HTTP stack like `monoio-http` (major rewrite)

### 3. Linux-Only

io_uring requires Linux 5.1+ (realistically 5.10+ for good feature coverage). Rebar would need a platform abstraction or feature flag to fall back to epoll on macOS/Windows.

### 4. Thread-Per-Core Model

io_uring runtimes typically use thread-per-core (no work stealing) rather than tokio's work-stealing scheduler. This changes how GenServer processes get scheduled and could affect rebar's concurrency model.

## Pragmatic Recommendation

The highest-ROI approach is a **hybrid model** — keep tokio for networking/scheduling but use io_uring selectively for:

- **Static file serving** in `StaticFiles` plug (where io_uring read shines)
- **Any future disk-based persistence or logging**

This can be done behind `cfg(target_os = "linux")` + a cargo feature flag (`io-uring`) without touching rebar's core abstractions. The GenServer, supervision, PubSub, and channel layers wouldn't need changes.

A full io_uring migration (replacing tokio entirely) would be a much larger undertaking that forces rethinking buffer ownership across the entire stack.

## Current State

The mahalo-endpoint crate now has a platform-adaptive architecture:

- `handler.rs` — shared request execution + `bind_socket` helper
- `worker.rs` — io_uring event loop (Linux only)
- `tcp_server.rs` — tokio TCP server (all platforms)
- `http_parse.rs` — zero-alloc HTTP/1.1 parser + serializer

This structure already implements the hybrid approach at the endpoint level, keeping rebar's core OTP abstractions untouched while gaining io_uring benefits for network I/O on Linux.
