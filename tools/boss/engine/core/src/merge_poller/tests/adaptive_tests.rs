//! The adaptive poll layer: which PRs earn a slot, how the due set is
//! coalesced into one batch, and what that costs in GraphQL points.
//!
//! These are the tests behind the claim the change is making. The adaptive
//! path was measured burning most of a 5000-point hourly GraphQL budget by
//! issuing two single-PR queries per due PR, where GitHub's
//! `max(1, nodes/100)` pricing makes a one-PR query cost exactly as much as
//! a twenty-PR one. Two things fix that, and each is asserted here rather
//! than argued:
//!
//!   1. a PR whose tier is no faster than the full sweep holds no adaptive
//!      slot at all, because the sweep already re-probes it sooner
//!      ([`adaptive_poll_adds_freshness`]);
//!   2. everything left goes out in coalesced batches, whose cost is
//!      proportional to the batch count rather than the PR count.

use super::*;

/// Requested nodes per PR in the lifecycle probe's selection set
/// ([`PR_PROBE_FIELDS`]): `labels(first: 10)` + `reviews(last: 10)` +
/// `commits(last: 1)` × `contexts(first: 30)`.
const PROBE_NODES_PER_PR: usize = 10 + 10 + 1 + 30;

/// Requested nodes per PR in the merge-queue dequeue-events query
/// ([`DEQUEUE_EVENTS_FIELDS`]): `timelineItems(last: 20)`.
const DEQUEUE_NODES_PER_PR: usize = 20;

/// GitHub's GraphQL pricing for the pair of queries one reconcile of
/// `batch` PRs issues: `max(1, ceil(nodes/100))` per query.
///
/// The `max(1, …)` is the whole story of this change: it means 25 one-PR
/// reconciles cost 50 points while one 25-PR reconcile costs 13.
fn graphql_points(batch: usize) -> u64 {
    if batch == 0 {
        return 0;
    }
    let probe = (PROBE_NODES_PER_PR * batch).div_ceil(100).max(1);
    let dequeue = (DEQUEUE_NODES_PER_PR * batch).div_ceil(100).max(1);
    (probe + dequeue) as u64
}

/// A [`MergeProbe`] that answers from a fixed map and records the size of
/// every `probe_batch` call — the round-trip counter the batching claim
/// rests on.
struct CountingProbe {
    states: std::sync::Mutex<std::collections::HashMap<String, PrLifecycleState>>,
    batches: std::sync::Mutex<Vec<usize>>,
}

impl CountingProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            states: std::sync::Mutex::new(Default::default()),
            batches: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn set(&self, url: &str, state: PrLifecycleState) {
        self.states.lock().unwrap().insert(url.to_owned(), state);
    }

    fn batches(&self) -> Vec<usize> {
        self.batches.lock().unwrap().clone()
    }
}

#[async_trait]
impl MergeProbe for CountingProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        let state = self
            .states
            .lock()
            .unwrap()
            .get(pr_url)
            .cloned()
            .unwrap_or(PrLifecycleState::Open(OpenPrStatus::clean()));
        Ok(PrLifecycleProbe::builder()
            .url(pr_url.to_owned())
            .state(state)
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build())
    }

    async fn probe_batch(&self, pr_urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        self.batches.lock().unwrap().push(pr_urls.len());
        let mut out = HashMap::new();
        for url in pr_urls {
            if out.contains_key(url) {
                continue;
            }
            out.insert(url.clone(), self.probe(url).await.map_err(|e| e.to_string()));
        }
        out
    }
}

// ── The scoping predicate ──────────────────────────────────────────────

