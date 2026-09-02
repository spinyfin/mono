use super::helpers::*;

// ----- Trunk merge-queue eviction detection -----

/// A PR evicted from the Trunk merge queue (combined CI failed on its
/// ephemeral `trunk-merge/*` construction branch) must flip its owning
/// chore to `blocked: ci_failure` and spawn exactly one revision, mirroring
/// the GH-native rebounce path.
#[tokio::test]
async fn trunk_eviction_flips_in_review_to_blocked_ci_failure() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1007";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "36883b5b-bbae-4841-b239-a554d73e6f30",
        "2026-07-23T01:32:50.000Z",
        &one_failure(),
    )
    .await;
    assert!(flipped, "eviction detection must flip chore to ci_failure");

    // In-flight signal stays armed while the revision runs (mirrors the
    // rebounce / on_ci_failure_detected in_review model).
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "in-flight ci_failure signal must stay armed while the revision runs",
    );

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");
    assert_eq!(attempt.failure_kind.as_deref(), Some("trunk_queue_eviction"));
    // Discriminator folds (trunk_entry_id, stateChangedAt) into
    // head_sha_at_trigger — no schema change needed for the dedup key.
    assert_eq!(
        attempt.head_sha_at_trigger,
        "trunk:36883b5b-bbae-4841-b239-a554d73e6f30@2026-07-23T01:32:50.000Z"
    );
    assert!(
        attempt.before_commit_sha.is_none(),
        "trunk eviction has no synthetic merge sha"
    );
    assert!(
        attempt.revision_task_id.is_some(),
        "attempt must have revision_task_id stamped"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 1, "eviction must spawn exactly one revision task");
}

/// Failing checks supplied from the Buildkite evidence recipe must be
/// stored on the `ci_remediations` row exactly like the rebounce path.
#[tokio::test]
async fn trunk_eviction_stores_failing_checks_from_failures_slice() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1008";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-checks", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let failures = vec![RequiredCheckFailure {
        name: "Trunk merge queue: flunge-ci".into(),
        conclusion: "failure".into(),
        target_url: "https://buildkite.com/flunge/flunge-ci/builds/2364#job-uuid".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("job-uuid".into()),
    }];

    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature-branch"),
        "entry-id-1",
        "2026-07-23T01:00:00.000Z",
        &failures,
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");
    let checks: Vec<serde_json::Value> = serde_json::from_str(&attempt.failed_checks).expect("valid JSON");
    assert!(
        !checks.is_empty(),
        "failed_checks must not be empty when failures were supplied"
    );
}

/// A second observation of the same eviction episode (same `trunk_entry_id`
/// + `stateChangedAt`) is idempotent — no duplicate remediation, no
/// duplicate revision.
#[tokio::test]
async fn trunk_eviction_detection_idempotent_on_same_episode() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1009";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let first = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "entry-A",
        "2026-07-23T02:00:00.000Z",
        &one_failure(),
    )
    .await;
    let second = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "entry-A",
        "2026-07-23T02:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(first, "first detection must flip the chore");
    assert!(!second, "second probe for the same episode must be a no-op");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 1, "duplicate probe must not spawn a second revision");
}

/// `id` was measured to be per-PR-stable, not per-episode (buildkite-log-
/// access investigation, 2026-07-22 addendum): a fresh episode on the SAME
/// `trunk_entry_id` but a NEW `stateChangedAt` (e.g. a resubmit that gets
/// evicted again) must be treated as a genuinely new episode, not
/// suppressed by the first one's dedup key.
#[tokio::test]
async fn trunk_eviction_new_state_changed_at_on_same_entry_id_is_a_new_episode() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1010";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-reepisode", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    let first = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-stable",
        "2026-07-23T03:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(first);

    let second = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-stable",
        "2026-07-23T04:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(second, "a new stateChangedAt on the same entry id must bounce again");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let rev_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 2, "a genuinely new episode must spawn its own revision");
}

