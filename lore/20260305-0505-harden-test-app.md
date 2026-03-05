# Harden Ice Cream Store Demo App

**Date**: 2026-03-05
**Context**: After adding Tera HTML pages, support chat, clickable flavor tiles, and real-time price updates, the test app has grown to ~1250 lines with several data duplication issues, zero test coverage, and some inconsistencies.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Extract toppings/specials data to shared constants | Easy | High | Ask first |
| 2 | Standardize error response key ("error" vs "reason") | Quick | Medium | Auto-fix |
| 3 | Extract `parse_id_param()` helper for repeated ID parsing | Quick | Low | Auto-fix |
| 4 | Add seed to price fluctuation for session variance | Quick | Low | Auto-fix |
| 5 | Extract SupportChannel chat logic to testable `ChatResponder` | Easy | Medium | Ask first |
| 6 | DRY up router scope cloning boilerplate (6 identical patterns) | Moderate | Medium | Ask first |
| 7 | Add test coverage for channels, render functions, and chat | Hard | High | Ask first |

## Opportunity Details

### #1 Extract toppings/specials data to shared constants
- **What**: Toppings data appears in `render_menu()`, `toppings_json()`, and hardcoded in `order.html`. Specials data appears in `render_menu()`, `render_about()`, and `specials_json()`. Extract to shared `fn toppings_data()` and `fn specials_data()` that return structured data, used by both API and template rendering.
- **Where**: `main.rs` lines 782-796, 831-835, 863-893; `order.html` topping options
- **Why**: Single source of truth prevents data drift bugs. Currently if you add a topping you must edit 3 places.
- **Trade-offs**: Minor refactor, template toppings would need to come from context instead of hardcoded HTML.

### #2 Standardize error response key
- **What**: Most endpoints use `{"error":"..."}` but `OrderChannel::handle_in` uses `{"reason":"..."}` in two places.
- **Where**: `main.rs` lines ~514, ~626 (channel reply errors)
- **Why**: Consistency for API consumers.

### #3 Extract `parse_id_param()` helper
- **What**: The pattern `conn.path_params.get("id").and_then(|s| s.parse().ok()).unwrap_or(0)` appears 5+ times across controllers.
- **Where**: `main.rs` FlavorController and OrderController methods
- **Why**: Reduces boilerplate, single place to change ID parsing logic.

### #4 Add seed to price fluctuation
- **What**: The delta formula `((tick * 7 + idx as u64 * 13) % 51) as i64 - 25` is fully deterministic. Add startup timestamp as seed for per-session variance.
- **Where**: `main.rs` price fluctuation background task (~line 1197)
- **Why**: More interesting demo behavior across restarts.

### #5 Extract SupportChannel chat logic to `ChatResponder`
- **What**: Move the if-else chain of keyword matching into a separate `struct ChatResponder` with a `fn respond(&self, message: &str) -> &str` method. Makes it unit-testable without spinning up channels.
- **Where**: `main.rs` SupportChannel::handle_in (~lines 674-697)
- **Why**: Testability, separation of concerns.

### #6 DRY up router scope cloning
- **What**: The browser scope has 6 routes each following the same pattern of cloning `tera` + `store`, wrapping in `plug_fn`, inner clone, async move. Consider a helper closure or macro.
- **Where**: `main.rs` lines 995-1038
- **Why**: Reduces ~60 lines of boilerplate to ~12.
- **Trade-offs**: A macro may obscure control flow. A closure helper may fight Rust's type system. Need to verify it works with the framework's `plug_fn` API.

### #7 Add test coverage
- **What**: Add `#[cfg(test)] mod tests` with tests for:
  - `ChatResponder::respond()` keyword matching (if #5 accepted)
  - `format_price()` and `format_flavor()` helpers
  - `parse_order_id()` topic parsing
  - Toppings/specials data helpers (if #1 accepted)
  - `parse_id_param()` (if #3 accepted)
- **Where**: `main.rs` bottom of file
- **Why**: Zero test coverage currently. These are pure functions that are easy to test.
- **Trade-offs**: Integration tests for full endpoint/channel flows would require spinning up the server, which is heavyweight for a demo app. Focus on unit tests for pure functions.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
