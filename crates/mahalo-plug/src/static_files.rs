// Static File Serving plug
use std::path::PathBuf;

use http::header::HeaderValue;
use http::{Method, StatusCode};
use mahalo_core::conn::Conn;
use mahalo_core::plug::{BoxFuture, Plug};

/// Serves static files from a directory under a URL prefix.
pub struct StaticFiles {
    url_prefix: String,
    dir: PathBuf,
    cache_control: HeaderValue,
}

impl StaticFiles {
    pub fn new(url_prefix: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        let mut prefix = url_prefix.into();
        if !prefix.starts_with('/') {
            prefix = format!("/{prefix}");
        }
        while prefix.ends_with('/') && prefix.len() > 1 {
            prefix.pop();
        }
        Self {
            url_prefix: prefix,
            dir: dir.into(),
            cache_control: HeaderValue::from_static("public, max-age=3600"),
        }
    }

    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = HeaderValue::from_str(&value.into())
            .expect("invalid cache-control header value");
        self
    }
}

fn mime_from_extension(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Compute a BLAKE3 ETag for file contents, set the `etag` response header,
/// and check `if-none-match`. Returns `(conn, true)` if the client's cached
/// version matches (caller should return 304), `(conn, false)` otherwise.
fn check_etag(conn: Conn, contents: &[u8]) -> (Conn, bool) {
    let hash = blake3::hash(contents);
    let hex = hash.to_hex();
    // Truncate to 32 hex chars (128-bit) — adequate collision resistance for ETags.
    let etag_value = format!("W/\"{}\"", &hex[..32]);

    let matches = conn
        .headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s == etag_value);

    let conn = conn.put_resp_header("etag", etag_value.as_str());
    (conn, matches)
}

/// Returns true if the given MIME type is likely compressible.
fn is_compressible(mime: &str) -> bool {
    if mime.starts_with("video/") || mime.starts_with("audio/") {
        return false;
    }
    if mime.starts_with("image/") && mime != "image/svg+xml" {
        return false;
    }
    !matches!(
        mime,
        "font/woff2"
            | "application/zip"
            | "application/gzip"
            | "application/x-bzip2"
            | "application/octet-stream"
    )
}

impl Plug for StaticFiles {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async move {
            // Only handle GET and HEAD
            if conn.method != Method::GET && conn.method != Method::HEAD {
                return conn;
            }

            let path = conn.uri.path();

            // Check URL prefix match
            let relative = if path == self.url_prefix {
                ""
            } else if let Some(rest) = path.strip_prefix(&self.url_prefix) {
                if let Some(stripped) = rest.strip_prefix('/') {
                    stripped
                } else {
                    return conn;
                }
            } else {
                return conn;
            };

            // Empty relative path means directory request — pass through
            if relative.is_empty() {
                return conn;
            }

            // Security: reject path traversal
            for segment in relative.split('/') {
                if segment == ".." {
                    return conn;
                }
            }

            // Resolve and canonicalize
            let file_path = self.dir.join(relative);
            let canonical = match tokio::fs::canonicalize(&file_path).await {
                Ok(p) => p,
                Err(_) => return conn,
            };
            let canonical_dir = match tokio::fs::canonicalize(&self.dir).await {
                Ok(p) => p,
                Err(_) => return conn,
            };
            if !canonical.starts_with(&canonical_dir) {
                return conn;
            }

            // Must be a file
            match tokio::fs::metadata(&canonical).await {
                Ok(m) if m.is_file() => {}
                _ => return conn,
            }

            // Determine content type
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let mime = mime_from_extension(ext);
            let compressible = is_compressible(mime);

