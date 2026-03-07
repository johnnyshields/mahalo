#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mahalo_bench::shared::{
    Fortune, User, World, fortune_rows, parse_count, render_fortunes_html, users_db, world_rows,
};
use rand::Rng;
use serde::Deserialize;

#[derive(Clone)]
struct AppState {
    worlds: Arc<Vec<World>>,
    fortunes: Arc<Vec<Fortune>>,
    users: Arc<Vec<User>>,
}

// ─── TechEmpower: Plaintext ─────────────────────────────────────────
async fn plaintext() -> impl IntoResponse {
    ([("content-type", "text/plain"), ("server", "axum")], "Hello, World!")
}

// ─── TechEmpower: JSON ──────────────────────────────────────────────
async fn json() -> impl IntoResponse {
    (
        [("content-type", "application/json"), ("server", "axum")],
        r#"{"message":"Hello, World!"}"#,
    )
}

// ─── TechEmpower: Single DB query ───────────────────────────────────
async fn db(State(state): State<AppState>) -> impl IntoResponse {
    let mut rng = rand::rng();
    let idx = rng.random_range(0..state.worlds.len());
    Json(state.worlds[idx].clone())
}

// ─── TechEmpower: Multiple queries ──────────────────────────────────
#[derive(Deserialize)]
struct QueriesParams {
    queries: Option<String>,
}

async fn queries(
    State(state): State<AppState>,
    Query(params): Query<QueriesParams>,
) -> impl IntoResponse {
    let count = parse_count(params.queries.as_deref());
    let mut rng = rand::rng();
    let results: Vec<World> = (0..count)
        .map(|_| state.worlds[rng.random_range(0..state.worlds.len())].clone())
        .collect();
    Json(results)
}

// ─── TechEmpower: Fortunes ──────────────────────────────────────────
async fn fortunes(State(state): State<AppState>) -> impl IntoResponse {
    Html(render_fortunes_html(&state.fortunes))
}

// ─── TechEmpower: Updates ───────────────────────────────────────────
async fn updates(
    State(state): State<AppState>,
    Query(params): Query<QueriesParams>,
) -> impl IntoResponse {
    let count = parse_count(params.queries.as_deref());
    let mut rng = rand::rng();
    let results: Vec<World> = (0..count)
        .map(|_| {
            let mut w = state.worlds[rng.random_range(0..state.worlds.len())].clone();
            w.random_number = rng.random_range(1..=10_000);
            w
        })
        .collect();
    Json(results)
}

// ─── TechEmpower: Cached queries ────────────────────────────────────
#[derive(Deserialize)]
struct CachedParams {
    count: Option<String>,
}

async fn cached_queries(
    State(state): State<AppState>,
    Query(params): Query<CachedParams>,
) -> impl IntoResponse {
    let count = parse_count(params.count.as_deref());
    let mut rng = rand::rng();
    let results: Vec<World> = (0..count)
        .map(|_| state.worlds[rng.random_range(0..state.worlds.len())].clone())
        .collect();
    Json(results)
}

// ─── REST API: User by ID ───────────────────────────────────────────
async fn user_by_id(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Response {
    if let Some(user) = state.users.iter().find(|u| u.id == id) {
        Json(user.clone()).into_response()
    } else {
        (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "application/json")], r#"{"error":"not found"}"#).into_response()
    }
}

// ─── REST API: Search ───────────────────────────────────────────────
#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    page: Option<usize>,
    limit: Option<usize>,
}

async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let q = params.q.as_deref().unwrap_or("");
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(20).min(100);
    let offset = page.saturating_sub(1) * limit;
    let results: Vec<User> = state
        .users
        .iter()
        .filter(|u| q.is_empty() || u.name.contains(q))
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    Json(results)
}

// ─── REST API: POST echo ────────────────────────────────────────────
async fn echo(body: axum::body::Bytes) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], body)
}

// ─── Browser page ───────────────────────────────────────────────────
async fn browser_page() -> Html<&'static str> {
    Html("<html><body><h1>Hello</h1></body></html>")
}

// ─── Redirect ───────────────────────────────────────────────────────
async fn redirect_handler() -> Redirect {
    Redirect::to("/plaintext")
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    let state = AppState {
        worlds: Arc::new(world_rows()),
        fortunes: Arc::new(fortune_rows()),
        users: Arc::new(users_db()),
    };

    let app = Router::new()
        // TechEmpower
        .route("/plaintext", get(plaintext))
        .route("/json", get(json))
        .route("/db", get(db))
        .route("/queries", get(queries))
        .route("/fortunes", get(fortunes))
        .route("/updates", get(updates))
        .route("/cached-queries", get(cached_queries))
        // REST API
        .route("/api/users/{id}", get(user_by_id))
        .route("/api/search", get(search))
        .route("/api/echo", post(echo))
        // Browser
        .route("/browser/page", get(browser_page))
        .route("/redirect", get(redirect_handler))
        .with_state(state);

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Axum (raw) listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
