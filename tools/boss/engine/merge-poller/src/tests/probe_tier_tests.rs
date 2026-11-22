//! A detection pass probes the PRs it acts on in full, every pass, and every
//! other tracked PR only for merge or close, on a slower cadence.
//!
//! The pass is driven through `run_one_pass_tracked` against a probe that
//! records which tier asked about which PR. Time is tokio's virtual clock.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex as StdMutex;

use super::*;
use crate::pass::PROBE_SNAPSHOT_MAX_AGE;
use crate::probe::FULL_PROBE_CHUNK_SIZE;
use crate::probe_tier::{DORMANT_AFTER_SECS, ProbeMemory, SHALLOW_PROBE_INTERVAL, TierInputs, TierReason, plan_tiers};
use tokio::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Full,
    Shallow,
}

/// A probe that answers from a [`StubProbe`] and records, per call, which
/// tier asked and for which PRs.
struct TierProbe {
    stub: Arc<StubProbe>,
    calls: StdMutex<Vec<(Tier, Vec<String>)>>,
}

impl TierProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stub: StubProbe::new(),
            calls: StdMutex::new(Vec::new()),
        })
    }

    /// The PRs each tier was asked about since the last call.
    fn take(&self) -> (HashSet<String>, HashSet<String>) {
        let mut full = HashSet::new();
        let mut shallow = HashSet::new();
        for (tier, urls) in std::mem::take(&mut *self.calls.lock().unwrap()) {
            match tier {
                Tier::Full => full.extend(urls),
                Tier::Shallow => shallow.extend(urls),
            }
        }
        (full, shallow)
    }
}

#[async_trait]
impl MergeProbe for TierProbe {
    async fn probe(&self, url: &str) -> Result<PrLifecycleProbe> {
        self.stub.probe(url).await
    }

    async fn probe_batch(&self, urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        self.calls.lock().unwrap().push((Tier::Full, urls.to_vec()));
        let mut out = HashMap::new();
        for url in urls {
            out.insert(url.clone(), self.stub.probe(url).await.map_err(|err| err.to_string()));
        }
        out
    }

    async fn probe_shallow_batch(&self, urls: &[String]) -> HashMap<String, std::result::Result<ShallowProbe, String>> {
        self.calls.lock().unwrap().push((Tier::Shallow, urls.to_vec()));
        let mut out = HashMap::new();
        for url in urls {
            let result = self
                .stub
                .probe(url)
                .await
                .map(ShallowProbe::from_full)
                .map_err(|err| err.to_string());
            out.insert(url.clone(), result);
        }
        out
    }
}

/// Full-tier probe that records per-chunk starts and, on the first batch,
/// lets later chunks take longer than [`PROBE_SNAPSHOT_MAX_AGE`]. A second
/// `probe_batch` is the walk's refresh: PR 1 has merged.
struct ChunkingProbe {
    stub: Arc<StubProbe>,
    batches: StdMutex<usize>,
}

impl ChunkingProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stub: StubProbe::new(),
            batches: StdMutex::new(0),
        })
    }

    fn batch_count(&self) -> usize {
        *self.batches.lock().unwrap()
    }
}

#[async_trait]
impl MergeProbe for ChunkingProbe {
    async fn probe(&self, url: &str) -> Result<PrLifecycleProbe> {
        self.stub.probe(url).await
    }

    async fn probe_batch(&self, urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let batch = {
            let mut batches = self.batches.lock().unwrap();
            *batches += 1;
            *batches
        };
        if batch > 1 {
            self.stub
                .set_with_base_head(&pr(1), PrLifecycleState::Merged, "base", "head");
        }
        let mut out = HashMap::new();
        for (index, chunk) in urls.chunks(FULL_PROBE_CHUNK_SIZE).enumerate() {
            let started = Instant::now();
            let _ = crate::probe_budget::CONTEXT.try_with(|context| {
                context.order.lock().unwrap().observation.record_chunk(chunk, started);
            });
            if index > 0 && batch == 1 {
                tokio::time::advance(PROBE_SNAPSHOT_MAX_AGE + Duration::from_secs(1)).await;
            }
            for url in chunk {
                out.insert(url.clone(), self.stub.probe(url).await.map_err(|err| err.to_string()));
            }
        }
        out
    }
}