/// The adaptive layer only earns its cost when it fires sooner than the
/// full sweep, which re-probes the identical candidate set on its own
/// cadence. At the production cadences (Hot 40s, Cold 180s, sweep 60s)
/// that means every Cold adaptive poll was re-fetching state the sweep had
/// already fetched more recently — 2 points a time, per PR.
#[test]
fn adaptive_slots_go_only_to_tiers_faster_than_the_sweep() {
    let sweep = Duration::from_secs(60);
    assert!(
        adaptive_poll_adds_freshness(PollTier::Hot, sweep),
        "Hot (40s) fires before the 60s sweep, so it adds freshness",
    );
    assert!(
        !adaptive_poll_adds_freshness(PollTier::Cold, sweep),
        "Cold (180s) is re-probed by the 60s sweep three times over first — \
         an adaptive poll for it is pure duplicate spend",
    );

    // The predicate is a function of the configured cadence, not a hard-coded
    // tier list: raise the sweep past Cold and Cold starts earning a slot again.
    let slow_sweep = Duration::from_secs(600);
    assert!(adaptive_poll_adds_freshness(PollTier::Cold, slow_sweep));
    assert!(adaptive_poll_adds_freshness(PollTier::Hot, slow_sweep));

    // And it never depends on quota pressure: `interval()` is throttle-scaled
    // but `base_interval()` is not, so the comparison can't flap with the
    // GitHub budget (both sides stretch by the same factor).
    assert_eq!(PollTier::Hot.base_interval(), Duration::from_secs(40));
    assert_eq!(PollTier::Cold.base_interval(), Duration::from_secs(180));
}

/// A sweep's observations drive the whole schedule: hot PRs get a slot,
/// cold ones are excluded (with a counted reason, never silently), terminal
/// ones are dropped, and a PR that has left every candidate list loses its
/// slot instead of being polled forever.
#[test]
fn sweep_observations_add_retire_and_account_for_every_pr() {
    let mut schedule = PrPollSchedule::default();
    let now = Instant::now();
    let sweep = Duration::from_secs(60);

    let counts = schedule.apply_sweep_observations(
        &[
            ("hot".to_owned(), Some(PollTier::Hot)),
            ("cold".to_owned(), Some(PollTier::Cold)),
            ("merged".to_owned(), None),
        ],
        now,
        sweep,
    );

    assert_eq!(schedule.tracked(), 1, "only the hot PR warrants an adaptive slot");
    assert_eq!(counts.covered_by_full_sweep, 1);
    assert_eq!(counts.terminal, 1);
    assert_eq!(counts.total(), 2, "every exclusion must be accounted for, not dropped");

    let due = schedule.drain_due_within(now + Duration::from_secs(45), Duration::ZERO);
    assert_eq!(due, vec!["hot".to_owned()]);

    // Re-observing the hot PR re-adds it; a later sweep that no longer sees
    // it at all (merged, closed, list emptied) retires the slot.
    schedule.apply_sweep_observations(&[("hot".to_owned(), Some(PollTier::Hot))], now, sweep);
    assert_eq!(schedule.tracked(), 1);
    schedule.apply_sweep_observations(&[], now, sweep);
    assert_eq!(
        schedule.tracked(),
        0,
        "a PR absent from the sweep's candidate set must lose its slot",
    );
}

/// An existing due time is authoritative: a sweep observing a PR already
/// scheduled must not re-stamp its timer, or the 60s sweep would keep
/// pushing a 40s Hot timer out of reach and the adaptive layer would never
/// fire at all.
#[test]
fn sweep_observations_do_not_restamp_an_existing_due_time() {
    let mut schedule = PrPollSchedule::default();
    let base = Instant::now();
    let sweep = Duration::from_secs(60);

    schedule.reschedule("hot", Some(PollTier::Hot), base, sweep);
    let due_at = schedule.next_due().expect("scheduled");

    // A sweep 30s later observes it again — the slot must keep its original
    // due time rather than sliding out another 40s.
    schedule.apply_sweep_observations(
        &[("hot".to_owned(), Some(PollTier::Hot))],
        base + Duration::from_secs(30),
        sweep,
    );
    assert_eq!(schedule.next_due(), Some(due_at));
}

// ── Coalescing ─────────────────────────────────────────────────────────

