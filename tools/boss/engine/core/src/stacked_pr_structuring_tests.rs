use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::test_support::*;
use crate::work::PendingMergeCheck;

const REPO: &str = "git@github.com:foo/bar.git";

fn branch(product: &str, pr: i64, files: &[&str]) -> InFlightBranch {
    InFlightBranch {
        work_item_id: format!("wi-{pr}"),
        product_id: product.to_owned(),
        pr_url: format!("https://github.com/foo/bar/pull/{pr}"),
        pr_number: pr,
        changed_files: files.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<String>>(),
    }
}

// ---------------------------------------------------------------------------
// Pure planner (`plan_stack_proposals`) — no I/O, no fakes.
// ---------------------------------------------------------------------------

#[test]
fn overlap_produces_one_ordered_proposal_base_is_lower_pr() {
    let branches = vec![
        branch("p1", 20, &["src/a.rs", "src/b.rs"]),
        branch("p1", 10, &["src/b.rs", "src/c.rs"]),
    ];
    let proposals = plan_stack_proposals(&branches);
    assert_eq!(proposals.len(), 1);
    let p = &proposals[0];
    // Lower PR number (10) is the base; the higher (20) is the dependent —
    // regardless of input ordering.
    assert_eq!(p.base_pr_number, 10);
    assert_eq!(p.dependent_pr_number, 20);
    assert_eq!(p.overlapping_files, vec!["src/b.rs".to_owned()]);
}

#[test]
fn disjoint_file_sets_produce_no_proposal() {
    let branches = vec![branch("p1", 1, &["src/a.rs"]), branch("p1", 2, &["src/b.rs"])];
    assert!(plan_stack_proposals(&branches).is_empty());
}

#[test]
fn different_products_are_never_paired() {
    let branches = vec![branch("p1", 1, &["src/shared.rs"]), branch("p2", 2, &["src/shared.rs"])];
    assert!(plan_stack_proposals(&branches).is_empty());
}

#[test]
fn overlapping_files_are_sorted_and_complete() {
    let branches = vec![
        branch("p1", 1, &["z.rs", "a.rs", "m.rs", "only_a.rs"]),
        branch("p1", 2, &["m.rs", "z.rs", "a.rs", "only_b.rs"]),
    ];
    let proposals = plan_stack_proposals(&branches);
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        proposals[0].overlapping_files,
        vec!["a.rs".to_owned(), "m.rs".to_owned(), "z.rs".to_owned()]
    );
}

#[test]
fn three_mutually_overlapping_branches_yield_three_sorted_pairs() {
    let branches = vec![
        branch("p1", 30, &["hot.rs"]),
        branch("p1", 10, &["hot.rs"]),
        branch("p1", 20, &["hot.rs"]),
    ];
    let proposals = plan_stack_proposals(&branches);
    // C(3,2) = 3 pairs, sorted by (base, dependent).
    let pairs: Vec<(i64, i64)> = proposals
        .iter()
        .map(|p| (p.base_pr_number, p.dependent_pr_number))
        .collect();
    assert_eq!(pairs, vec![(10, 20), (10, 30), (20, 30)]);
}

#[test]
fn duplicate_pr_number_is_skipped() {
    // Same PR listed twice must never pair with itself.
    let branches = vec![branch("p1", 5, &["src/a.rs"]), branch("p1", 5, &["src/a.rs"])];
    assert!(plan_stack_proposals(&branches).is_empty());
}

// ---------------------------------------------------------------------------
// `is_stack_worthy_file` — mechanical classes excluded from the signal.
// ---------------------------------------------------------------------------

#[test]
fn mechanical_files_are_not_stack_worthy() {
    assert!(!is_stack_worthy_file("Cargo.lock"));
    assert!(!is_stack_worthy_file("MODULE.bazel.lock"));
    assert!(!is_stack_worthy_file("tools/boss/cli/BUILD.bazel"));
    assert!(!is_stack_worthy_file("some/defs.bzl"));
    assert!(!is_stack_worthy_file("src/mod.rs"));
    assert!(!is_stack_worthy_file("pkg/lib.rs"));
}

#[test]
fn semantic_and_migration_files_are_stack_worthy() {
    assert!(is_stack_worthy_file("tools/boss/engine/core/src/completion.rs"));
    assert!(is_stack_worthy_file("tools/boss/engine/core/src/work/migrations_b.rs"));
}

// ---------------------------------------------------------------------------
// `StackingSchedule` rate limiting — deterministic via injected `Instant`s.
// ---------------------------------------------------------------------------

