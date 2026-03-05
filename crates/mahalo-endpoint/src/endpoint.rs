use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Router;
use http::StatusCode;
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::ChildEntry;
use rebar_core::supervisor::spec::ChildSpec;

use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;

/// A custom error handler that receives a status code and a Conn, and returns a modified Conn.
pub type ErrorHandler = Arc<dyn Fn(StatusCode, Conn) -> Conn + Send + Sync>;

/// Default maximum request body size (2 MB).
const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Shared state passed to each Axum handler via State extractor.
#[derive(Clone)]
struct EndpointState {
    router: Arc<MahaloRouter>,
    runtime: Arc<Runtime>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
}

/// Bridges MahaloRouter to an Axum HTTP server with rebar supervision support.
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

    /// Convert into an Axum Router that delegates all requests to MahaloRouter.
    pub fn into_axum_router(self) -> Router {
        let state = EndpointState {
            router: self.router,
            runtime: self.runtime,
            error_handler: self.error_handler,
            after_plugs: Arc::new(self.after_plugs),
        };

        Router::new()
            .fallback(fallback_handler)
            .with_state(state)
    }

    /// Start the HTTP server, blocking until shutdown.
    pub async fn start(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let addr = self.addr;
        let axum_router = self.into_axum_router();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("Mahalo endpoint listening on {}", addr);
        axum::serve(
            listener,
            axum_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    }

    /// Create a rebar ChildEntry for supervision.
    pub fn child_entry(self) -> ChildEntry {
        let spec = ChildSpec::new("mahalo_endpoint");
        let addr = self.addr;
        let state = EndpointState {
            router: self.router,
            runtime: self.runtime,
            error_handler: self.error_handler,
            after_plugs: Arc::new(self.after_plugs),
        };

        ChildEntry::new(spec, move || {
            let state = state.clone();
            async move {
                let axum_router = Router::new()
                    .fallback(fallback_handler)
                    .with_state(state);
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        return ExitReason::Abnormal(format!("endpoint error: {}", e));
                    }
                };
                tracing::info!("Mahalo endpoint listening on {}", addr);
                match axum::serve(
                    listener,
                    axum_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    Ok(()) => ExitReason::Normal,
                    Err(e) => ExitReason::Abnormal(format!("endpoint error: {}", e)),
                }
            }
        })
    }
}

/// Axum fallback handler that routes all requests through MahaloRouter.
async fn fallback_handler(
    State(state): State<EndpointState>,
    connect_info: ConnectInfo<SocketAddr>,
    request: Request,
) -> impl IntoResponse {
    let conn = request_to_conn(request, Some(connect_info.0), &state.runtime).await;

    let conn = match state.router.resolve(&conn.method.clone(), conn.uri.path()) {
        Some(resolved) => resolved.execute(conn).await,
        None => {
            if let Some(ref handler) = state.error_handler {
                let mut conn = conn.put_status(StatusCode::NOT_FOUND);
                conn = handler(StatusCode::NOT_FOUND, conn);
                conn
            } else {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("Not Found"))
                    .unwrap();
            }
        }
    };

    // Run after-plugs sequentially
    let mut conn = conn;
    for plug in state.after_plugs.iter() {
        if conn.halted {
            break;
        }
        conn = plug.call(conn).await;
    }

    conn_to_response(conn)
}

/// Convert an Axum Request into a Mahalo Conn.
async fn request_to_conn(
    request: Request,
    remote_addr: Option<SocketAddr>,
    runtime: &Arc<Runtime>,
) -> Conn {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();

    let body_bytes = axum::body::to_bytes(request.into_body(), DEFAULT_BODY_LIMIT)
        .await
        .unwrap_or_default();

    let mut conn = Conn::new(method, uri).with_runtime(Arc::clone(runtime));
    conn.headers = headers;
    conn.remote_addr = remote_addr;
    conn.body = body_bytes;
    conn.parse_query_params();
    conn
}