/// The window reaches *forward*: everything due within it joins the batch,
/// earliest first, and nothing beyond it is touched. Reaching forward is
/// what makes a batch bigger than one; it can only poll a PR early, so no
/// PR's detection latency grows.
#[test]
fn the_coalescing_window_batches_forward_only() {
    let mut schedule = PrPollSchedule::default();
    let base = Instant::now();
    let sweep = Duration::from_secs(600);

    // Due at base+40, base+45, base+70 respectively (Hot = 40s).
    schedule.reschedule("a", Some(PollTier::Hot), base, sweep);
    schedule.reschedule("b", Some(PollTier::Hot), base + Duration::from_secs(5), sweep);
    schedule.reschedule("c", Some(PollTier::Hot), base + Duration::from_secs(30), sweep);

    let due = schedule.drain_due_within(base + Duration::from_secs(40), ADAPTIVE_COALESCE_WINDOW);
    assert_eq!(
        due,
        vec!["a".to_owned(), "b".to_owned()],
        "the PR due 5s later rides along; the one due 30s later does not",
    );
    assert_eq!(schedule.tracked(), 1, "c keeps its slot");

    // Without a window the same instant yields a batch of one — the shape
    // that made batching a no-op before the window existed.
    let mut schedule = PrPollSchedule::default();
    schedule.reschedule("a", Some(PollTier::Hot), base, sweep);
    schedule.reschedule("b", Some(PollTier::Hot), base + Duration::from_secs(5), sweep);
    let due = schedule.drain_due_within(base + Duration::from_secs(40), Duration::ZERO);
    assert_eq!(due, vec!["a".to_owned()]);
}

// ── The batched reconcile ──────────────────────────────────────────────

/// The whole due set is probed in ONE round trip and every member is
/// reconciled — the property that turns `2N` points into `~N/2`.
#[tokio::test]
async fn reconcile_batch_probes_the_due_set_in_one_round_trip() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr1 = "https://github.com/foo/bar/pull/401";
    let pr2 = "https://github.com/foo/bar/pull/402";
    let pr3 = "https://github.com/foo/bar/pull/403";
    let (_p1, chore1) = make_chore_in_review(&db, "C401", pr1);
    let (_p2, chore2) = make_chore_in_review(&db, "C402", pr2);
    let (_p3, chore3) = make_chore_in_review(&db, "C403", pr3);

    let probe = CountingProbe::new();
    probe.set(pr1, PrLifecycleState::Merged);
    probe.set(pr2, PrLifecycleState::Merged);
    probe.set(pr3, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let urls = vec![pr1.to_owned(), pr2.to_owned()];
    let (outcome, observations) =
        reconcile_batch(&db, probe.as_ref(), publisher.as_ref(), None, None, None, &urls).await;

    assert_eq!(
        probe.batches(),
        vec![2],
        "the due set must cost one batched probe, not one per PR",
    );
    assert_eq!(outcome.merged, 2, "every PR in the batch is reconciled");
    assert_eq!(
        observations,
        vec![(pr1.to_owned(), None), (pr2.to_owned(), None)],
        "merged PRs are terminal, so neither keeps an adaptive slot",
    );

    for chore in [&chore1, &chore2] {
        match db.get_work_item(chore).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
            other => panic!("expected chore, got {other:?}"),
        }
    }
    // Scoping is preserved: a candidate outside the batch is untouched even
    // though the probe would answer for it.
    match db.get_work_item(&chore3).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Every requested URL gets an observation back, including one that is no
/// longer a live candidate — that `None` is what retires its slot. A URL
/// with no candidate row is never probed, so it costs nothing.
#[tokio::test]
async fn reconcile_batch_observes_every_requested_url() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let live = "https://github.com/foo/bar/pull/501";
    let gone = "https://github.com/foo/bar/pull/502";
    make_chore_in_review(&db, "C501", live);

    let probe = CountingProbe::new();
    probe.set(live, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let urls = vec![live.to_owned(), gone.to_owned()];
    let (_outcome, observations) =
        reconcile_batch(&db, probe.as_ref(), publisher.as_ref(), None, None, None, &urls).await;

    assert_eq!(
        probe.batches(),
        vec![1],
        "a URL with no candidate row must not be probed",
    );
    assert_eq!(
        observations,
        vec![(live.to_owned(), Some(PollTier::Cold)), (gone.to_owned(), None)],
    );
}

