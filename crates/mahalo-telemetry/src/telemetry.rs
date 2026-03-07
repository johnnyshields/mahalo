use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::trace;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// A telemetry event carrying hierarchical name, measurements, and metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TelemetryEvent {
    /// Hierarchical event name, e.g. `["mahalo", "endpoint", "stop"]`.
    pub name: Vec<String>,
    /// Numeric measurements such as `duration_ms`.
    pub measurements: HashMap<String, f64>,
    /// Arbitrary metadata attached to the event.
    pub metadata: HashMap<String, serde_json::Value>,
    /// Milliseconds since UNIX epoch when the event was emitted.
    pub timestamp: u64,
}

/// Returns the current time as milliseconds since UNIX epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Built-in event name constants
// ---------------------------------------------------------------------------

/// `["mahalo", "endpoint", "start"]`
pub const ENDPOINT_START: &[&str] = &["mahalo", "endpoint", "start"];
/// `["mahalo", "endpoint", "stop"]`
pub const ENDPOINT_STOP: &[&str] = &["mahalo", "endpoint", "stop"];
/// `["mahalo", "router", "dispatch"]`
pub const ROUTER_DISPATCH: &[&str] = &["mahalo", "router", "dispatch"];
/// `["mahalo", "channel", "join"]`
pub const CHANNEL_JOIN: &[&str] = &["mahalo", "channel", "join"];
/// `["mahalo", "channel", "handle_in"]`
pub const CHANNEL_HANDLE_IN: &[&str] = &["mahalo", "channel", "handle_in"];

/// Helper to convert a `&[&str]` constant into the owned `Vec<String>` form.
pub fn event_name(segments: &[&str]) -> Vec<String> {
    segments.iter().map(|s| (*s).to_string()).collect()
}

// ---------------------------------------------------------------------------
// Handler type
// ---------------------------------------------------------------------------

/// A telemetry handler receives events matching its registered prefix.
type Handler = Rc<dyn Fn(&TelemetryEvent)>;

struct HandlerEntry {
    /// The event name prefix this handler is attached to.
    prefix: Vec<String>,
    handler: Handler,
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

/// The Telemetry system for Mahalo.
///
/// Handlers are invoked inline when events are emitted via `execute()`.
/// Single-threaded — designed for use within one event loop.
///
/// `Telemetry` is `Clone` and can be shared within the same thread.
#[derive(Clone)]
pub struct Telemetry {
    handlers: Rc<RefCell<Vec<HandlerEntry>>>,
}

impl Telemetry {
    /// Create a new Telemetry instance.
    pub fn new(_capacity: usize) -> Self {
        Telemetry {
            handlers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Emit a telemetry event. Attached handlers are invoked inline.
    pub fn execute(
        &self,
        event_name: &[&str],
        measurements: HashMap<String, f64>,
        metadata: HashMap<String, serde_json::Value>,
    ) {
        let event = TelemetryEvent {
            name: event_name.iter().map(|s| (*s).to_string()).collect(),
            measurements,
            metadata,
            timestamp: now_ms(),
        };

        // Invoke matching handlers.
        let handlers = self.handlers.borrow();
        for entry in handlers.iter() {
            if event_matches(&event.name, &entry.prefix) {
                (entry.handler)(&event);
            }
        }
    }

    /// Attach a handler that is called for every event whose name starts with
    /// the given prefix.
    pub fn attach<F>(&self, event_name: &[&str], handler: F)
    where
        F: Fn(&TelemetryEvent) + 'static,
    {
        let prefix: Vec<String> = event_name.iter().map(|s| (*s).to_string()).collect();
        trace!(prefix = ?prefix, "attaching telemetry handler");
        let mut handlers = self.handlers.borrow_mut();
        handlers.push(HandlerEntry {
            prefix,
            handler: Rc::new(handler),
        });
    }

    /// Execute a closure wrapped in start/stop telemetry events.
    ///
    /// Emits `[...event_name, "start"]` before and `[...event_name, "stop"]`
    /// after the closure, with `duration_ms` measured automatically.
    pub async fn span<F, Fut, T>(
        &self,
        event_name: &[&str],
        metadata: HashMap<String, serde_json::Value>,
        f: F,
    ) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let mut start_name: Vec<&str> = event_name.to_vec();
        start_name.push("start");
        let mut stop_name: Vec<&str> = event_name.to_vec();
        stop_name.push("stop");

        // Emit start event.
        self.execute(&start_name, HashMap::new(), metadata.clone());

        let now = Instant::now();
        let result = f().await;
        let duration_ms = now.elapsed().as_secs_f64() * 1000.0;

        // Emit stop event with duration measurement.
        let mut measurements = HashMap::new();
        measurements.insert("duration_ms".to_string(), duration_ms);
        self.execute(&stop_name, measurements, metadata);

        result
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new(512)
    }
}

/// Returns true if `name` starts with `prefix`.
fn event_matches(name: &[String], prefix: &[String]) -> bool {
    if name.len() < prefix.len() {
        return false;
    }
    name[..prefix.len()] == *prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn execute_invokes_handler() {
        let telemetry = Telemetry::new(64);

        let called = Rc::new(Cell::new(false));
        let called_clone = Rc::clone(&called);

        telemetry.attach(ENDPOINT_STOP, move |event| {
            assert_eq!(event.measurements["duration_ms"], 42.0);
            called_clone.set(true);
        });

        let mut measurements = HashMap::new();
        measurements.insert("duration_ms".to_string(), 42.0);

        telemetry.execute(ENDPOINT_STOP, measurements, HashMap::new());

        assert!(called.get());
    }

    #[test]
    fn attach_handler_is_called() {
        let telemetry = Telemetry::new(64);

        let counter = Rc::new(Cell::new(0u32));
        let counter_clone = Rc::clone(&counter);

        telemetry.attach(ENDPOINT_STOP, move |_event| {
            counter_clone.set(counter_clone.get() + 1);
        });

        telemetry.execute(ENDPOINT_STOP, HashMap::new(), HashMap::new());

        assert_eq!(counter.get(), 1);
    }

    #[test]
    fn handler_matches_by_prefix() {
        let telemetry = Telemetry::new(64);

        let counter = Rc::new(Cell::new(0u32));
        let counter_clone = Rc::clone(&counter);

        // Attach to ["mahalo", "endpoint"] -- should match both start and stop.
        telemetry.attach(&["mahalo", "endpoint"], move |_event| {
            counter_clone.set(counter_clone.get() + 1);
        });

        telemetry.execute(ENDPOINT_START, HashMap::new(), HashMap::new());
        telemetry.execute(ENDPOINT_STOP, HashMap::new(), HashMap::new());
        // This should NOT match.
        telemetry.execute(ROUTER_DISPATCH, HashMap::new(), HashMap::new());

        assert_eq!(counter.get(), 2);
    }

    #[tokio::test]
    async fn span_emits_start_and_stop_with_duration() {
        let telemetry = Telemetry::new(64);

        let events = Rc::new(RefCell::new(Vec::new()));
        let events_clone = Rc::clone(&events);

        telemetry.attach(&["mahalo", "endpoint"], move |event| {
            events_clone.borrow_mut().push(event.clone());
        });

        let result = telemetry
            .span(&["mahalo", "endpoint"], HashMap::new(), || async { 42 })
            .await;

        assert_eq!(result, 42);

        let captured = events.borrow();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].name, event_name(&["mahalo", "endpoint", "start"]));
        assert_eq!(captured[1].name, event_name(&["mahalo", "endpoint", "stop"]));
        assert!(captured[1].measurements.contains_key("duration_ms"));
    }