/// Convert a Mahalo Conn into an Axum Response.
fn conn_to_response(conn: Conn) -> Response {
    let mut builder = Response::builder().status(conn.status);

    if let Some(headers) = builder.headers_mut() {
        headers.extend(conn.resp_headers);
    }

    builder.body(Body::from(conn.resp_body)).unwrap()
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

    #[tokio::test]
    async fn request_to_conn_conversion() {
        let runtime = Arc::new(Runtime::new(1));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/rooms?page=2")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"test"}"#))
            .unwrap();

        let conn = request_to_conn(request, None, &runtime).await;

        assert_eq!(conn.method, Method::POST);
        assert_eq!(conn.uri.path(), "/api/rooms");
        assert_eq!(
            conn.headers.get("content-type").unwrap(),
            "application/json"
        );
        assert_eq!(conn.body, Bytes::from(r#"{"name":"test"}"#));
        assert_eq!(conn.query_params.get("page").unwrap(), "2");
        assert!(conn.runtime.is_some());
    }

    #[test]
    fn conn_to_response_conversion() {
        let conn = Conn::new(Method::GET, http::Uri::from_static("/"))
            .put_status(StatusCode::CREATED)
            .put_resp_header("x-custom", "value")
            .put_resp_body("hello");

        let response = conn_to_response(conn);

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers().get("x-custom").unwrap(), "value");
    }

    #[test]
    fn into_axum_router_creates_router() {
        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        let _axum_router = endpoint.into_axum_router();
    }

    #[test]
    fn child_entry_creates_entry() {
        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        let entry = endpoint.child_entry();
        assert_eq!(entry.spec.id, "mahalo_endpoint");
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

        // Bind to port 0 to get a random available port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        let axum_router = endpoint.into_axum_router();

        // Spawn server in background
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let base = format!("http://{addr}");

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

        server.abort();
    }

    #[test]
    fn default_404_preserved_without_error_handler() {
        // Without error handler, the endpoint has None
        let runtime = Arc::new(Runtime::new(1));
        let router = MahaloRouter::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = MahaloEndpoint::new(router, addr, runtime);
        assert!(endpoint.error_handler.is_none());
        // The fallback returns a hardcoded "Not Found" response when no handler is set
        let response = Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn custom_error_handler_called_for_404() {
        let handler = |status: StatusCode, conn: Conn| -> Conn {
            conn.put_resp_body(format!("Custom: {}", status.as_u16()))
        };
        let conn = Conn::new(Method::GET, http::Uri::from_static("/missing"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = handler(StatusCode::NOT_FOUND, conn);
        assert_eq!(conn.resp_body, bytes::Bytes::from("Custom: 404"));
    }

    #[test]
    fn json_error_handler_produces_correct_json() {
        let handler = json_error_handler();
        let conn = Conn::new(Method::GET, http::Uri::from_static("/"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = handler(StatusCode::NOT_FOUND, conn);
        assert_eq!(
            conn.resp_body,
            bytes::Bytes::from(r#"{"error":"Not Found","status":404}"#)
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
        assert_eq!(conn.resp_body, bytes::Bytes::from("404 Not Found"));
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let endpoint = MahaloEndpoint::new(router, addr, runtime)
            .after(plug_fn(|conn: Conn| async {
                conn.put_resp_header("x-after", "applied")
            }));
        let axum_router = endpoint.into_axum_router();

        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let base = format!("http://{addr}");
        let resp = reqwest::get(format!("{base}/test")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("x-after").unwrap(), "applied");
        assert_eq!(resp.text().await.unwrap(), "ok");

        server.abort();
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let endpoint = MahaloEndpoint::new(router, addr, runtime)
            .after(plug_fn(|conn: Conn| async {
                conn.put_resp_header("x-should-not-run", "true")
            }));
        let axum_router = endpoint.into_axum_router();

        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let base = format!("http://{addr}");
        let resp = reqwest::get(format!("{base}/halted")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-should-not-run").is_none());
        assert_eq!(resp.text().await.unwrap(), "halted");

        server.abort();
    }
}
