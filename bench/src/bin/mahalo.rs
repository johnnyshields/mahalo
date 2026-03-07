#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;

use http::header::{HeaderValue, CONTENT_TYPE, SERVER};
use http::StatusCode;
use mahalo::{
    Conn, ETag, MahaloEndpoint, MahaloRouter, Pipeline, RequestId, SecureHeaders,
    plug_fn, request_logger, sync_plug_fn,
};
use mahalo_bench::shared::{
    Fortune, User, World, fortune_rows, parse_count, render_fortunes_html, users_db, world_rows,
};
use rand::Rng;

// ── Pre-validated header values ─────────────────────────────────────
static CT_TEXT: HeaderValue = HeaderValue::from_static("text/plain");
static CT_JSON: HeaderValue = HeaderValue::from_static("application/json");
static CT_HTML: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");
static SRV: HeaderValue = HeaderValue::from_static("mahalo");

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    // Pre-populate in-memory data (Send + Sync, shared across factories)
    let worlds: Arc<Vec<World>> = Arc::new(world_rows());
    let fortunes: Arc<Vec<Fortune>> = Arc::new(fortune_rows());
    let users: Arc<Vec<User>> = Arc::new(users_db());

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Mahalo listening on {addr}");

    let endpoint = MahaloEndpoint::new(
        {
            let worlds = worlds.clone();
            let fortunes = fortunes.clone();
            let users = users.clone();
            move || build_router(worlds.clone(), fortunes.clone(), users.clone())
        },
        addr,
    )
    .after(|| Box::new(ETag::new()))
    .after(|| {
        let (_start, finish) = request_logger();
        Box::new(finish)
    });

    endpoint.start().unwrap();
}

