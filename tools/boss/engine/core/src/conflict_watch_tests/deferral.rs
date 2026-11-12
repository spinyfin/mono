//! Just-in-time conflict resolution: a conflict on a PR nobody has asked to
//! merge is recorded and held; it is resolved through the ordinary path once
//! a merge is requested or a human asks. See [`crate::conflict_deferral`].

use std::sync::atomic::{AtomicUsize, Ordering};

use boss_engine_merge_poller::{ConflictHandling, ConflictResponse};

use super::helpers::*;
use crate::feature_flags::{
    CONFLICT_RESOLUTION_DEFERRAL_AB_SPLIT_FLAG, CONFLICT_RESOLUTION_DEFERRAL_FLAG, ConflictDeferralArm,
    FeatureFlagsStore,
};
use crate::merge_poller_adapters::CoreSweepHost;
use crate::work::{CONFLICT_STATUS_DEFERRAL_ENDED, CONFLICT_STATUS_DEFERRED, ConflictResolution};

fn conflicting(pr: &str) -> PrLifecycleProbe {
    probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()))
}

/// One detection pass, worker-only (no mechanical ladder), in `arm`.
async fn detect(
    db: &WorkDb,
    publisher: &RecordingPublisher,
    product: &str,
    chore: &str,
    probe: &PrLifecycleProbe,
    arm: ConflictDeferralArm,
) -> bool {
    on_conflict_detected_with(
        db,
        publisher,
        ConflictRemediationMode::Disabled,
        &open_checker(),
        &candidate(product, chore, &probe.url),
        probe,
        arm,
    )
    .await
}

fn ledger(db: &WorkDb, chore: &str) -> Vec<ConflictResolution> {
    let mut rows = db.list_conflict_resolutions(None, &[], Some(chore), None).unwrap();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    rows
}

fn revisions_of(db: &WorkDb, chore: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND kind = 'revision'",
            [chore],
            |row| row.get(0),
        )
        .unwrap()
}

/// What the merge poller writes after every detection pass: the PR's
/// mergeability as GitHub reported it.
fn poller_records_mergeability(db: &WorkDb, chore: &str, state: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_mergeable_state = ?2 WHERE id = ?1",
            rusqlite::params![chore, state],
        )
        .unwrap();
}

fn mergeable_state(db: &WorkDb, chore: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row("SELECT pr_mergeable_state FROM tasks WHERE id = ?1", [chore], |row| {
            row.get(0)
        })
        .unwrap()
}

/// Acceptance: a conflicted PR that is not ready to merge is recorded in the
/// ledger and nothing is done to resolve it — on the first pass or any later
/// one — and it is not reported clean.
#[tokio::test]
async fn a_conflicted_pr_nobody_asked_to_merge_is_recorded_but_not_resolved() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3559";
    let (product, chore) = make_in_review(&db, "C-deferred", pr);
    let publisher = RecordingPublisher::default();
    poller_records_mergeability(&db, &chore, "mergeable");

    let recorded = detect(
        &db,
        &publisher,
        &product,
        &chore,
        &conflicting(pr),
        ConflictDeferralArm::Deferred,
    )
    .await;
    assert!(recorded, "the pass that records the deferral reports a change");
    poller_records_mergeability(&db, &chore, "conflicting");

    // The ledger records the deferral.
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, CONFLICT_STATUS_DEFERRED);
    assert_eq!(rows[0].deferral_arm.as_deref(), Some("deferred"));
    assert_eq!(rows[0].deferral_end_reason, None);
    assert_eq!(rows[0].head_sha_before.as_deref(), Some("head456"));
    assert_eq!(rows[0].finished_at, None);

    // Nothing was done to resolve it.
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none());
    assert_eq!(revisions_of(&db, &chore), 0, "no conflict revision is minted");
    assert_eq!(chore_status(&db, &chore), (TaskStatus::InReview, None));
    assert!(db.active_blocked_signals(&chore).unwrap().is_empty());

    // It is announced once, as a deferral, so the board re-reads the card.
    // No "resolution started" lifecycle push goes out: nothing started.
    assert_eq!(
        publisher.events.lock().await.clone(),
        vec![(product.clone(), chore.clone(), "conflict_deferred".to_owned())]
    );
    assert!(publisher.typed_events.lock().await.is_empty());

    // Later passes over the still-conflicting, still-unrequested PR change nothing.
    for head in ["head456", "head789"] {
        let again = detect(
            &db,
            &publisher,
            &product,
            &chore,
            &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), head),
            ConflictDeferralArm::Deferred,
        )
        .await;
        assert!(!again, "a repeat probe of a deferred conflict is not a change");
    }
    assert_eq!(ledger(&db, &chore).len(), 1);
    assert_eq!(revisions_of(&db, &chore), 0);
    assert_eq!(publisher.events.lock().await.len(), 1);
    // The deferred PR still reads as conflicting; deferral never marks it clean.
    assert_eq!(mergeable_state(&db, &chore).as_deref(), Some("conflicting"));
}