            // Try precompressed variants for compressible types
            if compressible {
                let accept_encoding = conn
                    .headers
                    .get("accept-encoding")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                // Priority: br > zst > gz
                let variants: &[(&str, &str, &str)] = &[
                    ("br", ".br", "br"),
                    ("zstd", ".zst", "zstd"),
                    ("gzip", ".gz", "gzip"),
                ];

                for &(token, suffix, encoding) in variants {
                    if accept_encoding.contains(token) {
                        let compressed_path = canonical.with_extension(
                            format!(
                                "{}{}",
                                canonical.extension().and_then(|e| e.to_str()).unwrap_or(""),
                                suffix
                            ),
                        );
                        if let Ok(m) = tokio::fs::metadata(&compressed_path).await {
                            if m.is_file() {
                                let contents = match tokio::fs::read(&compressed_path).await {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                                let len = contents.len().to_string();
                                let is_head = conn.method == Method::HEAD;

                                let (conn, etag_match) = check_etag(conn, &contents);
                                let mut conn = conn
                                    .put_status(StatusCode::OK)
                                    .put_resp_header("content-encoding", encoding)
                                    .put_resp_header("vary", "accept-encoding")
                                    .put_resp_header("content-length", len.as_str());
                                conn.resp_headers
                                    .insert(http::header::CONTENT_TYPE, HeaderValue::from_static(mime));
                                conn.resp_headers
                                    .insert(http::header::CACHE_CONTROL, self.cache_control.clone());

                                if etag_match {
                                    return conn.put_status(StatusCode::NOT_MODIFIED)
                                        .put_resp_body(bytes::Bytes::new())
                                        .halt();
                                }

                                if !is_head {
                                    conn = conn.put_resp_body(contents);
                                }

                                return conn.halt();
                            }
                        }
                    }
                }
            }

            // Read file (no precompressed variant found or non-compressible)
            let contents = match tokio::fs::read(&canonical).await {
                Ok(c) => c,
                Err(_) => return conn,
            };

            let len = contents.len().to_string();
            let is_head = conn.method == Method::HEAD;

            let (conn, etag_match) = check_etag(conn, &contents);
            let mut conn = conn
                .put_status(StatusCode::OK)
                .put_resp_header("content-length", len.as_str());
            conn.resp_headers
                .insert(http::header::CONTENT_TYPE, HeaderValue::from_static(mime));
            conn.resp_headers
                .insert(http::header::CACHE_CONTROL, self.cache_control.clone());

            if compressible {
                conn = conn.put_resp_header("vary", "accept-encoding");
            }

            if etag_match {
                return conn.put_status(StatusCode::NOT_MODIFIED)
                    .put_resp_body(bytes::Bytes::new())
                    .halt();
            }

            if !is_head {
                conn = conn.put_resp_body(contents);
            }

            conn.halt()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mahalo_static_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn serves_file_with_correct_content_type() {
        let dir = temp_dir("serve");
        fs::write(dir.join("style.css"), "body { color: red; }").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/static/style.css".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_headers.get("content-type").unwrap(), "text/css");
        assert_eq!(conn.resp_headers.get("cache-control").unwrap(), "public, max-age=3600");
        assert_eq!(conn.resp_body, "body { color: red; }");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = temp_dir("traversal");
        fs::write(dir.join("secret.txt"), "secret").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/static/../secret.txt".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn passes_through_non_matching_prefix() {
        let dir = temp_dir("prefix");
        fs::write(dir.join("index.html"), "<html></html>").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/api/index.html".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn passes_through_for_post() {
        let dir = temp_dir("post");
        fs::write(dir.join("file.txt"), "hello").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::POST, "/static/file.txt".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn head_returns_headers_but_empty_body() {
        let dir = temp_dir("head");
        fs::write(dir.join("page.html"), "<h1>Hello</h1>").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::HEAD, "/static/page.html".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("content-type").unwrap(), "text/html");
        assert_eq!(conn.resp_headers.get("content-length").unwrap(), "14");
        assert!(conn.resp_body.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nonexistent_file_passes_through() {
        let dir = temp_dir("missing");

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/static/nope.txt".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serves_precompressed_brotli() {
        let dir = temp_dir("precomp_br");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        fs::write(dir.join("app.js.br"), "compressed-brotli-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "gzip, br".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("content-encoding").unwrap(), "br");
        assert_eq!(conn.resp_headers.get("vary").unwrap(), "accept-encoding");
        assert_eq!(conn.resp_headers.get("content-type").unwrap(), "text/javascript");
        assert_eq!(conn.resp_body.as_ref(), b"compressed-brotli-data");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_precompressed_for_non_compressible() {
        let dir = temp_dir("precomp_skip");
        fs::write(dir.join("photo.png"), "png-data").unwrap();
        fs::write(dir.join("photo.png.br"), "compressed-png").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::GET, "/static/photo.png".parse().unwrap());
        conn.headers.insert("accept-encoding", "br".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert!(conn.resp_headers.get("content-encoding").is_none());
        assert!(conn.resp_headers.get("vary").is_none());
        assert_eq!(conn.resp_body.as_ref(), b"png-data");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn compressible_file_sets_vary() {
        let dir = temp_dir("vary");
        fs::write(dir.join("style.css"), "body {}").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/static/style.css".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("vary").unwrap(), "accept-encoding");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn head_on_precompressed_returns_headers_no_body() {
        let dir = temp_dir("head_precomp");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        fs::write(dir.join("app.js.br"), "compressed-brotli-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::HEAD, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "br".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_headers.get("content-encoding").unwrap(), "br");
        assert_eq!(conn.resp_headers.get("content-type").unwrap(), "text/javascript");
        assert!(conn.resp_body.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gzip_fallback_when_br_missing() {
        let dir = temp_dir("gz_fallback");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        // Only .gz variant exists, no .br
        fs::write(dir.join("app.js.gz"), "gzip-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "gzip, br".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("content-encoding").unwrap(), "gzip");
        assert_eq!(conn.resp_body.as_ref(), b"gzip-data");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serves_zstd_variant() {
        let dir = temp_dir("zstd");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        fs::write(dir.join("app.js.zst"), "zstd-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "zstd".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("content-encoding").unwrap(), "zstd");
        assert_eq!(conn.resp_body.as_ref(), b"zstd-data");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn br_preferred_over_gzip_when_both_available() {
        let dir = temp_dir("priority");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        fs::write(dir.join("app.js.br"), "br-data").unwrap();
        fs::write(dir.join("app.js.gz"), "gz-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "gzip, br".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.resp_headers.get("content-encoding").unwrap(), "br");
        assert_eq!(conn.resp_body.as_ref(), b"br-data");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn etag_set_on_static_file() {
        let dir = temp_dir("etag");
        fs::write(dir.join("style.css"), "body {}").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());
        let conn = Conn::new(Method::GET, "/static/style.css".parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap();
        assert!(etag.starts_with("W/\""));
        assert_eq!(etag.len(), 36);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn static_file_304_on_matching_if_none_match() {
        let dir = temp_dir("etag304");
        fs::write(dir.join("style.css"), "body {}").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());

        // First request to get the ETag
        let conn = Conn::new(Method::GET, "/static/style.css".parse().unwrap());
        let conn = plug.call(conn).await;
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap().to_owned();

        // Second request with matching if-none-match
        let mut conn = Conn::new(Method::GET, "/static/style.css".parse().unwrap());
        conn.headers.insert("if-none-match", etag.parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::NOT_MODIFIED);
        assert!(conn.resp_body.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn precompressed_etag_and_304() {
        let dir = temp_dir("etag_precomp");
        fs::write(dir.join("app.js"), "console.log('hello')").unwrap();
        fs::write(dir.join("app.js.br"), "br-data").unwrap();

        let plug = StaticFiles::new("/static", dir.clone());

        // First request
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "br".parse().unwrap());
        let conn = plug.call(conn).await;
        let etag = conn.resp_headers.get("etag").unwrap().to_str().unwrap().to_owned();

        // Conditional request
        let mut conn = Conn::new(Method::GET, "/static/app.js".parse().unwrap());
        conn.headers.insert("accept-encoding", "br".parse().unwrap());
        conn.headers.insert("if-none-match", etag.parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::NOT_MODIFIED);
        assert!(conn.resp_body.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_compressible() {
        assert!(is_compressible("text/html"));
        assert!(is_compressible("text/css"));
        assert!(is_compressible("text/javascript"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("image/svg+xml"));

        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("image/jpeg"));
        assert!(!is_compressible("video/mp4"));
        assert!(!is_compressible("audio/mpeg"));
        assert!(!is_compressible("font/woff2"));
        assert!(!is_compressible("application/zip"));
        assert!(!is_compressible("application/gzip"));
        assert!(!is_compressible("application/octet-stream"));
    }
}
