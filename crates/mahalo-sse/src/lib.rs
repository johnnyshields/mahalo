use std::fmt::Write;
use std::time::Duration;

use local_sync::mpsc::unbounded as mpsc_unbounded;
use mahalo_core::conn::{Conn, KeepAlive, SseKeepAliveKey, SseStream, SseStreamKey};

/// A Server-Sent Event, built with a builder API.
///
/// # Examples
/// ```
/// use mahalo_sse::Event;
/// let event = Event::default().data("hello world");
/// let event = Event::default().data("msg").event("update").id("42");
/// let comment = Event::comment("keep-alive");
/// ```
#[derive(Default)]
pub struct Event {
    data: Option<String>,
    event: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    comment: Option<String>,
}

impl Event {
    /// Set the event data. Multi-line data is split into multiple `data:` lines.
    pub fn data(mut self, value: impl Into<String>) -> Self {
        self.data = Some(value.into());
        self
    }

    /// Set the event data from a serializable value (JSON-encoded).
    pub fn json_data<T: serde::Serialize>(mut self, value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(json) => self.data = Some(json),
            Err(e) => self.data = Some(format!("{{\"error\":\"{e}\"}}")),
        }
        self
    }

    /// Set the event type (maps to `event:` field).
    pub fn event(mut self, value: impl Into<String>) -> Self {
        self.event = Some(value.into());
        self
    }

    /// Set the event ID (maps to `id:` field).
    pub fn id(mut self, value: impl Into<String>) -> Self {
        self.id = Some(value.into());
        self
    }

    /// Set the retry interval (maps to `retry:` field, value in milliseconds).
    pub fn retry(mut self, duration: Duration) -> Self {
        self.retry = Some(duration);
        self
    }

    /// Create a comment-only event (`: text\n`).
    pub fn comment(text: impl Into<String>) -> Self {
        Self {
            comment: Some(text.into()),
            ..Default::default()
        }
    }

    /// Serialize this event to the SSE wire format.
    pub fn serialize(&self) -> String {
        let mut buf = String::new();

        if let Some(ref comment) = self.comment {
            for line in comment.lines() {
                let _ = write!(buf, ": {line}\n");
            }
            if self.data.is_none()
                && self.event.is_none()
                && self.id.is_none()
                && self.retry.is_none()
            {
                buf.push('\n');
                return buf;
            }
        }

        if let Some(ref event) = self.event {
            let _ = write!(buf, "event: {event}\n");
        }

        if let Some(ref id) = self.id {
            let _ = write!(buf, "id: {id}\n");
        }

        if let Some(ref retry) = self.retry {
            let _ = write!(buf, "retry: {}\n", retry.as_millis());
        }

        if let Some(ref data) = self.data {
            if data.is_empty() {
                buf.push_str("data: \n");
            } else {
                for line in data.lines() {
                    let _ = write!(buf, "data: {line}\n");
                }
            }
        }

        buf.push('\n');
        buf
    }
}

/// Sender half of an SSE channel. `!Send` — must stay on the spawning thread.
///
/// Events are serialized to the SSE wire format before being sent through the
/// channel, so the server can write raw strings without knowing about `Event`.
pub struct SseSender {
    tx: mpsc_unbounded::Tx<String>,
}

impl SseSender {
    /// Send an event. Returns `Err` if the receiver (connection) has been dropped.
    pub fn send(&self, event: Event) -> Result<(), SseSendError> {
        self.tx.send(event.serialize()).map_err(|_| SseSendError)
    }
}

/// Error returned when the SSE connection has been closed.
#[derive(Debug)]
pub struct SseSendError;

impl std::fmt::Display for SseSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SSE connection closed")
    }
}

impl std::error::Error for SseSendError {}

/// Options for configuring an SSE response.
#[derive(Default)]
pub struct SseOptions {
    keep_alive: Option<KeepAlive>,
}

impl SseOptions {
    /// Enable keep-alive comments at the given interval.
    pub fn keep_alive(mut self, config: KeepAlive) -> Self {
        self.keep_alive = Some(config);
        self
    }
}

