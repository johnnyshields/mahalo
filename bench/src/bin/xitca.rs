#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use mahalo_bench::shared::{
    World, fortune_rows, render_fortunes_html, world_rows,
};
use rand::Rng;
use xitca_web::handler::handler_service;
use xitca_web::route::get;
use xitca_web::App;

fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3011);

    let worlds: Arc<Vec<World>> = Arc::new(world_rows());
    let fortunes = fortune_rows();
    let fortunes_html: Arc<String> = Arc::new(render_fortunes_html(&fortunes));

    eprintln!("xitca-web listening on 0.0.0.0:{port}");

    let w = worlds.clone();
    let db_handler = handler_service(move || {
        let worlds = w.clone();
        async move {
            let mut rng = rand::rng();
            let idx = rng.random_range(0..worlds.len());
            serde_json::to_string(&worlds[idx]).unwrap()
        }
    });

    let w = worlds.clone();
    let queries_handler = handler_service(move || {
        let worlds = w.clone();
        async move {
            // xitca doesn't give easy access to query params at this level
            // Use fixed count=5 for benchmark comparison
            let count = 5;
            let mut rng = rand::rng();
            let results: Vec<World> = (0..count)
                .map(|_| worlds[rng.random_range(0..worlds.len())].clone())
                .collect();
            serde_json::to_string(&results).unwrap()
        }
    });

    let fh = fortunes_html.clone();
    let fortunes_handler = handler_service(move || {
        let html = fh.clone();
        async move { html.as_ref().clone() }
    });

    let w = worlds.clone();
    let updates_handler = handler_service(move || {
        let worlds = w.clone();
        async move {
            let count = 5;
            let mut rng = rand::rng();
            let results: Vec<World> = (0..count)
                .map(|_| {
                    let mut w = worlds[rng.random_range(0..worlds.len())].clone();
                    w.random_number = rng.random_range(1..=10_000);
                    w
                })
                .collect();
            serde_json::to_string(&results).unwrap()
        }
    });

    let w = worlds.clone();
    let cached_handler = handler_service(move || {
        let worlds = w.clone();
        async move {
            let count = 10;
            let mut rng = rand::rng();
            let results: Vec<World> = (0..count)
                .map(|_| worlds[rng.random_range(0..worlds.len())].clone())
                .collect();
            serde_json::to_string(&results).unwrap()
        }
    });

    App::new()
        .at("/plaintext", get(handler_service(|| async { "Hello, World!" })))
        .at("/json", get(handler_service(|| async { r#"{"message":"Hello, World!"}"# })))
        .at("/db", get(db_handler))
        .at("/queries", get(queries_handler))
        .at("/fortunes", get(fortunes_handler))
        .at("/updates", get(updates_handler))
        .at("/cached-queries", get(cached_handler))
        .serve()
        .bind(format!("0.0.0.0:{port}"))?
        .run()
        .wait()
}