/// Acceptance: the deferred conflict is resolved once a merge is requested,
/// whichever way the engine learns of the request, and through the normal
/// resolution vehicle.
#[tokio::test]
async fn a_deferred_conflict_is_resolved_when_a_merge_is_requested() {
    for signal in ["auto_merge", "merge_queue", "github_intent", "trunk_intent"] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/3559";
        let (product, chore) = make_in_review(&db, &format!("C-ready-{signal}"), pr);
        let publisher = RecordingPublisher::default();

        assert!(
            detect(
                &db,
                &publisher,
                &product,
                &chore,
                &conflicting(pr),
                ConflictDeferralArm::Deferred
            )
            .await
        );
        poller_records_mergeability(&db, &chore, "conflicting");
        assert_eq!(revisions_of(&db, &chore), 0, "{signal}: still deferred");

        // A merge is requested for the conflicted PR.
        let mut ready = conflicting(pr);
        match signal {
            "auto_merge" => ready.auto_merge_enabled = true,
            "merge_queue" => ready.in_merge_queue = true,
            "github_intent" => {
                db.record_github_merge_intent(
                    crate::work::GithubMergeIntentInsertInput::builder()
                        .work_item_id(chore.clone())
                        .pr_url(pr)
                        .head_sha("head456")
                        .build(),
                    "{}",
                )
                .unwrap();
            }
            _ => {
                db.insert_trunk_merge_intent(
                    crate::work::TrunkMergeIntentInsertInput::builder()
                        .work_item_id(chore.clone())
                        .pr_url(pr)
                        .pr_number(3559)
                        .repo("foo/bar")
                        .target_branch("main")
                        .build(),
                )
                .unwrap()
                .unwrap();
            }
        }
        let resolved = detect(&db, &publisher, &product, &chore, &ready, ConflictDeferralArm::Deferred).await;
        assert!(resolved, "{signal}: the ready pass starts the resolution");

        // The ledger shows the deferral, why it ended, and the attempt that followed.
        let rows = ledger(&db, &chore);
        assert_eq!(rows.len(), 2, "{signal}: {rows:?}");
        let (deferral, attempt) = (&rows[0], &rows[1]);
        assert_eq!(deferral.status, CONFLICT_STATUS_DEFERRAL_ENDED, "{signal}");
        assert_eq!(deferral.deferral_end_reason.as_deref(), Some("merge_requested"));
        assert!(deferral.finished_at.is_some());
        assert_eq!(attempt.status, "pending", "{signal}");
        assert_eq!(attempt.deferral_arm.as_deref(), Some("deferred"));
        assert_eq!(attempt.deferral_end_reason, None);

        // Resolution runs on the ordinary vehicle: a merge-conflict revision
        // on the parent's PR, the parent in Review with the conflict signal set.
        let revision = attempt.revision_task_id.as_deref().expect("a conflict revision");
        assert_eq!(
            task(&db, revision).created_via,
            format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{}", attempt.id),
            "{signal}"
        );
        assert_eq!(chore_status(&db, &chore), (TaskStatus::InReview, None));
        assert!(
            db.active_blocked_signals(&chore)
                .unwrap()
                .iter()
                .any(|signal| signal.reason == "merge_conflict")
        );
        assert!(db.open_conflict_deferral(&chore).unwrap().is_none());
    }
}

