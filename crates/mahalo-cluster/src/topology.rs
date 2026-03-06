use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;

/// A strategy for discovering cluster peers.
#[async_trait]
pub trait TopologyStrategy: Send + Sync + 'static {
    /// Get the initial list of peer addresses on startup.
    async fn initial_peers(&self) -> Vec<SocketAddr>;

    /// Poll for the current peer list (may change over time).
    async fn poll_peers(&self) -> Vec<SocketAddr>;

    /// How often to poll (default: 5 seconds).
    fn poll_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
}

/// A static list of peer addresses.
pub struct StaticTopology {
    peers: Vec<SocketAddr>,
}

impl StaticTopology {
    pub fn new(peers: Vec<SocketAddr>) -> Self {
        Self { peers }
    }
}

#[async_trait]
impl TopologyStrategy for StaticTopology {
    async fn initial_peers(&self) -> Vec<SocketAddr> {
        self.peers.clone()
    }

    async fn poll_peers(&self) -> Vec<SocketAddr> {
        self.peers.clone()
    }
}

/// DNS-based topology discovery. Resolves a hostname and collects all IP:port pairs.
pub struct DnsTopology {
    hostname: String,
    port: u16,
}

impl DnsTopology {
    pub fn new(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            port,
        }
    }

    async fn resolve(&self) -> Vec<SocketAddr> {
        use tokio::net::lookup_host;
        let query = format!("{}:{}", self.hostname, self.port);
        match lookup_host(&query).await {
            Ok(addrs) => addrs.collect(),
            Err(e) => {
                tracing::warn!(hostname = %self.hostname, "DNS topology lookup failed: {}", e);
                vec![]
            }
        }
    }
}

#[async_trait]
impl TopologyStrategy for DnsTopology {
    async fn initial_peers(&self) -> Vec<SocketAddr> {
        self.resolve().await
    }

    async fn poll_peers(&self) -> Vec<SocketAddr> {
        self.resolve().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_topology_returns_peers() {
        let peers: Vec<SocketAddr> = vec!["127.0.0.1:4000".parse().unwrap()];
        let topo = StaticTopology::new(peers.clone());
        assert_eq!(topo.initial_peers().await, peers);
        assert_eq!(topo.poll_peers().await, peers);
    }

    #[test]
    fn static_topology_default_interval() {
        let topo = StaticTopology::new(vec![]);
        assert_eq!(topo.poll_interval(), Duration::from_secs(5));
    }
}