fn build_router(
    worlds: Arc<Vec<World>>,
    fortunes: Arc<Vec<Fortune>>,
    users: Arc<Vec<User>>,
) -> MahaloRouter {
    let (log_start, _log_finish) = request_logger();

    // "bare" pipeline — zero middleware, raw speed
    let bare = Pipeline::new("bare");

    // "api" pipeline — request ID + server header
    let api = Pipeline::new("api")
        .plug(RequestId)
        .plug(sync_plug_fn(|conn: Conn| {
            conn.put_resp_header_static(SERVER, SRV.clone())
        }));

    // "browser" pipeline — full middleware stack
    let browser = Pipeline::new("browser")
        .plug(log_start)
        .plug(RequestId)
        .plug(SecureHeaders::new())
        .plug(sync_plug_fn(|conn: Conn| {
            conn.put_resp_header_static(SERVER, SRV.clone())
        }));

    MahaloRouter::new()
        .pipeline(bare)
        .pipeline(api)
        .pipeline(browser)
        // ─── TechEmpower: bare pipeline (raw throughput) ────────
        .scope("/", &["bare"], |s| {
            s.get("/plaintext", sync_plug_fn(|conn: Conn| {
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_TEXT.clone())
                    .put_resp_body_static(b"Hello, World!")
            }));
            s.get("/json", sync_plug_fn(|conn: Conn| {
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                    .put_resp_body_static(br#"{"message":"Hello, World!"}"#)
            }));
        })
        // ─── TechEmpower DB scenarios: api pipeline ─────────────
        .scope("/", &["api"], |s| {
            // Single DB query
            s.get("/db", plug_fn({
                let worlds = worlds.clone();
                move |conn: Conn| {
                    let worlds = worlds.clone();
                    async move {
                        let mut rng = rand::rng();
                        let idx = rng.random_range(0..worlds.len());
                        let body = serde_json::to_vec(&worlds[idx]).unwrap();
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                            .put_resp_body(body)
                    }
                }
            }));
            // Multiple DB queries
            s.get("/queries", plug_fn({
                let worlds = worlds.clone();
                move |conn: Conn| {
                    let worlds = worlds.clone();
                    async move {
                        let mut conn = conn;
                        conn.parse_query_params();
                        let count = parse_count(conn.query_param("queries"));
                        let mut rng = rand::rng();
                        let results: Vec<&World> = (0..count)
                            .map(|_| &worlds[rng.random_range(0..worlds.len())])
                            .collect();
                        let body = serde_json::to_vec(&results).unwrap();
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                            .put_resp_body(body)
                    }
                }
            }));
            // Fortunes — HTML template rendering
            s.get("/fortunes", plug_fn({
                let fortunes = fortunes.clone();
                move |conn: Conn| {
                    let fortunes = fortunes.clone();
                    async move {
                        let html = render_fortunes_html(&fortunes);
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_HTML.clone())
                            .put_resp_body(html)
                    }
                }
            }));
            // DB updates (read-modify-write)
            s.get("/updates", plug_fn({
                let worlds = worlds.clone();
                move |conn: Conn| {
                    let worlds = worlds.clone();
                    async move {
                        let mut conn = conn;
                        conn.parse_query_params();
                        let count = parse_count(conn.query_param("queries"));
                        let mut rng = rand::rng();
                        let results: Vec<World> = (0..count)
                            .map(|_| {
                                let mut w = worlds[rng.random_range(0..worlds.len())].clone();
                                w.random_number = rng.random_range(1..=10_000);
                                w
                            })
                            .collect();
                        let body = serde_json::to_vec(&results).unwrap();
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                            .put_resp_body(body)
                    }
                }
            }));
            // Cached queries
            s.get("/cached-queries", plug_fn({
                let worlds = worlds.clone();
                move |conn: Conn| {
                    let worlds = worlds.clone();
                    async move {
                        let mut conn = conn;
                        conn.parse_query_params();
                        let count = parse_count(conn.query_param("count"));
                        let mut rng = rand::rng();
                        let results: Vec<&World> = (0..count)
                            .map(|_| &worlds[rng.random_range(0..worlds.len())])
                            .collect();
                        let body = serde_json::to_vec(&results).unwrap();
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                            .put_resp_body(body)
                    }
                }
            }));
        })
        // ─── Realistic app scenarios: api pipeline ──────────────
        .scope("/api", &["api"], |s| {
            // Path parameter extraction — GET /api/users/:id
            s.get("/users/:id", plug_fn({
                let users = users.clone();
                move |conn: Conn| {
                    let users = users.clone();
                    async move {
                        let id: u64 = conn
                            .path_param("id")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        if let Some(user) = users.iter().find(|u| u.id == id) {
                            let body = serde_json::to_vec(user).unwrap();
                            conn.put_status(StatusCode::OK)
                                .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                                .put_resp_body(body)
                        } else {
                            conn.put_status(StatusCode::NOT_FOUND)
                                .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                                .put_resp_body_static(br#"{"error":"not found"}"#)
                        }
                    }
                }
            }));
            // Query parameter parsing — search with pagination
            s.get("/search", plug_fn({
                let users = users.clone();
                move |conn: Conn| {
                    let users = users.clone();
                    async move {
                        let mut conn = conn;
                        conn.parse_query_params();
                        let q = conn.query_param("q").unwrap_or("");
                        let page: usize = conn
                            .query_param("page")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1);
                        let limit: usize = conn
                            .query_param("limit")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(20)
                            .min(100);
                        let offset = (page.saturating_sub(1)) * limit;
                        let results: Vec<&User> = users
                            .iter()
                            .filter(|u| q.is_empty() || u.name.contains(q))
                            .skip(offset)
                            .take(limit)
                            .collect();
                        let body = serde_json::to_vec(&results).unwrap();
                        conn.put_status(StatusCode::OK)
                            .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                            .put_resp_body(body)
                    }
                }
            }));
            // POST JSON body echo — simulates API create
            s.post("/echo", plug_fn(|conn: Conn| async {
                let body = conn.body.clone();
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_JSON.clone())
                    .put_resp_body(body)
            }));
        })
        // ─── Browser pipeline: full middleware stack ─────────────
        .scope("/browser", &["browser"], |s| {
            s.get("/page", sync_plug_fn(|conn: Conn| {
                conn.put_status(StatusCode::OK)
                    .put_resp_header_static(CONTENT_TYPE, CT_HTML.clone())
                    .put_resp_body_static(b"<html><body><h1>Hello</h1></body></html>")
            }));
        })
        // ─── Misc ───────────────────────────────────────────────
        .get("/redirect", sync_plug_fn(|conn: Conn| {
            conn.put_status(StatusCode::FOUND)
                .put_resp_header(
                    http::header::LOCATION,
                    HeaderValue::from_static("/plaintext"),
                )
        }))
}
