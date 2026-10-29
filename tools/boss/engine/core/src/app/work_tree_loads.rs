//! Engine-wide board loads. The only query parameter today is product_id;
//! fetch_seq is correlation and delta is a per-session encoding choice.
use super::*;

type BoardSlot = Arc<StdMutex<Option<crate::work::board_projection::CachedProjection>>>;

#[derive(Default)]
struct Sources {
    // data_version is comparable only on the SAME connection. This dedicated
    // reader holds no transaction and never acquires the writer connection.
    probe: StdMutex<Option<WorkDb>>,
    // At most four retained products, each capped by the database adapter.
    // Individual locks preserve parallel reads for different products.
    projections: StdMutex<HashMap<String, (std::time::Instant, BoardSlot)>>,
}

#[derive(Default)]
pub(super) struct WorkTreeLoads {
    pub(super) writes: WriterHooks,
    flights: boss_single_flight::SingleFlight<String, (u64, i64), (u64, boss_protocol::WorkTree)>,
    // Per-item runtimes and per-owner review projections shared by every
    // product's assembly, validated against per-item epochs in each load's
    // own snapshot (see `board_runtime_cache` and `board_review_cache`).
    caches: crate::work::board_caches::BoardCaches,
    sources: Sources,
    // Per-session reply ordering; entries die with the last holder.
    session_order: StdMutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

/// What the session writer signals as board responses reach the socket.
#[derive(Default)]
pub(super) struct WriterHooks {
    pub(super) first_served: Arc<tokio::sync::Notify>,
    /// Test-owned receiver for finished population traces, taking the place of
    /// the process-wide timing log so tests observe exactly what the writer flushes.
    #[cfg(test)]
    pub(super) trace_sink: StdMutex<Option<TraceSink>>,
}

#[cfg(test)]
pub(super) type TraceSink = Arc<dyn Fn(&crate::population_timing::PopulationTrace) + Send + Sync>;

impl WorkTreeLoads {
    /// Emit one finished board population trace: to the test sink when one is
    /// installed, otherwise to the process-wide timing log.
    pub(super) fn flush_trace(&self, trace: &crate::population_timing::PopulationTrace) {
        #[cfg(test)]
        if let Some(sink) = self.writes.trace_sink.lock().expect("trace sink poisoned").clone() {
            return sink(trace);
        }
        if let Some(log) = crate::population_timing::global() {
            trace.flush(log);
        }
    }

    /// The lock that serializes one session's `GetWorkTree` replies: held from
    /// the cache update through the enqueue of every page, so response order
    /// matches cache sequence order even though admitted reads run concurrently.
    pub(super) fn session_order(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut orders = self.session_order.lock().expect("session order poisoned");
        orders.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = orders.get(session_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        orders.insert(session_id.to_owned(), Arc::downgrade(&lock));
        lock
    }

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
        let mut projections = self.sources.projections.lock().expect("board projections poisoned");
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
        let mut probe = self.sources.probe.lock().expect("tree freshness probe poisoned");
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

/// What happened when the writer put one frame on the socket.
pub(super) struct WrittenFrame<'a> {
    pub(super) request_id: Option<&'a str>,
    /// False only for a non-final page of a paged board snapshot.
    pub(super) last_board_page: bool,
    pub(super) write_failed: bool,
    pub(super) write_start: Instant,
}

impl SessionSink {
    /// Finish one written frame: advance or complete the board population
    /// trace and acknowledge delivery. A non-final page of a paged board
    /// snapshot re-stashes its trace and acknowledges nothing, whether or not
    /// a trace was stashed, so the trace is flushed and delivery completed
    /// exactly once, after the final frame. `flush` receives the finished
    /// trace.
    pub(super) fn finish_written_frame(
        &self,
        frame: WrittenFrame<'_>,
        trace: Option<crate::population_timing::PopulationTrace>,
        first_board_served: &tokio::sync::Notify,
        flush: impl FnOnce(&crate::population_timing::PopulationTrace),
    ) {
        let WrittenFrame {
            request_id,
            last_board_page,
            write_failed,
            write_start,
        } = frame;
        let mid_snapshot = !last_board_page && !write_failed;
        if let Some(mut t) = trace {
            if !write_failed && last_board_page {
                first_board_served.notify_one();
            }
            t.record_plain(
                crate::population_timing::segment::SOCKET_WRITE,
                crate::population_timing::elapsed_ms(write_start),
            );
            if mid_snapshot {
                self.stash_population_trace(request_id.expect("board response id"), t);
            } else {
                let total_ms = t.elapsed_ms();
                t.record_plain(crate::population_timing::segment::TOTAL, total_ms);
                t.log_slow();
                flush(&t);
            }
        }
        if !mid_snapshot {
            self.complete_response_delivery(request_id, !write_failed);
        }
    }
}
