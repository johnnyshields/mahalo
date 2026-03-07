use std::cell::RefCell;
use std::rc::Rc;

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
    pub fn start(runtime: &Runtime) -> Self {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(runtime, spec, vec![]);
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
    pub fn child_entry(runtime: &Runtime) -> (ChannelSupervisorHandle, ChildEntry) {
        // Pre-create the supervisor synchronously so the handle is usable immediately.
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(runtime, spec, vec![]);
        let handle_cell: Rc<RefCell<Option<SupervisorHandle>>> = Rc::new(RefCell::new(None));
        // Store handle immediately — no need to wait for factory.
        *handle_cell.borrow_mut() = Some(handle);
        let handle_cell_factory = Rc::clone(&handle_cell);

        let entry = ChildEntry::new(
            ChildSpec::new("mahalo_channel_supervisor").restart(RestartType::Permanent),
            move || {
                let _handle_cell = Rc::clone(&handle_cell_factory);
                async move {

                    // Keep this child alive until the runtime is dropped.
                    // The supervisor runs independently; this just keeps the
                    // ChildEntry alive to signal the parent supervisor.
                    loop {
                        monoio::time::sleep(std::time::Duration::from_secs(60)).await;
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
    handle_cell: Rc<RefCell<Option<SupervisorHandle>>>,
}

impl ChannelSupervisorHandle {
    /// Add a connection as a supervised temporary child.
    pub async fn add_connection(&self, entry: ChildEntry) -> Result<(), SupervisorError> {
        let handle = self
            .handle_cell
            .borrow()
            .clone()
            .ok_or(SupervisorError::Gone)?;
        handle.add_child(entry).await.map(|_| ())
    }
}

/// Build a `ChildEntry` for a single WebSocket connection.
///
/// The entry wraps the connection's read-loop and GenServer startup, and
/// returns `ExitReason::Normal` when the client disconnects.
pub fn connection_child_entry(
    factory: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitReason>>>
        + 'static,
) -> ChildEntry {
    let factory = Rc::new(RefCell::new(Some(factory)));
    ChildEntry::new(
        ChildSpec::new("conn").restart(RestartType::Temporary),
        move || {
            let factory = Rc::clone(&factory);
            let f = factory
                .borrow_mut()
                .take()
                .expect("connection factory called twice");
            f()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[monoio::test(enable_timer = true)]
    async fn channel_supervisor_starts() {
        let runtime = Runtime::new(1);
        let supervisor = ChannelSupervisor::start(&runtime);
        supervisor.shutdown();
    }

    #[monoio::test(enable_timer = true)]
    async fn channel_supervisor_add_connection_runs_factory() {
        use std::cell::Cell;
        let runtime = Runtime::new(1);
        let supervisor = ChannelSupervisor::start(&runtime);

        let ran = Rc::new(Cell::new(false));
        let ran_clone = Rc::clone(&ran);

        let entry = ChildEntry::new(
            ChildSpec::new("test_conn").restart(RestartType::Temporary),
            move || {
                let ran = Rc::clone(&ran_clone);
                async move {
                    ran.set(true);
                    ExitReason::Normal
                }
            },
        );

        supervisor.add_connection(entry).await.unwrap();
        monoio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(ran.get());

        supervisor.shutdown();
    }
}
