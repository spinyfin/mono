//! Heartbeat refresh of engine-selected queued card copy.
use super::*;

impl ExecutionCoordinator {
    /// The connection the queued-display pass reads through.
    ///
    /// The pass recomputes the runtime of every queued card inside one read
    /// transaction, so it holds its connection for as long as that takes: a
    /// time that grows with the queue and with the cost of each card's reads.
    /// On the connection every mutation shares, each write that arrived during
    /// a pass waited for the pass to end, including a person's answer to a
    /// question card. An independent WAL reader has its own lock and never
    /// blocks the writer. An in-memory database has no second connection to
    /// open, so it keeps reading through the one it has.
    pub fn queued_display_db(&self) -> Arc<WorkDb> {
        if let Some(reader) = self.queued_display_reader.get() {
            return reader.clone();
        }
        match self.work_db.reader() {
            Ok(reader) => self.queued_display_reader.get_or_init(|| Arc::new(reader)).clone(),
            Err(err) => {
                // Stale queued labels are worse than a pass that shares the
                // writer; the next pass tries to open the reader again.
                tracing::warn!(
                    ?err,
                    "queued display refresh: no read-only connection; reading through the shared one"
                );
                self.work_db.clone()
            }
        }
    }

    /// Recompute pending queued labels as time advances. Callers publish only
    /// displays that changed since the previous scan. The first pass seeds
    /// without republishing every queued card the tree already carried.
    pub async fn refresh_queued_displays(
        &self,
        previous: &mut Option<std::collections::HashMap<String, crate::work::QueuedDisplaySnapshot>>,
        now: i64,
    ) {
        let work_db = self.queued_display_db();
        let previous_scan = previous.clone().unwrap_or_default();
        let current =
            match crate::work::db_diagnostics::spawn_blocking(move || work_db.queued_displays(now, &previous_scan))
                .await
            {
                Ok(Ok(current)) => current,
                Ok(Err(err)) => {
                    tracing::warn!(?err, "failed to refresh queued displays");
                    return;
                }
                Err(err) => {
                    tracing::warn!(?err, "queued display refresh join failed");
                    return;
                }
            };
        match previous {
            None => {
                *previous = Some(current);
            }
            Some(prev) => {
                for (item, snap) in &current {
                    if prev.get(item).map(|s| &s.display) != Some(&snap.display) {
                        self.publisher
                            .publish_work_item_runtime_changed(&snap.product_id, item, "queued_display_changed")
                            .await;
                    }
                }
                *previous = Some(current);
            }
        }
    }
}