fn pr(n: u64) -> String {
    format!("https://github.com/foo/bar/pull/{n}")
}

fn product(db: &WorkDb, name: &str) -> String {
    create_test_product_with_repo(db, name, Some("git@github.com:foo/bar.git")).id
}

fn row(db: &WorkDb, product_id: &str, n: u64) -> String {
    chore_in_review_for_product(db, product_id, &format!("Card {n}"), &pr(n))
}

async fn pass(db: &WorkDb, probe: &dyn MergeProbe, state: &PassState) -> SweepOutcome {
    run_one_pass_tracked(
        db,
        probe,
        &RecordingPublisher::default(),
        SweepHandlers {
            cube_client: None,
            completion_handler: None,
            host: None,
            parked_ids: None,
        },
        state,
        Duration::MAX,
    )
    .await
    .0
}

fn is_done(db: &WorkDb, id: &str) -> bool {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(task) => task.status == TaskStatus::Done,
        other => panic!("expected chore, got {other:?}"),
    }
}

fn set_of(prs: &[u64]) -> HashSet<String> {
    prs.iter().map(|n| pr(*n)).collect()
}

fn active(ids: &[&str]) -> Option<HashSet<String>> {
    Some(ids.iter().map(|id| (*id).to_owned()).collect())
}

/// An open probe that GitHub last updated at `updated_at`.
fn open_updated_at(url: &str, updated_at: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(url)
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .updated_at(updated_at)
        .build()
}

/// A row in the active product gets the full probe on every pass; a row in
/// another product is probed only for merge or close, and only when due.
#[tokio::test(start_paused = true)]
async fn active_product_rows_are_probed_in_full_every_pass() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    row(&db, &mono, 2);
    row(&db, &other, 3);
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    for pass_number in 0..4 {
        pass(&db, probe.as_ref(), &state).await;
        let (full, shallow) = probe.take();
        assert_eq!(
            full,
            set_of(&[1, 2]),
            "pass {pass_number}: the active product is probed in full"
        );
        assert_eq!(
            shallow,
            if pass_number == 0 { set_of(&[3]) } else { HashSet::new() },
            "pass {pass_number}: the other product is probed once, then not until it is due",
        );
        tokio::time::advance(Duration::from_secs(60)).await;
    }
}

/// A merge in a product that is not active is still detected: at once on the
/// first pass, and later on the slow cadence — never faster, and never by a
/// full probe.
#[tokio::test(start_paused = true)]
async fn another_products_merge_is_detected_on_the_shallow_cadence() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    let merged_first = row(&db, &other, 2);
    let merged_later = row(&db, &other, 3);
    let probe = TierProbe::new();
    probe
        .stub
        .set_with_base_head(&pr(2), PrLifecycleState::Merged, "base", "head");
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    let outcome = pass(&db, probe.as_ref(), &state).await;
    assert_eq!(outcome.merged, 1);
    assert!(
        is_done(&db, &merged_first),
        "a merge in another product is seen on the first pass"
    );
    let (full, shallow) = probe.take();
    assert_eq!(full, set_of(&[1]));
    assert!(shallow.contains(&pr(2)) && shallow.contains(&pr(3)));

    probe
        .stub
        .set_with_base_head(&pr(3), PrLifecycleState::Merged, "base", "head");
    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    assert!(
        !is_done(&db, &merged_later),
        "not asked about again until the shallow interval has passed"
    );
    assert_eq!(probe.take().1, HashSet::new());

    tokio::time::advance(SHALLOW_PROBE_INTERVAL).await;
    let outcome = pass(&db, probe.as_ref(), &state).await;
    assert_eq!(outcome.merged, 1);
    assert!(is_done(&db, &merged_later), "seen once the interval has passed");
    let (full, shallow) = probe.take();
    assert_eq!(shallow, set_of(&[3]));
    assert!(!full.contains(&pr(3)), "never by a full probe");
}

