use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::executor::{ExecutorConfig, RebarExecutor};
use turbine_core::config::PoolConfig;
use rebar_core::io::TcpListener;
use rebar_core::runtime::Runtime;

use crate::endpoint::{ErrorHandler, WsConfig};

/// Spawn worker threads without joining them. Returns immediately.
///
/// Used by `child_entry` (supervision) where the supervisor manages the lifetime.
/// Takes Arc-wrapped factories that can be shared across worker threads.
pub(crate) fn spawn_workers(
    addr: SocketAddr,
    router_factory: Arc<dyn Fn() -> MahaloRouter + Send + Sync>,
    error_handler_factory: Option<Arc<dyn Fn() -> ErrorHandler + Send + Sync>>,
    after_plug_factories: Arc<Vec<Arc<dyn Fn() -> Box<dyn Plug> + Send + Sync>>>,
    body_limit: usize,
    ws_config_factory: Option<Arc<dyn Fn() -> WsConfig + Send + Sync>>,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    for i in 0..num_workers {
        let router_factory = Arc::clone(&router_factory);
        let error_handler_factory = error_handler_factory.clone();
        let after_plug_factories = Arc::clone(&after_plug_factories);
        let ws_config_factory = ws_config_factory.clone();
        let tls_config = tls_config.clone();

        std::thread::Builder::new()
            .name(format!("mahalo-worker-{i}"))
            .spawn(move || {
                run_worker_arc(
                    i, addr,
                    &*router_factory,
                    error_handler_factory.as_deref(),
                    &*after_plug_factories,
                    body_limit,
                    ws_config_factory.as_deref(),
                    tls_config,
                );
            })?;
    }

    Ok(())
}

/// Start the HTTP server, blocking until all workers complete.
///
/// Uses `std::thread::scope` so factory references can be borrowed across threads.
/// Used by `MahaloEndpoint::start()`.
pub(crate) fn start_server(
    addr: SocketAddr,
    router_factory: Arc<dyn Fn() -> MahaloRouter + Send + Sync>,
    error_handler_factory: Option<Arc<dyn Fn() -> ErrorHandler + Send + Sync>>,
    after_plug_factories: Vec<Arc<dyn Fn() -> Box<dyn Plug> + Send + Sync>>,
    body_limit: usize,
    ws_config_factory: Option<Arc<dyn Fn() -> WsConfig + Send + Sync>>,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(num_workers);

        for i in 0..num_workers {
            let rf = &*router_factory;
            let ehf = error_handler_factory.as_deref();
            let apf = &after_plug_factories;
            let wcf = ws_config_factory.as_deref();
            let tls = tls_config.clone();

            let handle = std::thread::Builder::new()
                .name(format!("mahalo-worker-{i}"))
                .spawn_scoped(scope, move || {
                    run_worker(i, addr, rf, ehf, apf, body_limit, wcf, tls);
                })
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(e)
                })?;

            handles.push(handle);
        }

        for handle in handles {
            handle
                .join()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("worker thread panicked: {:?}", e).into()
                })?;
        }

        Ok(())
    })
}

/// Worker body for Arc-based factories (used by spawn_workers).
fn run_worker_arc(
    worker_id: usize,
    addr: SocketAddr,
    router_factory: &(dyn Fn() -> MahaloRouter + Send + Sync),
    error_handler_factory: Option<&(dyn Fn() -> ErrorHandler + Send + Sync)>,
    after_plug_factories: &[Arc<dyn Fn() -> Box<dyn Plug> + Send + Sync>],
    body_limit: usize,
    ws_config_factory: Option<&(dyn Fn() -> WsConfig + Send + Sync)>,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) {
    setup_cpu_affinity(worker_id);

    let ex = RebarExecutor::new(ExecutorConfig {
        pool_config: Some(PoolConfig::default()),
        ..Default::default()
    })
    .expect("failed to build RebarExecutor");

    ex.block_on(async {
        let listener = match TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("mahalo-worker-{worker_id}: bind failed: {e}");
                return;
            }
        };

        let runtime = Rc::new(Runtime::new(worker_id as u64 + 1));
        let router = router_factory();
        let error_handler = error_handler_factory.map(|f| f());
        let after_plugs: Vec<Box<dyn Plug>> = after_plug_factories.iter().map(|f| f()).collect();
        let ws_config = ws_config_factory.map(|f| f());

        // Create thread-local TLS acceptor from shared config.
        let tls_acceptor = tls_config.map(rebar_core::tls::TlsAcceptor::new);

        crate::server::run_accept_loop(
            listener, router, error_handler, after_plugs,
            runtime, body_limit, ws_config, tls_acceptor,
        )
        .await;
    });
}

/// Worker body for reference-based factories (used by start_server via scoped threads).
fn run_worker(
    worker_id: usize,
    addr: SocketAddr,
    router_factory: &(dyn Fn() -> MahaloRouter + Send + Sync),
    error_handler_factory: Option<&(dyn Fn() -> ErrorHandler + Send + Sync)>,
    after_plug_factories: &[Arc<dyn Fn() -> Box<dyn Plug> + Send + Sync>],
    body_limit: usize,
    ws_config_factory: Option<&(dyn Fn() -> WsConfig + Send + Sync)>,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) {
    // Same implementation — delegate to shared code.
    run_worker_arc(
        worker_id, addr, router_factory, error_handler_factory,
        after_plug_factories, body_limit, ws_config_factory, tls_config,
    );
}

/// Best-effort CPU affinity pinning (Linux only).
fn setup_cpu_affinity(worker_id: usize) {
    #[cfg(target_os = "linux")]
    {
        let mut cpuset = nix::sched::CpuSet::new();
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if cpuset.set(worker_id % num_cpus).is_ok() {
            let _ = nix::sched::sched_setaffinity(
                nix::unistd::Pid::from_raw(0),
                &cpuset,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = worker_id;
}
