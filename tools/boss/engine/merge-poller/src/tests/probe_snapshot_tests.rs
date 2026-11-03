//! Direct tests for [`ProbeSnapshot`], the guard that stops a slow pass
//! writing lifecycle transitions off a stale view of GitHub.
//!
//! This is the safety net for the incident behind the remediation move-off:
//! PR #2346 merged ten seconds after a pass took its single batched probe
//! snapshot, and the pass — stuck behind 32 minutes of inline conflict
//! remediation — went on using that snapshot for another half hour. Once
//! remediation is off the detection path the ageing logic should almost never
//! fire, which is exactly why it needs its own tests: a safety net nothing
//! routinely exercises is a safety net that rots.
//!
//! Time is driven with tokio's virtual clock (`ProbeSnapshot::taken_at` is a
//! `tokio::time::Instant`), so "the pass got slow" is a deterministic
//! `advance()` rather than a real-world stall.

use boss_protocol::FrontendEvent;

use super::*;

/// The URL set of every `probe_batch` call, in call order.
type RecordedBatches = Arc<std::sync::Mutex<Vec<Vec<String>>>>;

/// Probe that records the exact URL set of every batch it is asked for, so a
/// test can assert both *how many* re-probes happened and *what* they cost.
struct RecordingProbe {
    inner: Arc<StubProbe>,
    batches: RecordedBatches,
}

impl RecordingProbe {
    fn new(inner: Arc<StubProbe>) -> (Arc<Self>, RecordedBatches) {
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                inner,
                batches: batches.clone(),
            }),
            batches,
        )
    }
}

#[async_trait]
impl MergeProbe for RecordingProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        self.inner.probe(pr_url).await
    }

    async fn probe_batch(&self, pr_urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        self.batches.lock().unwrap().push(pr_urls.to_vec());
        let mut out = HashMap::new();
        for url in pr_urls {
            if out.contains_key(url) {
                continue;
            }
            let result = self.inner.probe(url).await.map_err(|err| err.to_string());
            out.insert(url.clone(), result);
        }
        out
    }
}

/// Batch sizes, in call order.
fn batch_sizes(batches: &RecordedBatches) -> Vec<usize> {
    batches.lock().unwrap().iter().map(Vec::len).collect()
}

/// Publisher that ages the pass by one full snapshot lifetime every time a PR
/// is retired as merged. The deterministic stand-in for a pass that has got
/// slow enough for its snapshot to go stale part-way through its candidate
/// walk — without needing anything in the pass to actually block.
struct AgeingPublisher {
    inner: RecordingPublisher,
}

impl AgeingPublisher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RecordingPublisher::default(),
        })
    }
}

#[async_trait]
impl ExecutionPublisher for AgeingPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        self.inner.publish(execution_id, work_item_id, status, reason).await;
    }

    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.inner
            .publish_work_item_changed(product_id, work_item_id, reason)
            .await;
        if reason == "pr_merged" {
            tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE + Duration::from_secs(1)).await;
        }
    }

    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        self.inner.publish_frontend_event_on_product(product_id, event).await;
    }
}

fn urls(n: usize) -> Vec<String> {
    (1..=n)
        .map(|i| format!("https://github.com/foo/bar/pull/{i}"))
        .collect()
}

/// (a) A snapshot inside its max age is used as-is. The closure that would
/// build the re-probe URL set is never even called, so the fresh path costs
/// nothing — no allocation, and certainly no GitHub round trip.
#[tokio::test(start_paused = true)]
async fn a_fresh_snapshot_is_never_re_probed() {
    let (probe, batches) = RecordingProbe::new(StubProbe::new());
    let probe_urls = urls(3);
    let mut snapshot = ProbeSnapshot::new(probe.probe_batch(&probe_urls).await);
    assert_eq!(batch_sizes(&batches), vec![3], "setup: one batched probe up front");

    tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE - Duration::from_secs(1)).await;

    assert!(
        snapshot
            .ensure_fresh(probe.as_ref(), || panic!("a fresh snapshot must not re-probe"))
            .await,
    );
    assert_eq!(
        batch_sizes(&batches),
        vec![3],
        "a snapshot inside PROBE_SNAPSHOT_MAX_AGE must not cost a second batch",
    );
}

