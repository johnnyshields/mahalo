use mahalo_core::conn::{AssignKey, Conn};
use mahalo_core::plug::{BoxFuture, Plug};
use std::time::Instant;

pub struct RequestStartTime;
impl AssignKey for RequestStartTime {
    type Value = Instant;
}

pub fn request_logger() -> (impl Plug, impl Plug) {
    (LoggerStart, LoggerFinish)
}

struct LoggerStart;
impl Plug for LoggerStart {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            conn.assign::<RequestStartTime>(Instant::now())
        })
    }
}

struct LoggerFinish;
impl Plug for LoggerFinish {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            if let Some(start) = conn.get_assign::<RequestStartTime>() {
                let elapsed = start.elapsed();
                tracing::info!(
                    method = %conn.method,
                    path = %conn.uri.path(),
                    status = conn.status.as_u16(),
                    duration_ms = elapsed.as_millis() as u64,
                    "Request completed"
                );
            } else {
                tracing::info!(
                    method = %conn.method,
                    path = %conn.uri.path(),
                    status = conn.status.as_u16(),
                    "Request completed"
                );
            }
            conn
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, StatusCode, Uri};
    use mahalo_core::plug::Plug;

    #[monoio::test(enable_timer = true)]
    async fn start_plug_stores_instant() {
        let (start, _finish) = request_logger();
        let conn = Conn::new(Method::GET, Uri::from_static("/test"));
        let conn = start.call(conn).await;
        assert!(conn.get_assign::<RequestStartTime>().is_some());
    }

    #[monoio::test(enable_timer = true)]
    async fn finish_plug_does_not_panic_without_start() {
        let (_start, finish) = request_logger();
        let conn = Conn::new(Method::GET, Uri::from_static("/test"))
            .put_status(StatusCode::NOT_FOUND);
        let conn = finish.call(conn).await;
        assert_eq!(conn.status, StatusCode::NOT_FOUND);
    }

    #[monoio::test(enable_timer = true)]
    async fn full_round_trip() {
        let (start, finish) = request_logger();
        let conn = Conn::new(Method::POST, Uri::from_static("/api/users"))
            .put_status(StatusCode::CREATED);
        let conn = start.call(conn).await;
        let conn = finish.call(conn).await;
        assert_eq!(conn.status, StatusCode::CREATED);
        assert!(conn.get_assign::<RequestStartTime>().is_some());
    }
}
