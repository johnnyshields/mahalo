use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use http::StatusCode;
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{ChildEntry, SupervisorHandle, start_supervisor};
use rebar_core::supervisor::spec::{ChildSpec, RestartStrategy, SupervisorSpec};

use mahalo_channel::socket::ChannelRouter;
use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_pubsub::PubSub;
use mahalo_router::MahaloRouter;

/// A custom error handler that receives a status code and a Conn, and returns a modified Conn.
///
/// Uses `Rc` because it is thread-local (one per worker thread).
pub type ErrorHandler = Rc<dyn Fn(StatusCode, Conn) -> Conn>;

/// Default maximum request body size (2 MB).
pub(crate) const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Bundles WebSocket channel configuration (channel router + PubSub).
///
/// Thread-local (created per worker via factory). Uses `Rc` because
/// ChannelRouter contains `Rc<dyn Channel>`.
pub struct WsConfig {
    pub channel_router: Rc<ChannelRouter>,
    pub pubsub: PubSub,
}

/// Bridges MahaloRouter to an HTTP server with rebar supervision support.
///
/// Uses a factory pattern: each worker OS thread calls the factories to create
/// thread-local (`!Send`) instances of the router, error handler, and after-plugs.
/// The factories themselves are `Send + Sync` so they can be shared via `Arc`.
pub struct MahaloEndpoint {
    router_factory: Arc<dyn Fn() -> MahaloRouter + Send + Sync>,
    addr: SocketAddr,
    error_handler_factory: Option<Arc<dyn Fn() -> ErrorHandler + Send + Sync>>,
    after_plug_factories: Vec<Arc<dyn Fn() -> Box<dyn Plug> + Send + Sync>>,
    ws_config_factory: Option<Arc<dyn Fn() -> WsConfig + Send + Sync>>,
}

impl MahaloEndpoint {
    pub fn new(
        router_factory: impl Fn() -> MahaloRouter + Send + Sync + 'static,
        addr: SocketAddr,
    ) -> Self {
        Self {
            router_factory: Arc::new(router_factory),
            addr,
            error_handler_factory: None,
            after_plug_factories: Vec::new(),
            ws_config_factory: None,
        }
    }

    /// Set a custom error handler for unmatched routes (404s).
    ///
    /// The closure is `Send + Sync` (plain function), wrapped into a per-thread `Rc`
    /// factory automatically.
    pub fn error_handler(
        mut self,
        handler: impl Fn(StatusCode, Conn) -> Conn + Send + Sync + 'static,
    ) -> Self {
        let handler = Arc::new(handler);
        self.error_handler_factory = Some(Arc::new(move || {
            let h = handler.clone();
            Rc::new(move |status, conn| h(status, conn))
        }));
        self
    }

    /// Configure WebSocket channel support via a factory.
    ///
    /// The factory is called on each worker thread to create a thread-local
    /// `WsConfig` (ChannelRouter contains `Rc<dyn Channel>`, so it is `!Send`).
    pub fn channels(
        mut self,
        factory: impl Fn() -> WsConfig + Send + Sync + 'static,
    ) -> Self {
        self.ws_config_factory = Some(Arc::new(factory));
        self
    }

    /// Add an after-plug factory that produces a plug for each worker thread.
    pub fn after(mut self, factory: impl Fn() -> Box<dyn Plug> + Send + Sync + 'static) -> Self {
        self.after_plug_factories.push(Arc::new(factory));
        self
    }

    /// Start the HTTP server, blocking until shutdown.
    pub fn start(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let addr = self.addr;
        tracing::info!("Mahalo endpoint listening on {}", addr);

        crate::worker::start_server(
            addr,
            self.router_factory,
            self.error_handler_factory,
            self.after_plug_factories,
            DEFAULT_BODY_LIMIT,
            self.ws_config_factory,
        )
    }