/// Budget exhaustion routes to `blocked: ci_failure_exhausted` exactly like
/// the rebounce and per-PR-CI paths, sharing the same `ci_attempt_budget`
/// accounting.
#[tokio::test]
async fn trunk_eviction_lands_exhausted_when_budget_is_zero() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1011";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-exhausted", pr);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute("UPDATE products SET ci_attempt_budget = 0 WHERE id = ?1", [&product])
        .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "entry-exhausted",
        "2026-07-23T05:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(flipped);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

/// A manual suppression pinned to the eviction's discriminator (as the
/// human would set after manually moving the chore out of `blocked:
/// ci_failure`) must be honoured, mirroring the rebounce suppression path.
#[tokio::test]
async fn trunk_eviction_honors_manual_suppression() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1012";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-suppressed", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let discriminator = "trunk:entry-suppressed@2026-07-23T06:00:00.000Z";

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_failure_suppressions (work_item_id, head_sha, created_at) VALUES (?1, ?2, '0')",
        rusqlite::params![&chore, discriminator],
    )
    .unwrap();
    drop(conn);

    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        Some("feature"),
        "entry-suppressed",
        "2026-07-23T06:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(!flipped, "manually suppressed episode must not flip the chore");
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

/// The queue-side guards in `on_ci_in_flight_supersedes_failure` and
/// `on_ci_resolved` must treat `trunk_queue_eviction` exactly like
/// `merge_queue_rebounce`: a clean/in-progress head-branch CI probe must
/// NOT clear the block, since the PR's own head-branch CI is green for
/// both — the eviction lives on a different, ephemeral commit.
#[tokio::test]
async fn trunk_eviction_block_is_not_cleared_by_head_branch_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1013";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-noclear", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-noclear",
        "2026-07-23T07:00:00.000Z",
        &one_failure(),
    )
    .await;

    let superseded = on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("pr-head")).await;
    assert!(
        !superseded,
        "InFlight supersede must decline for an active trunk_queue_eviction attempt"
    );

    let resolved = on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await;
    assert!(
        !resolved,
        "on_ci_resolved must decline for an active trunk_queue_eviction attempt"
    );

    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "ci_failure in-flight signal must stay armed",
    );
}

/// Drive one eviction episode to the point where a fix revision exists,
/// returning `(chore, candidate, attempt, revision_task_id)`. Shared by the
/// auto-resubmit trigger tests below so each can vary only the revision's
/// final status.
async fn evicted_with_a_fix_revision(
    db: &WorkDb,
    pub_: &RecordingPublisher,
    slug: &str,
    pr: &str,
    pr_number: i64,
) -> (String, PendingMergeCheck, boss_protocol::CiRemediation, String) {
    let (product, chore) = make_in_review(db, slug, pr);
    let cand = candidate(&product, &chore, pr);

    let intent = db
        .insert_trunk_merge_intent(
            crate::work::TrunkMergeIntentInsertInput::builder()
                .work_item_id(chore.clone())
                .pr_url(pr)
                .pr_number(pr_number)
                .repo("foo/bar")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .unwrap();
    // Mirrors what `trunk_queue_poller::apply_resolved_state` does before
    // handing off to `on_trunk_queue_eviction_detected` in production.
    db.record_trunk_merge_intent_state(&intent.id, "failed").unwrap();

    on_trunk_queue_eviction_detected(
        db,
        pub_,
        &cand,
        Some("feature"),
        &format!("entry-{slug}"),
        "2026-07-23T08:00:00.000Z",
        &one_failure(),
    )
    .await;

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("active attempt row");
    let revision_id = attempt.revision_task_id.clone().expect("revision spawned");
    (chore, cand, attempt, revision_id)
}

fn set_revision_status(db: &WorkDb, revision_id: &str, status: &str) {
    db.update_work_item(
        revision_id,
        crate::work::WorkItemPatch {
            status: Some(status.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
}

/// Once the spawned revision comes to rest at `in_review` AND the head-branch
/// CI probe is clean, `on_ci_resolved` DOES retire a `trunk_queue_eviction`
/// attempt — and tells the Trunk merge intent it's clear to auto-resubmit
/// (design §"Eviction: a first-class failure signal", step 4). This is the
/// one exception to `trunk_eviction_block_is_not_cleared_by_head_branch_ci`.
///
/// `in_review`, not `done`, is the load-bearing status here: it is the ONLY
/// terminal a revision on a still-open PR can reach. See
/// `trunk_eviction_resubmit_does_not_require_the_unreachable_done_status`.
#[tokio::test]
async fn trunk_eviction_block_clears_once_the_revision_comes_to_rest() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());
    let (chore, cand, attempt, revision_id) = evicted_with_a_fix_revision(
        &db,
        pub_.as_ref(),
        "C-trunk-evict-lands",
        "https://github.com/foo/bar/pull/1014",
        1014,
    )
    .await;

    // While the revision is still running, `on_ci_resolved` must decline
    // (same guard as `trunk_eviction_block_is_not_cleared_by_head_branch_ci`)
    // — this is what keeps a green head-branch probe from flip-flopping the
    // episode the moment it is detected.
    set_revision_status(&db, &revision_id, "active");
    assert!(!on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await);

    set_revision_status(&db, &revision_id, "in_review");

    let resolved = on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await;
    assert!(
        resolved,
        "on_ci_resolved must retire once the revision comes to rest at in_review"
    );

    assert!(
        !db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "ci_failure in-flight signal must clear",
    );
    let retired = db.get_ci_remediation(&attempt.id).unwrap().expect("row still exists");
    assert_eq!(retired.status, "succeeded");

    let intent = db.get_active_trunk_merge_intent(&chore).unwrap().expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT),
        "the intent must be marked ready for the poller to auto-resubmit",
    );
}

