//! Engine-wide board loads. The only query parameter today is product_id;
//! fetch_seq is correlation and delta is a per-session encoding choice.
use super::*;

type BoardSlot = Arc<StdMutex<Option<crate::work::board_projection::CachedProjection>>>;

#[derive(Default)]
pub(super) struct WorkTreeLoads {
    flights: boss_single_flight::SingleFlight<String, (u64, i64), (u64, boss_protocol::WorkTree)>,
    // data_version is comparable only on the SAME connection. This dedicated
    // reader holds no transaction and never acquires the writer connection.
    probe: StdMutex<Option<WorkDb>>,
    // Per-item runtimes and per-owner review projections shared by every
    // product's assembly, validated against per-item epochs in each load's
    // own snapshot (see `board_runtime_cache` and `board_review_cache`).
    caches: crate::work::board_caches::BoardCaches,
    // At most four retained products, each capped by the database adapter.
    // Individual locks preserve parallel reads for different products.
    projections: StdMutex<HashMap<String, (std::time::Instant, BoardSlot)>>,
}

impl WorkTreeLoads {
    pub(super) fn load(
        &self,
        db: &WorkDb,
        revision: &AtomicU64,
        metrics: &crate::metrics::Registry,
        product_id: &str,
        trace: &mut crate::population_timing::PopulationTrace,
    ) -> Result<Arc<boss_single_flight::Loaded<(u64, boss_protocol::WorkTree)>>, String> {
        self.flights.run(
            product_id.to_owned(),
            || self.freshness(db, revision),
            || {
                // Dispatch supplied this request's independent read-only
                // connection. Only the batch owner scans it (with Deferred).
                // The detached spawn_blocking job survives client disconnects.
                let snapshot_revision = revision.load(Ordering::SeqCst);
                let slot = self.projection(product_id);
                let mut projection = slot.lock().expect("board projection poisoned");
                let (tree, refresh) = db
                    .refresh_board_projection(
                        product_id,
                        &mut projection,
                        trace,
                        &self.caches,
                        boss_engine_utils::epoch_time::now_epoch_secs(),
                    )
                    .map_err(|err| err.to_string())?;
                match refresh {
                    crate::work::board_projection::Refresh::Snapshot(reason) => {
                        crate::board_invalidation_metrics::record_full_assembly(metrics);
                        metrics.counter_inc_by_dynamic(
                            &format!("board.projection.snapshot.{reason}"),
                            "Board projection snapshot recoveries.",
                            1,
                        );
                    }
                    crate::work::board_projection::Refresh::Incremental(count) => {
                        metrics.counter_inc_by_dynamic(
                            "board.projection.incremental",
                            "Incremental board refreshes.",
                            1,
                        );
                        metrics.counter_inc_by_dynamic(
                            "board.projection.cards",
                            "Cards refreshed incrementally.",
                            count as u64,
                        );
                    }
                }
                Ok((snapshot_revision, tree))
            },
        )
    }

    fn projection(&self, product: &str) -> BoardSlot {
        let mut projections = self.projections.lock().expect("board projections poisoned");
        if let Some((accessed, slot)) = projections.get_mut(product) {
            *accessed = std::time::Instant::now();
            return slot.clone();
        }
        if projections.len() >= 4 {
            // Eviction affects reuse only. An in-flight reader owns its Arc,
            // and a later request starts from a fresh database snapshot.
            let victim = projections
                .iter()
                .min_by_key(|(_, (accessed, _))| *accessed)
                .map(|(id, _)| id.clone())
                .unwrap();
            projections.remove(&victim);
        }
        let slot = Arc::new(StdMutex::new(None));
        projections.insert(product.to_owned(), (std::time::Instant::now(), slot.clone()));
        slot
    }
    // Both fences are database-wide: unrelated writes may conservatively
    // require a follow-up, but never serialize scans for different products.
    fn freshness(&self, db: &WorkDb, revision: &AtomicU64) -> Result<(u64, i64), String> {
        let revision = revision.load(Ordering::SeqCst);
        let mut probe = self.probe.lock().expect("tree freshness probe poisoned");
        if probe.is_none() {
            *probe = Some(db.reader().map_err(|err| err.to_string())?);
        }
        let version = probe
            .as_ref()
            .unwrap()
            .read_data_version()
            .map_err(|err| err.to_string())?;
        Ok((revision, version))
    }
}

#[cfg(test)]
#[path = "work_tree_loads_tests.rs"]
mod tests;
