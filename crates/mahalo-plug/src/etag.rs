use mahalo_core::conn::Conn;
use mahalo_core::plug::{BoxFuture, Plug};
use http::StatusCode;

pub struct ETag;

impl ETag {
    pub fn new() -> Self {
        ETag
    }
}

impl Default for ETag {
    fn default() -> Self {
        Self::new()
    }
}

impl Plug for ETag {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            if conn.status != StatusCode::OK || conn.resp_body.is_empty() {
                return conn;
            }

            let hash = blake3::hash(&conn.resp_body);
            let hex = hash.to_hex();
            // Truncate to 32 hex chars (128-bit) — more than adequate collision
            // resistance for ETags, and keeps the header compact.
            let etag_value = format!("W/\"{}\"", &hex[..32]);

            let if_none_match = conn
                .headers
                .get("if-none-match")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned());

            let conn = conn.put_resp_header("etag", etag_value.as_str());

            if if_none_match.as_deref() == Some(etag_value.as_str()) {
                conn.put_status(StatusCode::NOT_MODIFIED)
                    .put_resp_body(bytes::Bytes::new())
                    .halt()
            } else {
                conn
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, Uri};

    fn make_conn(body: &str) -> Conn {
        Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::OK)
            .put_resp_body(bytes::Bytes::from(body.to_owned()))
    }

    #[monoio::test(enable_timer = true)]
    async fn etag_computed_for_200_with_body() {
        let conn = make_conn("hello world");
        let conn = ETag::new().call(conn).await;
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap();
        assert!(etag.starts_with("W/\""));
        assert!(etag.ends_with('"'));
        // 16 bytes = 32 hex chars, plus W/"..." wrapper = 36 chars
        assert_eq!(etag.len(), 36);
    }

    #[monoio::test(enable_timer = true)]
    async fn returns_304_when_if_none_match_matches() {
        // First call to get the ETag
        let conn = make_conn("hello world");
        let conn = ETag::new().call(conn).await;
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap().to_owned();

        // Second call with matching if-none-match
        let mut conn = make_conn("hello world");
        conn.headers.insert("if-none-match", etag.parse().unwrap());
        let conn = ETag::new().call(conn).await;
        assert_eq!(conn.status, StatusCode::NOT_MODIFIED);
        assert!(conn.resp_body.is_empty());
        assert!(conn.halted);
    }

    #[monoio::test(enable_timer = true)]
    async fn normal_response_when_no_if_none_match() {
        let conn = make_conn("hello world");
        let conn = ETag::new().call(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body.as_ref(), b"hello world");
        assert!(!conn.halted);
        assert!(conn.resp_headers.get("etag").is_some());
    }

    #[monoio::test(enable_timer = true)]
    async fn normal_response_when_if_none_match_differs() {
        let mut conn = make_conn("hello world");
        conn.headers.insert("if-none-match", "W/\"deadbeef\"".parse().unwrap());
        let conn = ETag::new().call(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert!(!conn.halted);
        assert!(conn.resp_headers.get("etag").is_some());
    }

    #[monoio::test(enable_timer = true)]
    async fn skips_non_200_responses() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::CREATED)
            .put_resp_body("created");
        let conn = ETag::new().call(conn).await;
        assert!(conn.resp_headers.get("etag").is_none());

        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::NOT_FOUND)
            .put_resp_body("not found");
        let conn = ETag::new().call(conn).await;
        assert!(conn.resp_headers.get("etag").is_none());
    }

    #[monoio::test(enable_timer = true)]
    async fn overwrites_pre_existing_etag_header() {
        let conn = make_conn("hello world")
            .put_resp_header("etag", "W/\"old-etag-value\"");
        let conn = ETag::new().call(conn).await;
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap();
        // Should be the freshly computed BLAKE3 ETag, not the old one.
        assert!(etag.starts_with("W/\""));
        assert_ne!(etag, "W/\"old-etag-value\"");
        assert_eq!(etag.len(), 36);
    }

    #[monoio::test(enable_timer = true)]
    async fn skips_empty_body() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::OK);
        let conn = ETag::new().call(conn).await;
        assert!(conn.resp_headers.get("etag").is_none());
    }
}
