# Telemetry Guide

Mahalo Telemetry provides structured observability events for monitoring, metrics, and debugging. Handlers are synchronous and thread-local (`Rc<RefCell>`), with no broadcast channels or atomics.

## Overview

The telemetry system has three operations:

1. **Execute** - Emit a named event with measurements and metadata
2. **Attach** - Register handlers that respond to events matching a name prefix
3. **Span** - Wrap async work in start/stop events with automatic duration measurement

## Creating a Telemetry Instance

```rust
use mahalo::Telemetry;

let telemetry = Telemetry::new(512);

// Or use the default (capacity 512)
let telemetry = Telemetry::default();
```

`Telemetry` is `Clone` and thread-local (not `Send`). Each worker thread creates its own instance.

## Emitting Events

```rust
use std::collections::HashMap;

let mut measurements = HashMap::new();
measurements.insert("duration_ms".to_string(), 42.0);

let mut metadata = HashMap::new();
metadata.insert("method".to_string(), serde_json::json!("GET"));
metadata.insert("path".to_string(), serde_json::json!("/api/users"));

telemetry.execute(
    &["mahalo", "endpoint", "stop"],
    measurements,
    metadata,
);
```

Events are dispatched synchronously to all matching attached handlers inline.

## Attaching Handlers

Handlers match by event name **prefix**. A handler attached to `["mahalo", "endpoint"]` will fire for both `["mahalo", "endpoint", "start"]` and `["mahalo", "endpoint", "stop"]`.

```rust
telemetry.attach(&["mahalo", "endpoint"], |event| {
    println!(
        "Event: {:?} | Duration: {:?}ms",
        event.name,
        event.measurements.get("duration_ms")
    );
});
```

Handlers run inline during `execute()`. Keep them fast to avoid blocking the request path.

## Spans

`span()` wraps async work with automatic start/stop events and duration measurement:

```rust
let result = telemetry.span(
    &["mahalo", "db", "query"],
    HashMap::from([("table".into(), serde_json::json!("users"))]),
    || async {
        // Your async work here
        db.query("SELECT * FROM users").await
    },
).await;
```

This emits:
1. `["mahalo", "db", "query", "start"]` with the metadata (before the work)
2. `["mahalo", "db", "query", "stop"]` with `duration_ms` measurement + metadata (after the work)

## Built-in Event Names

Mahalo defines these event name constants:

| Constant | Name | Description |
|----------|------|-------------|
| `ENDPOINT_START` | `["mahalo", "endpoint", "start"]` | HTTP request received |
| `ENDPOINT_STOP` | `["mahalo", "endpoint", "stop"]` | HTTP response sent |
| `ROUTER_DISPATCH` | `["mahalo", "router", "dispatch"]` | Route matched and dispatched |
| `CHANNEL_JOIN` | `["mahalo", "channel", "join"]` | WebSocket channel joined |
| `CHANNEL_HANDLE_IN` | `["mahalo", "channel", "handle_in"]` | Channel event handled |

Use the `event_name()` helper to convert constants to owned `Vec<String>`:

```rust
use mahalo::{event_name, ENDPOINT_STOP};

let name: Vec<String> = event_name(ENDPOINT_STOP);
// ["mahalo", "endpoint", "stop"]
```

## TelemetryEvent Structure

```rust
pub struct TelemetryEvent {
    pub name: Vec<String>,                          // Hierarchical name
    pub measurements: HashMap<String, f64>,         // Numeric metrics
    pub metadata: HashMap<String, serde_json::Value>, // Arbitrary context
    pub timestamp: u64,                             // Unix epoch millis
}
```

## Example: Request Logging

```rust
use mahalo::{Telemetry, ENDPOINT_STOP};

let telemetry = Telemetry::default();

telemetry.attach(&["mahalo", "endpoint", "stop"], |event| {
    let method = event.metadata.get("method").and_then(|v| v.as_str()).unwrap_or("?");
    let path = event.metadata.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let duration = event.measurements.get("duration_ms").unwrap_or(&0.0);
    tracing::info!("{method} {path} completed in {duration:.2}ms");
});
```

## Example: Prometheus Metrics

```rust
telemetry.attach(&["mahalo"], |event| {
    let name = event.name.join(".");
    for (key, value) in &event.measurements {
        prometheus::histogram!(format!("{name}.{key}"), *value);
    }
});
```