/// A parked row is kept only to record that its PR merged or closed, so it is
/// watched on the shallow cadence and still moves to done when the PR merges.
#[tokio::test(start_paused = true)]
async fn a_parked_rows_merge_is_detected_on_the_shallow_cadence() {
    let (_dir, db) = open_db();
    let mono = product(&db, "Mono");
    row(&db, &mono, 1);
    let parked = row(&db, &mono, 2);
    db.park_work_item_as_actor(&parked, true, Some("not now"), "human")
        .unwrap();
    let probe = TierProbe::new();
    let state = PassState::default();

    pass(&db, probe.as_ref(), &state).await;
    let (full, shallow) = probe.take();
    assert_eq!((full, shallow), (set_of(&[1]), set_of(&[2])));

    probe
        .stub
        .set_with_base_head(&pr(2), PrLifecycleState::Merged, "base", "head");
    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    assert!(!is_done(&db, &parked));

    tokio::time::advance(SHALLOW_PROBE_INTERVAL).await;
    let outcome = pass(&db, probe.as_ref(), &state).await;
    assert_eq!(outcome.merged, 1);
    assert!(
        is_done(&db, &parked),
        "a parked row whose PR merged still moves to done"
    );
}

/// A PR closed without merging is recorded the same way.
#[tokio::test(start_paused = true)]
async fn another_products_unmerged_close_is_detected() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    let closed = row(&db, &other, 2);
    let probe = TierProbe::new();
    probe.stub.set(&pr(2), PrLifecycleState::ClosedUnmerged);
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    let outcome = pass(&db, probe.as_ref(), &state).await;

    assert_eq!(outcome.closed_unmerged, 1);
    assert!(is_done(&db, &closed));
}

/// What a shallow probe finds on an open PR is nothing to act on: the pass
/// walks only the PRs it probed in full.
#[tokio::test(start_paused = true)]
async fn shallow_rows_that_are_still_open_are_not_walked() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    for n in 2..=6 {
        row(&db, &other, n);
    }
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;

    let timings = state.trace().timings();
    let open_walk = timings
        .phases
        .iter()
        .find(|phase| phase.phase == PassPhase::OpenWalk)
        .unwrap();
    assert_eq!(
        (open_walk.items_done, open_walk.items_total),
        (1, 1),
        "{}",
        timings.breakdown()
    );
    assert!(!timings.open_walk_truncated());
}

/// Blocked rows are acted on whatever product they are in.
#[tokio::test(start_paused = true)]
async fn blocked_rows_are_probed_in_full_in_every_product() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    let blocked = row(&db, &other, 2);
    db.mark_chore_blocked_ci_failure(&blocked, &pr(2), None).unwrap();
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;

    let (full, shallow) = probe.take();
    assert_eq!(
        full,
        set_of(&[1, 2]),
        "the blocked row in the other product is probed in full"
    );
    assert_eq!(shallow, HashSet::new());
}

/// A merge in flight is the next thing that happens to a PR, in any product.
#[tokio::test(start_paused = true)]
async fn a_row_with_a_merge_in_flight_is_probed_in_full_in_every_product() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    let merging = row(&db, &other, 2);
    row(&db, &other, 3);
    db.record_merge_intent(&merging, &pr(2), Some("head-2")).unwrap();
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;

    let (full, shallow) = probe.take();
    assert_eq!(full, set_of(&[1, 2]));
    assert_eq!(shallow, set_of(&[3]));
}

/// A PR last seen in a merge queue keeps the full probe after the active
/// products move to another product.
#[tokio::test(start_paused = true)]
async fn a_pr_last_seen_queued_stays_in_the_full_tier() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    row(&db, &other, 2);
    let probe = TierProbe::new();
    probe.stub.states.lock().unwrap().insert(
        pr(2),
        Ok(probe_with_queue_fields(
            true,
            Some("QUEUED"),
            Some(1),
            Some("2026-11-21T00:00:00Z"),
            false,
            None,
        )),
    );
    let state = PassState::default();

    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(
        probe.take().0,
        set_of(&[1, 2]),
        "no product singled out: everything is active"
    );

    state.set_active_products(active(&[&mono]));
    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    let (full, shallow) = probe.take();
    assert_eq!((full, shallow), (set_of(&[1, 2]), HashSet::new()));
}

