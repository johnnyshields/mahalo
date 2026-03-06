use std::net::SocketAddr;
use std::sync::Arc;

use http::StatusCode;
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{ChildEntry, SupervisorHandle, start_supervisor};
use rebar_core::supervisor::spec::{ChildSpec, RestartStrategy, SupervisorSpec};

use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;

/// A custom error handler that receives a status code and a Conn, and returns a modified Conn.
pub type ErrorHandler = Arc<dyn Fn(StatusCode, Conn) -> Conn + Send + Sync>;

/// Default maximum request body size (2 MB).
pub(crate) const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Bridges MahaloRouter to an HTTP server with rebar supervision support.
///
/// On Linux, uses io_uring for maximum performance. On other platforms
/// (macOS, Windows), falls back to a tokio-based TCP server.
pub struct MahaloEndpoint {
    router: Arc<MahaloRouter>,
    addr: SocketAddr,
    runtime: Arc<Runtime>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Vec<Box<dyn Plug>>,
}

impl MahaloEndpoint {
    pub fn new(router: MahaloRouter, addr: SocketAddr, runtime: Arc<Runtime>) -> Self {
        Self {
            router: Arc::new(router),
            addr,
            runtime,
            error_handler: None,
            after_plugs: Vec::new(),
        }
    }

    /// Set a custom error handler for unmatched routes (404s).
    pub fn error_handler(
        mut self,
        handler: impl Fn(StatusCode, Conn) -> Conn + Send + Sync + 'static,
    ) -> Self {
        self.error_handler = Some(Arc::new(handler));
        self
    }

    /// Add an after-plug that runs after the main handler on every request.
    pub fn after(mut self, plug: impl Plug) -> Self {
        self.after_plugs.push(Box::new(plug));
        self
    }

    /// Start the HTTP server, blocking until shutdown.
    ///
    /// Uses io_uring on Linux, tokio TCP on other platforms.
    pub async fn start(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let addr = self.addr;
        tracing::info!("Mahalo endpoint listening on {}", addr);

        #[cfg(target_os = "linux")]
        {
            crate::worker::start_uring_server(
                addr,
                self.router,
                self.error_handler,
                Arc::new(self.after_plugs),
                self.runtime,
                DEFAULT_BODY_LIMIT,
            )
        }

        #[cfg(not(target_os = "linux"))]
        {
            crate::tcp_server::start_tcp_server(
                addr,
                self.router,
                self.error_handler,
                Arc::new(self.after_plugs),
                self.runtime,
                DEFAULT_BODY_LIMIT,
            )
        }
    }

    /// Create a rebar ChildEntry for supervision.
    pub fn child_entry(self) -> ChildEntry {
        let spec = ChildSpec::new("mahalo_endpoint");
        let addr = self.addr;
        let router = self.router;
        let runtime = self.runtime;
        let error_handler = self.error_handler;
        let after_plugs = Arc::new(self.after_plugs);

        ChildEntry::new(spec, move || {
            let router = Arc::clone(&router);
            let runtime = Arc::clone(&runtime);
            let error_handler = error_handler.clone();
            let after_plugs = Arc::clone(&after_plugs);
            async move {
                tracing::info!("Mahalo endpoint listening on {}", addr);

                #[cfg(target_os = "linux")]
                let result = crate::worker::start_uring_server(
                    addr,
                    router,
                    error_handler,
                    after_plugs,
                    runtime,
                    DEFAULT_BODY_LIMIT,
                );

                #[cfg(not(target_os = "linux"))]
                let result = crate::tcp_server::start_tcp_server(
                    addr,
                    router,
                    error_handler,
                    after_plugs,
                    runtime,
                    DEFAULT_BODY_LIMIT,
                );

                match result {
                    Ok(()) => ExitReason::Normal,
                    Err(e) => ExitReason::Abnormal(format!("endpoint error: {}", e)),
                }
            }
        })
    }

    /// Start the endpoint as a supervised process tree.
    pub async fn start_supervised(self) -> SupervisorHandle {
        let runtime = Arc::clone(&self.runtime);
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let children = vec![self.child_entry()];
        start_supervisor(runtime, spec, children).await
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
        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        let entry = endpoint.child_entry();
        assert_eq!(entry.spec.id, "mahalo_endpoint");
    }

    #[test]
    fn default_404_preserved_without_error_handler() {
        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        assert!(endpoint.error_handler.is_none());
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

    #[tokio::test]
    async fn integration_start_server_and_make_request() {
        use mahalo_core::plug::plug_fn;

        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new()
            .get(
                "/health",
                plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK)
                        .put_resp_body("ok")
                }),
            )
            .post(
                "/echo",
                plug_fn(|conn: Conn| async {
                    let body = conn.body.clone();
                    conn.put_status(StatusCode::OK).put_resp_body(body)
                }),
            );

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        // For integration tests, we need to find a free port and start the server
        let listener = std::net::TcpListener::bind(addr).unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let endpoint = MahaloEndpoint::new(router, bound_addr, runtime);

        // Spawn server in background thread (io_uring blocks)
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(endpoint.start()).unwrap();
        });

        // Give the server a moment to bind
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let base = format!("http://{bound_addr}");

        // Test GET /health
        let resp = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");

        // Test POST /echo
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/echo"))
            .body("hello mahalo")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "hello mahalo");

        // Test 404
        let resp = reqwest::get(format!("{base}/nonexistent")).await.unwrap();
        assert_eq!(resp.status(), 404);

        drop(server);
    }

    #[tokio::test]
    async fn after_plugs_execute_post_handler() {
        use mahalo_core::plug::plug_fn;

        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new().get(
            "/test",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK).put_resp_body("ok")
            }),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let endpoint = MahaloEndpoint::new(router, addr, runtime)
            .after(plug_fn(|conn: Conn| async {
                conn.put_resp_header("x-after", "applied")
            }));

        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(endpoint.start()).unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let base = format!("http://{addr}");
        let resp = reqwest::get(format!("{base}/test")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("x-after").unwrap(), "applied");
        assert_eq!(resp.text().await.unwrap(), "ok");

        drop(server);
    }

    #[tokio::test]
    async fn after_plugs_respect_halted_conn() {
        use mahalo_core::plug::plug_fn;

        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new().get(
            "/halted",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body("halted")
                    .halt()
            }),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let endpoint = MahaloEndpoint::new(router, addr, runtime)
            .after(plug_fn(|conn: Conn| async {
                conn.put_resp_header("x-should-not-run", "true")
            }));

        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(endpoint.start()).unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let base = format!("http://{addr}");
        let resp = reqwest::get(format!("{base}/halted")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-should-not-run").is_none());
        assert_eq!(resp.text().await.unwrap(), "halted");

        drop(server);
    }
}
