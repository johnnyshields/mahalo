use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use io_uring::IoUring;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;

use crate::endpoint::ErrorHandler;
use crate::uring::{BufferPool, ConnectionPool, run_event_loop};

/// Start a multi-threaded io_uring HTTP server.
///
/// Spawns one worker thread per available CPU core, each with its own
/// listening socket (via `SO_REUSEPORT`), io_uring instance, connection pool,
/// buffer pool, and single-threaded tokio runtime for executing async handlers.
pub fn start_uring_server(
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

        let handle = std::thread::Builder::new()
            .name(format!("uring-worker-{i}"))
            .spawn(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                // Best-effort CPU affinity pinning.
                #[cfg(target_os = "linux")]
                {
                    let mut cpuset = nix::sched::CpuSet::new();
                    let num_cpus = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1);
                    if cpuset.set(i % num_cpus).is_ok() {
                        let _ = nix::sched::sched_setaffinity(
                            nix::unistd::Pid::from_raw(0),
                            &cpuset,
                        );
                    }
                }

                // Create the listening socket with SO_REUSEPORT.
                let socket = crate::handler::bind_socket(addr, true)?;

                let listen_fd = socket.as_raw_fd();

                // Build io_uring with optimal flags for single-threaded use.
                // SQPOLL: kernel-side submission polling (requires root, falls back gracefully).
                // COOP_TASKRUN + SINGLE_ISSUER: reduce kernel overhead for single-threaded.
                let mut ring = IoUring::builder()
                    .setup_sqpoll(2000)
                    .setup_coop_taskrun()
                    .setup_single_issuer()
                    .build(4096)
                    .or_else(|_| {
                        // Fallback without SQPOLL (doesn't require root).
                        IoUring::builder()
                            .setup_coop_taskrun()
                            .setup_single_issuer()
                            .build(4096)
                    })
                    .or_else(|_| IoUring::new(4096))?;

                // Per-worker pools.
                let mut buf_pool = BufferPool::new(4096, 8192);
                let mut conn_pool = ConnectionPool::new(10_000);

                // Per-worker single-threaded tokio runtime for async handlers.
                let tokio_rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;

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
                );

                // Keep socket alive so the fd isn't closed during the event loop.
                drop(socket);
                Ok(())
            })?;

        handles.push(handle);
    }

    // Wait for all workers (they run forever unless something goes wrong).
    for handle in handles {
        handle
            .join()
            .map_err(|e| format!("worker thread panicked: {:?}", e))??;
    }

    Ok(())
}