/// Durable queue membership survives an engine restart: a fresh pass with empty
/// in-memory flags still probes a queued other-product row in full.
#[tokio::test(start_paused = true)]
async fn a_queued_other_product_row_is_full_after_restart() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    let queued = row(&db, &other, 2);
    seed_queued(&db, &queued, Some(1), "2026-11-21T00:00:00Z", "QUEUED");
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;

    let (full, shallow) = probe.take();
    assert_eq!(
        (full, shallow),
        (set_of(&[1, 2]), HashSet::new()),
        "durable merge_queue_state puts the other-product row in the full tier"
    );
}

/// A shallow probe that sees GitHub queue the PR promotes it on the next pass.
#[tokio::test(start_paused = true)]
async fn an_external_enqueue_promotes_a_shallow_row_on_the_next_pass() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    row(&db, &other, 2);
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(probe.take(), (set_of(&[1]), set_of(&[2])));

    probe.stub.states.lock().unwrap().insert(
        pr(2),
        Ok(probe_with_queue_fields(
            true,
            Some("QUEUED"),
            Some(1),
            Some("2026-11-21T00:00:00Z"),
            false,
            None,
        )),
    );
    tokio::time::advance(SHALLOW_PROBE_INTERVAL).await;
    pass(&db, probe.as_ref(), &state).await;
    let (full, shallow) = probe.take();
    assert_eq!(full, set_of(&[1]));
    assert_eq!(
        shallow,
        set_of(&[2]),
        "this pass's shallow probe is what notices the queue"
    );

    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(
        probe.take(),
        (set_of(&[1, 2]), HashSet::new()),
        "queued in memory: full on the next pass"
    );
}

/// A parked row with an active merge request stays in the full tier so the
/// merge is seen on the next pass, not five minutes later. Open-PR upkeep
/// still does not run while it is parked.
#[tokio::test(start_paused = true)]
async fn a_parked_row_with_a_merge_intent_is_probed_in_full() {
    let (_dir, db) = open_db();
    let mono = product(&db, "Mono");
    row(&db, &mono, 1);
    let parked = row(&db, &mono, 2);
    db.park_work_item_as_actor(&parked, true, Some("not now"), "human")
        .unwrap();
    db.record_merge_intent(&parked, &pr(2), Some("head-2")).unwrap();
    let probe = TierProbe::new();
    let state = PassState::default();

    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(
        probe.take(),
        (set_of(&[1, 2]), HashSet::new()),
        "merge in flight wins over the parked demotion"
    );

    probe
        .stub
        .set_with_base_head(&pr(2), PrLifecycleState::Merged, "base", "head");
    tokio::time::advance(Duration::from_secs(60)).await;
    let outcome = pass(&db, probe.as_ref(), &state).await;
    assert_eq!(outcome.merged, 1);
    assert!(
        is_done(&db, &parked),
        "a parked merge-intent row is marked done on the next full pass"
    );
}

/// The first full chunk answers quickly; later chunks take more than the
/// snapshot's max age. The walk must re-probe before acting, or it would
/// write from an Open result that has since merged.
#[tokio::test(start_paused = true)]
async fn a_slow_later_chunk_ages_the_snapshot_from_the_first_chunk_start() {
    let (_dir, db) = open_db();
    let mono = product(&db, "Mono");
    let first = row(&db, &mono, 1);
    for n in 2..=(FULL_PROBE_CHUNK_SIZE as u64 + 1) {
        row(&db, &mono, n);
    }
    let probe = ChunkingProbe::new();
    let state = PassState::default();

    let outcome = pass(&db, probe.as_ref(), &state).await;

    assert_eq!(
        probe.batch_count(),
        2,
        "the walk re-probes once the first chunk is older than the snapshot max age"
    );
    assert_eq!(outcome.merged, 1);
    assert!(
        is_done(&db, &first),
        "the refresh sees the first PR merge instead of acting on the stale open result"
    );
}