#[test]
fn pass_is_throttled_to_the_min_interval() {
    let mut sched = StackingSchedule::default();
    let t0 = Instant::now();
    assert!(sched.pass_due(t0), "a fresh schedule is always due");
    sched.mark_pass(t0);
    assert!(!sched.pass_due(t0 + MIN_PASS_INTERVAL / 2), "too soon for another pass");
    assert!(sched.pass_due(t0 + MIN_PASS_INTERVAL), "due again after the interval");
}

#[test]
fn same_pair_is_not_reoffered_within_the_reoffer_interval() {
    let mut sched = StackingSchedule::default();
    let t0 = Instant::now();
    let pair = (10, 20);
    assert!(sched.offer_due(pair, t0));
    sched.mark_offered(pair, t0);
    assert!(!sched.offer_due(pair, t0 + REOFFER_INTERVAL / 2));
    assert!(sched.offer_due(pair, t0 + REOFFER_INTERVAL));
    // A different pair is independent.
    assert!(sched.offer_due((10, 30), t0 + REOFFER_INTERVAL / 2));
}

#[test]
fn prune_drops_stale_offer_timestamps() {
    let mut sched = StackingSchedule::default();
    let t0 = Instant::now();
    sched.mark_offered((10, 20), t0);
    sched.prune(t0 + REOFFER_INTERVAL);
    assert!(
        sched.last_offered.is_empty(),
        "entries older than the interval are dropped"
    );
}

// ---------------------------------------------------------------------------
// Orchestration (`run_stacking_pass`) — WorkDb + fake fetcher + publisher.
// ---------------------------------------------------------------------------

/// Scripted [`PrChangedFilesFetcher`]: returns the configured file list per
/// PR URL, records each call, and can be told to error for specific URLs.
struct ScriptFetcher {
    files: HashMap<String, Vec<String>>,
    errors: HashSet<String>,
    calls: Mutex<Vec<String>>,
}

impl ScriptFetcher {
    fn new(files: HashMap<String, Vec<String>>) -> Self {
        Self {
            files,
            errors: HashSet::new(),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl PrChangedFilesFetcher for ScriptFetcher {
    async fn changed_files(&self, pr_url: &str) -> Result<Vec<String>> {
        self.calls.lock().await.push(pr_url.to_owned());
        if self.errors.contains(pr_url) {
            anyhow::bail!("changed_files boom for {pr_url}");
        }
        Ok(self.files.get(pr_url).cloned().unwrap_or_default())
    }
}

fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
        rusqlite::params![product_id, if enabled { 1 } else { 0 }],
    )
    .unwrap();
}

/// Build an in-review chore with a PR under `product_id`, returning the
/// poller-shaped candidate.
fn in_review_pr(db: &WorkDb, product_id: &str, label: &str, pr_number: i64) -> PendingMergeCheck {
    let chore = create_test_chore_manual(db, product_id.to_owned(), format!("{label}-chore"));
    let pr_url = format!("https://github.com/foo/bar/pull/{pr_number}");
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    PendingMergeCheck {
        work_item_id: chore.id,
        product_id: product_id.to_owned(),
        pr_url,
    }
}

fn registry_with_counters() -> Registry {
    let registry = Registry::new();
    init(&registry);
    registry
}

/// Collect every `StackProposalOffered` event the publisher captured, as
/// `(base_pr, dependent_pr, overlapping_files)`.
async fn offered(publisher: &RecordingPublisher) -> Vec<(i64, i64, Vec<String>)> {
    publisher
        .typed_events
        .lock()
        .await
        .iter()
        .filter_map(|(_, e)| match e {
            FrontendEvent::StackProposalOffered {
                base_pr_number,
                dependent_pr_number,
                overlapping_files,
                ..
            } => Some((*base_pr_number, *dependent_pr_number, overlapping_files.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn predicted_pair_emits_one_offer_with_counters() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let product = create_test_product_with_repo(&db, "StackPair", Some(REPO));
    let c1 = in_review_pr(&db, &product.id, "a", 10);
    let c2 = in_review_pr(&db, &product.id, "b", 20);

    let mut files = HashMap::new();
    files.insert(
        c1.pr_url.clone(),
        vec!["src/completion.rs".to_owned(), "Cargo.lock".to_owned()],
    );
    files.insert(
        c2.pr_url.clone(),
        vec!["src/completion.rs".to_owned(), "src/other.rs".to_owned()],
    );
    let fetcher = ScriptFetcher::new(files);
    let publisher = RecordingPublisher::default();
    let registry = registry_with_counters();
    let mut schedule = StackingSchedule::default();

    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &[c1, c2]).await;

    let events = offered(&publisher).await;
    assert_eq!(events.len(), 1, "exactly one ordered-stack offer");
    assert_eq!(events[0].0, 10, "base is the lower PR number");
    assert_eq!(events[0].1, 20, "dependent is the higher PR number");
    // Cargo.lock overlap is filtered out; only the semantic file drives it.
    assert_eq!(events[0].2, vec!["src/completion.rs".to_owned()]);

    assert_eq!(registry.counter_value("stacked_pr_structuring.offered"), Some(1));
    let per_product = format!(
        "stacked_pr_structuring.{}.offered",
        sanitize_metric_name_component(&product.id)
    );
    assert_eq!(registry.counter_value(&per_product), Some(1));
}

#[tokio::test]
async fn lockfile_only_overlap_is_not_offered() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let product = create_test_product_with_repo(&db, "StackLock", Some(REPO));
    let c1 = in_review_pr(&db, &product.id, "a", 10);
    let c2 = in_review_pr(&db, &product.id, "b", 20);

    let mut files = HashMap::new();
    // Both touch only Cargo.lock — mechanical, must not trigger an offer.
    files.insert(c1.pr_url.clone(), vec!["Cargo.lock".to_owned(), "src/a.rs".to_owned()]);
    files.insert(c2.pr_url.clone(), vec!["Cargo.lock".to_owned(), "src/b.rs".to_owned()]);
    let fetcher = ScriptFetcher::new(files);
    let publisher = RecordingPublisher::default();
    let registry = registry_with_counters();
    let mut schedule = StackingSchedule::default();

    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &[c1, c2]).await;

    assert!(offered(&publisher).await.is_empty());
    assert_eq!(registry.counter_value("stacked_pr_structuring.offered"), Some(0));
}

#[tokio::test]
async fn opted_out_product_never_fetches_or_offers() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "StackOptOut", Some(REPO));
    let c1 = in_review_pr(&db, &product.id, "a", 10);
    let c2 = in_review_pr(&db, &product.id, "b", 20);
    set_product_auto_pr_maintenance(&db_path, &product.id, false);

