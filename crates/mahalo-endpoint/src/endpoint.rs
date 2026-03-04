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
use mahalo_router::MahaloRouter;

/// Default maximum request body size (2 MB).
const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Shared state passed to each Axum handler via State extractor.
#[derive(Clone)]
struct EndpointState {
    router: Arc<MahaloRouter>,
    runtime: Arc<Runtime>,
}

/// Bridges MahaloRouter to an Axum HTTP server with rebar supervision support.
pub struct MahaloEndpoint {
    router: Arc<MahaloRouter>,
    addr: SocketAddr,
    runtime: Arc<Runtime>,
}

impl MahaloEndpoint {
    pub fn new(router: MahaloRouter, addr: SocketAddr, runtime: Arc<Runtime>) -> Self {
        Self {
            router: Arc::new(router),
            addr,
            runtime,
        }
    }

    /// Convert into an Axum Router that delegates all requests to MahaloRouter.
    pub fn into_axum_router(&self) -> Router {
        let state = EndpointState {
            router: Arc::clone(&self.router),
            runtime: Arc::clone(&self.runtime),
        };

        Router::new()
            .fallback(fallback_handler)
            .with_state(state)
    }

    /// Start the HTTP server, blocking until shutdown.
    pub async fn start(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let axum_router = self.into_axum_router();
        let listener = tokio::net::TcpListener::bind(self.addr).await?;
        tracing::info!("Mahalo endpoint listening on {}", self.addr);
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
        let router = self.router;
        let addr = self.addr;
        let runtime = self.runtime;

        ChildEntry::new(spec, move || {
            let router = Arc::clone(&router);
            let runtime = Arc::clone(&runtime);
            async move {
                let endpoint = MahaloEndpoint {
                    router,
                    addr,
                    runtime,
                };
                match endpoint.start().await {
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

    match state.router.resolve(&conn.method.clone(), conn.uri.path()) {
        Some(resolved) => {
            let conn = resolved.execute(conn).await;
            conn_to_response(conn)
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap(),
    }
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
        headers.extend(conn.resp_headers.into_iter());
    }

    builder.body(Body::from(conn.resp_body)).unwrap()
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
}