/// Counts engine-direct rebases, so a test can tell the escalation ladder ran.
#[derive(Default)]
struct CountingRebaseCube {
    rebases: AtomicUsize,
}

crate::stub_cube_client! { CountingRebaseCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "repo-1".to_owned() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease {
            lease_id: "L1".to_owned(),
            workspace_id: "W1".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/deferral-ladder-ws"),
            dirty_verified: None,
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<RebaseOutcome> {
        self.rebases.fetch_add(1, Ordering::SeqCst);
        Ok(RebaseOutcome { clean: true, pushed: true, conflicted_files: Vec::new(), linearized_commits: 0, linearize_decline: None })
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
} }

/// The ready-to-merge path does not get a shortcut: a released conflict
/// enters the same escalation ladder as one that was never deferred, and
/// while it is deferred the ladder is not touched at all.
#[tokio::test]
async fn a_released_conflict_goes_through_the_escalation_ladder() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3559";
    let (product, chore) = make_in_review(&db, "C-ladder", pr);
    let publisher = RecordingPublisher::default();
    let cube = CountingRebaseCube::default();
    let pass = |probe: PrLifecycleProbe| {
        let (db, publisher, cube) = (&db, &publisher, &cube);
        let candidate = candidate(&product, &chore, pr);
        async move {
            on_conflict_detected_with(
                db,
                publisher,
                ConflictRemediationMode::Inline(cube),
                &open_checker(),
                &candidate,
                &probe,
                ConflictDeferralArm::Deferred,
            )
            .await
        }
    };

    assert!(pass(conflicting(pr)).await);
    poller_records_mergeability(&db, &chore, "conflicting");
    assert!(!pass(conflicting(pr)).await);
    assert_eq!(
        cube.rebases.load(Ordering::SeqCst),
        0,
        "a deferred conflict costs no rebase"
    );

    let mut ready = conflicting(pr);
    ready.auto_merge_enabled = true;
    assert!(pass(ready).await);
    assert_eq!(
        cube.rebases.load(Ordering::SeqCst),
        1,
        "the released conflict ran rung 1"
    );
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].deferral_end_reason.as_deref(), Some("merge_requested"));
    assert_eq!(rows[1].status, "succeeded");
    assert_eq!(rows[1].resolved_by_rung, Some(1));
}

/// "Or when a human asks": retrying the deferred row releases it, and the
/// next pass resolves the conflict instead of deferring it again.
#[tokio::test]
async fn a_human_request_releases_a_deferred_conflict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3559";
    let (product, chore) = make_in_review(&db, "C-human", pr);
    let publisher = RecordingPublisher::default();

    assert!(
        detect(
            &db,
            &publisher,
            &product,
            &chore,
            &conflicting(pr),
            ConflictDeferralArm::Deferred
        )
        .await
    );
    poller_records_mergeability(&db, &chore, "conflicting");
    let deferral = db.open_conflict_deferral(&chore).unwrap().unwrap();

    let released = db.retry_conflict_resolution(&deferral.id).unwrap().unwrap();
    assert_eq!(released.deferral_end_reason.as_deref(), Some("human_request"));
    assert_eq!(revisions_of(&db, &chore), 0, "the request alone starts nothing");

    // Still no merge request and still the deferring arm, but the conflict
    // has been asked for, so the next pass resolves it.
    assert!(
        detect(
            &db,
            &publisher,
            &product,
            &chore,
            &conflicting(pr),
            ConflictDeferralArm::Deferred
        )
        .await
    );
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].status, "pending");
    assert!(rows[1].revision_task_id.is_some());
    assert!(db.open_conflict_deferral(&chore).unwrap().is_none());

    // And it is not deferred again while that resolution is in flight.
    assert!(
        !detect(
            &db,
            &publisher,
            &product,
            &chore,
            &conflicting(pr),
            ConflictDeferralArm::Deferred
        )
        .await
    );
    assert_eq!(ledger(&db, &chore).len(), 2);
}