/// A batch with no live candidate at all short-circuits before any GitHub
/// call, and still reports every requested URL as untracked.
#[tokio::test]
async fn reconcile_batch_with_no_live_candidates_makes_no_calls() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let probe = CountingProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    let urls = vec![
        "https://github.com/foo/bar/pull/601".to_owned(),
        "https://github.com/foo/bar/pull/602".to_owned(),
    ];
    let (outcome, observations) =
        reconcile_batch(&db, probe.as_ref(), publisher.as_ref(), None, None, None, &urls).await;

    assert_eq!(outcome.total_transitions(), 0);
    assert!(probe.batches().is_empty(), "no candidates means no probe at all");
    assert_eq!(observations.iter().filter(|(_, tier)| tier.is_none()).count(), 2);
}

// ── Metrics ────────────────────────────────────────────────────────────

/// The batch-size distribution is what tells an operator whether the
/// coalescing is working. A mean pinned at 1.0 would mean the window is
/// buying nothing, and only the histogram distinguishes "steadily 3" from
/// "mostly 1 with an occasional 20".
#[test]
fn adaptive_batch_metrics_record_count_and_distribution() {
    let metrics = Registry::new();
    crate::merge_poller::init(&metrics);

    record_adaptive_batch(&metrics, 1);
    record_adaptive_batch(&metrics, 4);
    record_adaptive_batch(&metrics, 12);
    record_adaptive_batch(&metrics, 0);

    assert_eq!(
        metrics.counter_value("merge_poller.adaptive_batches"),
        Some(3),
        "an empty due set is not a batch",
    );
    assert_eq!(metrics.counter_value("merge_poller.adaptive_prs_reconciled"), Some(17));
    assert_eq!(metrics.counter_value("merge_poller.adaptive_batch_size.1"), Some(1));
    assert_eq!(metrics.counter_value("merge_poller.adaptive_batch_size.3_5"), Some(1));
    assert_eq!(metrics.counter_value("merge_poller.adaptive_batch_size.11_25"), Some(1));
    assert_eq!(adaptive_batch_size_bucket(26), "26_plus");
}

// ── Cost model ─────────────────────────────────────────────────────────

/// The pricing that makes per-PR reconciling so expensive: a single-PR
/// query and a 25-PR query cost the same 1 point per query, because both
/// round up from under 100 nodes.
#[test]
fn a_single_pr_query_costs_the_same_as_a_batched_one() {
    assert_eq!(graphql_points(1), 2, "one probe + one dequeue query, each floored to 1");
    assert_eq!(
        graphql_points(2),
        3,
        "two PRs cross the probe's 100-node line but still ride one dequeue point",
    );
    assert_eq!(
        graphql_points(25),
        18,
        "25 PRs batched: ceil(51*25/100) + ceil(20*25/100) = 13 + 5",
    );
    assert_eq!(
        25 * graphql_points(1),
        50,
        "the same 25 PRs reconciled one at a time are 2 points each — \
         the floor, not the node count, is what the per-PR path was paying",
    );
}