/// Regression guard for the circular dependency that stranded a real PR out
/// of the Trunk queue for 50 h 23 m.
///
/// The auto-resubmit used to require the fix revision to reach `done`. A
/// revision on an open PR **cannot** reach `done`: every writer of that
/// status (`flip_in_review_revisions_to_done`,
/// `WorkerPrCompletionTarget::Done`) fires only once the parent PR merges or
/// closes — so the resubmit could only fire after the very merge it existed
/// to cause. The old unit test passed solely because it wrote `status =
/// 'done'` on the revision directly, manufacturing a state production could
/// never produce.
///
/// This test therefore asserts the *negative*: the trigger must not depend on
/// `done`. It drives the revision only as far as production can — `in_review`
/// — and requires the whole chain (attempt retired, signal cleared, resubmit
/// sentinel written) to complete from there. If someone reinstates a
/// `done`-only gate, this fails.
#[tokio::test]
async fn trunk_eviction_resubmit_does_not_require_the_unreachable_done_status() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());
    let (chore, cand, _attempt, revision_id) = evicted_with_a_fix_revision(
        &db,
        pub_.as_ref(),
        "C-trunk-evict-open-pr",
        "https://github.com/foo/bar/pull/1015",
        1015,
    )
    .await;

    set_revision_status(&db, &revision_id, "in_review");
    // The parent chore is still `in_review` — its PR is open and unmerged,
    // which is precisely the state in which the old gate was unsatisfiable.
    let parent = db.get_work_item(&chore).unwrap();
    assert!(
        matches!(parent, boss_protocol::WorkItem::Chore(ref t) if t.status == boss_protocol::TaskStatus::InReview),
        "precondition: the parent PR has NOT merged, so the chore is still in_review: {parent:?}",
    );

    assert!(
        on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await,
        "the resubmit trigger must be satisfiable while the parent PR is still open",
    );
    assert_eq!(
        db.get_active_trunk_merge_intent(&chore)
            .unwrap()
            .expect("still active")
            .last_trunk_state
            .as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT),
    );
}

