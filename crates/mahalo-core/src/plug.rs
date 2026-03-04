use crate::conn::Conn;
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The core Plug trait -- every middleware is a Plug.
pub trait Plug: Send + Sync + 'static {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn>;
}

/// Wrapper to make async functions into Plugs.
pub struct PlugFn<F>(pub F);

impl<F, Fut> Plug for PlugFn<F>
where
    F: Fn(Conn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Conn> + Send + 'static,
{
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(self.0(conn))
    }
}

/// Helper to create a `PlugFn` from an async function.
pub fn plug_fn<F, Fut>(f: F) -> PlugFn<F>
where
    F: Fn(Conn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Conn> + Send + 'static,
{
    PlugFn(f)
}
