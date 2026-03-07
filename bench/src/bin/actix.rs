#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use actix_web::web::{self, Data, Path, Query};
use actix_web::{App, HttpResponse, HttpServer, get, post};
use mahalo_bench::shared::{
    Fortune, User, World, fortune_rows, parse_count, render_fortunes_html, users_db, world_rows,
};
use rand::Rng;
use serde::Deserialize;

struct AppState {
    worlds: Vec<World>,
    fortunes: Vec<Fortune>,
    users: Vec<User>,
}

// ─── TechEmpower ────────────────────────────────────────────────────

#[get("/plaintext")]
async fn plaintext() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain")
        .insert_header(("server", "actix"))
        .body("Hello, World!")
}

#[get("/json")]
async fn json() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .insert_header(("server", "actix"))
        .body(r#"{"message":"Hello, World!"}"#)
}

#[get("/db")]
async fn db(data: Data<AppState>) -> HttpResponse {
    let mut rng = rand::rng();
    let idx = rng.random_range(0..data.worlds.len());
    HttpResponse::Ok().json(&data.worlds[idx])
}

#[derive(Deserialize)]
struct QueriesParams {
    queries: Option<String>,
}

#[get("/queries")]
async fn queries(
    data: Data<AppState>,
    params: Query<QueriesParams>,
) -> HttpResponse {
    let count = parse_count(params.queries.as_deref());
    let mut rng = rand::rng();
    let results: Vec<&World> = (0..count)
        .map(|_| &data.worlds[rng.random_range(0..data.worlds.len())])
        .collect();
    HttpResponse::Ok().json(results)
}

#[get("/fortunes")]
async fn fortunes(data: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_fortunes_html(&data.fortunes))
}

#[get("/updates")]
async fn updates(
    data: Data<AppState>,
    params: Query<QueriesParams>,
) -> HttpResponse {
    let count = parse_count(params.queries.as_deref());
    let mut rng = rand::rng();
    let results: Vec<World> = (0..count)
        .map(|_| {
            let mut w = data.worlds[rng.random_range(0..data.worlds.len())].clone();
            w.random_number = rng.random_range(1..=10_000);
            w
        })
        .collect();
    HttpResponse::Ok().json(results)
}

#[derive(Deserialize)]
struct CachedParams {
    count: Option<String>,
}

#[get("/cached-queries")]
async fn cached_queries(
    data: Data<AppState>,
    params: Query<CachedParams>,
) -> HttpResponse {
    let count = parse_count(params.count.as_deref());
    let mut rng = rand::rng();
    let results: Vec<&World> = (0..count)
        .map(|_| &data.worlds[rng.random_range(0..data.worlds.len())])
        .collect();
    HttpResponse::Ok().json(results)
}

// ─── REST API ───────────────────────────────────────────────────────

#[get("/api/users/{id}")]
async fn user_by_id(data: Data<AppState>, path: Path<u64>) -> HttpResponse {
    let id = path.into_inner();
    if let Some(user) = data.users.iter().find(|u| u.id == id) {
        HttpResponse::Ok().json(user)
    } else {
        HttpResponse::NotFound().json(serde_json::json!({"error": "not found"}))
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    page: Option<usize>,
    limit: Option<usize>,
}

#[get("/api/search")]
async fn search(data: Data<AppState>, params: Query<SearchParams>) -> HttpResponse {
    let q = params.q.as_deref().unwrap_or("");
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(20).min(100);
    let offset = page.saturating_sub(1) * limit;
    let results: Vec<&User> = data
        .users
        .iter()
        .filter(|u| q.is_empty() || u.name.contains(q))
        .skip(offset)
        .take(limit)
        .collect();
    HttpResponse::Ok().json(results)
}

#[post("/api/echo")]
async fn echo(body: web::Bytes) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body)
}

// ─── Browser / misc ─────────────────────────────────────────────────

#[get("/browser/page")]
async fn browser_page() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body("<html><body><h1>Hello</h1></body></html>")
}

#[get("/redirect")]
async fn redirect_handler() -> HttpResponse {
    HttpResponse::Found()
        .insert_header(("location", "/plaintext"))
        .finish()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3002);

    let state = Data::new(AppState {
        worlds: world_rows(),
        fortunes: fortune_rows(),
        users: users_db(),
    });

    eprintln!("Actix-web listening on 0.0.0.0:{port}");

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .service(plaintext)
            .service(json)
            .service(db)
            .service(queries)
            .service(fortunes)
            .service(updates)
            .service(cached_queries)
            .service(user_by_id)
            .service(search)
            .service(echo)
            .service(browser_page)
            .service(redirect_handler)
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
