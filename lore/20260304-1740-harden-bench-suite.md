# Harden: Benchmark Suite

## Context

The benchmark suite was built rapidly in a single session. It works but has dead code, redundant files, a hacky non-Rust framework benchmark section in `run.sh`, and inconsistencies. This plan cleans it up.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Remove dead Fastify files and references | Quick | Medium | Auto-fix |
| 2 | Remove unused `rails_app.ru` (never wired into runner) | Quick | Low | Auto-fix |
| 3 | Remove unused `PORT_FASTIFY` variable and dead `start_fastify()` | Quick | Low | Auto-fix |
| 4 | Fix non-Rust bench lines — `|| true` swallows `should_run` failures | Quick | High | Auto-fix |
| 5 | Remove `actix-rt` dep (unused — `actix-web` pulls it in) | Quick | Low | Auto-fix |
| 6 | Cargo.toml: remove `serde_json` dep (unused in bench bins) | Quick | Low | Auto-fix |
| 7 | JSON results: use `jq` to build valid JSON instead of heredoc string interpolation | Easy | Medium | Ask first |
| 8 | Build Rust servers once, not per-framework (build duplicated in `start_mahalo`) | Easy | Medium | Ask first |

## Opportunity Details

### #1 Remove dead Fastify files
- **What**: Delete `bench/frameworks/node/fastify.js`, remove `fastify` from considerations
- **Where**: `bench/frameworks/node/fastify.js`
- **Why**: Fastify hangs on Node v24+; file is dead weight and confusing

### #2 Remove unused `rails_app.ru`
- **What**: Delete `bench/frameworks/ruby/rails_app.ru`
- **Where**: `bench/frameworks/ruby/rails_app.ru`
- **Why**: Never wired into `run.sh` — it was aspirational, not functional. Requires Rails gem which isn't in the Gemfile.

### #3 Remove dead Fastify port/function from `run.sh`
- **What**: Remove `PORT_FASTIFY=3004`, the `start_fastify()` function, and `start_fastify` call from `main()`
- **Where**: `bench/run.sh` lines 41, 242-245, 344
- **Why**: Dead code left from disabled Fastify support

### #4 Fix non-Rust bench lines swallowing errors
- **What**: The lines like `should_run "express" && curl ... && bench_framework ... || true` are broken — if `should_run` returns false (filtered out), the `|| true` still runs, meaning control flow is wrong. Restructure to use proper `if` blocks.
- **Where**: `bench/run.sh` lines 357-359
- **Why**: Correctness — currently `--framework mahalo` would skip Express but `|| true` masks it incorrectly. The lines are also unreadable.

### #5 Remove unused `actix-rt` dependency
- **What**: Remove `actix-rt = "2"` from `bench/Cargo.toml`
- **Where**: `bench/Cargo.toml` line 27
- **Why**: Not imported anywhere. `actix-web` brings in its own runtime via `#[actix_web::main]`.

### #6 Remove unused `serde_json` dependency
- **What**: Remove `serde_json.workspace = true` from `bench/Cargo.toml`
- **Where**: `bench/Cargo.toml` line 25
- **Why**: Not imported in any bench binary. Only `serde` is used (by actix for `Serialize`).

### #7 Build valid JSON results
- **What**: Replace the heredoc-based JSON building (`cat >> ... <<JSONEOF`) with `jq -n` to properly escape values and produce valid JSON.
- **Where**: `bench/run.sh` `run_benchmark()` function, lines 183-187
- **Why**: Current approach breaks if any parsed value contains quotes or special characters. The `rps_num` stripping commas is also fragile.

### #8 Build Rust servers once
- **What**: Move the `cargo build` call out of `start_mahalo()` into the main flow so it runs once before any server starts. Currently only `start_mahalo` builds, but all three Rust binaries come from the same crate so the build compiles all of them.
- **Where**: `bench/run.sh` — extract build from `start_mahalo()`, add to `main()` before server starts
- **Why**: Cleaner separation. If you run `--framework actix`, the build wouldn't happen at all currently (it's inside `start_mahalo`). This is a bug.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
