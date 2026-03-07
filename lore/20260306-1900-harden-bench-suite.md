# Harden: Benchmark Suite

## Context
The bench suite was expanded from plaintext/json to include TechEmpower scenarios (db, queries, fortunes, updates, cached-queries), REST API scenarios (path params, search, POST echo), middleware pipelines, and new frameworks (xitca-web, may-minihttp, granian, falcon). This review hardens the implementation for correctness and consistency.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add missing POST /api/echo to Elixir and Ruby | Quick | High | Ask first |
| 2 | Fix xitca query param parsing (hardcoded counts) | Quick | High | Ask first |
| 3 | Fix Elixir parse_count misuse for page param | Quick | Medium | Ask first |
| 4 | Fix run.sh CPU pinning wraparound bug | Quick | Medium | Ask first |
| 5 | Extract shared query-param helper for may-minihttp/xitca | Easy | Medium | Ask first |
| 6 | Fix axum /api/users/:id 404 response to return JSON | Quick | Low | Ask first |

## Opportunity Details

### #1 — Add missing POST /api/echo to Elixir and Ruby
- **What**: Add `POST /api/echo` endpoint to Elixir (`bench_phoenix.ex`) and Ruby (`config.ru`) — both currently lack it, causing benchmark parity failures when run.sh tests the `/api` scenario group.
- **Where**: `bench/frameworks/elixir/lib/bench_phoenix.ex`, `bench/frameworks/ruby/config.ru`
- **Why**: Without this, the bench suite silently produces errors for these frameworks on the echo test.

### #2 — Fix xitca query param parsing
- **What**: The xitca bench server hardcodes query counts (5, 5, 10) instead of parsing the actual `?queries=N` / `?count=N` params. Use the same `parse_query_param()` helper already in `shared.rs` (via the manual split approach also used in may-minihttp).
- **Where**: `bench/src/bin/xitca.rs` — queries, updates, cached-queries handlers
- **Why**: Without this, xitca always returns fixed-size arrays regardless of the benchmark's `?queries=20` param, making its numbers non-comparable.

### #3 — Fix Elixir parse_count misuse for page param
- **What**: The `/api/search` handler calls `BenchPhoenix.Data.parse_count(conn.query_params["page"])` which clamps to 1..500. Page numbers shouldn't be clamped at 500 — use a direct integer parse with default 1 instead.
- **Where**: `bench/frameworks/elixir/lib/bench_phoenix.ex`, search handler
- **Why**: Correctness — page 501+ would silently clamp to 500.

### #4 — Fix run.sh CPU pinning wraparound
- **What**: When `NEXT_CORE + CORES` exceeds available CPUs, `run_pinned` falls back to no pinning but never resets `NEXT_CORE`. All subsequent servers also lose pinning. Reset `NEXT_CORE=0` on wraparound.
- **Where**: `bench/run.sh`, `run_pinned()` function
- **Why**: With `--cores 4` on an 8-core machine, only the first 2 servers get properly pinned.

### #5 — Extract shared query-param helper
- **What**: Both `may_minihttp.rs` and `xitca.rs` have identical manual `parse_query_param(query, key)` functions. Move to `shared.rs`.
- **Where**: `bench/src/shared.rs`, `bench/src/bin/may_minihttp.rs`, `bench/src/bin/xitca.rs`
- **Why**: DRY — same 8-line function duplicated.

### #6 — Fix axum 404 JSON response
- **What**: `axum_raw.rs` `user_by_id` returns `(StatusCode::NOT_FOUND, r#"{"error":"not found"}"#)` without Content-Type header. Add JSON content-type.
- **Where**: `bench/src/bin/axum_raw.rs`, `user_by_id` handler
- **Why**: Consistency with other frameworks that return proper JSON 404s.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
