use std::net::SocketAddr;
use std::sync::Arc;

use http::StatusCode;
use mahalo::{Conn, MahaloEndpoint, MahaloRouter, plug_fn};
use rebar_core::runtime::Runtime;

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
                    .put_resp_header("content-type", "text/plain")
                    .put_resp_body("Hello, World!")
            }),
        )
        .get(
            "/json",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_header("content-type", "application/json")
                    .put_resp_body(r#"{"message":"Hello, World!"}"#)
            }),
        );

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Mahalo listening on {addr}");

    let endpoint = MahaloEndpoint::new(router, addr, runtime);
    endpoint.start().await.unwrap();
}
