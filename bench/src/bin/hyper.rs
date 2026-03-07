#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use mahalo_bench::shared::{
    Fortune, User, World, fortune_rows, parse_count, parse_query_param, render_fortunes_html,
    users_db, world_rows,
};
use rand::Rng;
use tokio::net::TcpListener;

type Body = Full<Bytes>;

struct AppState {
    worlds: Vec<World>,
    fortunes: Vec<Fortune>,
    users: Vec<User>,
}

fn ok_response(content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("server", "hyper")
        .body(Full::new(body.into()))
        .unwrap()
}

fn json_response(body: impl Into<Bytes>) -> Response<Body> {
    ok_response("application/json", body)
}

async fn handle(
    req: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Body>, std::convert::Infallible> {
    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");

    let resp = match (req.method(), path) {
        // ─── TechEmpower: Plaintext ─────────────────────────────────
        (&Method::GET, "/plaintext") => ok_response("text/plain", "Hello, World!"),

        // ─── TechEmpower: JSON ──────────────────────────────────────
        (&Method::GET, "/json") => json_response(r#"{"message":"Hello, World!"}"#),

        // ─── TechEmpower: Single DB query ───────────────────────────
        (&Method::GET, "/db") => {
            let mut rng = rand::rng();
            let idx = rng.random_range(0..state.worlds.len());
            let body = serde_json::to_vec(&state.worlds[idx]).unwrap();
            json_response(body)
        }

        // ─── TechEmpower: Multiple queries ──────────────────────────
        (&Method::GET, "/queries") => {
            let count = parse_count(parse_query_param(query, "queries"));
            let mut rng = rand::rng();
            let results: Vec<&World> = (0..count)
                .map(|_| &state.worlds[rng.random_range(0..state.worlds.len())])
                .collect();
            let body = serde_json::to_vec(&results).unwrap();
            json_response(body)
        }

        // ─── TechEmpower: Fortunes ──────────────────────────────────
        (&Method::GET, "/fortunes") => {
            let html = render_fortunes_html(&state.fortunes);
            ok_response("text/html; charset=utf-8", html)
        }

        // ─── TechEmpower: Updates ───────────────────────────────────
        (&Method::GET, "/updates") => {
            let count = parse_count(parse_query_param(query, "queries"));
            let mut rng = rand::rng();
            let results: Vec<World> = (0..count)
                .map(|_| {
                    let mut w = state.worlds[rng.random_range(0..state.worlds.len())].clone();
                    w.random_number = rng.random_range(1..=10_000);
                    w
                })
                .collect();
            let body = serde_json::to_vec(&results).unwrap();
            json_response(body)
        }

        // ─── TechEmpower: Cached queries ────────────────────────────
        (&Method::GET, "/cached-queries") => {
            let count = parse_count(parse_query_param(query, "count"));
            let mut rng = rand::rng();
            let results: Vec<&World> = (0..count)
                .map(|_| &state.worlds[rng.random_range(0..state.worlds.len())])
                .collect();
            let body = serde_json::to_vec(&results).unwrap();
            json_response(body)
        }

        // ─── REST API: User by ID ───────────────────────────────────
        (&Method::GET, p) if p.starts_with("/api/users/") => {
            let id_str = &p["/api/users/".len()..];
            match id_str.parse::<u64>() {
                Ok(id) => {
                    if let Some(user) = state.users.iter().find(|u| u.id == id) {
                        let body = serde_json::to_vec(user).unwrap();
                        json_response(body)
                    } else {
                        Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(r#"{"error":"not found"}"#)))
                            .unwrap()
                    }
                }
                Err(_) => Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from("invalid id")))
                    .unwrap(),
            }
        }

        // ─── REST API: Search ───────────────────────────────────────
        (&Method::GET, "/api/search") => {
            let q = parse_query_param(query, "q").unwrap_or("");
            let page: usize = parse_query_param(query, "page")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let limit: usize = parse_query_param(query, "limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20)
                .min(100);
            let offset = page.saturating_sub(1) * limit;
            let results: Vec<&User> = state
                .users
                .iter()
                .filter(|u| q.is_empty() || u.name.contains(q))
                .skip(offset)
                .take(limit)
                .collect();
            let body = serde_json::to_vec(&results).unwrap();
            json_response(body)
        }

        // ─── REST API: POST echo ────────────────────────────────────
        (&Method::POST, "/api/echo") => {
            use http_body_util::BodyExt;
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Full::new(body))
                .unwrap()
        }

        // ─── Browser page ───────────────────────────────────────────
        (&Method::GET, "/browser/page") => {
            ok_response("text/html", "<html><body><h1>Hello</h1></body></html>")
        }

        // ─── Redirect ───────────────────────────────────────────────
        (&Method::GET, "/redirect") => Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "/plaintext")
            .body(Full::new(Bytes::new()))
            .unwrap(),

        // ─── 404 ────────────────────────────────────────────────────
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap(),
    };

    Ok(resp)
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3012);

    let state = Arc::new(AppState {
        worlds: world_rows(),
        fortunes: fortune_rows(),
        users: users_db(),
    });

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    eprintln!("Hyper (raw) listening on {addr}");

    let listener = TcpListener::bind(addr).await.unwrap();

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(req, state.clone())))
                .await
            {
                eprintln!("connection error: {err}");
            }
        });
    }
}
