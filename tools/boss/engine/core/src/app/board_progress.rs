//! Cold-board-build progress pushes. The `GetWorkTree` handler installs a
//! [`ProgressSink`] on its population trace; each report is published, in
//! order, on the product's work topic so every session watching the product
//! (the requester or not) can show what the engine is doing while the first
//! snapshot is still being assembled. See [`crate::population_progress`].
use super::*;
use crate::population_progress::{PopulationProgress, ProgressSink};

/// A sink that forwards reports to `product_id`'s work topic.
///
/// The sink is called from a blocking database thread, so it only queues the
/// report; one forwarding task per sink publishes them FIFO. The task ends
/// when the sink (held by the trace) is dropped.
pub(super) fn topic_progress_sink(server_state: &ServerState, product_id: &str) -> ProgressSink {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PopulationProgress>();
    let broker = server_state.topic_broker.clone();
    let product_id = product_id.to_owned();
    let topic = work_product_topic(&product_id);
    boss_task_scope::spawn(async move {
        while let Some(progress) = rx.recv().await {
            let envelope = FrontendEventEnvelope::push(progress_event(&product_id, &progress));
            broker.publish(&topic, envelope).await;
        }
    });
    Arc::new(move |progress| {
        // A closed channel means the forwarder ended; progress is best-effort.
        let _ = tx.send(progress);
    })
}

pub(super) fn progress_event(product_id: &str, progress: &PopulationProgress) -> FrontendEvent {
    FrontendEvent::BoardPopulationProgress {
        product_id: product_id.to_owned(),
        phase: progress.phase,
        stage: progress.stage.map(str::to_owned),
        elapsed_ms: progress.elapsed_ms.max(0.0).round() as u64,
        items: progress.items.and_then(|n| u64::try_from(n).ok()),
        bytes: progress.bytes.and_then(|n| u64::try_from(n).ok()),
    }
}

#[cfg(test)]
#[path = "board_progress_tests.rs"]
mod tests;
