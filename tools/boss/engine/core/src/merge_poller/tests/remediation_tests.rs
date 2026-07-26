//! The property this file exists to protect: **a pass whose conflict
//! remediation blocks must not delay detection of an unrelated candidate.**
//!
//! `e64dda13b` (#1968) hung the escalation ladder off `sweep_one` with an
//! inline `.await`. Because `run_one_pass` is awaited one candidate at a
//! time on a single task, one PR's cube-lease-plus-rebase stalled the whole
//! pass: in the observed incident, `boss_engine::merge_poller` emitted zero
//! trace lines for 32 minutes while seven ladder runs executed back to back,
//! and PR #2346 — which merged ten seconds after that pass took its probe
//! snapshot — was not moved to `done` for another 33m36s.
//!
//! So the tests below put a conflicting PR and an unrelated *merged* PR in
//! the same pass, hand remediation a job that parks indefinitely, and assert
//! the merged PR is still detected and retired while that remediation is
//! outstanding.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::conflict_remediation::{ConflictRemediationJob, ConflictRemediationQueue};
use crate::work::ConflictResolution;

/// Remediation job that records what it was handed and then parks until the
/// test releases it — the stand-in for the ladder's minutes of cube lease,
/// rebase, and push.
struct ParkingJob {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    finished: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl ConflictRemediationJob for ParkingJob {
    async fn run(&self, candidate: PendingMergeCheck, _attempt: ConflictResolution, _probe: PrLifecycleProbe) {
        self.seen.lock().unwrap().push(candidate.pr_url.clone());
        if let Ok(permit) = self.release.acquire().await {
            permit.forget();
        }
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
}

struct ParkedRemediation {
    queue: ConflictRemediationQueue,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    finished: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

fn parked_remediation() -> ParkedRemediation {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let job = Arc::new(ParkingJob {
        seen: seen.clone(),
        finished: finished.clone(),
        release: release.clone(),
    });
    ParkedRemediation {
        queue: ConflictRemediationQueue::new(job),
        seen,
        finished,
        release,
    }
}

/// A completion handler with `conflict_ladder_mechanical_rebase` ON — the
/// only configuration in which the ladder (and therefore this whole code
/// path) is reachable at all.
fn handler_with_ladder_enabled(
    db: Arc<WorkDb>,
    publisher: Arc<RecordingPublisher>,
    flags_dir: &tempfile::TempDir,
) -> WorkerCompletionHandler {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.set("conflict_ladder_mechanical_rebase", true).unwrap();
    assert!(
        flags.is_enabled("conflict_ladder_mechanical_rebase"),
        "setup: the ladder flag must be on or the deferral path is unreachable",
    );
    WorkerCompletionHandler::new(
        db,
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        publisher,
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
    )
    .with_feature_flags(flags)
}

async fn wait_until(mut pred: impl FnMut() -> bool) -> bool {
    for _ in 0..2000 {
        if pred() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    pred()
}

#[tokio::test]
async fn blocked_remediation_does_not_delay_detection_of_an_unrelated_pr() {
    let db = Arc::new(WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap());
    let publisher = Arc::new(RecordingPublisher::default());
    let probe = StubProbe::new();
    let flags_dir = tempdir().unwrap();

    let conflicting_pr = "https://github.com/foo/bar/pull/2309";
    let merged_pr = "https://github.com/foo/bar/pull/2346";
    let (product, conflicted_chore) = make_chore_in_review(&db, "conflicted", conflicting_pr);
    let merged_chore = chore_in_review_for_product(&db, &product, "merged-meanwhile", merged_pr);

    probe.set_with_base_head(
        conflicting_pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );
    probe.set(merged_pr, PrLifecycleState::Merged);

    let handler = handler_with_ladder_enabled(db.clone(), publisher.clone(), &flags_dir);
    let remediation = parked_remediation();

    // The whole point: this must come back promptly even though the
    // remediation it just handed off will never finish.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            Some(&NoopCubeClient as &dyn CubeClient),
            Some(&handler),
            Some(&remediation.queue),
        ),
    )
    .await
    .expect("run_one_pass must not block on conflict remediation");

    assert_eq!(
        outcome.conflict_flagged, 1,
        "the conflict must still be observed and recorded by the detection pass, got: {outcome:?}",
    );
    assert_eq!(
        outcome.merged, 1,
        "the unrelated merged PR must be detected in the SAME pass as the blocked remediation, got: {outcome:?}",
    );
    match db.get_work_item(&merged_chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::Done,
            "the merged PR's work item must reach done without waiting for the conflicting PR's remediation",
        ),
        other => panic!("expected chore, got {other:?}"),
    }

    // …and remediation really did get handed off, still outstanding.
    assert!(
        wait_until(|| !remediation.seen.lock().unwrap().is_empty()).await,
        "the ladder must run in the background, not be dropped",
    );
    assert_eq!(
        remediation.seen.lock().unwrap().as_slice(),
        [conflicting_pr.to_owned()],
        "exactly the conflicting PR was handed to the remediator",
    );
    assert_eq!(
        remediation.finished.load(Ordering::SeqCst),
        0,
        "precondition of this test: remediation is still blocked while the pass has already returned",
    );

    // A `conflict_resolutions` attempt is the durable record that the
    // conflict was observed; it stays `pending` with no revision until the
    // background ladder decides what to do, so a lost enqueue is retried
    // rather than lost.
    let attempt = db
        .latest_conflict_resolution_for_work_item(&conflicted_chore)
        .unwrap()
        .expect("conflict detection must record an attempt row even though remediation is deferred");
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.revision_task_id, None);

    remediation.release.add_permits(4);
}

#[tokio::test]
async fn repeat_passes_do_not_stack_ladder_runs_on_the_same_pr() {
    let db = Arc::new(WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap());
    let publisher = Arc::new(RecordingPublisher::default());
    let probe = StubProbe::new();
    let flags_dir = tempdir().unwrap();

    let pr = "https://github.com/foo/bar/pull/2309";
    make_chore_in_review(&db, "conflicted", pr);
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );

    let handler = handler_with_ladder_enabled(db.clone(), publisher.clone(), &flags_dir);
    let remediation = parked_remediation();

    for _ in 0..3 {
        run_one_pass(
            db.as_ref(),
            probe.as_ref(),
            publisher.as_ref(),
            Some(&NoopCubeClient as &dyn CubeClient),
            Some(&handler),
            Some(&remediation.queue),
        )
        .await;
        // Let any spawned task actually get going before the next pass, so
        // "no second run" can't pass merely because nothing had started.
        assert!(wait_until(|| !remediation.seen.lock().unwrap().is_empty()).await);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        remediation.seen.lock().unwrap().len(),
        1,
        "a PR with a ladder run already in flight must not get another one on every sweep",
    );

    remediation.release.add_permits(4);
}
