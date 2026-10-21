//! Synchronous crash diagnostics and cleanup; never depends on Tokio.

use super::*;

/// Tmux workers survive both orderly shutdown and crashes for adoption by the
/// next engine. Remote PIDs are not local signal targets. Historical workers
/// with uncertain ownership are preserved for rollback/drain, just as in
/// `shutdown_workers`. Only an explicitly non-tmux local shell that is still
/// a direct child of THIS engine can require emergency cleanup. Never infer
/// ownership or non-adoptability from a positive recorded PID alone.
fn emergency_shell_pids(server: &ServerState) -> Vec<libc::pid_t> {
    let engine_pid = std::process::id() as libc::pid_t;
    server
        .live_worker_states
        .snapshot()
        .into_iter()
        .filter(|state| {
            state.tmux_hosted == Some(false)
                && state.host_id.as_deref() == Some("local")
                && state.shell_pid > 0
                && crate::worker_registry::parent_pid(state.shell_pid).ok().flatten() == Some(engine_pid)
        })
        .map(|state| state.shell_pid)
        .collect()
}

/// This process-wide hook also runs for panics caught by Tokio tasks. A task
/// panic is never authority to kill adoptable workers or clear their registry.
/// The weak reference does not extend the server's lifetime after shutdown.
pub(super) fn install(server: &Arc<ServerState>) {
    let weak = Arc::downgrade(server);
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let thread = std::thread::current();
        tracing::error!(
            panic_payload = payload,
            location = %info.location().map(ToString::to_string).unwrap_or_else(|| "unknown".into()),
            thread = thread.name().unwrap_or("unnamed"),
            "engine panic",
        );
        if let Some(server) = weak.upgrade() {
            let pids = emergency_shell_pids(&server);
            if !pids.is_empty() {
                tracing::error!(
                    count = pids.len(),
                    "engine panic: emergency-killing non-adoptable engine-owned shells before unwind",
                );
                signal_shell_pids(&pids, Duration::from_millis(500));
            }
        }
        prior(info);
    }));
}

#[cfg(test)]
mod tests;