/// A revision that was *withdrawn* (archived as moot/superseded) is not
/// evidence that a fix landed, so it must not trigger a resubmit. The
/// stalled-resubmit backstop in `trunk_queue_poller` is what surfaces this
/// to a human instead.
#[tokio::test]
async fn trunk_eviction_resubmit_declines_for_an_archived_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());
    let (chore, cand, _attempt, revision_id) = evicted_with_a_fix_revision(
        &db,
        pub_.as_ref(),
        "C-trunk-evict-archived",
        "https://github.com/foo/bar/pull/1016",
        1016,
    )
    .await;

    set_revision_status(&db, &revision_id, "archived");

    assert!(
        !on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await,
        "a withdrawn revision must not be read as a landed fix",
    );
    assert_eq!(
        db.get_active_trunk_merge_intent(&chore)
            .unwrap()
            .expect("still active")
            .last_trunk_state
            .as_deref(),
        Some("failed"),
        "the intent must stay parked for the backstop to notice",
    );
}

/// Once the shared CI-attempt budget exhausts on a `trunk_queue_eviction`
/// episode, the Trunk merge intent must retire to `exhausted` (design
/// §"Eviction…", step 4: "the intent is marked exhausted") and the Merging-
/// lane columns must clear so the card doesn't sit showing a stale "queued"
/// badge while the parent is `blocked: ci_failure_exhausted`.
#[tokio::test]
async fn trunk_eviction_budget_exhaustion_retires_the_intent() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1015";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-exhaust", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    db.insert_trunk_merge_intent(
        crate::work::TrunkMergeIntentInsertInput::builder()
            .work_item_id(chore.clone())
            .pr_url(pr)
            .pr_number(1015)
            .repo("foo/bar")
            .target_branch("main")
            .build(),
    )
    .unwrap()
    .unwrap();
    db.set_task_merge_queue_state(&chore, Some("queued"), Some(r#"{"source":"trunk","state":"pending"}"#))
        .unwrap();

    db.set_ci_attempt_budget(&chore, Some(0)).unwrap();

    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-exhaust",
        "2026-07-23T09:00:00.000Z",
        &one_failure(),
    )
    .await;
    assert!(flipped, "budget-exhausted episode still flips the parent");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));

    assert!(
        db.get_active_trunk_merge_intent(&chore).unwrap().is_none(),
        "the intent must retire (no longer active) once the budget is exhausted",
    );
    let (state, _) = {
        let conn = db.connect().unwrap();
        conn.query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            rusqlite::params![&chore],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
    };
    assert!(state.is_none(), "the card must leave the Merging lane");
}

