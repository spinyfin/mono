//! Engine-wide board loads. The only query parameter today is product_id;
//! fetch_seq is correlation and delta is a per-session encoding choice.
use super::*;

#[derive(Default)]
pub(super) struct WorkTreeLoads {
    flights: boss_single_flight::SingleFlight<String, (u64, i64), (u64, boss_protocol::WorkTree)>,
    // data_version is comparable only on the SAME connection. This dedicated
    // reader holds no transaction and never acquires the writer connection.
    probe: StdMutex<Option<WorkDb>>,
    // Per-item runtimes shared by every product's assembly, validated against
    // per-item epochs in each load's own snapshot (see `board_runtime_cache`).
    runtimes: crate::work::board_runtime_cache::BoardRuntimeCache,
}

impl WorkTreeLoads {
    pub(super) fn load(
        &self,
        db: &WorkDb,
        revision: &AtomicU64,
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
                db.get_work_tree_cached(product_id, trace, &self.runtimes)
                    .map(|tree| (snapshot_revision, tree))
                    .map_err(|err| err.to_string())
            },
        )
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