    /// Create a rebar ChildEntry for supervision.
    pub fn child_entry(self) -> ChildEntry {
        let spec = ChildSpec::new("mahalo_endpoint");
        let addr = self.addr;
        let router_factory = self.router_factory;
        let error_handler_factory = self.error_handler_factory;
        let after_plug_factories = Arc::new(self.after_plug_factories);
        let ws_config_factory = self.ws_config_factory;

        ChildEntry::new(spec, move || {
            let router_factory = Arc::clone(&router_factory);
            let error_handler_factory = error_handler_factory.clone();
            let after_plug_factories = Arc::clone(&after_plug_factories);
            let ws_config_factory = ws_config_factory.clone();

            async move {
                tracing::info!("Mahalo endpoint listening on {}", addr);

                let spawn_result = crate::worker::spawn_workers(
                    addr,
                    router_factory,
                    error_handler_factory,
                    after_plug_factories,
                    DEFAULT_BODY_LIMIT,
                    ws_config_factory,
                );

                match spawn_result {
                    Ok(()) => {
                        // Workers are running. Park forever — supervisor controls our lifetime.
                        std::future::pending::<()>().await;
                        ExitReason::Normal
                    }
                    Err(e) => ExitReason::Abnormal(format!("endpoint error: {}", e)),
                }
            }
        })
    }

    /// Start the endpoint as a supervised process tree.
    pub fn start_supervised(self) -> SupervisorHandle {
        let runtime = Runtime::new(1);
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let children = vec![self.child_entry()];
        start_supervisor(&runtime, spec, children)
    }
}

/// Returns an error handler that produces JSON responses like `{"error":"Not Found","status":404}`.
pub fn json_error_handler() -> impl Fn(StatusCode, Conn) -> Conn + Send + Sync {
    move |status: StatusCode, conn: Conn| {
        let reason = status.canonical_reason().unwrap_or("Unknown");
        let body = format!(r#"{{"error":"{}","status":{}}}"#, reason, status.as_u16());
        conn.put_resp_header("content-type", "application/json")
            .put_resp_body(body)
    }
}

/// Returns an error handler that produces plain text responses like `"404 Not Found"`.
pub fn text_error_handler() -> impl Fn(StatusCode, Conn) -> Conn + Send + Sync {
    move |status: StatusCode, conn: Conn| {
        let reason = status.canonical_reason().unwrap_or("Unknown");
        let body = format!("{} {}", status.as_u16(), reason);
        conn.put_resp_body(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::Method;

    #[test]
    fn child_entry_creates_entry() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(MahaloRouter::new, addr);
        let entry = endpoint.child_entry();
        assert_eq!(entry.spec.id, "mahalo_endpoint");
    }

    #[test]
    fn default_404_preserved_without_error_handler() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(MahaloRouter::new, addr);
        assert!(endpoint.error_handler_factory.is_none());
    }

    #[test]
    fn custom_error_handler_called_for_404() {
        let handler = |status: StatusCode, conn: Conn| -> Conn {
            conn.put_resp_body(format!("Custom: {}", status.as_u16()))
        };
        let conn = Conn::new(Method::GET, http::Uri::from_static("/missing"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = handler(StatusCode::NOT_FOUND, conn);
        assert_eq!(conn.resp_body, Bytes::from("Custom: 404"));
    }

    #[test]
    fn json_error_handler_produces_correct_json() {
        let handler = json_error_handler();
        let conn = Conn::new(Method::GET, http::Uri::from_static("/"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = handler(StatusCode::NOT_FOUND, conn);
        assert_eq!(
            conn.resp_body,
            Bytes::from(r#"{"error":"Not Found","status":404}"#)
        );
        assert_eq!(
            conn.resp_headers.get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn text_error_handler_produces_correct_text() {
        let handler = text_error_handler();
        let conn = Conn::new(Method::GET, http::Uri::from_static("/"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = handler(StatusCode::NOT_FOUND, conn);
        assert_eq!(conn.resp_body, Bytes::from("404 Not Found"));
    }
}
