// Static File Serving plug
use std::path::PathBuf;

use http::{Method, StatusCode};
use mahalo_core::conn::Conn;
use mahalo_core::plug::{BoxFuture, Plug};

/// Serves static files from a directory under a URL prefix.
pub struct StaticFiles {
    url_prefix: String,
    dir: PathBuf,
    cache_control: String,
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
            cache_control: "public, max-age=3600".to_string(),
        }
    }

    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = value.into();
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
                if rest.starts_with('/') {
                    &rest[1..]
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

            // Read file
            let contents = match tokio::fs::read(&canonical).await {
                Ok(c) => c,
                Err(_) => return conn,
            };

            // Determine content type
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let mime = mime_from_extension(ext);
            let len = contents.len().to_string();
            let is_head = conn.method == Method::HEAD;

            let mut conn = conn
                .put_status(StatusCode::OK)
                .put_resp_header("content-type", mime)
                .put_resp_header("cache-control", self.cache_control.as_str())
                .put_resp_header("content-length", len.as_str());

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
}
