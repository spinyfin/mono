//! Pass-scoped deadlines, attendance and cancellation-safe measurements.
use super::*;
use crate::walk_order::Rotation;
use std::future::Future;
use tokio::time::Instant;

#[derive(Debug, Default)]
pub(crate) struct ProbeOrder {
    rotation: Rotation,
    terminal: std::collections::HashSet<String>,
    /// When each URL's most recent probe chunk started this pass. The snapshot
    /// ages from the oldest of those starts, not from when the last chunk
    /// finished.
    pub(crate) observation: BatchObservation,
}

impl ProbeOrder {
    pub(crate) fn retain(&mut self, urls: &[String]) {
        let live = urls.iter().map(String::as_str).collect();
        self.rotation.retain(&live);
        self.terminal.retain(|url| live.contains(url.as_str()));
    }
    pub(crate) fn order(&self, urls: &[String]) -> Vec<String> {
        let mut urls = self.rotation.order(urls.to_vec(), String::as_str);
        urls.sort_by_key(|url| !self.terminal.contains(url));
        urls
    }
    pub(crate) fn attempt(&mut self, url: &str) {
        self.rotation.stamp(url);
    }
    pub(crate) fn observe(&mut self, results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>) {
        for (url, result) in results {
            if let Ok(probe) = result {
                if matches!(probe.state, PrLifecycleState::Open(_)) {
                    self.terminal.remove(url);
                } else {
                    self.terminal.insert(url.clone());
                }
            }
        }
    }
}

/// When each URL's chunk (or, for a stub, its batch) started. The oldest
/// start is the snapshot's age: later chunks cannot make an early result
/// look fresh.
#[derive(Debug, Default, Clone)]
pub(crate) struct BatchObservation {
    pub oldest_start: Option<Instant>,
    pub per_url: HashMap<String, Instant>,
}

impl BatchObservation {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn record_chunk(&mut self, urls: &[String], started: Instant) {
        self.oldest_start = Some(match self.oldest_start {
            Some(previous) => previous.min(started),
            None => started,
        });
        for url in urls {
            self.per_url.entry(url.clone()).or_insert(started);
        }
    }

    pub(crate) fn taken_at(&self) -> Instant {
        self.oldest_start.unwrap_or_else(Instant::now)
    }

    pub(crate) fn observed_at(&self, url: &str) -> Instant {
        self.per_url.get(url).copied().unwrap_or_else(Instant::now)
    }
}

pub(crate) struct ProbeContext {
    pub(crate) pass_end: Option<Instant>,
    pub(crate) trace: Arc<PassTrace>,
    pub(crate) order: Arc<Mutex<ProbeOrder>>,
}

tokio::task_local! { pub(crate) static CONTEXT: ProbeContext; }

#[derive(Default)]
pub(crate) struct FallbackWork {
    timing: FallbackTiming,
    requested: std::collections::HashSet<String>,
    attempted: std::collections::HashSet<String>,
    completed: std::collections::HashSet<String>,
}
tokio::task_local! { pub(crate) static FALLBACK: Mutex<FallbackWork>; }

#[derive(Default)]
struct FallbackTiming {
    started: Option<Instant>,
    elapsed: Duration,
    depth: usize,
}

/// Nested bisections overlap; only the outer fallback span owns elapsed time.
pub(crate) struct FallbackSpan;
pub(crate) fn start_fallback(urls: &[String]) -> FallbackSpan {
    let _ = FALLBACK.try_with(|work| {
        let mut work = work.lock().unwrap();
        work.timing.started.get_or_insert_with(Instant::now);
        work.timing.depth += 1;
        work.requested.extend(urls.iter().cloned());
    });
    FallbackSpan
}
impl Drop for FallbackSpan {
    fn drop(&mut self) {
        let _ = FALLBACK.try_with(|work| {
            let mut work = work.lock().unwrap();
            work.timing.depth -= 1;
            if work.timing.depth == 0
                && let Some(started) = work.timing.started.take()
            {
                work.timing.elapsed += started.elapsed();
            }
        });
    }
}

pub(crate) fn attend(urls: &[String]) {
    let _ = FALLBACK.try_with(|work| {
        let mut work = work.lock().unwrap();
        if work.timing.started.is_some() {
            for url in urls {
                work.attempted.insert(url.clone());
                let _ = CONTEXT.try_with(|c| c.order.lock().unwrap().attempt(url));
            }
        }
    });
}