/// (b) Past the max age the snapshot is re-taken, and `taken_at` restarts —
/// so the pass gets a full fresh window rather than re-probing on every
/// subsequent candidate.
#[tokio::test(start_paused = true)]
async fn a_stale_snapshot_is_re_probed_and_its_clock_restarts() {
    let (probe, batches) = RecordingProbe::new(StubProbe::new());
    let probe_urls = urls(3);
    let mut snapshot = ProbeSnapshot::new(probe.probe_batch(&probe_urls).await);

    tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE + Duration::from_secs(1)).await;
    assert!(snapshot.ensure_fresh(probe.as_ref(), || probe_urls.clone()).await);
    assert_eq!(
        batch_sizes(&batches),
        vec![3, 3],
        "a stale snapshot must be re-taken before any further state is written",
    );

    // Still inside the *new* window: free again.
    tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE - Duration::from_secs(1)).await;
    assert!(
        snapshot
            .ensure_fresh(probe.as_ref(), || panic!("taken_at was not refreshed by the re-probe"))
            .await,
    );
    assert_eq!(batch_sizes(&batches), vec![3, 3]);
}

/// (c) The refresh budget is finite. Once it is spent, `ensure_fresh` reports
/// failure rather than re-probing forever or — far worse — writing state off
/// the stale snapshot.
#[tokio::test(start_paused = true)]
async fn ensure_fresh_gives_up_once_the_per_pass_refresh_budget_is_spent() {
    let (probe, batches) = RecordingProbe::new(StubProbe::new());
    let probe_urls = urls(3);
    let mut snapshot = ProbeSnapshot::new(probe.probe_batch(&probe_urls).await);

    for n in 0..MAX_PROBE_REFRESHES_PER_PASS {
        tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE + Duration::from_secs(1)).await;
        assert!(
            snapshot.ensure_fresh(probe.as_ref(), || probe_urls.clone()).await,
            "refresh {n} is within budget",
        );
    }
    tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE + Duration::from_secs(1)).await;

    assert!(
        !snapshot.ensure_fresh(probe.as_ref(), || probe_urls.clone()).await,
        "past the budget the caller must be told to stop, not handed a stale snapshot",
    );
    assert_eq!(
        batches.lock().unwrap().len(),
        1 + usize::from(MAX_PROBE_REFRESHES_PER_PASS),
        "a pathologically slow pass must not become an unbounded GitHub-quota amplifier",
    );
}

/// (c) again, end to end: a pass that goes stale mid-walk stops rather than
/// transitioning its remaining candidates, and every re-probe covers only the
/// part of the walk that has not happened yet.
///
/// Four merged PRs, and each retire ages the snapshot past its lifetime:
/// candidate 1 runs on the initial snapshot, candidates 2 and 3 each spend one
/// refresh, and candidate 4 finds the budget gone and is left for the next
/// pass — which loses nothing, since every candidate list is rebuilt from the
/// DB.
#[tokio::test(start_paused = true)]
async fn a_pass_that_goes_stale_mid_walk_stops_and_re_probes_only_the_remainder() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let stub = StubProbe::new();
    let mut chores = Vec::new();
    for (i, pr) in urls(4).iter().enumerate() {
        let (_product, chore) = make_chore_in_review(&db, &format!("C{i}"), pr);
        stub.set(pr, PrLifecycleState::Merged);
        chores.push(chore);
    }
    let (probe, batches) = RecordingProbe::new(stub);
    let publisher = AgeingPublisher::new();

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert_eq!(
        outcome.merged, 3,
        "the pass must abandon its remainder once the refresh budget is spent, got: {outcome:?}",
    );
    let done = chores
        .iter()
        .filter(|id| match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => t.status == TaskStatus::Done,
            other => panic!("expected chore, got {other:?}"),
        })
        .count();
    assert_eq!(
        done, 3,
        "exactly the candidates walked before the budget ran out may transition",
    );
    assert_eq!(
        batch_sizes(&batches),
        vec![4, 3, 2],
        "each re-probe must cover only the unprocessed remainder of the walk, \
         not the pass's whole original candidate set",
    );
}
