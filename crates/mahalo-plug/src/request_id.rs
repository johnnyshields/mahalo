use mahalo_core::conn::{AssignKey, Conn};
use mahalo_core::plug::{BoxFuture, Plug};

pub struct RequestIdKey;
impl AssignKey for RequestIdKey {
    type Value = String;
}

pub struct RequestId;

impl Plug for RequestId {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = conn
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            conn.assign::<RequestIdKey>(id.clone())
                .put_resp_header("x-request-id", id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, Uri};

    #[monoio::test(enable_timer = true)]
    async fn preserves_existing_request_id_header() {
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        conn.headers
            .insert("x-request-id", "my-custom-id".parse().unwrap());

        let conn = RequestId.call(conn).await;

        assert_eq!(
            conn.resp_headers.get("x-request-id").unwrap(),
            "my-custom-id"
        );
        assert_eq!(
            conn.get_assign::<RequestIdKey>(),
            Some(&"my-custom-id".to_string())
        );
    }

    #[monoio::test(enable_timer = true)]
    async fn generates_uuid_when_header_missing() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = RequestId.call(conn).await;

        let id = conn.resp_headers.get("x-request-id").unwrap().to_str().unwrap();
        assert!(!id.is_empty());
        assert_eq!(conn.get_assign::<RequestIdKey>(), Some(&id.to_string()));
    }

    #[monoio::test(enable_timer = true)]
    async fn generated_id_is_valid_uuid() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = RequestId.call(conn).await;

        let id = conn.resp_headers.get("x-request-id").unwrap().to_str().unwrap();
        let parsed = uuid::Uuid::parse_str(id).expect("should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
    }
}
