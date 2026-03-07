use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{SupervisorHandle, start_supervisor};
use rebar_core::supervisor::spec::{RestartStrategy, SupervisorSpec};

use mahalo_channel::socket::ChannelRouter;
use mahalo_channel::supervisor::{ChannelSupervisor, ChannelSupervisorHandle};
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
pub struct MahaloApplication {
    addr: SocketAddr,
    router_factory: Arc<dyn Fn() -> MahaloRouter + Send + Sync>,
    channel_router_factory: Option<Arc<dyn Fn() -> ChannelRouter + Send + Sync>>,
}

impl MahaloApplication {
    /// Create a new application builder.
    pub fn builder() -> MahaloApplicationBuilder {
        MahaloApplicationBuilder::default()
    }

    /// Start the application, returning the top-level supervisor handle.
    ///
    /// The returned `SupervisorHandle` keeps the supervision tree running.
    /// Drop it or call `.shutdown()` to stop.
    pub async fn start(self) -> (SupervisorHandle, PubSub, Option<ChannelSupervisorHandle>) {
        let runtime = Rc::new(Runtime::new(1));
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let mut children = Vec::new();

        // 1. PubSub (Permanent) — properly supervised
        let (pubsub, pubsub_entry) = PubSub::new_supervised();
        children.push(pubsub_entry);

        // 2. ChannelSupervisor (Permanent) — if WebSocket configured
        let channel_supervisor = if self.channel_router_factory.is_some() {
            let (handle, entry) = ChannelSupervisor::child_entry(&runtime);
            children.push(entry);
            Some(handle)
        } else {
            None
        };

        // 3. HTTP Endpoint (Permanent) — with optional WebSocket support
        let rf = self.router_factory;
        let mut endpoint = MahaloEndpoint::new(move || rf(), self.addr);
        if let Some(cr_factory) = self.channel_router_factory {
            let pubsub = pubsub.clone();
            endpoint = endpoint.channels(move || {
                use crate::endpoint::WsConfig;
                WsConfig {
                    channel_router: Rc::new(cr_factory()),
                    pubsub: pubsub.clone(),
                }
            });
        }
        children.push(endpoint.child_entry());

        let handle = start_supervisor(&runtime, spec, children);

        // Give supervised processes a moment to start.
        monoio::time::sleep(std::time::Duration::from_millis(10)).await;

        (handle, pubsub, channel_supervisor)
    }
}

/// Builder for [`MahaloApplication`].
pub struct MahaloApplicationBuilder {
    addr: Option<SocketAddr>,
    router_factory: Option<Arc<dyn Fn() -> MahaloRouter + Send + Sync>>,
    channel_router_factory: Option<Arc<dyn Fn() -> ChannelRouter + Send + Sync>>,
}

impl Default for MahaloApplicationBuilder {
    fn default() -> Self {
        Self {
            addr: None,
            router_factory: None,
            channel_router_factory: None,
        }
    }
}

impl MahaloApplicationBuilder {
    /// Set the socket address to bind the HTTP server to.
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Set the Mahalo router factory.
    pub fn router(mut self, factory: impl Fn() -> MahaloRouter + Send + Sync + 'static) -> Self {
        self.router_factory = Some(Arc::new(factory));
        self
    }

    /// Set the channel router factory for WebSocket support.
    pub fn channel_router(
        mut self,
        factory: impl Fn() -> ChannelRouter + Send + Sync + 'static,
    ) -> Self {
        self.channel_router_factory = Some(Arc::new(factory));
        self
    }

    /// Build the application. Panics if required fields (addr) are not set.
    pub fn build(self) -> MahaloApplication {
        let addr = self.addr.expect("MahaloApplicationBuilder: addr is required");
        let router_factory = self.router_factory
            .unwrap_or_else(|| Arc::new(MahaloRouter::new));

        MahaloApplication {
            addr,
            router_factory,
            channel_router_factory: self.channel_router_factory,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(app.channel_router_factory.is_none());
    }

    // Integration tests temporarily removed — they need to be adapted for monoio.
}
