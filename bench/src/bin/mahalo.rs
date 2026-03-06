#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;

use http::StatusCode;
use http::header::{HeaderValue, CONTENT_TYPE};
use mahalo::{Conn, MahaloEndpoint, MahaloRouter, plug_fn};
use rebar_core::runtime::Runtime;

/// Pre-validated header values — avoids per-request parsing.
static CT_TEXT: HeaderValue = HeaderValue::from_static("text/plain");
static CT_JSON: HeaderValue = HeaderValue::from_static("application/json");

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let runtime = Arc::new(Runtime::new(1));

    let router = MahaloRouter::new()
        .get(
            "/plaintext",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_TEXT.clone())
                    .put_resp_body("Hello, World!")
            }),
        )
        .get(
            "/json",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                    .put_resp_body(r#"{"message":"Hello, World!"}"#)
            }),
        );

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Mahalo listening on {addr}");

    let endpoint = MahaloEndpoint::new(router, addr, runtime);
    endpoint.start().await.unwrap();
}
