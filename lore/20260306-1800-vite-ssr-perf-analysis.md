# Vite SSR Performance Analysis: Mahalo vs Hono vs Zig/Bun

## Can Mahalo Serve Vite SSR?

Yes, but Mahalo has no embedded JS runtime. Practical approaches:

1. **Node sidecar**: Spawn a Node.js process running the Vite SSR bundle, call it via HTTP/stdin from a Mahalo plug. Serve client assets with `StaticFiles`.
2. **Embedded V8**: Use `deno_core` or `v8` crate to execute JS SSR within a Mahalo plug.
3. **Pre-render / SSG**: Vite SSG mode at build time, serve as static files.
4. **Reverse proxy**: Mahalo proxies SSR requests to a Node SSR server on another port.

Approach 1 (sidecar) is most common for Rust frameworks doing JS SSR.

## Mahalo vs Hono Performance

**Where Mahalo wins:**
- Static asset serving — io_uring on Linux + zero-copy HTTP parsing vs Node/Bun event loop
- Middleware overhead — Rust plug pipeline is plain function calls, no GC pauses
- Memory footprint — significantly lower under load at high concurrency

**Where roughly equal:**
- SSR render time — JS execution dominates regardless of framework; V8 is the bottleneck

**Where Hono may win:**
- SSR latency (sidecar approach) — Mahalo adds 1-5ms IPC overhead per request calling out to Node; Hono runs in-process
- Developer velocity — single-language JS stack, no FFI boundary
- Bun + Hono — Bun's HTTP server (written in Zig) is competitive with many native solutions

**Bottom line:** For Vite SSR, JS rendering is almost always the bottleneck. Framework overhead difference is single-digit milliseconds. Pick Mahalo for Rust ecosystem benefits (memory safety, low resource usage at scale), not raw SSR speed. At high concurrency with heavy static traffic alongside SSR, Mahalo's io_uring path shows a meaningful edge.

## Zig vs Rust Performance

No meaningful difference. Both compile to LLVM IR and produce nearly identical machine code. Benchmarks trade blows within noise.

The difference is in what they encourage:
- **Zig**: No hidden allocations, no hidden control flow. Every allocation is explicit. Leads to faster code by default through forced awareness.
- **Rust**: Zero-cost abstractions (iterators, monomorphization) can optimize equally well, but it's easier to accidentally pull in allocations via trait objects, Vec, String, etc.

For web servers specifically, performance depends on syscall strategy (io_uring vs epoll vs kqueue), allocation patterns, and data layout — both languages give full control over all of these.

Bun's speed comes from smart engineering (arena allocators, minimal copying, integrated HTTP parser), not from Zig-the-language. The same choices in Rust yield the same results.

## What is LLVM?

LLVM (Low Level Virtual Machine) is a compiler infrastructure that turns source code into machine instructions:

```
Source code -> Frontend -> LLVM IR -> Optimizer -> Machine code
```

- **Frontend**: Language-specific (rustc, clang, Zig). Parses code into LLVM intermediate representation.
- **LLVM IR**: Portable, language-agnostic assembly-like format. This is why Rust and Zig produce nearly identical binaries.
- **Optimizer**: Runs passes (inlining, dead code elimination, vectorization) on the IR.
- **Backend**: Converts optimized IR to target machine code (x86, ARM, RISC-V, WebAssembly).

Started by Chris Lattner ~2003 at University of Illinois. Used by Rust, Swift, Zig, Clang, Julia, Kotlin Native, and others. Lets language designers write just a frontend and get world-class optimization + multi-platform support for free.
