//! Shared data and helpers for all bench servers.
//!
//! Simulates TechEmpower-style DB rows in memory so we benchmark the
//! framework, not a database driver.

use rand::Rng;
use serde::Serialize;

/// TechEmpower "World" row.
#[derive(Clone, Serialize)]
pub struct World {
    pub id: i32,
    #[serde(rename = "randomNumber")]
    pub random_number: i32,
}

/// TechEmpower "Fortune" row.
#[derive(Clone, Serialize)]
pub struct Fortune {
    pub id: i32,
    pub message: String,
}

/// Pre-populated in-memory "database" of 10 000 World rows.
pub fn world_rows() -> Vec<World> {
    let mut rng = rand::rng();
    (1..=10_000)
        .map(|id| World {
            id,
            random_number: rng.random_range(1..=10_000),
        })
        .collect()
}

/// Canonical Fortune rows (matches TechEmpower dataset).
pub fn fortune_rows() -> Vec<Fortune> {
    vec![
        Fortune { id: 1,  message: "fortune: No such file or directory".into() },
        Fortune { id: 2,  message: "A computer scientist is someone who fixes things that aren't broken.".into() },
        Fortune { id: 3,  message: "After all is said and done, more is said than done.".into() },
        Fortune { id: 4,  message: "Any program that runs right is obsolete.".into() },
        Fortune { id: 5,  message: "A list is only as strong as its weakest link. — Donald Knuth".into() },
        Fortune { id: 6,  message: "Feature: A bug with seniority.".into() },
        Fortune { id: 7,  message: "Computers make very fast, very accurate mistakes.".into() },
        Fortune { id: 8,  message: "<script>alert(\"This should not be displayed in a browser alert box.\");</script>".into() },
        Fortune { id: 9,  message: "A computer program does what you tell it to do, not what you want it to do.".into() },
        Fortune { id: 10, message: "If Java had true garbage collection, most programs would delete themselves upon execution.".into() },
        Fortune { id: 11, message: "フレームワークのベンチマーク".into() },
        Fortune { id: 12, message: "The best thing about a boolean is even if you are wrong, you are only off by a bit.".into() },
    ]
}

/// Parse `count` query param, clamped to 1..=500 (TechEmpower spec).
pub fn parse_count(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 500)
}

/// Extract a query-string parameter by key from a raw `&str` query string.
///
/// Example: `parse_query_param("queries=20&page=1", "queries")` → `Some("20")`
pub fn parse_query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Render fortunes as HTML (TechEmpower spec: add extra row, sort, escape).
pub fn render_fortunes_html(db_rows: &[Fortune]) -> String {
    let mut rows: Vec<(i32, &str)> = db_rows.iter().map(|f| (f.id, f.message.as_str())).collect();
    rows.push((0, "Additional fortune added at request time."));
    rows.sort_by(|a, b| a.1.cmp(b.1));

    let mut html = String::with_capacity(2048);
    html.push_str("<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>");
    for (id, msg) in &rows {
        html.push_str("<tr><td>");
        html.push_str(&itoa_str(*id));
        html.push_str("</td><td>");
        escape_html(&mut html, msg);
        html.push_str("</td></tr>");
    }
    html.push_str("</table></body></html>");
    html
}

fn itoa_str(n: i32) -> String {
    let mut buf = itoa::Buffer::new();
    buf.format(n).to_owned()
}

fn escape_html(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
}

/// Simulated "user" object for REST API scenarios.
#[derive(Clone, Serialize)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
    pub role: String,
}

pub fn users_db() -> Vec<User> {
    (1..=1000)
        .map(|id| User {
            id,
            name: format!("User {id}"),
            email: format!("user{id}@example.com"),
            role: if id % 10 == 0 { "admin".into() } else { "member".into() },
        })
        .collect()
}
