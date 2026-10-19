//! The events-socket accept loop: one connection per hook event from a
//! worker's `boss-event` shim, each dispatched on its own task.
//!
//! The loop owns the accept-order sequence stamped on every event, before the
//! connection is handed to its own task — see [`accept_stamped`], and
//! `LiveWorkerStateRegistry::apply_sequenced_event` for why the live-state
//! reducer needs it.

use super::*;

/// Accept-order sequence for the live-state reducer.
///
/// Every connection is handled on its own task, so two hooks the worker
/// emitted in order can reach `apply_hook_event` in either order — and a turn
/// boundary does strictly more work on the way there than the tool events
/// that follow it. The shim connects synchronously from inside the driver's
/// hook, so accept order *is* emission order; the number must therefore be
/// assigned at accept time, before the task is spawned, which is what lets the
/// reducer refuse to let a late `Stop` overwrite the `PreToolUse` the worker
/// emitted after it.
#[derive(Debug, Default)]
struct IngressSequencer {
    last: u64,
}

impl IngressSequencer {
    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }
}

/// Accept one connection and stamp it with the next ingress sequence.
async fn accept_stamped(
    listener: &UnixListener,
    sequencer: &mut IngressSequencer,
) -> std::io::Result<(UnixStream, u64)> {
    let (stream, _) = listener.accept().await?;
    Ok((stream, sequencer.next()))
}

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
    let mut sequencer = IngressSequencer::default();
    loop {
        match accept_stamped(&listener, &mut sequencer).await {
            Ok((stream, ingress_seq)) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connections_are_stamped_in_strictly_increasing_accept_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut sequencer = IngressSequencer::default();

        let mut stamps = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..4 {
            clients.push(UnixStream::connect(&path).await.unwrap());
            let (_stream, seq) = accept_stamped(&listener, &mut sequencer).await.unwrap();
            stamps.push(seq);
        }
        assert_eq!(stamps, vec![1, 2, 3, 4]);
    }
}