/// A PR GitHub has not touched for two weeks is watched cheaply, and
/// returns to the full tier as soon as a probe shows it was updated.
#[tokio::test(start_paused = true)]
async fn a_dormant_pr_returns_to_the_full_tier_when_it_is_updated() {
    let (_dir, db) = open_db();
    let mono = product(&db, "Mono");
    row(&db, &mono, 1);
    row(&db, &mono, 2);
    let probe = TierProbe::new();
    let old = "2020-01-01T00:00:00Z";
    probe
        .stub
        .states
        .lock()
        .unwrap()
        .insert(pr(1), Ok(open_updated_at(&pr(1), old)));
    let state = PassState::default();

    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(
        probe.take().0,
        set_of(&[1, 2]),
        "nothing is known yet, so nothing is dormant"
    );

    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    let (full, shallow) = probe.take();
    assert_eq!(
        (full, shallow),
        (set_of(&[2]), HashSet::new()),
        "dormant, and seen a minute ago"
    );

    // Somebody pushes to PR 1.
    let fresh = chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs(), 0)
        .unwrap()
        .to_rfc3339();
    probe
        .stub
        .states
        .lock()
        .unwrap()
        .insert(pr(1), Ok(open_updated_at(&pr(1), &fresh)));
    tokio::time::advance(SHALLOW_PROBE_INTERVAL).await;
    pass(&db, probe.as_ref(), &state).await;
    let (full, shallow) = probe.take();
    assert_eq!(
        (full, shallow),
        (set_of(&[2]), set_of(&[1])),
        "the shallow probe reads updatedAt"
    );

    tokio::time::advance(Duration::from_secs(60)).await;
    pass(&db, probe.as_ref(), &state).await;
    assert_eq!(probe.take().0, set_of(&[1, 2]), "active again: full every pass");
}

/// A shallow probe that fails leaves its PR due, so it is asked again next
/// pass rather than waiting out the interval.
#[tokio::test(start_paused = true)]
async fn a_failed_shallow_probe_is_retried_on_the_next_pass() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    row(&db, &other, 2);
    let probe = TierProbe::new();
    probe.stub.set_err(&pr(2), "HTTP 504");
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    for _ in 0..3 {
        pass(&db, probe.as_ref(), &state).await;
        assert_eq!(probe.take().1, set_of(&[2]), "still due after a failure");
        tokio::time::advance(Duration::from_secs(60)).await;
    }
}

/// With no product singled out every product is active, as before tiering.
#[tokio::test(start_paused = true)]
async fn every_product_is_active_when_none_is_singled_out() {
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    row(&db, &mono, 1);
    row(&db, &other, 2);
    let probe = TierProbe::new();
    let state = PassState::default();
    state.set_active_products(Some(HashSet::new()));

    pass(&db, probe.as_ref(), &state).await;

    assert_eq!(probe.take(), (set_of(&[1, 2]), HashSet::new()));
}

/// The pass logs, per tier, how many PRs it probed, how many chunks were
/// attempted, failed and deferred, and how long the oldest unobserved PR has
/// gone unseen, so starvation shows as a number that keeps growing.
#[tokio::test(start_paused = true)]
async fn the_pass_logs_probe_tiers_and_the_oldest_unobserved_age() {
    let logs = log_capture::install();
    let (_dir, db) = open_db();
    let (mono, other) = (product(&db, "Mono"), product(&db, "Other"));
    for n in 1..=3 {
        row(&db, &mono, 7000 + n);
    }
    for n in 1..=7 {
        row(&db, &other, 8000 + n);
    }
    let probe = TierProbe::new();
    // Never answered: the oldest unobserved age keeps growing.
    for n in 1..=7 {
        probe.stub.set_err(&pr(8000 + n), "HTTP 504");
    }
    let state = PassState::default();
    state.set_active_products(active(&[&mono]));

    pass(&db, probe.as_ref(), &state).await;
    tokio::time::advance(Duration::from_secs(400)).await;
    pass(&db, probe.as_ref(), &state).await;

    let logs = String::from_utf8_lossy(&logs.lock()).into_owned();
    let line = logs
        .lines()
        .rev()
        .find(|line| line.contains("probe tiers") && line.contains("shallow_prs=7"))
        .unwrap_or_else(|| panic!("no tier line in:\n{logs}"));
    for expected in [
        "full_prs=3",
        "full_observed=3",
        "shallow_prs=7",
        "shallow_due=7",
        "shallow_observed=0",
        "shallow_oldest_unobserved_secs=400",
        "full_oldest_unobserved_secs=0",
        "rows_active_product=3",
        "rows_other_product=7",
    ] {
        assert!(line.contains(expected), "{expected} missing from: {line}");
    }
}

