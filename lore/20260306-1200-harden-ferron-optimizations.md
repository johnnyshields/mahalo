# Hardening: Ferron-Inspired Performance Optimizations

## Context
Recent commits `ce93003` and `75ca425` added Ferron-inspired optimizations: BLAKE3 ETag, mimalloc, precompressed static files, pre-parsed headers, EMFILE backoff. This hardening pass reviews those changes for duplication, inconsistencies, missing tests, and dead code.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add comment explaining BLAKE3 16-byte truncation in etag.rs | Quick | Low | Ask first |
| 2 | Use `HeaderValue::from_static()` for MIME content-type in static_files | Quick | Low | Ask first |
| 3 | Add missing ETag edge-case test (pre-existing etag header) | Quick | Medium | Ask first |
| 4 | Add missing static_files tests (HEAD on precompressed, gzip fallback, zstd variant) | Easy | Medium | Ask first |
| 5 | Add ETag to StaticFiles responses (precompressed + normal) | Easy | High | Ask first |
| 6 | Dedup `try_parse_request` / `try_parse_into_conn` in http_parse.rs | Moderate | High | Ask first |

## Opportunity Details

### 1. Add comment explaining BLAKE3 truncation
- **What**: Add a brief comment at the `&hex[..32]` line explaining the intentional truncation (128-bit collision resistance is more than adequate for ETags).
- **Where**: `crates/mahalo-plug/src/etag.rs` line 28
- **Why**: Prevents future confusion about why we discard half the hash.

### 2. Use `HeaderValue::from_static()` for MIME content-type
- **What**: Since `mime_from_extension()` returns `&'static str`, use `HeaderValue::from_static(mime)` and `conn.resp_headers.insert()` directly instead of `put_resp_header(str, str)` which re-parses every request.
- **Where**: `crates/mahalo-plug/src/static_files.rs` — both the precompressed and normal serve paths
- **Why**: Consistent with the pre-parsed header pattern already used for cache-control and secure_headers.

### 3. Add missing ETag edge-case test
- **What**: Test that ETag plug overwrites a pre-existing `etag` response header (document intended behavior).
- **Where**: `crates/mahalo-plug/src/etag.rs` test module
- **Why**: Edge case not covered. Current behavior overwrites — test should document this.

### 4. Add missing static_files tests
- **What**: Add tests for: (a) HEAD request on precompressed file, (b) gzip fallback when br variant missing, (c) zstd variant serving, (d) priority order when multiple encodings accepted
- **Where**: `crates/mahalo-plug/src/static_files.rs` test module
- **Why**: Precompressed serving is new code with several branches untested.

### 5. Add ETag to StaticFiles responses
- **What**: Compute BLAKE3 ETag inside StaticFiles for served files (both normal and precompressed). Check `if-none-match` and return 304. Static files call `.halt()`, so after-plugs like ETag never run.
- **Where**: `crates/mahalo-plug/src/static_files.rs` — both serve paths
- **Why**: Without this, clients can't use conditional GETs on static files, defeating HTTP caching.
- **Trade-offs**: Adds blake3 computation per static file serve (cheap vs disk I/O). Already in the crate's dependencies.

### 6. Dedup `try_parse_request` / `try_parse_into_conn`
- **What**: Extract shared parsing logic (httparse → method/uri/headers/body/keep_alive) into an internal helper. Both functions call the helper; `try_parse_request` creates a new Conn, `try_parse_into_conn` calls `conn.reset()` then populates. ~80 lines of near-identical logic.
- **Where**: `crates/mahalo-endpoint/src/http_parse.rs` lines 44-134 vs 144-225
- **Why**: Bugs fixed in one won't propagate to the other. Safety comments already diverged between the two.
- **Trade-offs**: Slightly more complex function signatures. The two paths differ in how headers are populated (new HeaderMap vs reusing existing), so the helper needs a callback or enum strategy.

## Verification
```bash
cargo check --workspace
cargo test --workspace
```

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