    #[test]
    fn execute_with_no_handlers_does_not_panic() {
        let telemetry = Telemetry::new(64);
        telemetry.execute(ENDPOINT_START, HashMap::new(), HashMap::new());
    }

    #[test]
    fn metadata_is_forwarded() {
        let telemetry = Telemetry::new(64);

        let captured = Rc::new(RefCell::new(None));
        let captured_clone = Rc::clone(&captured);

        telemetry.attach(ENDPOINT_START, move |event| {
            *captured_clone.borrow_mut() = Some(event.clone());
        });

        let mut meta = HashMap::new();
        meta.insert("method".to_string(), serde_json::json!("GET"));
        meta.insert("path".to_string(), serde_json::json!("/api/users"));

        telemetry.execute(ENDPOINT_START, HashMap::new(), meta);

        let event = captured.borrow();
        let event = event.as_ref().unwrap();
        assert_eq!(event.metadata["method"], serde_json::json!("GET"));
        assert_eq!(event.metadata["path"], serde_json::json!("/api/users"));
    }

    #[test]
    fn event_name_helper() {
        assert_eq!(
            event_name(ENDPOINT_START),
            vec!["mahalo".to_string(), "endpoint".to_string(), "start".to_string()]
        );
    }

    #[test]
    fn event_has_timestamp() {
        let telemetry = Telemetry::new(64);

        let captured = Rc::new(RefCell::new(None));
        let captured_clone = Rc::clone(&captured);

        telemetry.attach(ENDPOINT_START, move |event| {
            *captured_clone.borrow_mut() = Some(event.clone());
        });

        let before = now_ms();
        telemetry.execute(ENDPOINT_START, HashMap::new(), HashMap::new());

        let event = captured.borrow();
        let event = event.as_ref().unwrap();
        assert!(event.timestamp >= before);
        assert!(event.timestamp <= now_ms());
    }

    #[test]
    fn telemetry_default() {
        let _telemetry = Telemetry::default();
    }

    #[test]
    fn event_matches_short_name() {
        let name = vec!["a".to_string()];
        let prefix = vec!["a".to_string(), "b".to_string()];
        assert!(!event_matches(&name, &prefix));
    }

    #[test]
    fn now_ms_returns_reasonable_value() {
        let ts = now_ms();
        assert!(ts > 1_700_000_000_000, "now_ms() returned {ts}, expected > 1_700_000_000_000");
    }

    #[tokio::test]
    async fn span_measures_duration() {
        use tokio::time::Duration;

        let telemetry = Telemetry::new(64);

        let events = Rc::new(RefCell::new(Vec::new()));
        let events_clone = Rc::clone(&events);

        telemetry.attach(&["mahalo", "endpoint"], move |event| {
            events_clone.borrow_mut().push(event.clone());
        });

        telemetry
            .span(&["mahalo", "endpoint"], HashMap::new(), || async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            })
            .await;

        let captured = events.borrow();
        let stop = &captured[1];
        let duration = stop.measurements["duration_ms"];
        assert!(duration >= 5.0, "duration_ms should be >= 5ms, got {duration}");
    }
}
