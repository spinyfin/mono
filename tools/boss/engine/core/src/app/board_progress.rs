//! Cold-board-build progress pushes. The `GetWorkTree` handler installs a
//! [`ProgressSink`] on its population trace; each report is published, in
//! order, on the product's work topic so every session watching the product
//! (the requester or not) can show what the engine is doing while the first
//! snapshot is still being assembled. See [`crate::population_progress`].
use super::*;
use crate::population_progress::{PopulationProgress, ProgressSink};

enum ForwardedProgress {
    Report(PopulationProgress),
    Barrier(oneshot::Sender<()>),
}

/// A sink that forwards reports to `product_id`'s work topic.
///
/// The sink is called from a blocking database thread, so it only queues the
/// report; one forwarding task per sink publishes them FIFO. The task ends
/// when the sink (held by the trace) is dropped.
pub(super) fn topic_progress_sink(
    server_state: &ServerState,
    product_id: &str,
) -> (ProgressSink, impl std::future::Future<Output = ()> + use<>) {
    // A barrier queues all build progress before the board reply.
    // Otherwise a delayed Started push could arrive after the snapshot and
    // incorrectly put a loaded board back into refreshing.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ForwardedProgress>();
    let broker = server_state.topic_broker.clone();
    let product_id = product_id.to_owned();
    let topic = work_product_topic(&product_id);
    boss_task_scope::spawn(async move {
        while let Some(progress) = rx.recv().await {
            match progress {
                ForwardedProgress::Report(progress) => {
                    let envelope = FrontendEventEnvelope::push(progress_event(&product_id, &progress));
                    broker.publish(&topic, envelope).await;
                }
                ForwardedProgress::Barrier(barrier) => {
                    let _ = barrier.send(());
                }
            }
        }
    });
    let reports = tx.clone();
    let sink: ProgressSink = Arc::new(move |progress| {
        let _ = reports.send(ForwardedProgress::Report(progress));
    });
    (sink, async move {
        let (sent, received) = oneshot::channel();
        if tx.send(ForwardedProgress::Barrier(sent)).is_ok() {
            let _ = received.await;
        }
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

/// Write stage entry on the same writer before the large response frame.
pub(super) async fn write_stage<W: tokio::io::AsyncWrite + Unpin>(
    sink: &SessionSink,
    writer: &mut W,
    session_id: &str,
    trace: Option<&crate::population_timing::PopulationTrace>,
    stage: &'static str,
) -> std::io::Result<()> {
    if let Some((product, progress)) = trace.and_then(|t| t.writer_stage(stage)) {
        let event = FrontendEventEnvelope::push(progress_event(product, &progress));
        let line = serde_json::to_string(&event)?;
        sink.write_frame(writer, &line, session_id).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "board_progress_tests.rs"]
mod tests;
