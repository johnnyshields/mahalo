use std::sync::Arc;

use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{ChildEntry, SupervisorHandle, start_supervisor};
use rebar_core::supervisor::spec::{ChildSpec, RestartStrategy, RestartType, SupervisorError, SupervisorSpec};

/// Supervises all WebSocket channel connection GenServers.
///
/// Each WebSocket connection is started as a `Temporary` child — it is not
/// restarted on crash (Phoenix semantics: a disconnected client simply
/// reconnects). The supervisor itself is `Permanent` and is part of the
/// top-level mahalo supervision tree.
#[derive(Clone)]
pub struct ChannelSupervisor {
    handle: SupervisorHandle,
}

impl ChannelSupervisor {
    /// Start a ChannelSupervisor under the given runtime.
    pub async fn start(runtime: Arc<Runtime>) -> Self {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(runtime, spec, vec![]).await;
        Self { handle }
    }

    /// Add a connection as a supervised temporary child.
    ///
    /// Returns `Err` only if the supervisor itself has exited.
    pub async fn add_connection(&self, entry: ChildEntry) -> Result<(), SupervisorError> {
        self.handle.add_child(entry).await.map(|_| ())
    }

    /// Create a `(ChannelSupervisor, ChildEntry)` pair for embedding in a
    /// parent supervision tree.
    ///
    /// The returned `ChannelSupervisor` handle is usable immediately — it is
    /// backed by a channel that the spawned supervisor will consume when its
    /// `ChildEntry` factory is called.
    pub fn child_entry(runtime: Arc<Runtime>) -> (ChannelSupervisorHandle, ChildEntry) {
        use std::sync::Mutex;

        // We pre-create the supervisor so the handle is usable before the
        // ChildEntry factory runs.
        //
        // The ChildEntry factory simply waits until the supervisor exits, which
        // will happen when the runtime shuts down.
        let runtime_clone = Arc::clone(&runtime);
        let handle_cell: Arc<Mutex<Option<SupervisorHandle>>> = Arc::new(Mutex::new(None));
        let handle_cell_factory = Arc::clone(&handle_cell);

        let entry = ChildEntry::new(
            ChildSpec::new("mahalo_channel_supervisor").restart(RestartType::Permanent),
            move || {
                let runtime = Arc::clone(&runtime_clone);
                let handle_cell = Arc::clone(&handle_cell_factory);
                async move {
                    let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
                    let handle = start_supervisor(Arc::clone(&runtime), spec, vec![]).await;
                    // Store handle so outer code can use it.
                    *handle_cell.lock().unwrap() = Some(handle);

                    // Keep this child alive until the runtime is dropped.
                    // The supervisor runs independently; this just keeps the
                    // ChildEntry alive to signal the parent supervisor.
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                    #[allow(unreachable_code)]
                    ExitReason::Normal
                }
            },
        );

        let proxy = ChannelSupervisorHandle {
            handle_cell,
        };

        (proxy, entry)
    }

    /// Gracefully shut down the supervisor.
    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

/// A lazily-resolved handle to a `ChannelSupervisor` created via
/// [`ChannelSupervisor::child_entry()`]. The inner `SupervisorHandle` is
/// populated once the `ChildEntry` factory runs.
#[derive(Clone)]
pub struct ChannelSupervisorHandle {
    handle_cell: Arc<std::sync::Mutex<Option<SupervisorHandle>>>,
}

impl ChannelSupervisorHandle {
    /// Add a connection as a supervised temporary child.
    pub async fn add_connection(&self, entry: ChildEntry) -> Result<(), SupervisorError> {
        let handle = self
            .handle_cell
            .lock()
            .unwrap()
            .clone()
            .ok_or(SupervisorError::Gone)?;
        handle.add_child(entry).await.map(|_| ())
    }
}

/// Build a `ChildEntry` for a single WebSocket connection.
///
/// The entry wraps the connection's read-loop and GenServer startup, and
/// returns `ExitReason::Normal` when the client disconnects.
///
/// `factory` only needs `Send`, not `Sync` — `WebSocket` and other connection
/// types are typically `!Sync`. The `Arc<Mutex<Option<F>>>` wrapper makes the
/// outer closure satisfy the `Sync` bound required by `ChildEntry`.
pub fn connection_child_entry(
    factory: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitReason> + Send>>
        + Send
        + 'static,
) -> ChildEntry {
    let factory = Arc::new(std::sync::Mutex::new(Some(factory)));
    ChildEntry::new(
        ChildSpec::new("conn").restart(RestartType::Temporary),
        move || {
            let factory = Arc::clone(&factory);
            let f = factory
                .lock()
                .unwrap()
                .take()
                .expect("connection factory called twice");
            f()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_supervisor_starts() {
        let runtime = Arc::new(Runtime::new(1));
        let supervisor = ChannelSupervisor::start(Arc::clone(&runtime)).await;
        supervisor.shutdown();
    }

    #[tokio::test]
    async fn channel_supervisor_add_connection_runs_factory() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let runtime = Arc::new(Runtime::new(1));
        let supervisor = ChannelSupervisor::start(Arc::clone(&runtime)).await;

        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);

        let entry = ChildEntry::new(
            ChildSpec::new("test_conn").restart(RestartType::Temporary),
            move || {
                let ran = Arc::clone(&ran_clone);
                async move {
                    ran.store(true, Ordering::SeqCst);
                    ExitReason::Normal
                }
            },
        );

        supervisor.add_connection(entry).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(ran.load(Ordering::SeqCst));

        supervisor.shutdown();
    }
}