// ── The tier decision, without a pass ──────────────────────────────────────

fn candidate(work_item_id: &str, product_id: &str, n: u64) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id: work_item_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: pr(n),
    }
}

#[test]
fn the_tier_decision_orders_merging_before_parked_before_product_before_dormancy() {
    let now = Instant::now();
    let rows = [
        candidate("parked-merging", "a", 1),
        candidate("merging-elsewhere", "b", 2),
        candidate("elsewhere", "b", 3),
        candidate("dormant", "a", 4),
        candidate("live", "a", 5),
    ];
    let mut memory = ProbeMemory::default();
    memory.track(&rows.iter().map(|row| row.pr_url.as_str()).collect(), now);
    let epoch_now = 1_800_000_000;
    memory.observe_full(&pr(4), &open_updated_at(&pr(4), "2020-01-01T00:00:00Z"), now);
    let parked: HashSet<String> = ["parked-merging".to_owned()].into();
    let merging: HashSet<String> = ["parked-merging".to_owned(), "merging-elsewhere".to_owned()].into();
    let active_products: HashSet<String> = ["a".to_owned()].into();

    let plan = plan_tiers(
        &TierInputs {
            always_full: vec![],
            in_review: &rows,
            parked: &parked,
            merge_in_flight: Some(&merging),
            active_products: Some(&active_products),
        },
        &memory,
        now,
        epoch_now,
    );

    assert_eq!(plan.full, vec![pr(1), pr(2), pr(5)]);
    assert_eq!(plan.shallow, vec![pr(3), pr(4)]);
    let counts = |reason: TierReason| plan.reasons.get(reason.key()).copied().unwrap_or(0);
    assert_eq!(counts(TierReason::Parked), 0);
    assert_eq!(counts(TierReason::MergeInFlight), 2);
    assert_eq!(counts(TierReason::OtherProduct), 1);
    assert_eq!(counts(TierReason::Dormant), 1);
    assert_eq!(counts(TierReason::ActiveProduct), 1);
}

#[test]
fn unknown_merges_in_flight_probe_every_row_in_full() {
    let now = Instant::now();
    let rows = [candidate("elsewhere", "b", 1), candidate("parked", "b", 2)];
    let mut memory = ProbeMemory::default();
    memory.track(&rows.iter().map(|row| row.pr_url.as_str()).collect(), now);
    let parked: HashSet<String> = ["parked".to_owned()].into();
    let active_products: HashSet<String> = ["a".to_owned()].into();

    let plan = plan_tiers(
        &TierInputs {
            always_full: vec![],
            in_review: &rows,
            parked: &parked,
            merge_in_flight: None,
            active_products: Some(&active_products),
        },
        &memory,
        now,
        1_800_000_000,
    );

    assert_eq!((plan.full, plan.shallow), (vec![pr(1), pr(2)], vec![]));
}

#[test]
fn memory_forgets_untracked_prs_and_never_remembers_an_unmerged_verdict() {
    let now = Instant::now();
    let mut memory = ProbeMemory::default();
    memory.track(&[pr(1).as_str(), pr(2).as_str()].into_iter().collect(), now);
    memory.observe_shallow(&pr(1), Some("2020-01-01T00:00:00Z"), true, now);
    assert!(memory.is_queued(&pr(1)), "a shallow probe can notice a queue");
    memory.observe_shallow(&pr(1), Some("2020-01-01T00:00:00Z"), false, now);
    assert!(!memory.is_queued(&pr(1)));
    assert!(memory.is_dormant(&pr(1), 1_800_000_000));
    assert!(!memory.is_dormant(&pr(2), 1_800_000_000), "never probed is not dormant");
    assert!(!memory.shallow_due(&pr(1), now));
    assert!(memory.shallow_due(&pr(2), now), "never observed is always due");

    memory.track(&[pr(2).as_str()].into_iter().collect(), now);
    assert!(
        !memory.is_dormant(&pr(1), 1_800_000_000),
        "forgotten with the row; a returning PR starts unobserved",
    );
    assert_eq!(DORMANT_AFTER_SECS, 14 * 24 * 60 * 60);
}
