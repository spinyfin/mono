//! Heartbeat refresh of engine-selected queued card copy.
use super::*;

impl ExecutionCoordinator {
    /// Recompute pending queued labels as time advances. Callers publish only
    /// displays that changed since the previous scan. The first pass seeds
    /// without republishing every queued card the tree already carried.
    pub async fn refresh_queued_displays(
        &self,
        previous: &mut Option<std::collections::HashMap<String, crate::work::QueuedDisplaySnapshot>>,
        now: i64,
    ) {
        let work_db = self.work_db.clone();
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