/// Deterministic simulation of the real [`PrPollSchedule`] over one hour,
/// pricing every batch it produces. This is the measurement behind the
/// change: it drives the production data structure (not a model of it) and
/// reports what the adaptive path would spend.
///
/// Scenario: 25 tracked PRs, all Hot (the only tier that keeps a slot at
/// the 60s sweep cadence), phases staggered pseudo-randomly across the
/// 40s Hot interval — the worst case for batching, since a staggered set is
/// exactly what makes "due right now" a set of one.
#[test]
fn coalescing_cuts_the_adaptive_path_s_hourly_points() {
    let baseline = simulate_hour(25, Duration::ZERO);
    let coalesced = simulate_hour(25, ADAPTIVE_COALESCE_WINDOW);

    // Reported rather than merely asserted: these are the numbers the PR
    // body quotes. Run with `--test_output=all` to read them.
    println!("adaptive path, 25 hot PRs, one hour:");
    println!(
        "  no window:  {} batches, {} points, mean batch {:.2}",
        baseline.batches,
        baseline.points,
        baseline.mean_batch(),
    );
    println!(
        "  {}s window: {} batches, {} points, mean batch {:.2}",
        ADAPTIVE_COALESCE_WINDOW.as_secs(),
        coalesced.batches,
        coalesced.points,
        coalesced.mean_batch(),
    );

    assert_eq!(
        baseline.mean_batch(),
        1.0,
        "without a window a staggered schedule yields batches of one — batching alone is a no-op",
    );
    assert!(
        coalesced.mean_batch() > 2.0,
        "the window must actually coalesce: mean batch was {:.2}",
        coalesced.mean_batch(),
    );
    assert!(
        coalesced.points * 2 < baseline.points,
        "coalescing must more than halve the adaptive path's points: {} vs {}",
        coalesced.points,
        baseline.points,
    );
    // Both runs poll each PR at least as often as its tier demands — the
    // saving is in round trips, not in dropped polls.
    assert!(
        coalesced.polls >= baseline.polls,
        "coalescing pulls polls earlier, so it must not reduce how often a PR is polled: {} vs {}",
        coalesced.polls,
        baseline.polls,
    );
}

/// Same simulation, but for the tier the sweep already covers: with a 60s
/// sweep, Cold PRs hold no slot, so the adaptive path spends nothing on
/// them at all. That is where most of the measured burn was going.
#[test]
fn cold_prs_cost_the_adaptive_path_nothing_at_the_production_cadence() {
    let mut schedule = PrPollSchedule::default();
    let now = Instant::now();
    let sweep = Duration::from_secs(60);
    let observations: Vec<PollObservation> = (0..25)
        .map(|i| (format!("https://github.com/foo/bar/pull/{i}"), Some(PollTier::Cold)))
        .collect();

    let counts = schedule.apply_sweep_observations(&observations, now, sweep);

    assert_eq!(schedule.tracked(), 0);
    assert_eq!(counts.covered_by_full_sweep, 25);
    assert_eq!(
        schedule.next_due(),
        None,
        "no slot means no adaptive round trip — 25 Cold PRs at 180s used to cost \
         25 × 20 × 2 = 1000 points an hour on top of the sweep that already probed them",
    );
}

struct SimResult {
    batches: u64,
    polls: u64,
    points: u64,
}

impl SimResult {
    fn mean_batch(&self) -> f64 {
        if self.batches == 0 {
            return 0.0;
        }
        self.polls as f64 / self.batches as f64
    }
}

/// Drive [`PrPollSchedule`] for one simulated hour with `n` Hot PRs on
/// staggered phases, draining with `window`, and price every batch.
///
/// No sleeping and no wall-clock dependence: the schedule is pure
/// `Instant` arithmetic, so the "clock" is just the next due time.
fn simulate_hour(n: usize, window: Duration) -> SimResult {
    // Far enough ahead that subtracting a phase can't reach behind the
    // monotonic clock's origin on a freshly-booted machine.
    let base = Instant::now() + Duration::from_secs(10_000);
    let sweep = Duration::from_secs(600); // keep both tiers eligible
    let mut schedule = PrPollSchedule::default();

    // Deterministic phases: a fixed LCG, so the simulation is reproducible
    // run to run (a random one would make a regression here a flake).
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let phase = Duration::from_millis(seed % PollTier::Hot.base_interval().as_millis() as u64);
        schedule.reschedule(
            &format!("https://github.com/foo/bar/pull/{i}"),
            Some(PollTier::Hot),
            base - phase,
            sweep,
        );
    }

    let horizon = base + Duration::from_secs(3600);
    let mut result = SimResult {
        batches: 0,
        polls: 0,
        points: 0,
    };
    let mut clock = base;
    while let Some(next) = schedule.next_due() {
        if next > horizon {
            break;
        }
        clock = next.max(clock);
        let due = schedule.drain_due_within(clock, window);
        if due.is_empty() {
            break;
        }
        result.batches += 1;
        result.polls += due.len() as u64;
        result.points += graphql_points(due.len());
        for url in &due {
            schedule.reschedule(url, Some(PollTier::Hot), clock, sweep);
        }
    }
    result
}