/// The far more common counterpart to `trunk_eviction_budget_exhaustion_retires_the_intent`:
/// when budget is available and the episode spawns a `fix` revision instead
/// of exhausting, the Merging-lane columns must ALSO clear so the card
/// leaves the Merging lane. Before this fix, the clear only existed on the
/// budget-exhausted exit — the ordinary "eviction spawns a fix revision"
/// exit left `merge_queue_state` / `merge_queue_detail` stuck at `"queued"`
/// indefinitely, since a trunk product's regular PR probe deliberately
/// preserves those columns (`preserve_merge_queue_state`) and nothing else
/// ever wrote them. Unlike the exhausted case, the Trunk merge intent
/// itself must stay ACTIVE here: the fix revision is expected to land and
/// resubmit it, not retire it.
#[tokio::test]
async fn trunk_eviction_clears_merge_queue_state_when_spawning_a_fix_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1181";
    let (product, chore) = make_in_review(&db, "C-trunk-evict-clears-queue", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    db.insert_trunk_merge_intent(
        crate::work::TrunkMergeIntentInsertInput::builder()
            .work_item_id(chore.clone())
            .pr_url(pr)
            .pr_number(1181)
            .repo("foo/bar")
            .target_branch("main")
            .build(),
    )
    .unwrap()
    .unwrap();
    db.set_task_merge_queue_state(&chore, Some("queued"), Some(r#"{"source":"trunk","state":"testing"}"#))
        .unwrap();

    let flipped = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-clears-queue",
        "2026-07-31T22:34:24.000Z",
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "eviction detection must flip the chore and spawn a fix revision"
    );

    let (state, detail) = {
        let conn = db.connect().unwrap();
        conn.query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            rusqlite::params![&chore],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
    };
    assert!(
        state.is_none(),
        "the card must leave the Merging lane on the non-exhausted eviction path too"
    );
    assert!(detail.is_none());

    assert!(
        db.get_active_trunk_merge_intent(&chore).unwrap().is_some(),
        "unlike budget exhaustion, the intent stays active — the fix revision is expected to resubmit it",
    );
}

/// A Trunk eviction with no construction-build evidence still mints a
/// revision. The `"Trunk Merge Queue (<branch>)"` check can fail in
/// seconds with an empty summary while `mergeable` stays MERGEABLE —
/// that is a stuck queue rejection, not "nothing happened". The worker
/// prompt's no-evidence STOP section is what keeps this from repeating
/// the empty-commit force-push; refusing the flip left the class
/// invisible.
#[tokio::test]
async fn a_trunk_episode_with_no_failing_checks_still_mints_a_revision() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/1137";
    let (product, chore) = make_in_review(&db, "C-queue-side-no-checks", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    let evicted = on_trunk_queue_eviction_detected(
        &db,
        pub_.as_ref(),
        &cand,
        Some("feature"),
        "entry-no-evidence",
        "2026-07-25T03:54:24.000Z",
        &[],
    )
    .await;
    assert!(
        evicted,
        "a confirmed Trunk eviction must mint even without a construction build"
    );

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent stays in Review while the revision runs"
    );
    assert_eq!(reason, None);
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row is created");
    assert_eq!(attempt.failure_kind.as_deref(), Some("trunk_queue_eviction"));
    assert_eq!(attempt.failed_checks, "[]");
    let revision = db
        .get_work_item(attempt.revision_task_id.as_deref().expect("revision stamped"))
        .unwrap();
    let WorkItem::Task(rev) = revision else {
        panic!("expected revision task");
    };
    assert_eq!(rev.description, "Investigate merge-queue rejection");
}

fn recent_trunk_check() -> crate::merge_poller::TrunkQueueCheckFailure {
    crate::merge_poller::TrunkQueueCheckFailure {
        name: "Trunk Merge Queue (main)".to_owned(),
        conclusion: "FAILURE".to_owned(),
        details_url: "https://app.trunk.io/example/merge-queue/q/1".to_owned(),
        completed_at: Some(
            chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs() - 30, 0)
                .unwrap()
                .to_rfc3339(),
        ),
    }
}

fn head_check_probe(pr: &str, ci: OpenPrCiStatus) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr)
        .state(PrLifecycleState::Open(crate::merge_poller::OpenPrStatus {
            mergeability: crate::merge_poller::OpenPrMergeability::Clean,
            ci,
        }))
        .head_ref_oid("head-check-sha")
        .head_ref_name("feature")
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .trunk_queue_check_failure(recent_trunk_check())
        .build()
}

/// Merge-poller path: a Direct product's failed Trunk check mints a
/// revision from the already-captured probe leaf, without a Trunk intent.
#[tokio::test]
async fn head_check_eviction_mints_for_a_direct_product() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1508";
    let (product, chore) = make_in_review(&db, "C-head-check", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let minted = on_trunk_head_check_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &head_check_probe(pr, OpenPrCiStatus::Clean),
    )
    .await;
    assert!(minted);
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt");
    assert_eq!(attempt.failure_kind.as_deref(), Some("trunk_queue_eviction"));
    assert!(attempt.head_sha_at_trigger.contains("check:head-check-sha"));
}

/// In-flight PR-head checks are waiting, not a stuck queue rejection.
#[tokio::test]
async fn head_check_eviction_skips_when_required_checks_are_in_flight() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1509";
    let (product, chore) = make_in_review(&db, "C-head-check-wait", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let minted = on_trunk_head_check_eviction_detected(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &head_check_probe(pr, OpenPrCiStatus::InFlight),
    )
    .await;
    assert!(!minted);
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}
