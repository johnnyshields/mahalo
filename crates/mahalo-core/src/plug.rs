use crate::conn::Conn;
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The core Plug trait -- every middleware is a Plug.
pub trait Plug: 'static {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn>;

    /// Optional synchronous fast-path. If a plug can execute without async,
    /// override this to return `Ok(conn)` and avoid the BoxFuture allocation.
    ///
    /// Returns `Ok(conn)` when the plug handled the request synchronously, or
    /// `Err(conn)` (giving it back unchanged) to signal that the caller should
    /// fall back to the async `call()` method instead.
    ///
    /// The default returns `Err(conn)`, meaning `call()` will be used.
    #[inline]
    fn call_sync(&self, conn: Conn) -> Result<Conn, Conn> {
        Err(conn)
    }
}

/// Wrapper to make async functions into Plugs.
pub struct PlugFn<F>(pub F);

impl<F, Fut> Plug for PlugFn<F>
where
    F: Fn(Conn) -> Fut + 'static,
    Fut: Future<Output = Conn> + 'static,
{
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(self.0(conn))
    }
}

/// Helper to create a `PlugFn` from an async function.
pub fn plug_fn<F, Fut>(f: F) -> PlugFn<F>
where
    F: Fn(Conn) -> Fut + 'static,
    Fut: Future<Output = Conn> + 'static,
{
    PlugFn(f)
}

/// Wrapper for synchronous plug functions — avoids BoxFuture heap allocation.
pub struct SyncPlugFn<F>(pub F);

impl<F> Plug for SyncPlugFn<F>
where
    F: Fn(Conn) -> Conn + 'static,
{
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(std::future::ready(self.0(conn)))
    }

    #[inline]
    fn call_sync(&self, conn: Conn) -> Result<Conn, Conn> {
        Ok(self.0(conn))
    }
}

/// Create a plug from a synchronous function. Zero-allocation fast path —
/// avoids BoxFuture heap allocation when used with the optimized pipeline.
pub fn sync_plug_fn<F>(f: F) -> SyncPlugFn<F>
where
    F: Fn(Conn) -> Conn + 'static,
{
    SyncPlugFn(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, StatusCode, Uri};

    #[tokio::test]
    async fn plug_fn_creates_callable_plug() {
        let plug = plug_fn(|conn: Conn| async {
            conn.put_status(StatusCode::IM_A_TEAPOT)
        });
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = plug.call(conn).await;
        assert_eq!(conn.status, StatusCode::IM_A_TEAPOT);
    }

    #[tokio::test]
    async fn plug_fn_can_modify_body() {
        let plug = plug_fn(|conn: Conn| async {
            conn.put_resp_body("hello from plug")
        });
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = plug.call(conn).await;
        assert_eq!(conn.resp_body, "hello from plug");
    }

    #[test]
    fn sync_plug_fn_call_sync_returns_ok() {
        let plug = sync_plug_fn(|conn: Conn| conn.put_status(StatusCode::NOT_FOUND));
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        match plug.call_sync(conn) {
            Ok(conn) => assert_eq!(conn.status, StatusCode::NOT_FOUND),
            Err(_) => panic!("call_sync should return Ok for SyncPlugFn"),
        }
    }

    #[tokio::test]
    async fn sync_plug_fn_call_async_fallback() {
        let plug = sync_plug_fn(|conn: Conn| conn.put_resp_body("sync body"));
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = plug.call(conn).await;
        assert_eq!(conn.resp_body, "sync body");
    }

    #[test]
    fn async_plug_fn_call_sync_returns_err() {
        let plug = plug_fn(|conn: Conn| async { conn });
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = plug.call_sync(conn);
        assert!(result.is_err());
    }

}
