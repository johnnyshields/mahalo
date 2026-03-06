use std::net::SocketAddr;
use std::sync::Arc;

use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{SupervisorHandle, start_supervisor};
use rebar_core::supervisor::spec::{RestartStrategy, SupervisorSpec};

use mahalo_channel::socket::ChannelRouter;
use mahalo_channel::supervisor::ChannelSupervisor;
use mahalo_pubsub::PubSub;
use mahalo_router::MahaloRouter;

use crate::endpoint::MahaloEndpoint;

/// Top-level application that wires together the full supervision tree.
///
/// ```text
/// MahaloSupervisor (OneForOne)
///   ├── mahalo_pubsub (Permanent)
///   ├── mahalo_channel_supervisor (Permanent)
///   └── mahalo_endpoint (Permanent)
/// ```
///
/// # Example
///
/// ```ignore
/// let _supervisor = MahaloApplication::builder()
///     .bind("0.0.0.0:4000".parse().unwrap())
///     .router(build_router())
///     .channel_router(build_channel_router())
///     .build()
///     .start()
///     .await;
/// tokio::signal::ctrl_c().await.ok();
/// ```
pub struct MahaloApplication {
    addr: SocketAddr,
    router: MahaloRouter,
    channel_router: Option<ChannelRouter>,
    runtime: Arc<Runtime>,
    // Internal: holds the started PubSub handle after build.
    pubsub: Option<PubSub>,
}

impl MahaloApplication {
    /// Create a new application builder.
    pub fn builder() -> MahaloApplicationBuilder {
        MahaloApplicationBuilder::default()
    }

    /// Returns the PubSub handle. Available after [`start()`] returns.
    ///
    /// Panics if called before `start()`.
    pub fn pubsub(&self) -> &PubSub {
        self.pubsub.as_ref().expect("PubSub not yet started")
    }

    /// Start the application, returning the top-level supervisor handle.
    ///
    /// The returned `SupervisorHandle` keeps the supervision tree running.
    /// Drop it or call `.shutdown()` to stop.
    pub async fn start(mut self) -> (SupervisorHandle, PubSub) {
        let runtime = Arc::clone(&self.runtime);
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let mut children = Vec::new();

        // 1. PubSub (Permanent) — properly supervised
        let (pubsub, pubsub_entry) = PubSub::new_supervised();
        children.push(pubsub_entry);
        self.pubsub = Some(pubsub.clone());

        // 2. ChannelSupervisor (Permanent) — if WebSocket configured
        if self.channel_router.is_some() {
            let (_handle, entry) = ChannelSupervisor::child_entry(Arc::clone(&runtime));
            children.push(entry);
        }

        // 3. HTTP Endpoint (Permanent)
        let endpoint = MahaloEndpoint::new(self.router, self.addr, Arc::clone(&runtime));
        children.push(endpoint.child_entry());

        let handle = start_supervisor(runtime, spec, children).await;

        // Give supervised processes a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        (handle, pubsub)
    }
}

/// Builder for [`MahaloApplication`].
pub struct MahaloApplicationBuilder {
    addr: Option<SocketAddr>,
    router: Option<MahaloRouter>,
    channel_router: Option<ChannelRouter>,
    node_id: u64,
}

impl Default for MahaloApplicationBuilder {
    fn default() -> Self {
        Self {
            addr: None,
            router: None,
            channel_router: None,
            node_id: 1,
        }
    }
}

impl MahaloApplicationBuilder {
    /// Set the rebar runtime node ID (default: 1).
    pub fn node_id(mut self, id: u64) -> Self {
        self.node_id = id;
        self
    }

    /// Set the socket address to bind the HTTP server to.
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Set the Mahalo router.
    pub fn router(mut self, r: MahaloRouter) -> Self {
        self.router = Some(r);
        self
    }

    /// Set the channel router for WebSocket support.
    pub fn channel_router(mut self, cr: ChannelRouter) -> Self {
        self.channel_router = Some(cr);
        self
    }

    /// Build the application. Panics if required fields (addr, router) are not set.
    pub fn build(self) -> MahaloApplication {
        let addr = self.addr.expect("MahaloApplicationBuilder: addr is required");
        let router = self.router.unwrap_or_default();
        let runtime = Arc::new(Runtime::new(self.node_id));

        MahaloApplication {
            addr,
            router,
            channel_router: self.channel_router,
            runtime,
            pubsub: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use mahalo_core::conn::Conn;
    use mahalo_core::plug::plug_fn;

    #[test]
    fn builder_requires_addr() {
        let result = std::panic::catch_unwind(|| {
            MahaloApplicationBuilder::default().build();
        });
        assert!(result.is_err(), "should panic without addr");
    }

    #[test]
    fn builder_with_defaults() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let app = MahaloApplication::builder().bind(addr).build();
        assert_eq!(app.addr, addr);
        assert!(app.channel_router.is_none());
    }

    #[tokio::test]
    async fn start_creates_supervision_tree() {
        let router = MahaloRouter::new().get(
            "/health",
            plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK).put_resp_body("ok") }),
        );
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let (supervisor, pubsub) = MahaloApplication::builder()
            .bind(addr)
            .router(router)
            .build()
            .start()
            .await;

        // PubSub should be functional
        let mut rx = pubsub.subscribe("test:topic").await.unwrap();
        pubsub.broadcast("test:topic", "hello", serde_json::json!({}));
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.recv(),
        )
        .await
        .expect("timed out")
        .expect("recv error");
        assert_eq!(msg.event, "hello");

        supervisor.shutdown();
    }
}
