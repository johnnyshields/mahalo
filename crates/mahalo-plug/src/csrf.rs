use mahalo_core::conn::{AssignKey, Conn};
use mahalo_core::plug::{BoxFuture, Plug};

use base64::Engine;
use hmac::{Hmac, Mac};
use http::{Method, StatusCode};
use rand::Rng;
use sha2::Sha256;

pub struct CsrfToken;
impl AssignKey for CsrfToken {
    type Value = String;
}

pub struct CsrfProtection {
    secret: Vec<u8>,
}

impl CsrfProtection {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
        }
    }

    fn generate_token(&self) -> String {
        let mut random_bytes = [0u8; 32];
        rand::rng().fill(&mut random_bytes);

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key size");
        mac.update(&random_bytes);
        let hmac_result = mac.finalize().into_bytes();

        let mut combined = Vec::with_capacity(64);
        combined.extend_from_slice(&random_bytes);
        combined.extend_from_slice(&hmac_result);

        base64::engine::general_purpose::STANDARD.encode(&combined)
    }

    fn validate_token(&self, token: &str) -> bool {
        let decoded = match base64::engine::general_purpose::STANDARD.decode(token) {
            Ok(d) => d,
            Err(_) => return false,
        };

        if decoded.len() != 64 {
            return false;
        }

        let (random_bytes, provided_mac) = decoded.split_at(32);

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key size");
        mac.update(random_bytes);

        mac.verify_slice(provided_mac).is_ok()
    }

    fn is_safe_method(method: &Method) -> bool {
        matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
    }
}

impl Plug for CsrfProtection {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            if Self::is_safe_method(&conn.method) {
                let token = self.generate_token();
                conn.assign::<CsrfToken>(token.clone())
                    .put_resp_header("x-csrf-token", token)
            } else {
                // Unsafe method: validate token
                let token = conn
                    .headers
                    .get("x-csrf-token")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_owned());

                match token {
                    None => conn
                        .put_status(StatusCode::FORBIDDEN)
                        .put_resp_body("Forbidden")
                        .halt(),
                    Some(t) if !self.validate_token(&t) => conn
                        .put_status(StatusCode::FORBIDDEN)
                        .put_resp_body("Forbidden")
                        .halt(),
                    Some(t) => conn.assign::<CsrfToken>(t),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Uri;

    fn make_conn(method: Method) -> Conn {
        Conn::new(method, Uri::from_static("/test"))
    }

    #[tokio::test]
    async fn safe_method_generates_token_and_sets_header() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::GET);

        let conn = plug.call(conn).await;

        let assign = conn.get_assign::<CsrfToken>().expect("token in assigns");
        assert!(!assign.is_empty());

        let header = conn
            .resp_headers
            .get("x-csrf-token")
            .expect("header set")
            .to_str()
            .unwrap();
        assert_eq!(header, assign);
    }

    #[tokio::test]
    async fn valid_token_on_post_passes_through() {
        let plug = CsrfProtection::new("test-secret");

        // Generate a valid token
        let token = plug.generate_token();

        let mut conn = make_conn(Method::POST);
        conn.headers
            .insert("x-csrf-token", token.parse().unwrap());

        let conn = plug.call(conn).await;

        assert!(!conn.halted);
        assert_eq!(conn.status, StatusCode::OK);
        assert!(conn.get_assign::<CsrfToken>().is_some());
    }

    #[tokio::test]
    async fn missing_token_on_post_returns_403() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::POST);

        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn invalid_token_returns_403() {
        let plug = CsrfProtection::new("test-secret");

        let mut conn = make_conn(Method::DELETE);
        conn.headers
            .insert("x-csrf-token", "totally-invalid-token".parse().unwrap());

        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn tampered_token_returns_403() {
        let plug = CsrfProtection::new("test-secret");
        let token = plug.generate_token();

        // Tamper with the token by using a different secret
        let other_plug = CsrfProtection::new("other-secret");
        let bad_token = other_plug.generate_token();

        let mut conn = make_conn(Method::POST);
        conn.headers
            .insert("x-csrf-token", bad_token.parse().unwrap());

        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);

        // Also verify original token works with original secret
        let mut conn2 = make_conn(Method::POST);
        conn2
            .headers
            .insert("x-csrf-token", token.parse().unwrap());
        let conn2 = plug.call(conn2).await;
        assert!(!conn2.halted);
    }

    #[tokio::test]
    async fn head_generates_token() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::HEAD);
        let conn = plug.call(conn).await;

        assert!(!conn.halted);
        assert!(conn.get_assign::<CsrfToken>().is_some());
        assert!(conn.resp_headers.get("x-csrf-token").is_some());
    }

    #[tokio::test]
    async fn options_generates_token() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::OPTIONS);
        let conn = plug.call(conn).await;

        assert!(!conn.halted);
        assert!(conn.get_assign::<CsrfToken>().is_some());
        assert!(conn.resp_headers.get("x-csrf-token").is_some());
    }

    #[tokio::test]
    async fn put_without_token_returns_403() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::PUT);
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn patch_without_token_returns_403() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::PATCH);
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_without_token_returns_403() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::DELETE);
        let conn = plug.call(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn put_with_valid_token_passes() {
        let plug = CsrfProtection::new("test-secret");
        let token = plug.generate_token();
        let mut conn = make_conn(Method::PUT);
        conn.headers.insert("x-csrf-token", token.parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);
    }

    #[tokio::test]
    async fn patch_with_valid_token_passes() {
        let plug = CsrfProtection::new("test-secret");
        let token = plug.generate_token();
        let mut conn = make_conn(Method::PATCH);
        conn.headers.insert("x-csrf-token", token.parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);
    }

    #[tokio::test]
    async fn delete_with_valid_token_passes() {
        let plug = CsrfProtection::new("test-secret");
        let token = plug.generate_token();
        let mut conn = make_conn(Method::DELETE);
        conn.headers.insert("x-csrf-token", token.parse().unwrap());
        let conn = plug.call(conn).await;

        assert!(!conn.halted);
    }

    #[tokio::test]
    async fn get_passes_through_without_token() {
        let plug = CsrfProtection::new("test-secret");
        let conn = make_conn(Method::GET);

        let conn = plug.call(conn).await;

        assert!(!conn.halted);
        assert_eq!(conn.status, StatusCode::OK);
        // Token is generated for safe methods, not required
        assert!(conn.get_assign::<CsrfToken>().is_some());
    }
}
