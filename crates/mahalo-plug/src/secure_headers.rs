use mahalo_core::conn::Conn;
use mahalo_core::plug::{BoxFuture, Plug};

pub struct SecureHeaders {
    headers: Vec<(String, String)>,
}

impl SecureHeaders {
    pub fn new() -> Self {
        Self {
            headers: vec![
                ("x-content-type-options".into(), "nosniff".into()),
                ("x-frame-options".into(), "SAMEORIGIN".into()),
                ("x-xss-protection".into(), "1; mode=block".into()),
                (
                    "strict-transport-security".into(),
                    "max-age=31536000; includeSubDomains".into(),
                ),
                ("x-download-options".into(), "noopen".into()),
                ("x-permitted-cross-domain-policies".into(), "none".into()),
                (
                    "referrer-policy".into(),
                    "strict-origin-when-cross-origin".into(),
                ),
            ],
        }
    }

    pub fn put(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        if let Some(existing) = self.headers.iter_mut().find(|(k, _)| k == &name) {
            existing.1 = value.into();
        } else {
            self.headers.push((name, value.into()));
        }
        self
    }

    pub fn remove(mut self, name: &str) -> Self {
        self.headers.retain(|(k, _)| k != name);
        self
    }
}

impl Default for SecureHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl Plug for SecureHeaders {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let mut conn = conn;
            for (name, value) in &self.headers {
                conn = conn.put_resp_header(name.as_str(), value.as_str());
            }
            conn
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, Uri};

    fn test_conn() -> Conn {
        Conn::new(Method::GET, Uri::from_static("/"))
    }

    #[tokio::test]
    async fn test_default_headers_applied() {
        let plug = SecureHeaders::new();
        let conn = plug.call(test_conn()).await;

        assert_eq!(
            conn.resp_headers.get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            conn.resp_headers.get("x-frame-options").unwrap(),
            "SAMEORIGIN"
        );
        assert_eq!(
            conn.resp_headers.get("x-xss-protection").unwrap(),
            "1; mode=block"
        );
        assert_eq!(
            conn.resp_headers.get("strict-transport-security").unwrap(),
            "max-age=31536000; includeSubDomains"
        );
        assert_eq!(
            conn.resp_headers.get("x-download-options").unwrap(),
            "noopen"
        );
        assert_eq!(
            conn.resp_headers
                .get("x-permitted-cross-domain-policies")
                .unwrap(),
            "none"
        );
        assert_eq!(
            conn.resp_headers.get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
    }

    #[tokio::test]
    async fn test_put_adds_header() {
        let plug = SecureHeaders::new().put("x-custom", "custom-value");
        let conn = plug.call(test_conn()).await;

        assert_eq!(
            conn.resp_headers.get("x-custom").unwrap(),
            "custom-value"
        );
        // Default headers still present
        assert_eq!(
            conn.resp_headers.get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }

    #[tokio::test]
    async fn test_put_replaces_header() {
        let plug = SecureHeaders::new().put("x-frame-options", "DENY");
        let conn = plug.call(test_conn()).await;

        assert_eq!(conn.resp_headers.get("x-frame-options").unwrap(), "DENY");
    }

    #[tokio::test]
    async fn test_remove_header() {
        let plug = SecureHeaders::new().remove("x-xss-protection");
        let conn = plug.call(test_conn()).await;

        assert!(conn.resp_headers.get("x-xss-protection").is_none());
        // Other headers still present
        assert_eq!(
            conn.resp_headers.get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }

    #[tokio::test]
    async fn test_existing_response_headers_preserved() {
        let plug = SecureHeaders::new();
        let conn = test_conn().put_resp_header("x-existing", "preserved");
        let conn = plug.call(conn).await;

        assert_eq!(
            conn.resp_headers.get("x-existing").unwrap(),
            "preserved"
        );
        assert_eq!(
            conn.resp_headers.get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }
}