/// Set up an SSE response on the given Conn, returning the Conn and an SseSender.
///
/// Sets `Content-Type: text/event-stream`, `Cache-Control: no-cache`, and stores
/// the `SseStream` receiver in assigns for the server to consume.
pub fn sse_response(conn: Conn, options: SseOptions) -> (Conn, SseSender) {
    let (tx, rx) = mpsc_unbounded::channel();

    let conn = conn
        .put_status(http::StatusCode::OK)
        .put_resp_header_static(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("text/event-stream"),
        )
        .put_resp_header_static(
            http::header::CACHE_CONTROL,
            http::header::HeaderValue::from_static("no-cache"),
        )
        .assign::<SseStreamKey>(SseStream { rx });

    let conn = if let Some(ka) = options.keep_alive {
        conn.assign::<SseKeepAliveKey>(ka)
    } else {
        conn
    };

    (conn, SseSender { tx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_data_only() {
        let s = Event::default().data("hello").serialize();
        assert_eq!(s, "data: hello\n\n");
    }

    #[test]
    fn event_multiline_data() {
        let s = Event::default().data("line1\nline2\nline3").serialize();
        assert_eq!(s, "data: line1\ndata: line2\ndata: line3\n\n");
    }

    #[test]
    fn event_with_type_and_id() {
        let s = Event::default()
            .data("msg")
            .event("update")
            .id("42")
            .serialize();
        assert_eq!(s, "event: update\nid: 42\ndata: msg\n\n");
    }

    #[test]
    fn event_with_retry() {
        let s = Event::default()
            .data("msg")
            .retry(Duration::from_secs(5))
            .serialize();
        assert_eq!(s, "retry: 5000\ndata: msg\n\n");
    }

    #[test]
    fn event_comment_only() {
        let s = Event::comment("keep-alive").serialize();
        assert_eq!(s, ": keep-alive\n\n");
    }

    #[test]
    fn event_all_fields() {
        let s = Event::default()
            .data("payload")
            .event("update")
            .id("99")
            .retry(Duration::from_millis(3000))
            .serialize();
        assert_eq!(
            s,
            "event: update\nid: 99\nretry: 3000\ndata: payload\n\n"
        );
    }

    #[test]
    fn json_data_serialization() {
        let data = serde_json::json!({"key": "value", "num": 42});
        let s = Event::default().json_data(&data).serialize();
        // json_data produces a single-line JSON string
        assert!(s.starts_with("data: {"));
        assert!(s.ends_with("\n\n"));
        assert!(s.contains("\"key\":\"value\""));
    }

    #[test]
    fn sse_response_sets_headers() {
        let conn = Conn::test();
        let (conn, _sender) = sse_response(conn, SseOptions::default());

        assert_eq!(
            conn.resp_headers.get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            conn.resp_headers.get("cache-control").unwrap(),
            "no-cache"
        );
        assert!(conn.get_assign::<SseStreamKey>().is_some());
        assert!(conn.get_assign::<SseKeepAliveKey>().is_none());
    }

    #[test]
    fn sse_response_with_keep_alive() {
        let conn = Conn::test();
        let options = SseOptions::default()
            .keep_alive(KeepAlive::new(Duration::from_secs(15)));
        let (conn, _sender) = sse_response(conn, options);

        let ka = conn.get_assign::<SseKeepAliveKey>().unwrap();
        assert_eq!(ka.interval, Duration::from_secs(15));
        assert_eq!(ka.text, "keep-alive");
    }

    #[test]
    fn keep_alive_custom_text() {
        let ka = KeepAlive::new(Duration::from_secs(10)).text("ping");
        assert_eq!(ka.text, "ping");
        assert_eq!(ka.interval, Duration::from_secs(10));
    }

    #[test]
    fn json_data_serialization_error() {
        use serde::ser::{Serializer, Error as _};

        // A type that always fails serialization.
        struct BadValue;
        impl serde::Serialize for BadValue {
            fn serialize<S: Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(S::Error::custom("intentional failure"))
            }
        }

        // json_data() converts the error into a {"error":"..."} payload
        // rather than panicking.
        let s = Event::default().json_data(&BadValue).serialize();
        assert!(s.starts_with("data: {\"error\":"), "should contain error payload: {s}");
        assert!(s.contains("intentional failure"), "should include error message: {s}");
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn event_empty_data() {
        let s = Event::default().data("").serialize();
        assert_eq!(s, "data: \n\n");
    }

    #[test]
    fn comment_multiline() {
        let s = Event::comment("line1\nline2").serialize();
        assert_eq!(s, ": line1\n: line2\n\n");
    }
}
