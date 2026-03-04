use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri};
use rebar_core::runtime::Runtime;

/// Typed key for the assigns map.
pub trait AssignKey: Send + Sync + 'static {
    type Value: Send + Sync + 'static;
}

/// Central request + response struct, inspired by Phoenix's `Plug.Conn`.
pub struct Conn {
    // Request
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub path_params: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    pub remote_addr: Option<SocketAddr>,
    pub body: Bytes,

    // Response
    pub status: StatusCode,
    pub resp_headers: HeaderMap,
    pub resp_body: Bytes,

    // State
    pub halted: bool,
    assigns: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    pub runtime: Option<Arc<Runtime>>,
}

impl Conn {
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            remote_addr: None,
            body: Bytes::new(),
            status: StatusCode::OK,
            resp_headers: HeaderMap::new(),
            resp_body: Bytes::new(),
            halted: false,
            assigns: HashMap::new(),
            runtime: None,
        }
    }

    pub fn put_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

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

    pub fn put_resp_body(mut self, body: impl Into<Bytes>) -> Self {
        self.resp_body = body.into();
        self
    }

    pub fn halt(mut self) -> Self {
        self.halted = true;
        self
    }

    pub fn assign<K: AssignKey>(mut self, value: K::Value) -> Self {
        self.assigns.insert(TypeId::of::<K>(), Box::new(value));
        self
    }

    pub fn get_assign<K: AssignKey>(&self) -> Option<&K::Value> {
        self.assigns
            .get(&TypeId::of::<K>())
            .and_then(|v| v.downcast_ref())
    }

    pub fn with_runtime(mut self, runtime: Arc<Runtime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Parse the query string from the URI into `query_params`.
    pub fn parse_query_params(&mut self) {
        if let Some(query) = self.uri.query() {
            self.query_params = query
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    let key = parts.next()?;
                    let value = parts.next().unwrap_or("");
                    Some((key.to_string(), value.to_string()))
                })
                .collect();
        }
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
    fn parse_query_params_works() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/search?q=rust&page=2"));
        conn.parse_query_params();
        assert_eq!(conn.query_params.get("q").unwrap(), "rust");
        assert_eq!(conn.query_params.get("page").unwrap(), "2");
    }

    #[test]
    fn parse_query_params_empty() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/search"));
        conn.parse_query_params();
        assert!(conn.query_params.is_empty());
    }
}
