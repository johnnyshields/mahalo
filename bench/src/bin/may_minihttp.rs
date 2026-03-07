#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io;

use mahalo_bench::shared::{
    World, fortune_rows, parse_count, render_fortunes_html, world_rows,
};
use may_minihttp::{HttpService, HttpServiceFactory, Request, Response};
use rand::Rng;

struct Techempower {
    worlds: Vec<World>,
    fortunes_html: String,
}

impl HttpService for Techempower {
    fn call(&mut self, req: Request, rsp: &mut Response) -> io::Result<()> {
        let path = req.path();
        // Split off query string
        let (path, query) = path.split_once('?').unwrap_or((path, ""));

        match path {
            "/plaintext" => {
                rsp.header("Content-Type: text/plain");
                rsp.body("Hello, World!");
            }
            "/json" => {
                rsp.header("Content-Type: application/json");
                rsp.body(r#"{"message":"Hello, World!"}"#);
            }
            "/db" => {
                rsp.header("Content-Type: application/json");
                let mut rng = rand::rng();
                let idx = rng.random_range(0..self.worlds.len());
                rsp.body_vec(serde_json::to_vec(&self.worlds[idx]).unwrap());
            }
            "/queries" => {
                rsp.header("Content-Type: application/json");
                let count = parse_query_count(query, "queries");
                let mut rng = rand::rng();
                let results: Vec<&World> = (0..count)
                    .map(|_| &self.worlds[rng.random_range(0..self.worlds.len())])
                    .collect();
                rsp.body_vec(serde_json::to_vec(&results).unwrap());
            }
            "/fortunes" => {
                rsp.header("Content-Type: text/html; charset=utf-8");
                rsp.body_vec(self.fortunes_html.as_bytes().to_vec());
            }
            "/updates" => {
                rsp.header("Content-Type: application/json");
                let count = parse_query_count(query, "queries");
                let mut rng = rand::rng();
                let results: Vec<World> = (0..count)
                    .map(|_| {
                        let mut w = self.worlds[rng.random_range(0..self.worlds.len())].clone();
                        w.random_number = rng.random_range(1..=10_000);
                        w
                    })
                    .collect();
                rsp.body_vec(serde_json::to_vec(&results).unwrap());
            }
            "/cached-queries" => {
                rsp.header("Content-Type: application/json");
                let count = parse_query_count(query, "count");
                let mut rng = rand::rng();
                let results: Vec<&World> = (0..count)
                    .map(|_| &self.worlds[rng.random_range(0..self.worlds.len())])
                    .collect();
                rsp.body_vec(serde_json::to_vec(&results).unwrap());
            }
            _ => {
                rsp.status_code(404, "Not Found");
                rsp.body("Not Found");
            }
        }
        Ok(())
    }
}

fn parse_query_count(query: &str, key: &str) -> usize {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return parse_count(Some(v));
            }
        }
    }
    1
}

struct TechempowerFactory {
    worlds: Vec<World>,
    fortunes_html: String,
}

impl HttpServiceFactory for TechempowerFactory {
    type Service = Techempower;
    fn new_service(&self, _id: usize) -> Self::Service {
        Techempower {
            worlds: self.worlds.clone(),
            fortunes_html: self.fortunes_html.clone(),
        }
    }
}

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3010);

    let worlds = world_rows();
    let fortunes = fortune_rows();
    let fortunes_html = render_fortunes_html(&fortunes);

    eprintln!("may-minihttp listening on 0.0.0.0:{port}");

    let factory = TechempowerFactory {
        worlds,
        fortunes_html,
    };
    let _server = factory.start(format!("0.0.0.0:{port}")).unwrap();

    // Block forever
    std::thread::park();
}
