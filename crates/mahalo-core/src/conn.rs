use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri};
use local_sync::mpsc::unbounded as mpsc_unbounded;
use percent_encoding::percent_decode_str;
use rebar_core::runtime::Runtime;
use smallvec::SmallVec;

/// Typed key for the assigns map.
pub trait AssignKey: 'static {
    type Value: 'static;
}

/// Inline key-value pair for path parameters. Avoids HashMap allocation for
/// the common case of 0-4 path parameters.
pub type PathParams = SmallVec<[(&'static str, SmallVec<[u8; 32]>); 4]>;

/// Central request + response struct, inspired by Phoenix's `Plug.Conn`.
pub struct Conn {
    // Request
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub path_params: PathParams,
    pub query_params: Option<HashMap<String, String>>,
    pub remote_addr: Option<SocketAddr>,
    pub body: Bytes,

    // Response
    pub status: StatusCode,
    pub resp_headers: HeaderMap,
    pub resp_body: Bytes,

    // State
    pub halted: bool,
    assigns: Option<HashMap<TypeId, Box<dyn Any>>>,
    pub runtime: Option<Rc<Runtime>>,
}

impl Conn {
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            path_params: SmallVec::new(),
            query_params: None,
            remote_addr: None,
            body: Bytes::new(),
            status: StatusCode::OK,
            resp_headers: HeaderMap::new(),
            resp_body: Bytes::new(),
            halted: false,
            assigns: None,
            runtime: None,
        }
    }

    /// Reset this Conn for reuse, preserving backing allocations.
    /// Clears all fields but keeps HashMap/HeaderMap capacity.
    /// The runtime Arc is preserved (not dropped/re-cloned) since it
    /// typically stays the same across keep-alive requests.
    pub fn reset(&mut self, method: Method, uri: Uri) {
        self.method = method;
        self.uri = uri;
        self.headers.clear();
        self.path_params.clear();
        if let Some(ref mut qp) = self.query_params {
            qp.clear();
        }
        self.remote_addr = None;
        self.body = Bytes::new();
        self.status = StatusCode::OK;
        self.resp_headers.clear();
        self.resp_body = Bytes::new();
        self.halted = false;
        if let Some(ref mut assigns) = self.assigns {
            assigns.clear();
        }
        // Intentionally preserve self.runtime — it's the same across keep-alive
        // requests on the same worker thread, avoiding an Arc drop + clone cycle.
    }

    /// Get a path parameter by name. O(n) scan over the SmallVec, but
    /// n is typically 0-4 so this is faster than HashMap lookup.
    #[inline]
    pub fn path_param(&self, name: &str) -> Option<&str> {
        for (k, v) in &self.path_params {
            if *k == name {
                return std::str::from_utf8(v).ok();
            }
        }
        None
    }

    #[inline]
    pub fn put_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    #[inline]
    pub fn put_resp_header(
        mut self,
        key: impl TryInto<http::header::HeaderName>,
        value: impl TryInto<http::header::HeaderValue>,
    ) -> Self {
        if let (Ok(k), Ok(v)) = (key.try_into(), value.try_into()) {
            self.resp_headers.insert(k, v);
        }
        self
    }

    /// Fast path for setting response headers with pre-validated HeaderName constants.
    /// Use with `http::header::CONTENT_TYPE`, `http::header::CACHE_CONTROL`, etc.
    #[inline]
    pub fn put_resp_header_static(
        mut self,
        key: http::header::HeaderName,
        value: http::header::HeaderValue,
    ) -> Self {
        self.resp_headers.insert(key, value);
        self
    }

    #[inline]
    pub fn put_resp_body(mut self, body: impl Into<Bytes>) -> Self {
        self.resp_body = body.into();
        self
    }

    /// Zero-copy static body — avoids atomic ref-count overhead of Bytes::from().
    #[inline]
    pub fn put_resp_body_static(mut self, body: &'static [u8]) -> Self {
        self.resp_body = Bytes::from_static(body);
        self
    }

    #[inline]
    pub fn halt(mut self) -> Self {
        self.halted = true;
        self
    }

    pub fn assign<K: AssignKey>(mut self, value: K::Value) -> Self {
        self.assigns
            .get_or_insert_with(HashMap::new)
            .insert(TypeId::of::<K>(), Box::new(value));
        self
    }

    pub fn get_assign<K: AssignKey>(&self) -> Option<&K::Value> {
        self.assigns
            .as_ref()?
            .get(&TypeId::of::<K>())
            .and_then(|v| v.downcast_ref())
    }

    pub fn take_assign<K: AssignKey>(&mut self) -> Option<K::Value> {
        self.assigns
            .as_mut()?
            .remove(&TypeId::of::<K>())
            .and_then(|v| v.downcast().ok().map(|b| *b))
    }

    #[inline]
    pub fn with_runtime(mut self, runtime: Rc<Runtime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Create a test Conn with GET / defaults.
    pub fn test() -> Self {
        Self::new(Method::GET, Uri::from_static("/"))
    }

    /// Get a response header value as a &str, for convenience in tests.
    pub fn get_resp_header(&self, name: &str) -> Option<&str> {
        self.resp_headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Parse the query string from the URI into `query_params`.
    /// Keys and values are percent-decoded. Lazily allocates the HashMap.
    pub fn parse_query_params(&mut self) {
        if let Some(query) = self.uri.query() {
            let map: HashMap<String, String> = query
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    let key = parts.next()?;
                    let value = parts.next().unwrap_or("");
                    let key = percent_decode_str(key).decode_utf8_lossy().into_owned();
                    let value = percent_decode_str(value).decode_utf8_lossy().into_owned();
                    Some((key, value))
                })
                .collect();
            self.query_params = Some(map);
        }
    }

    /// Get a query parameter by name, returning None if query params haven't been parsed yet.
    #[inline]
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query_params.as_ref()?.get(name).map(|s| s.as_str())
    }
}