pub(crate) fn completed(url: &str) {
    let _ = CONTEXT.try_with(|c| c.order.lock().unwrap().attempt(url));
    let _ = FALLBACK.try_with(|work| {
        let mut work = work.lock().unwrap();
        if work.requested.contains(url) {
            work.completed.insert(url.to_owned());
        }
    });
}

pub(crate) struct FallbackReport;
impl Drop for FallbackReport {
    fn drop(&mut self) {
        let _ = FALLBACK.try_with(|work| {
            let work = work.lock().unwrap();
            if !work.requested.is_empty() {
                let _ = CONTEXT.try_with(|c| {
                    c.trace.record_probe_work(
                        "probe_fallback",
                        work.timing.elapsed,
                        work.attempted.len(),
                        work.completed.len(),
                        work.requested.len().saturating_sub(work.completed.len()),
                    )
                });
            }
        });
    }
}

/// The allowance for one chunk of a probe, taken when the chunk starts: at
/// most 30 s, and at most a quarter of what is left of the pass.
pub(crate) fn deadline() -> Instant {
    let now = Instant::now();
    let allowance = CONTEXT
        .try_with(|context| context.pass_end.map(|end| end.saturating_duration_since(now) / 4))
        .ok()
        .flatten()
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(30));
    now + allowance
}

/// Recomputes the allowance for every batch, including late refreshes.
pub(crate) struct PassProbe<'a> {
    pub(crate) inner: &'a dyn MergeProbe,
    pub(crate) state: &'a PassState,
    pub(crate) pass_end: Option<Instant>,
}

#[async_trait]
impl MergeProbe for PassProbe<'_> {
    async fn probe(&self, url: &str) -> Result<PrLifecycleProbe> {
        self.inner.probe(url).await
    }
    async fn probe_batch(&self, urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let started = Instant::now();
        let results = CONTEXT.scope(self.context(), self.inner.probe_batch(urls)).await;
        self.record_unrecorded(urls, started);
        results
    }
    async fn probe_shallow_batch(&self, urls: &[String]) -> HashMap<String, std::result::Result<ShallowProbe, String>> {
        let started = Instant::now();
        let results = CONTEXT
            .scope(self.context(), self.inner.probe_shallow_batch(urls))
            .await;
        self.record_unrecorded(urls, started);
        results
    }
}

impl PassProbe<'_> {
    fn context(&self) -> ProbeContext {
        ProbeContext {
            pass_end: self.pass_end,
            trace: self.state.trace.clone(),
            order: self.state.probe_order.clone(),
        }
    }

    /// Stubs never go through [`crate::probe::CommandMergeProbe::probe_chunked`],
    /// so they leave no per-chunk start. Stamp the batch start for any URL
    /// the inner probe did not record.
    fn record_unrecorded(&self, urls: &[String], started: Instant) {
        let mut order = self.state.probe_order.lock().unwrap();
        let missing: Vec<String> = urls
            .iter()
            .filter(|url| !order.observation.per_url.contains_key(*url))
            .cloned()
            .collect();
        if !missing.is_empty() {
            order.observation.record_chunk(&missing, started);
        }
    }
}

/// Dropped on cancellation as well as normal completion.
pub(crate) struct Measurement {
    trace: Option<Arc<PassTrace>>,
    kind: &'static str,
    started: Instant,
    counts: ProbeWork,
}
impl Measurement {
    pub(crate) fn start(kind: &'static str, total: usize) -> Self {
        Self {
            trace: CONTEXT.try_with(|c| c.trace.clone()).ok(),
            kind,
            started: Instant::now(),
            counts: ProbeWork {
                deferred: total,
                ..Default::default()
            },
        }
    }
    pub(crate) fn attempt(&mut self) {
        self.counts.attempted += 1;
    }
    pub(crate) fn complete(&mut self) {
        self.counts.completed += 1;
        self.counts.deferred = self.counts.deferred.saturating_sub(1);
    }
}
impl Drop for Measurement {
    fn drop(&mut self) {
        if let Some(trace) = &self.trace {
            trace.record_probe_work(
                self.kind,
                self.started.elapsed(),
                self.counts.attempted,
                self.counts.completed,
                self.counts.deferred,
            );
        }
    }
}

pub(crate) async fn bounded<T>(deadline: Instant, work: impl Future<Output = T>) -> std::result::Result<T, String> {
    if Instant::now() >= deadline {
        return Err("probe deadline exhausted; retrying next pass".to_owned());
    }
    tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| "probe deadline exhausted; retrying next pass".to_owned())
}