/// Kill switch and experiment arms: with deferral off, or in the control arm,
/// a conflict is resolved on detection, and one deferred earlier is released.
#[tokio::test]
async fn deferral_off_or_the_control_arm_resolves_on_detection() {
    for (arm, label) in [
        (ConflictDeferralArm::Off, None),
        (ConflictDeferralArm::Control, Some("control")),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/3559";
        let (product, chore) = make_in_review(&db, &format!("C-{arm:?}"), pr);
        let publisher = RecordingPublisher::default();

        // Deferred while the PR was in the deferring arm …
        assert!(
            detect(
                &db,
                &publisher,
                &product,
                &chore,
                &conflicting(pr),
                ConflictDeferralArm::Deferred
            )
            .await
        );
        poller_records_mergeability(&db, &chore, "conflicting");

        // … and released on the first pass after it no longer is.
        assert!(detect(&db, &publisher, &product, &chore, &conflicting(pr), arm).await);
        let rows = ledger(&db, &chore);
        assert_eq!(rows.len(), 2, "{arm:?}: {rows:?}");
        assert_eq!(rows[0].status, CONFLICT_STATUS_DEFERRAL_ENDED);
        assert_eq!(rows[0].deferral_end_reason.as_deref(), Some("deferral_disabled"));
        assert_eq!(rows[1].status, "pending");
        assert!(rows[1].revision_task_id.is_some());
        assert_eq!(rows[1].deferral_arm.as_deref(), label, "{arm:?}");
    }

    // A PR that was never deferred behaves exactly as before in both.
    for (arm, label) in [
        (ConflictDeferralArm::Off, None),
        (ConflictDeferralArm::Control, Some("control")),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/3560";
        let (product, chore) = make_in_review(&db, &format!("C-fresh-{arm:?}"), pr);
        let publisher = RecordingPublisher::default();
        assert!(detect(&db, &publisher, &product, &chore, &conflicting(pr), arm).await);
        let rows = ledger(&db, &chore);
        assert_eq!(rows.len(), 1, "{arm:?}: {rows:?}");
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].deferral_arm.as_deref(), label, "{arm:?}");
    }
}

/// A fix that is already running is never pulled back into a deferral, and a
/// row waiting on a human for its conflict keeps the existing handling.
#[tokio::test]
async fn a_conflict_already_being_resolved_is_not_deferred() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3559";
    let (product, chore) = make_in_review(&db, "C-in-flight", pr);
    let publisher = RecordingPublisher::default();

    // Resolution started before deferral was switched on.
    assert!(
        detect(
            &db,
            &publisher,
            &product,
            &chore,
            &conflicting(pr),
            ConflictDeferralArm::Off
        )
        .await
    );
    poller_records_mergeability(&db, &chore, "conflicting");
    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap().unwrap();

    let changed = detect(
        &db,
        &publisher,
        &product,
        &chore,
        &conflicting(pr),
        ConflictDeferralArm::Deferred,
    )
    .await;
    assert!(!changed, "an in-flight revision makes the probe an idempotent no-op");
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, attempt.id);
    assert_eq!(rows[0].status, "pending");
    assert!(db.open_conflict_deferral(&chore).unwrap().is_none());
}

fn flags(dir: &tempfile::TempDir, deferral: bool, ab_split: bool) -> Arc<FeatureFlagsStore> {
    let store = FeatureFlagsStore::new(dir.path().join("feature-flags.toml"));
    store.set(CONFLICT_RESOLUTION_DEFERRAL_FLAG, deferral).unwrap();
    store.set(CONFLICT_RESOLUTION_DEFERRAL_AB_SPLIT_FLAG, ab_split).unwrap();
    Arc::new(store)
}

async fn host_detects(host: &CoreSweepHost, db: &WorkDb, product: &str, chore: &str, pr: &str) -> bool {
    host.on_conflict_detected(
        db,
        &RecordingPublisher::default(),
        ConflictResponse::WORKER_ONLY,
        &open_checker(),
        &candidate(product, chore, pr),
        &conflicting(pr),
    )
    .await
}

