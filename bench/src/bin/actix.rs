use actix_web::{App, HttpResponse, HttpServer, get, web};
use serde::Serialize;

#[derive(Serialize)]
struct JsonMessage {
    message: &'static str,
}

#[get("/plaintext")]
async fn plaintext() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain")
        .body("Hello, World!")
}

#[get("/json")]
async fn json() -> web::Json<JsonMessage> {
    web::Json(JsonMessage {
        message: "Hello, World!",
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3002);

    eprintln!("Actix-web listening on 0.0.0.0:{port}");

    HttpServer::new(|| App::new().service(plaintext).service(json))
        .bind(("0.0.0.0", port))?
        .run()
        .await
}
