use std::net::SocketAddr;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;

async fn plaintext() -> impl IntoResponse {
    (
        [("content-type", "text/plain")],
        "Hello, World!",
    )
}

async fn json() -> impl IntoResponse {
    (
        [("content-type", "application/json")],
        r#"{"message":"Hello, World!"}"#,
    )
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    let app = Router::new()
        .route("/plaintext", get(plaintext))
        .route("/json", get(json));

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Axum (raw) listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