/// The production sweep host reads the flags on every detection: the A/B
/// split defers odd-numbered PRs and resolves even-numbered ones, and with
/// no flags (or the flag off) nothing is deferred.
#[tokio::test]
async fn the_sweep_host_assigns_arms_by_pr_number_parity() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let odd = "https://github.com/foo/bar/pull/3559";
    let even = "https://github.com/foo/bar/pull/3560";
    let (odd_product, odd_chore) = make_in_review(&db, "C-odd", odd);
    let (even_product, even_chore) = make_in_review(&db, "C-even", even);

    let split = CoreSweepHost::with_remediation(None).with_feature_flags(flags(&dir, true, true));
    assert!(host_detects(&split, &db, &odd_product, &odd_chore, odd).await);
    assert!(host_detects(&split, &db, &even_product, &even_chore, even).await);

    let odd_rows = ledger(&db, &odd_chore);
    assert_eq!(odd_rows.len(), 1, "{odd_rows:?}");
    assert_eq!(odd_rows[0].status, CONFLICT_STATUS_DEFERRED);
    assert_eq!(odd_rows[0].deferral_arm.as_deref(), Some("deferred"));
    assert_eq!(revisions_of(&db, &odd_chore), 0);

    let even_rows = ledger(&db, &even_chore);
    assert_eq!(even_rows.len(), 1, "{even_rows:?}");
    assert_eq!(even_rows[0].status, "pending");
    assert_eq!(even_rows[0].deferral_arm.as_deref(), Some("control"));
    assert_eq!(revisions_of(&db, &even_chore), 1);

    // Without the split every PR is deferred; without the flag, none is.
    for (deferral, ab_split, expected) in [(true, false, CONFLICT_STATUS_DEFERRED), (false, true, "pending")] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (product, chore) = make_in_review(&db, "C-even-again", even);
        let host = CoreSweepHost::with_remediation(None).with_feature_flags(flags(&dir, deferral, ab_split));
        assert!(host_detects(&host, &db, &product, &chore, even).await);
        let rows = ledger(&db, &chore);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, expected, "deferral={deferral} ab_split={ab_split}");
    }
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (product, chore) = make_in_review(&db, "C-no-flags", odd);
    assert!(host_detects(&CoreSweepHost::with_remediation(None), &db, &product, &chore, odd).await);
    assert_eq!(ledger(&db, &chore)[0].status, "pending");
}

/// A deferred conflict that goes away on its own is closed without a
/// resolution run, and the next conflict on the same PR is a new one.
#[tokio::test]
async fn a_deferred_conflict_that_clears_on_its_own_is_closed_without_a_resolution() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3559";
    let (product, chore) = make_in_review(&db, "C-cleared", pr);
    let host = CoreSweepHost::with_remediation(None).with_feature_flags(flags(&dir, true, true));

    assert!(host_detects(&host, &db, &product, &chore, pr).await);
    poller_records_mergeability(&db, &chore, "conflicting");

    // The poller sees the PR mergeable again (the author rebased it).
    let closed = host.retire_moot_conflict_revisions(&db, &candidate(&product, &chore, pr), &[]);
    assert_eq!(closed, 1, "the cleared deferral is reported so the board refreshes");
    poller_records_mergeability(&db, &chore, "mergeable");
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, CONFLICT_STATUS_DEFERRAL_ENDED);
    assert_eq!(rows[0].deferral_end_reason.as_deref(), Some("conflict_cleared"));
    assert_eq!(revisions_of(&db, &chore), 0, "nothing was ever spent resolving it");
    assert_eq!(
        host.retire_moot_conflict_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0,
        "a clean PR with nothing deferred is a no-op"
    );

    // It conflicts again later: recorded as a new deferral, at the same head.
    assert!(host_detects(&host, &db, &product, &chore, pr).await);
    let rows = ledger(&db, &chore);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].status, CONFLICT_STATUS_DEFERRED);
    assert_eq!(revisions_of(&db, &chore), 0);
}
