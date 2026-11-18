//! The task that runs one admitted read: it waits for the read's admission
//! permits, runs the handler on the blocking pool, and then settles the
//! read's coalescing batch, if it leads one.
use super::*;

/// Spawn `handler` for the read `waiting` stands for. `read_batch` is the
/// coalescing batch this read leads, if any; its followers are answered from
/// what the handler sends, or told why it sent nothing.
pub(super) fn spawn(
    waiting: read_admission::WaitingRead,
    read_batch: Option<Arc<read_coalesce::Batch>>,
    handler: impl Future<Output = ()> + Send + 'static,
    sink: Arc<SessionSink>,
    session_id: String,
    request_id: String,
) {
    // A coalescing leader reports to its batch whether it is blocked on a
    // SQLite lock, which is what keeps the batch open to followers.
    let handler: Pin<Box<dyn Future<Output = ()> + Send>> = match &read_batch {
        Some(batch) => Box::pin(boss_work_db::contention::READ_WAIT.scope(batch.wait(), handler)),
        None => Box::pin(handler),
    };
    boss_task_scope::spawn(async move {
        let permits = waiting.acquire().await;
        // Synchronous SQLite work must not occupy the async
        // runtime threads that serve writes and live status.
        // Keep permits inside the blocking job even if its
        // client disconnects; cancellation cannot inflate capacity.
        let batch = read_batch.clone();
        let result = boss_task_scope::spawn_blocking_future(async move {
            permits.started();
            if let Some(batch) = &batch {
                // From here identical reads start a new batch: this
                // handler's snapshot predates them.
                batch.start();
            }
            handler.await;
            permits
        })
        .await;
        match result {
            Ok(Some(permits)) => {
                // Followers are answered before the read's admission
                // permits are released, so a later read cannot be
                // admitted ahead of the replies this one owes.
                if let Some(batch) = &read_batch {
                    batch.finish(&sink);
                }
                drop(permits);
            }
            // Cancelled by engine shutdown before the handler finished:
            // the leader never replied, so its followers are told rather
            // than left waiting.
            Ok(None) => {
                if let Some(batch) = &read_batch {
                    batch.fail(&sink, "read RPC cancelled: engine shutting down");
                }
            }
            Err(err) => {
                tracing::error!(?err, %session_id, "read RPC handler failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::Error {
                        message: "read RPC handler failed".into(),
                    },
                );
                if let Some(batch) = &read_batch {
                    batch.fail(&sink, "read RPC handler failed");
                }
            }
        }
        if let Some(batch) = &read_batch {
            sink.untap_response(batch.leader_request_id());
        }
    });
}
