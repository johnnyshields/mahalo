//! Shared data and helpers for all bench servers.
//!
//! Simulates TechEmpower-style DB rows in memory so we benchmark the
//! framework, not a database driver.

use rand::Rng;
use serde::Serialize;

/// Parse PORT env var with a default fallback.
pub fn parse_port(default: u16) -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_count ──────────────────────────────────────────────────

    #[test]
    fn parse_count_none_defaults_to_1() {
        assert_eq!(parse_count(None), 1);
    }

    #[test]
    fn parse_count_zero_clamps_to_1() {
        assert_eq!(parse_count(Some("0")), 1);
    }

    #[test]
    fn parse_count_max_boundary() {
        assert_eq!(parse_count(Some("500")), 500);
    }

    #[test]
    fn parse_count_over_max_clamps() {
        assert_eq!(parse_count(Some("501")), 500);
    }

    #[test]
    fn parse_count_non_numeric_defaults_to_1() {
        assert_eq!(parse_count(Some("abc")), 1);
    }

    #[test]
    fn parse_count_valid_mid_range() {
        assert_eq!(parse_count(Some("20")), 20);
    }

    // ── parse_query_param ────────────────────────────────────────────

    #[test]
    fn parse_query_param_basic() {
        assert_eq!(parse_query_param("queries=20", "queries"), Some("20"));
    }

    #[test]
    fn parse_query_param_missing_key() {
        assert_eq!(parse_query_param("queries=20", "page"), None);
    }

    #[test]
    fn parse_query_param_multiple_params() {
        assert_eq!(
            parse_query_param("queries=20&page=3", "page"),
            Some("3")
        );
    }

    #[test]
    fn parse_query_param_empty_query() {
        assert_eq!(parse_query_param("", "queries"), None);
    }

    #[test]
    fn parse_query_param_no_equals() {
        assert_eq!(parse_query_param("novalue", "novalue"), None);
    }

    // ── render_fortunes_html ─────────────────────────────────────────

    #[test]
    fn fortunes_html_contains_extra_row() {
        let html = render_fortunes_html(&fortune_rows());
        assert!(html.contains("Additional fortune added at request time."));
    }

    #[test]
    fn fortunes_html_has_id_zero() {
        let html = render_fortunes_html(&fortune_rows());
        assert!(html.contains("<td>0</td>"));
    }

    #[test]
    fn fortunes_html_sorted_by_message() {
        let html = render_fortunes_html(&fortune_rows());
        // "Additional fortune..." sorts before "After all is said..."
        let pos_additional = html.find("Additional fortune").unwrap();
        let pos_after = html.find("After all is said").unwrap();
        assert!(pos_additional < pos_after);
    }

    #[test]
    fn fortunes_html_escapes_script_tag() {
        let html = render_fortunes_html(&fortune_rows());
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn fortunes_html_escapes_quotes() {
        let html = render_fortunes_html(&fortune_rows());
        // The fortune contains: alert("This should not...")
        assert!(html.contains("&quot;"));
    }

    #[test]
    fn fortunes_html_has_13_rows() {
        // 12 DB rows + 1 extra = 13
        let html = render_fortunes_html(&fortune_rows());
        let count = html.matches("<tr><td>").count();
        assert_eq!(count, 13);
    }

    // ── users_db ─────────────────────────────────────────────────────

    #[test]
    fn users_db_has_1000_entries() {
        assert_eq!(users_db().len(), 1000);
    }

    #[test]
    fn users_db_id_range() {
        let users = users_db();
        assert_eq!(users.first().unwrap().id, 1);
        assert_eq!(users.last().unwrap().id, 1000);
    }

    #[test]
    fn users_db_admin_every_10th() {
        let users = users_db();
        assert_eq!(users[9].role, "admin");   // id=10
        assert_eq!(users[8].role, "member");  // id=9
    }

    // ── world_rows ───────────────────────────────────────────────────

    #[test]
    fn world_rows_has_10000_entries() {
        assert_eq!(world_rows().len(), 10_000);
    }

    #[test]
    fn world_rows_id_range() {
        let worlds = world_rows();
        assert_eq!(worlds.first().unwrap().id, 1);
        assert_eq!(worlds.last().unwrap().id, 10_000);
    }

    #[test]
    fn world_rows_random_numbers_in_range() {
        let worlds = world_rows();
        for w in &worlds {
            assert!(w.random_number >= 1 && w.random_number <= 10_000);
        }
    }
}