// --- SSE types (minimal, lives in core to avoid circular deps) ---

/// Assign key for SSE stream receiver.
pub struct SseStreamKey;
impl AssignKey for SseStreamKey {
    type Value = SseStream;
}

/// Assign key for SSE keep-alive config.
pub struct SseKeepAliveKey;
impl AssignKey for SseKeepAliveKey {
    type Value = KeepAlive;
}

/// Receiver half of SSE channel. Events are pre-serialized to SSE wire format strings.
pub struct SseStream {
    pub rx: mpsc_unbounded::Rx<String>,
}

/// Keep-alive configuration for SSE connections.
pub struct KeepAlive {
    pub interval: Duration,
    pub text: String,
}

impl KeepAlive {
    /// Create a new keep-alive config.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            text: "keep-alive".to_string(),
        }
    }

    /// Set custom keep-alive comment text.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conn_defaults() {
        let conn = Conn::new(Method::GET, Uri::from_static("/hello"));
        assert_eq!(conn.method, Method::GET);
        assert_eq!(conn.uri, "/hello");
        assert_eq!(conn.status, StatusCode::OK);
        assert!(!conn.halted);
        assert!(conn.resp_body.is_empty());
    }

    #[test]
    fn builder_methods() {
        let conn = Conn::new(Method::POST, Uri::from_static("/api"))
            .put_status(StatusCode::CREATED)
            .put_resp_header("content-type", "application/json")
            .put_resp_body(r#"{"ok":true}"#);

        assert_eq!(conn.status, StatusCode::CREATED);
        assert_eq!(conn.resp_headers.get("content-type").unwrap(), "application/json");
        assert_eq!(conn.resp_body, r#"{"ok":true}"#);
    }

    #[test]
    fn halt_sets_flag() {
        let conn = Conn::new(Method::GET, Uri::from_static("/")).halt();
        assert!(conn.halted);
    }

    struct UserId;
    impl AssignKey for UserId {
        type Value = u64;
    }

    struct UserName;
    impl AssignKey for UserName {
        type Value = String;
    }

    #[test]
    fn assigns_typed() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .assign::<UserId>(42)
            .assign::<UserName>("alice".to_string());

        assert_eq!(conn.get_assign::<UserId>(), Some(&42));
        assert_eq!(conn.get_assign::<UserName>(), Some(&"alice".to_string()));
    }

    #[test]
    fn get_missing_assign_returns_none() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        assert_eq!(conn.get_assign::<UserId>(), None);
    }

    #[test]
    fn take_assign_removes_value() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"))
            .assign::<UserId>(42)
            .assign::<UserName>("alice".to_string());

        assert_eq!(conn.take_assign::<UserId>(), Some(42));
        assert_eq!(conn.get_assign::<UserId>(), None);
        assert_eq!(conn.get_assign::<UserName>(), Some(&"alice".to_string()));
    }

    #[test]
    fn take_assign_missing_returns_none() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        assert_eq!(conn.take_assign::<UserId>(), None);
    }

    #[test]
    fn parse_query_params_works() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/search?q=rust&page=2"));
        conn.parse_query_params();
        assert_eq!(conn.query_param("q").unwrap(), "rust");
        assert_eq!(conn.query_param("page").unwrap(), "2");
    }

    #[test]
    fn parse_query_params_empty() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/search"));
        conn.parse_query_params();
        assert!(conn.query_params.is_none());
    }

    #[test]
    fn parse_query_params_url_decoded() {
        let mut conn = Conn::new(
            Method::GET,
            "/search?name=hello%20world&city=S%C3%A3o%20Paulo".parse::<Uri>().unwrap(),
        );
        conn.parse_query_params();
        assert_eq!(conn.query_param("name").unwrap(), "hello world");
        assert_eq!(conn.query_param("city").unwrap(), "São Paulo");
    }

    #[test]
    fn parse_query_params_plus_not_decoded_as_space() {
        let mut conn = Conn::new(
            Method::GET,
            "/search?q=a+b".parse::<Uri>().unwrap(),
        );
        conn.parse_query_params();
        // percent-encoding does not decode '+' as space (that's form-urlencoded)
        assert_eq!(conn.query_param("q").unwrap(), "a+b");
    }

    #[test]
    fn with_runtime() {
        let runtime = Rc::new(Runtime::new(1));
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .with_runtime(runtime);
        assert!(conn.runtime.is_some());
    }

    #[test]
    fn put_resp_header_invalid() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_resp_header("", "value");
        assert!(conn.resp_headers.is_empty());
    }

    #[test]
    fn reset_clears_all_fields() {
        let conn = Conn::new(Method::POST, "/api".parse::<Uri>().unwrap())
            .put_status(StatusCode::CREATED)
            .put_resp_header("content-type", "application/json")
            .put_resp_body("body")
            .assign::<UserId>(42)
            .halt();

        let mut conn = conn;
        conn.headers.insert("host", "example.com".parse().unwrap());
        conn.path_params.push(("id", SmallVec::from_slice(b"1")));
        conn.query_params = Some(HashMap::from([("q".into(), "test".into())]));
        conn.remote_addr = Some("1.2.3.4:80".parse().unwrap());

        conn.reset(Method::GET, Uri::from_static("/new"));

        assert_eq!(conn.method, Method::GET);
        assert_eq!(conn.uri, "/new");
        assert!(conn.headers.is_empty());
        assert!(conn.path_params.is_empty());
        assert!(conn.query_params.as_ref().map_or(true, |qp| qp.is_empty()));
        assert!(conn.remote_addr.is_none());
        assert!(conn.body.is_empty());
        assert_eq!(conn.status, StatusCode::OK);
        assert!(conn.resp_headers.is_empty());
        assert!(conn.resp_body.is_empty());
        assert!(!conn.halted);
        assert!(conn.get_assign::<UserId>().is_none());
    }

    #[test]
    fn put_resp_body_static_zero_copy() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_resp_body_static(b"hello static");
        assert_eq!(conn.resp_body, &b"hello static"[..]);
        // Bytes::from_static does not allocate — the ptr points into the binary's
        // read-only data segment, not the heap.
        assert_eq!(conn.resp_body.len(), 12);
    }

    #[test]
    fn reset_preserves_capacity() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        // Add enough headers to force capacity growth.
        for i in 0..20 {
            conn.headers.insert(
                format!("x-header-{i}").parse::<http::header::HeaderName>().unwrap(),
                "value".parse().unwrap(),
            );
        }
        let cap_before = conn.headers.capacity();

        conn.reset(Method::POST, Uri::from_static("/other"));

        // HeaderMap capacity should be preserved after clear.
        assert!(conn.headers.capacity() >= cap_before);
        assert!(conn.headers.is_empty());
    }
}
