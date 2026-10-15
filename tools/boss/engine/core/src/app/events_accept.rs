//! The events-socket accept loop: one connection per hook event from a
//! worker's `boss-event` shim, each dispatched on its own task.
//!
//! Split out of `server.rs` for size. The one piece of state the loop owns
//! is the accept-order sequence it stamps on every event — see the comment
//! on it, and `LiveWorkerStateRegistry::apply_sequenced_event` for why the
//! live-state reducer needs it.

use super::*;

pub(super) async fn run_events_accept_loop(listener: UnixListener, server_state: Arc<ServerState>) {
    let local_addr = listener.local_addr().ok();
    let path_display = local_addr
        .as_ref()
        .and_then(|a| a.as_pathname())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_owned());
    tracing::info!(
        events_socket_path = %path_display,
        "events socket: accept loop started",
    );
    if let Some(p) = local_addr.as_ref().and_then(|a| a.as_pathname()) {
        crate::audit::record_accept_loop_started("events", p);
    }
    // This socket only ever hears from a `ProgressIngress::HookCallback`
    // driver's `boss-event` shim (a `StdoutJsonl` driver never connects to
    // it), but different connections carry different workers' drivers
    // (Claude, Codex, Grok, …) — so the driver is resolved per connection
    // inside `handle_connection`, from the payload's `_boss_run_id` via
    // `WorkDb::get_execution_driver_slug`, rather than once here for every
    // connection. The registry is still built once: it is stateless and
    // shared read-only across connections.
    let driver_registry = std::sync::Arc::new(crate::driver::DriverRegistry::default());
    // Accept-order sequence for the live-state reducer. Every connection
    // below is handled on its own task, so two hooks the worker emitted in
    // order can reach `apply_event` in either order — and a turn boundary
    // does strictly more work on the way there than the tool events that
    // follow it. The shim connects synchronously from inside the driver's
    // hook, so accept order *is* emission order; stamping it here, before
    // the task is spawned, is what lets the reducer refuse to let a late
    // `Stop` overwrite the `PreToolUse` the worker emitted after it.
    let mut next_ingress_seq: u64 = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                next_ingress_seq += 1;
                let ingress_seq = next_ingress_seq;
                let server_state = server_state.clone();
                let driver_registry = driver_registry.clone();
                boss_task_scope::spawn(async move {
                    match handle_connection(stream, &driver_registry, &server_state.work_db).await {
                        Ok(incoming) => {
                            let incoming = incoming.with_ingress_seq(ingress_seq);
                            tracing::info!(
                                peer_pid = ?incoming.connection.peer_pid,
                                run_id = ?incoming.run_id,
                                ingress_seq,
                                event = ?incoming.event,
                                "events socket: hook event received",
                            );
                            dispatch_worker_event_fanout(&server_state, &incoming).await;
                        }
                        Err(err) => {
                            tracing::warn!(?err, "events socket: failed to handle connection");
                        }
                    }
                });
            }
            Err(err) => {
                tracing::error!(?err, "events socket accept failed");
            }
        }
    }
}