    let mut files = HashMap::new();
    files.insert(c1.pr_url.clone(), vec!["src/completion.rs".to_owned()]);
    files.insert(c2.pr_url.clone(), vec!["src/completion.rs".to_owned()]);
    let fetcher = ScriptFetcher::new(files);
    let publisher = RecordingPublisher::default();
    let registry = registry_with_counters();
    let mut schedule = StackingSchedule::default();

    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &[c1, c2]).await;

    assert!(offered(&publisher).await.is_empty(), "opted-out product gets no offer");
    assert!(
        fetcher.calls.lock().await.is_empty(),
        "opted-out product must not even fetch changed files"
    );
}

#[tokio::test]
async fn a_fetch_error_skips_that_candidate_without_pairing() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let product = create_test_product_with_repo(&db, "StackErr", Some(REPO));
    let c1 = in_review_pr(&db, &product.id, "a", 10);
    let c2 = in_review_pr(&db, &product.id, "b", 20);

    let mut files = HashMap::new();
    files.insert(c2.pr_url.clone(), vec!["src/completion.rs".to_owned()]);
    let mut fetcher = ScriptFetcher::new(files);
    fetcher.errors.insert(c1.pr_url.clone()); // c1's fetch blows up.
    let publisher = RecordingPublisher::default();
    let registry = registry_with_counters();
    let mut schedule = StackingSchedule::default();

    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &[c1, c2]).await;

    // With only one usable branch there is no pair, so no offer — and the
    // pass did not panic on the error.
    assert!(offered(&publisher).await.is_empty());
}

#[tokio::test]
async fn second_pass_in_quick_succession_is_throttled() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let product = create_test_product_with_repo(&db, "StackThrottle", Some(REPO));
    let c1 = in_review_pr(&db, &product.id, "a", 10);
    let c2 = in_review_pr(&db, &product.id, "b", 20);

    let mut files = HashMap::new();
    files.insert(c1.pr_url.clone(), vec!["src/completion.rs".to_owned()]);
    files.insert(c2.pr_url.clone(), vec!["src/completion.rs".to_owned()]);
    let fetcher = ScriptFetcher::new(files);
    let publisher = RecordingPublisher::default();
    let registry = registry_with_counters();
    let mut schedule = StackingSchedule::default();

    let candidates = [c1, c2];
    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &candidates).await;
    let calls_after_first = fetcher.calls.lock().await.len();
    // Immediate second call is throttled by MIN_PASS_INTERVAL: no new fetches,
    // no new offers.
    run_stacking_pass(&db, &publisher, &fetcher, &registry, &mut schedule, &candidates).await;

    assert_eq!(offered(&publisher).await.len(), 1, "only the first pass offered");
    assert_eq!(
        fetcher.calls.lock().await.len(),
        calls_after_first,
        "throttled pass performs no additional fetches",
    );
}
