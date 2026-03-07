use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::thread::JoinHandle;

use io_uring::IoUring;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::uring::{BufferPool, ConnectionPool, run_event_loop};

/// Spawn io_uring worker threads, returning their join handles.
///
/// Each worker gets its own listening socket (SO_REUSEPORT), io_uring instance,
/// connection pool, buffer pool, and single-threaded tokio runtime.
fn spawn_workers(
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) -> Result<Vec<JoinHandle<()>>, Box<dyn std::error::Error + Send + Sync>> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let error_handler = Arc::new(error_handler);
    let mut handles = Vec::with_capacity(num_workers);

    for i in 0..num_workers {
        let router = Arc::clone(&router);
        let error_handler = Arc::clone(&error_handler);
        let after_plugs = Arc::clone(&after_plugs);
        let runtime = Arc::clone(&runtime);
        let ws_config = ws_config.clone();

        let handle = std::thread::Builder::new()
            .name(format!("uring-worker-{i}"))
            .spawn(move || {
                run_worker(i, addr, router, error_handler, after_plugs, runtime, body_limit, ws_config);
            })?;

        handles.push(handle);
    }

    Ok(handles)
}

/// Spawn io_uring worker threads without joining them. Returns immediately.
///
/// Used by `child_entry` (supervision) where the supervisor manages the lifetime.
pub fn spawn_uring_workers(
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Detach handles — supervisor manages lifetime.
    let _handles = spawn_workers(addr, router, error_handler, after_plugs, runtime, body_limit, ws_config)?;
    Ok(())
}

/// Start a multi-threaded io_uring HTTP server (blocking).
///
/// Spawns workers and joins them. Blocks forever. Used by `MahaloEndpoint::start()`.
pub fn start_uring_server(
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let handles = spawn_workers(addr, router, error_handler, after_plugs, runtime, body_limit, ws_config)?;

    // Wait for all workers (they run forever unless something goes wrong).
    for handle in handles {
        handle
            .join()
            .map_err(|e| format!("worker thread panicked: {:?}", e))?;
    }

    Ok(())
}

/// Single worker thread body. Runs the io_uring event loop (blocks forever).
fn run_worker(
    worker_id: usize,
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Arc<Option<ErrorHandler>>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) {
    // Best-effort CPU affinity pinning.
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

    // Create the listening socket with SO_REUSEPORT.
    let socket = match crate::handler::bind_socket(addr, true) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("uring-worker-{worker_id}: bind failed: {e}");
            return;
        }
    };

    let listen_fd = socket.as_raw_fd();

    // Build io_uring with optimal flags for single-threaded use.
    let mut ring = match IoUring::builder()
        .setup_sqpoll(2000)
        .setup_coop_taskrun()
        .setup_single_issuer()
        .build(4096)
        .or_else(|_| {
            IoUring::builder()
                .setup_coop_taskrun()
                .setup_single_issuer()
                .build(4096)
        })
        .or_else(|_| IoUring::new(4096))
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("uring-worker-{worker_id}: io_uring init failed: {e}");
            return;
        }
    };

    // Per-worker pools.
    let mut buf_pool = BufferPool::new(4096, 8192);
    let mut conn_pool = ConnectionPool::new(10_000);

    // Register read buffers with io_uring for zero-copy kernel I/O.
    let iovecs = buf_pool.iovecs();
    let bufs_registered = unsafe { ring.submitter().register_buffers(&iovecs) }.is_ok();

    // Per-worker single-threaded tokio runtime for async handlers.
    let tokio_rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("uring-worker-{worker_id}: tokio init failed: {e}");
            return;
        }
    };

    let error_handler_ref: &Option<ErrorHandler> = &*error_handler;

    // Run the event loop (blocks forever).
    run_event_loop(
        &mut ring,
        listen_fd,
        &mut conn_pool,
        &mut buf_pool,
        &router,
        error_handler_ref,
        &after_plugs,
        &runtime,
        body_limit,
        &tokio_rt,
        ws_config.as_ref(),
        bufs_registered,
    );

    // Keep socket alive so the fd isn't closed during the event loop.
    drop(socket);
}
