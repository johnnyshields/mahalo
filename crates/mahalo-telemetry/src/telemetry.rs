use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, RwLock};
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
type Handler = Arc<dyn Fn(&TelemetryEvent) + Send + Sync>;

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
/// Uses its own `broadcast` channel (separate from rebar's `EventBus` which
/// is typed to `LifecycleEvent`) so that application-level telemetry events
/// can flow independently of runtime lifecycle events.
///
/// `Telemetry` is `Clone + Send + Sync` and can be shared across handlers.
#[derive(Clone)]
pub struct Telemetry {
    tx: broadcast::Sender<TelemetryEvent>,
    handlers: Arc<RwLock<Vec<HandlerEntry>>>,
}

impl Telemetry {
    /// Create a new Telemetry instance with the given broadcast channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Telemetry {
            tx,
            handlers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Subscribe to the raw event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryEvent> {
        self.tx.subscribe()
    }

    /// Emit a telemetry event synchronously. Attached handlers are invoked
    /// inline, and the event is also sent on the broadcast channel.
    pub async fn execute(
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
        {
            let handlers = self.handlers.read().await;
            for entry in handlers.iter() {
                if event_matches(&event.name, &entry.prefix) {
                    (entry.handler)(&event);
                }
            }
        }

        // Broadcast to subscribers (no receivers is normal for telemetry).
        let _ = self.tx.send(event);
    }

    /// Attach a handler that is called for every event whose name starts with
    /// the given prefix.
    pub async fn attach<F>(&self, event_name: &[&str], handler: F)
    where
        F: Fn(&TelemetryEvent) + Send + Sync + 'static,
    {
        let prefix: Vec<String> = event_name.iter().map(|s| (*s).to_string()).collect();
        trace!(prefix = ?prefix, "attaching telemetry handler");
        let mut handlers = self.handlers.write().await;
        handlers.push(HandlerEntry {
            prefix,
            handler: Arc::new(handler),
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
        self.execute(&start_name, HashMap::new(), metadata.clone())
            .await;

        let now = Instant::now();
        let result = f().await;
        let duration_ms = now.elapsed().as_secs_f64() * 1000.0;

        // Emit stop event with duration measurement.
        let mut measurements = HashMap::new();
        measurements.insert("duration_ms".to_string(), duration_ms);
        self.execute(&stop_name, measurements, metadata).await;

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
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn execute_sends_to_broadcast() {
        let telemetry = Telemetry::new(64);
        let mut rx = telemetry.subscribe();

        let mut measurements = HashMap::new();
        measurements.insert("duration_ms".to_string(), 42.0);

        telemetry
            .execute(ENDPOINT_STOP, measurements, HashMap::new())
            .await;

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(event.name, event_name(ENDPOINT_STOP));
        assert_eq!(event.measurements["duration_ms"], 42.0);
    }

    #[tokio::test]
    async fn attach_handler_is_called() {
        let telemetry = Telemetry::new(64);

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);

        telemetry
            .attach(ENDPOINT_STOP, move |_event| {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        telemetry
            .execute(ENDPOINT_STOP, HashMap::new(), HashMap::new())
            .await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn handler_matches_by_prefix() {
        let telemetry = Telemetry::new(64);

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);

        // Attach to ["mahalo", "endpoint"] -- should match both start and stop.
        telemetry
            .attach(&["mahalo", "endpoint"], move |_event| {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        telemetry
            .execute(ENDPOINT_START, HashMap::new(), HashMap::new())
            .await;
        telemetry
            .execute(ENDPOINT_STOP, HashMap::new(), HashMap::new())
            .await;
        // This should NOT match.
        telemetry
            .execute(ROUTER_DISPATCH, HashMap::new(), HashMap::new())
            .await;

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn span_emits_start_and_stop_with_duration() {
        let telemetry = Telemetry::new(64);
        let mut rx = telemetry.subscribe();

        let result = telemetry
            .span(&["mahalo", "endpoint"], HashMap::new(), || async { 42 })
            .await;

        assert_eq!(result, 42);

        // Should receive start event first.
        let start = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(
            start.name,
            event_name(&["mahalo", "endpoint", "start"])
        );

        // Then stop event with duration_ms.
        let stop = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(
            stop.name,
            event_name(&["mahalo", "endpoint", "stop"])
        );
        assert!(stop.measurements.contains_key("duration_ms"));
    }

    #[tokio::test]
    async fn execute_with_no_handlers_does_not_panic() {
        let telemetry = Telemetry::new(64);
        telemetry
            .execute(ENDPOINT_START, HashMap::new(), HashMap::new())
            .await;
    }

    #[tokio::test]
    async fn metadata_is_forwarded() {
        let telemetry = Telemetry::new(64);
        let mut rx = telemetry.subscribe();

        let mut meta = HashMap::new();
        meta.insert("method".to_string(), serde_json::json!("GET"));
        meta.insert("path".to_string(), serde_json::json!("/api/users"));

        telemetry
            .execute(ENDPOINT_START, HashMap::new(), meta)
            .await;

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert_eq!(event.metadata["method"], serde_json::json!("GET"));
        assert_eq!(event.metadata["path"], serde_json::json!("/api/users"));
    }

    #[tokio::test]
    async fn telemetry_is_clone_send_sync() {
        fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
        assert_clone_send_sync::<Telemetry>();
    }

    #[tokio::test]
    async fn event_name_helper() {
        assert_eq!(
            event_name(ENDPOINT_START),
            vec!["mahalo".to_string(), "endpoint".to_string(), "start".to_string()]
        );
    }

    #[tokio::test]
    async fn event_has_timestamp() {
        let telemetry = Telemetry::new(64);
        let mut rx = telemetry.subscribe();

        let before = now_ms();
        telemetry
            .execute(ENDPOINT_START, HashMap::new(), HashMap::new())
            .await;

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        assert!(event.timestamp >= before);
        assert!(event.timestamp <= now_ms());
    }

    #[tokio::test]
    async fn telemetry_default() {
        let telemetry = Telemetry::default();
        let _rx = telemetry.subscribe();
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
        let telemetry = Telemetry::new(64);
        let mut rx = telemetry.subscribe();

        telemetry
            .span(&["mahalo", "endpoint"], HashMap::new(), || async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            })
            .await;

        // Skip start event.
        let _ = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        let stop = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");

        let duration = stop.measurements["duration_ms"];
        assert!(duration >= 5.0, "duration_ms should be >= 5ms, got {duration}");
    }
}
