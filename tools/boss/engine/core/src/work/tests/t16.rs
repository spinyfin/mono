use super::*;

// Behaviour coverage for the merge/CI blocked-work poller queries in
// `work/blocking.rs` that the merge poller's blocked-work dispatch drives
// but that had no direct test. Each test plants DB rows in specific states
// (via the public flip helpers where they exist, and raw SQL where a test
// needs a shape the guarded public path won't produce) and asserts exactly
// which work items / attempts come back or how a mutation resolves —
// observable behaviour, never SQL internals.

/// Drive a task row straight into a blocking-poller state, bypassing the
/// WHERE-guarded public flips so a test can construct exactly the row shape
/// each query filters on (e.g. `blocked` with a NULL `blocked_reason`, which
/// no public flip produces). `updated_at` is set verbatim so ORDER-BY
/// assertions are deterministic.
fn set_blocking_state(
    db: &WorkDb,
    task_id: &str,
    status: &str,
    blocked_reason: Option<&str>,
    pr_url: Option<&str>,
    updated_at: &str,
) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks
            SET status = ?2, blocked_reason = ?3, pr_url = ?4, updated_at = ?5
          WHERE id = ?1",
        params![task_id, status, blocked_reason, pr_url, updated_at],
    )
    .unwrap();
}

/// Soft-delete a task (stamp `deleted_at`) so the `deleted_at IS NULL`
/// predicate on each poller query can be exercised.
fn soft_delete(db: &WorkDb, task_id: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
        params![task_id, now_string()],
    )
    .unwrap();
}

/// Flip a task's `kind` directly so the `kind IN (chore-like…)` predicate
/// can be exercised with a non-chore-like kind (e.g. `revision`).
fn set_kind(db: &WorkDb, task_id: &str, kind: &str) {
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET kind = ?2 WHERE id = ?1", params![task_id, kind])
        .unwrap();
}

/// Reasons of the currently-active blocked signals for a work item.
fn active_signal_reasons(db: &WorkDb, work_item_id: &str) -> Vec<String> {
    db.active_blocked_signals(work_item_id)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect()
}

/// Insert a pending `conflict_resolutions` row for `work_item_id`, then
/// (optionally) point it at a revision task. `revision` = `Some(id)` sets
/// `revision_task_id` (the "owned by the revision substrate" shape);
/// `None` leaves it NULL.
fn seed_conflict_resolution(db: &WorkDb, product_id: &str, work_item_id: &str, pr_url: &str, revision: Option<&str>) {
    db.insert_conflict_resolution(
        ConflictResolutionInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url(pr_url)
            .pr_number(1)
            .head_branch("feature")
            .base_branch("main")
            .build(),
    )
    .unwrap();
    if let Some(rev) = revision {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE conflict_resolutions SET revision_task_id = ?2 WHERE work_item_id = ?1",
            params![work_item_id, rev],
        )
        .unwrap();
    }
}

/// Insert a pending `ci_remediations` row for `work_item_id` (unique key is
/// `(work_item_id, head_sha, attempt_kind)`, so `head_sha` varies per call
/// site) and return its id. `revision` = `Some(id)` sets `revision_task_id`.
fn seed_ci_remediation(
    db: &WorkDb,
    product_id: &str,
    work_item_id: &str,
    pr_url: &str,
    head_sha: &str,
    revision: Option<&str>,
) -> String {
    let attempt = db
        .insert_ci_remediation(
            CiRemediationInsertInput::builder()
                .product_id(product_id)
                .work_item_id(work_item_id)
                .pr_url(pr_url)
                .pr_number(1)
                .head_branch("feature")
                .head_sha_at_trigger(head_sha)
                .attempt_kind("fix")
                .consumes_budget(1)
                .failed_checks("[]")
                .failure_kind("pr_branch_ci")
                .build(),
        )
        .unwrap()
        .expect("insert must land a pending remediation");
    if let Some(rev) = revision {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
            params![attempt.id, rev],
        )
        .unwrap();
    }
    attempt.id
}

/// Insert a `work_executions` row of `kind='ci_remediation'` with the given
/// status for `work_item_id`, so the "no live execution" guard in
/// `list_stranded_ci_remediation_attempts` can be exercised.
fn insert_ci_remediation_execution(db: &WorkDb, work_item_id: &str, status: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
         VALUES (?1, ?2, 'ci_remediation', ?3, 'git@github.com:spinyfin/mono.git', ?4)",
        params![next_id("exec"), work_item_id, status, now_string()],
    )
    .unwrap();
}

fn pr(n: u32) -> String {
    format!("https://github.com/spinyfin/mono/pull/{n}")
}

/// `list_chores_blocked_on_merge_conflict` returns only chore-like tasks
/// that are `blocked: merge_conflict` with a non-empty `pr_url` and no
/// `deleted_at`, ordered `updated_at ASC`. Every row that fails exactly one
/// predicate is excluded.
#[test]
fn list_chores_blocked_on_merge_conflict_filters_and_orders() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "mc-list");

    // Two genuine rows, planted out of `updated_at` order so the ASC
    // ordering is observable in the result.
    let later = create_test_chore(&db, product_id.clone(), "mc-later").id;
    set_blocking_state(&db, &later, "blocked", Some("merge_conflict"), Some(&pr(1)), "200");
    let earlier = create_test_chore(&db, product_id.clone(), "mc-earlier").id;
    set_blocking_state(&db, &earlier, "blocked", Some("merge_conflict"), Some(&pr(2)), "100");

    // Each of these fails exactly one predicate and must be excluded.
    let wrong_status = create_test_chore(&db, product_id.clone(), "active").id;
    set_blocking_state(
        &db,
        &wrong_status,
        "active",
        Some("merge_conflict"),
        Some(&pr(3)),
        "150",
    );
    let wrong_reason = create_test_chore(&db, product_id.clone(), "ci-reason").id;
    set_blocking_state(&db, &wrong_reason, "blocked", Some("ci_failure"), Some(&pr(4)), "150");
    let empty_pr = create_test_chore(&db, product_id.clone(), "empty-pr").id;
    set_blocking_state(&db, &empty_pr, "blocked", Some("merge_conflict"), Some(""), "150");
    let null_pr = create_test_chore(&db, product_id.clone(), "null-pr").id;
    set_blocking_state(&db, &null_pr, "blocked", Some("merge_conflict"), None, "150");
    let soft_deleted = create_test_chore(&db, product_id.clone(), "deleted").id;
    set_blocking_state(
        &db,
        &soft_deleted,
        "blocked",
        Some("merge_conflict"),
        Some(&pr(5)),
        "150",
    );
    soft_delete(&db, &soft_deleted);
    let wrong_kind = create_test_chore(&db, product_id.clone(), "revision-kind").id;
    set_blocking_state(&db, &wrong_kind, "blocked", Some("merge_conflict"), Some(&pr(6)), "150");
    set_kind(&db, &wrong_kind, "revision");

    let got: Vec<String> = db
        .list_chores_blocked_on_merge_conflict()
        .unwrap()
        .into_iter()
        .map(|c| c.work_item_id)
        .collect();
    assert_eq!(
        got,
        vec![earlier, later],
        "only the two genuine merge_conflict rows, ordered by updated_at ASC",
    );
}

/// `list_chores_blocked_on_ci_failure` returns BOTH `ci_failure` and
/// `ci_failure_exhausted` rows, and excludes unrelated blocked reasons and
/// empty-PR rows.
#[test]
fn list_chores_blocked_on_ci_failure_includes_exhausted_excludes_others() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "ci-list");

    let ci_failure = create_test_chore(&db, product_id.clone(), "ci-failure").id;
    set_blocking_state(&db, &ci_failure, "blocked", Some("ci_failure"), Some(&pr(1)), "100");
    let exhausted = create_test_chore(&db, product_id.clone(), "ci-exhausted").id;
    set_blocking_state(
        &db,
        &exhausted,
        "blocked",
        Some("ci_failure_exhausted"),
        Some(&pr(2)),
        "200",
    );

    // Unrelated blocked reasons and an empty-PR ci_failure are excluded.
    let merge_conflict = create_test_chore(&db, product_id.clone(), "merge-conflict").id;
    set_blocking_state(
        &db,
        &merge_conflict,
        "blocked",
        Some("merge_conflict"),
        Some(&pr(3)),
        "150",
    );
    let dependency = create_test_chore(&db, product_id.clone(), "dependency").id;
    set_blocking_state(&db, &dependency, "blocked", Some("dependency"), Some(&pr(4)), "150");
    let empty_pr = create_test_chore(&db, product_id.clone(), "empty-pr").id;
    set_blocking_state(&db, &empty_pr, "blocked", Some("ci_failure"), Some(""), "150");

    let mut got: Vec<String> = db
        .list_chores_blocked_on_ci_failure()
        .unwrap()
        .into_iter()
        .map(|c| c.work_item_id)
        .collect();
    got.sort();
    let mut want = vec![ci_failure, exhausted];
    want.sort();
    assert_eq!(got, want, "both ci_failure and ci_failure_exhausted, nothing else");
}

/// `list_chores_stranded_blocked_remediation` returns chore-like tasks that
/// are `blocked` with a NULL scalar `blocked_reason`, a bound PR, no active
/// `task_blocked_signals` row, and a remediation attempt (`conflict_resolutions`
/// OR `ci_remediations`) carrying a `revision_task_id`. Rows failing any one
/// predicate are excluded.
#[test]
fn list_chores_stranded_blocked_remediation_filters() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "stranded-list");

    // A: stranded, owned by a conflict-resolution revision → included.
    let via_crz = create_test_chore(&db, product_id.clone(), "via-crz").id;
    set_blocking_state(&db, &via_crz, "blocked", None, Some(&pr(1)), "100");
    seed_conflict_resolution(&db, &product_id, &via_crz, &pr(1), Some("rev_crz"));

    // B: stranded, owned by a ci-remediation revision → included.
    let via_ci = create_test_chore(&db, product_id.clone(), "via-ci").id;
    set_blocking_state(&db, &via_ci, "blocked", None, Some(&pr(2)), "200");
    seed_ci_remediation(&db, &product_id, &via_ci, &pr(2), "sha-b", Some("rev_ci"));

    // C: non-NULL blocked_reason → excluded (owned by another subsystem).
    let has_reason = create_test_chore(&db, product_id.clone(), "has-reason").id;
    set_blocking_state(&db, &has_reason, "blocked", Some("merge_conflict"), Some(&pr(3)), "150");
    seed_conflict_resolution(&db, &product_id, &has_reason, &pr(3), Some("rev_c"));

    // D: an active blocked-signal row → excluded.
    let has_signal = create_test_chore(&db, product_id.clone(), "has-signal").id;
    set_blocking_state(&db, &has_signal, "blocked", None, Some(&pr(4)), "150");
    seed_conflict_resolution(&db, &product_id, &has_signal, &pr(4), Some("rev_d"));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO task_blocked_signals (work_item_id, reason, attempt_id, created_at, cleared_at)
             VALUES (?1, 'merge_conflict', NULL, ?2, NULL)",
            params![has_signal, now_string()],
        )
        .unwrap();
    }

    // E: remediation attempt exists but with a NULL revision_task_id (a pure
    // dependency block, not owned by the revision substrate) → excluded.
    let no_revision = create_test_chore(&db, product_id.clone(), "no-revision").id;
    set_blocking_state(&db, &no_revision, "blocked", None, Some(&pr(5)), "150");
    seed_conflict_resolution(&db, &product_id, &no_revision, &pr(5), None);

    // F: empty PR → excluded.
    let empty_pr = create_test_chore(&db, product_id.clone(), "empty-pr").id;
    set_blocking_state(&db, &empty_pr, "blocked", None, Some(""), "150");
    seed_ci_remediation(&db, &product_id, &empty_pr, "", "sha-f", Some("rev_f"));

    let mut got: Vec<String> = db
        .list_chores_stranded_blocked_remediation()
        .unwrap()
        .into_iter()
        .map(|c| c.work_item_id)
        .collect();
    got.sort();
    let mut want = vec![via_crz, via_ci];
    want.sort();
    assert_eq!(got, want, "only the two revision-owned stranded rows");
}

/// `list_stranded_ci_remediation_attempts` returns pending `ci_remediations`
/// with a NULL `revision_task_id` whose parent is `blocked: ci_failure` and
/// that have NO live (`ready`/`running`/`waiting_human`) `ci_remediation`
/// execution. Rows with a revision, a live execution, a non-blocked / wrong-
/// reason parent, or a non-pending attempt are excluded.
#[test]
fn list_stranded_ci_remediation_attempts_filters() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "stranded-ci");

    // A: genuinely stranded → included.
    let stranded = create_test_chore(&db, product_id.clone(), "stranded").id;
    set_blocking_state(&db, &stranded, "blocked", Some("ci_failure"), Some(&pr(1)), "100");
    let stranded_attempt = seed_ci_remediation(&db, &product_id, &stranded, &pr(1), "sha-a", None);

    // B: attempt owns a revision → excluded.
    let has_revision = create_test_chore(&db, product_id.clone(), "has-revision").id;
    set_blocking_state(&db, &has_revision, "blocked", Some("ci_failure"), Some(&pr(2)), "100");
    seed_ci_remediation(&db, &product_id, &has_revision, &pr(2), "sha-b", Some("rev_b"));

    // C: a live ci_remediation execution exists → excluded.
    let has_live_exec = create_test_chore(&db, product_id.clone(), "has-live-exec").id;
    set_blocking_state(&db, &has_live_exec, "blocked", Some("ci_failure"), Some(&pr(3)), "100");
    seed_ci_remediation(&db, &product_id, &has_live_exec, &pr(3), "sha-c", None);
    insert_ci_remediation_execution(&db, &has_live_exec, "running");

    // D: parent not blocked (in_review) → excluded.
    let not_blocked = create_test_chore(&db, product_id.clone(), "not-blocked").id;
    set_blocking_state(&db, &not_blocked, "in_review", None, Some(&pr(4)), "100");
    seed_ci_remediation(&db, &product_id, &not_blocked, &pr(4), "sha-d", None);

    // E: parent blocked on a different reason → excluded.
    let wrong_reason = create_test_chore(&db, product_id.clone(), "wrong-reason").id;
    set_blocking_state(
        &db,
        &wrong_reason,
        "blocked",
        Some("merge_conflict"),
        Some(&pr(5)),
        "100",
    );
    seed_ci_remediation(&db, &product_id, &wrong_reason, &pr(5), "sha-e", None);

    // F: attempt no longer pending → excluded.
    let terminal_attempt = create_test_chore(&db, product_id.clone(), "terminal-attempt").id;
    set_blocking_state(
        &db,
        &terminal_attempt,
        "blocked",
        Some("ci_failure"),
        Some(&pr(6)),
        "100",
    );
    let terminal_id = seed_ci_remediation(&db, &product_id, &terminal_attempt, &pr(6), "sha-f", None);
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE ci_remediations SET status = 'succeeded' WHERE id = ?1",
            params![terminal_id],
        )
        .unwrap();
    }

    let got: Vec<String> = db
        .list_stranded_ci_remediation_attempts()
        .unwrap()
        .into_iter()
        .map(|a| a.attempt_id)
        .collect();
    assert_eq!(
        got,
        vec![stranded_attempt],
        "only the single genuinely-stranded attempt"
    );
}

/// A `ci_remediation` execution in a terminal status does NOT count as a
/// live executor, so a pending attempt guarded only by such an execution is
/// still stranded. Pins the exact `status IN (…)` set the guard uses.
#[test]
fn list_stranded_ci_remediation_attempts_ignores_terminal_execution() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "stranded-ci-terminal");

    let chore = create_test_chore(&db, product_id.clone(), "terminal-exec").id;
    set_blocking_state(&db, &chore, "blocked", Some("ci_failure"), Some(&pr(1)), "100");
    let attempt = seed_ci_remediation(&db, &product_id, &chore, &pr(1), "sha-1", None);
    // A completed execution must not shield the stranded attempt.
    insert_ci_remediation_execution(&db, &chore, "completed");

    let got: Vec<String> = db
        .list_stranded_ci_remediation_attempts()
        .unwrap()
        .into_iter()
        .map(|a| a.attempt_id)
        .collect();
    assert_eq!(
        got,
        vec![attempt],
        "a terminal ci_remediation execution does not count as a live executor",
    );
}

/// `recanonicalize_blocked_merge_conflict` re-claims a stranded NULL-reason
/// blocked parent as `blocked: merge_conflict`, arming the signal, and
/// returns the updated task. The WHERE guard (`blocked_reason IS NULL` and a
/// matching `pr_url`) makes a repeat call and a wrong-PR call return `None`.
#[test]
fn recanonicalize_blocked_merge_conflict_some_then_none() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "recanon-mc");
    let url = pr(1);

    let chore = create_test_chore(&db, product_id.clone(), "stranded").id;
    set_blocking_state(&db, &chore, "blocked", None, Some(&url), "100");

    // Some: NULL reason → merge_conflict, signal armed.
    let updated = db
        .recanonicalize_blocked_merge_conflict(&chore, &url)
        .unwrap()
        .expect("stranded NULL-reason row is re-canonicalised");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);

    // None: the reason is no longer NULL, so the guard misses.
    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore, &url)
            .unwrap()
            .is_none(),
        "a second call misses the blocked_reason IS NULL guard",
    );
}

/// Wrong-PR miss for the merge-conflict recanonicalisation: a stranded row
/// whose `pr_url` does not match the argument is left untouched (`None`).
#[test]
fn recanonicalize_blocked_merge_conflict_pr_mismatch_is_none() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "recanon-mc-mismatch");

    let chore = create_test_chore(&db, product_id.clone(), "stranded").id;
    set_blocking_state(&db, &chore, "blocked", None, Some(&pr(1)), "100");

    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore, &pr(999))
            .unwrap()
            .is_none(),
        "a non-matching pr_url misses the guard and mutates nothing",
    );
    // The row is untouched: still NULL reason, no signal armed.
    assert_eq!(db.task_blocked_reason(&chore).unwrap(), None);
    assert!(active_signal_reasons(&db, &chore).is_empty());
}

/// `recanonicalize_blocked_ci_failure` is the CI analogue: NULL-reason
/// stranded parent → `blocked: ci_failure` with the signal armed (`Some`),
/// and a repeat call misses the guard (`None`).
#[test]
fn recanonicalize_blocked_ci_failure_some_then_none() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "recanon-ci");
    let url = pr(1);

    let chore = create_test_chore(&db, product_id.clone(), "stranded").id;
    set_blocking_state(&db, &chore, "blocked", None, Some(&url), "100");

    let updated = db
        .recanonicalize_blocked_ci_failure(&chore, &url)
        .unwrap()
        .expect("stranded NULL-reason row is re-canonicalised to ci_failure");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("ci_failure"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);

    assert!(
        db.recanonicalize_blocked_ci_failure(&chore, &url).unwrap().is_none(),
        "a second call misses the blocked_reason IS NULL guard",
    );
}

/// `is_conflict_resolution_revision_live` is true only while the revision
/// task's status is one of `todo`/`active`/`blocked` and it is not
/// soft-deleted; a terminal status, a soft-delete, or an unknown id yield
/// false.
#[test]
fn is_conflict_resolution_revision_live_true_and_false() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "revision-live");
    let parent = make_chore_root(&db, &product_id, "parent");
    let revision = insert_revision_row(&db, &product_id, &parent);

    // Fresh revision rows are `todo` → live.
    assert!(db.is_conflict_resolution_revision_live(&revision).unwrap());

    let conn = db.connect().unwrap();
    // A terminal status (in_review) is not live.
    conn.execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", params![revision])
        .unwrap();
    assert!(!db.is_conflict_resolution_revision_live(&revision).unwrap());

    // A live-status row that is soft-deleted is not live.
    conn.execute(
        "UPDATE tasks SET status = 'active', deleted_at = ?2 WHERE id = ?1",
        params![revision, now_string()],
    )
    .unwrap();
    assert!(!db.is_conflict_resolution_revision_live(&revision).unwrap());

    // Unknown id → false.
    assert!(!db.is_conflict_resolution_revision_live("task_does_not_exist").unwrap());
}

/// `task_blocked_reason` returns the scalar reason only for a live,
/// currently-`blocked` task with a non-NULL reason; every other shape
/// (not blocked, NULL reason, soft-deleted, unknown) is `None`.
#[test]
fn task_blocked_reason_some_and_none() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "blocked-reason");
    let url = pr(1);

    let chore = create_test_chore(&db, product_id.clone(), "chore").id;

    // Not blocked → None.
    assert_eq!(db.task_blocked_reason(&chore).unwrap(), None);

    // Blocked with a reason → Some(reason).
    set_blocking_state(&db, &chore, "blocked", Some("ci_failure"), Some(&url), "100");
    assert_eq!(db.task_blocked_reason(&chore).unwrap().as_deref(), Some("ci_failure"));

    // Blocked but NULL reason → None.
    set_blocking_state(&db, &chore, "blocked", None, Some(&url), "100");
    assert_eq!(db.task_blocked_reason(&chore).unwrap(), None);

    // Blocked with a reason but soft-deleted → None.
    set_blocking_state(&db, &chore, "blocked", Some("merge_conflict"), Some(&url), "100");
    soft_delete(&db, &chore);
    assert_eq!(db.task_blocked_reason(&chore).unwrap(), None);

    // Unknown id → None.
    assert_eq!(db.task_blocked_reason("chr_does_not_exist").unwrap(), None);
}

// ── clear_chore_blocked_merge_conflict_for_attempt ──────────────────────
//
// Coverage for `WorkDb::clear_chore_blocked_merge_conflict_for_attempt` —
// the stricter auto-retire path whose WHERE clause additionally pins
// `blocked_attempt_id = ?attempt_id` (design Q5), so the engine only ever
// undoes *its own* blocked row. Not covered by the poller-query tests
// above, which exercise reads and the recanonicalise/task_blocked_reason
// writes but not this attempt-guarded clear.

/// Stand up a product-with-repo + a chore flipped to
/// `blocked: merge_conflict` against `pr_url`, then insert a pending
/// conflict-resolution attempt for it — which stamps
/// `tasks.blocked_attempt_id` with the attempt id. Returns the chore id
/// and the freshly-inserted attempt. Mirrors the arrangement
/// `conflict_watch::on_conflict_detected` produces in production.
fn seed_blocked_merge_conflict_with_attempt(
    label: &str,
    pr_url: &str,
    pr_number: i64,
    base_sha: &str,
) -> (WorkDb, String, ConflictResolution) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), format!("Chore {label}"));
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.mark_chore_blocked_merge_conflict(&chore.id, pr_url).unwrap();

    let attempt = db
        .insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product.id.clone())
                .work_item_id(chore.id.clone())
                .pr_url(pr_url)
                .pr_number(pr_number)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger(base_sha)
                .head_sha_before("head-before")
                .build(),
        )
        .unwrap()
        .expect("first insert must produce a pending attempt");
    (db, chore.id, attempt)
}

/// (status, blocked_reason, blocked_attempt_id) read straight from the
/// row so the assertions pin the observable parent state independent of
/// the projection layer.
fn parent_state(db: &WorkDb, task_id: &str) -> (String, Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status, blocked_reason, blocked_attempt_id FROM tasks WHERE id = ?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// The `cleared_at` value for the (single) `merge_conflict` signal row of
/// a work item. `None` means the row is still armed (active).
fn merge_conflict_signal_cleared_at(db: &WorkDb, work_item_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT cleared_at FROM task_blocked_signals
              WHERE work_item_id = ?1 AND reason = 'merge_conflict'",
            params![work_item_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// Happy path: when `work_item_id` + `pr_url` + `blocked_attempt_id` all
/// match, the stricter clear retires the parent to `in_review`, NULLs the
/// reason / attempt-id columns, and stamps the matching
/// `task_blocked_signals` row `cleared_at`.
#[test]
fn clear_for_attempt_retires_when_all_three_match() {
    let pr_url = "https://github.com/foo/bar/pull/1";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-match", pr_url, 1, "sha-1");

    // Precondition: blocked on merge_conflict, attempt-id stamped, signal armed.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_none(),
        "signal must be armed before the clear"
    );

    let cleared = db
        .clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, &attempt.id)
        .unwrap()
        .expect("all three keys match — the row must be cleared");
    assert_eq!(cleared.status, TaskStatus::InReview);
    assert_eq!(cleared.blocked_reason, None);

    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "in_review");
    assert_eq!(reason, None);
    assert_eq!(attempt_id, None, "blocked_attempt_id must be NULLed on retire");
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_some(),
        "the matching side-table signal row must be stamped cleared_at on success"
    );

    // Idempotent: a second call finds nothing to clear.
    assert!(
        db.clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, &attempt.id)
            .unwrap()
            .is_none(),
        "repeat clear is a no-op once the row has been retired"
    );
}

/// The load-bearing guard: an `attempt_id` mismatch (the scenario the
/// stricter variant exists to protect against — a human re-flipped the
/// chore under a *different* attempt id) is a no-op even when
/// `work_item_id`, `pr_url`, and reason all match. `Ok(None)`, and every
/// column plus the armed signal is left untouched.
#[test]
fn clear_for_attempt_is_noop_on_attempt_mismatch() {
    let pr_url = "https://github.com/foo/bar/pull/2";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-attempt-miss", pr_url, 2, "sha-2");

    let result = db
        .clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, "crz_someone_elses")
        .unwrap();
    assert!(
        result.is_none(),
        "a mismatched attempt id must not clear the row (design Q5 guard)"
    );

    // Row and signal are entirely untouched.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(
        attempt_id.as_deref(),
        Some(attempt.id.as_str()),
        "the original attempt id must survive a guard miss"
    );
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_none(),
        "the signal must stay armed when the attempt-id guard misses"
    );
}

/// A `pr_url` mismatch is likewise a no-op — the PR was re-pointed under
/// the engine, so the row is not ours to retire even with the right
/// attempt id.
#[test]
fn clear_for_attempt_is_noop_on_pr_url_mismatch() {
    let pr_url = "https://github.com/foo/bar/pull/3";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-pr-miss", pr_url, 3, "sha-3");

    assert!(
        db.clear_chore_blocked_merge_conflict_for_attempt(
            &chore_id,
            "https://github.com/foo/bar/pull/999",
            &attempt.id,
        )
        .unwrap()
        .is_none(),
        "a mismatched pr_url must not clear the row"
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert!(merge_conflict_signal_cleared_at(&db, &chore_id).is_none());
}

// ── CI-budget + blocked-signal helpers ──────────────────────────────────
//
// Coverage for the budget/signal cluster in `work/blocking.rs` that the
// merge poller and the `boss engine ci` verbs drive but that no test
// exercised directly: `effective_ci_budget`, `ci_budget_snapshot`,
// `active_blocked_signals`, and the two `rearm_blocked_*_signal` helpers.
// Each plants DB rows in a specific shape and asserts the return value and
// the resulting `task_blocked_signals` / `tasks` state — observable
// behaviour, never SQL internals.

/// Plant `products.ci_attempt_budget` directly so a test can control the
/// product-level default, including out-of-range values the public setter
/// would clamp. `None` leaves the column NULL (the COALESCE default of 3).
fn set_product_ci_budget(db: &WorkDb, product_id: &str, budget: Option<i64>) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET ci_attempt_budget = ?2 WHERE id = ?1",
        params![product_id, budget],
    )
    .unwrap();
}

/// Plant `tasks.ci_attempt_budget` (the per-PR override) directly, bypassing
/// `set_ci_attempt_budget`'s server-side clamp so the read-path clamp in
/// `effective_ci_budget` / `ci_budget_snapshot` can be exercised with values
/// outside `0..=10`.
fn set_task_ci_budget_raw(db: &WorkDb, task_id: &str, budget: Option<i64>) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET ci_attempt_budget = ?2 WHERE id = ?1",
        params![task_id, budget],
    )
    .unwrap();
}

/// Plant `tasks.ci_attempts_used` directly so a test can assert the snapshot
/// echoes the counter.
fn set_task_ci_used(db: &WorkDb, task_id: &str, used: i64) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET ci_attempts_used = ?2 WHERE id = ?1",
        params![task_id, used],
    )
    .unwrap();
}

/// Insert a `task_blocked_signals` row with a caller-chosen `created_at` and
/// `cleared_at` so ordering and the `cleared_at IS NULL` filter in
/// `active_blocked_signals` can be pinned deterministically. `(work_item_id,
/// reason)` is unique, so each call site varies the reason per work item.
fn insert_signal(db: &WorkDb, work_item_id: &str, reason: &str, created_at: &str, cleared_at: Option<&str>) {
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO task_blocked_signals (work_item_id, reason, attempt_id, created_at, cleared_at)
         VALUES (?1, ?2, NULL, ?3, ?4)",
        params![work_item_id, reason, created_at, cleared_at],
    )
    .unwrap();
}

/// `cleared_at` for the single signal of `reason` on a work item; `None`
/// means the row is still armed (active).
fn signal_cleared_at(db: &WorkDb, work_item_id: &str, reason: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT cleared_at FROM task_blocked_signals
              WHERE work_item_id = ?1 AND reason = ?2",
            params![work_item_id, reason],
            |r| r.get(0),
        )
        .unwrap()
}

/// `effective_ci_budget` prefers the per-PR override over the product default.
#[test]
fn effective_ci_budget_prefers_per_pr_override() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "budget-override");
    set_product_ci_budget(&db, &product_id, Some(5));
    let chore = create_test_chore(&db, product_id.clone(), "override").id;
    set_task_ci_budget_raw(&db, &chore, Some(7));
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        7,
        "the per-PR override wins over the product default",
    );
}

/// With no per-PR override the product default applies.
#[test]
fn effective_ci_budget_falls_back_to_product_default() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "budget-product");
    set_product_ci_budget(&db, &product_id, Some(6));
    let chore = create_test_chore(&db, product_id.clone(), "no-override").id;
    assert_eq!(db.effective_ci_budget(&chore).unwrap(), 6);
}

/// Neither row carries a value → the documented COALESCE default of 3.
#[test]
fn effective_ci_budget_defaults_to_three_when_neither_set() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "budget-default");
    // `make_revision_product` leaves `products.ci_attempt_budget` NULL, and
    // a fresh chore carries no per-PR override.
    let chore = create_test_chore(&db, product_id.clone(), "default").id;
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        3,
        "COALESCE fallback of 3 when neither row carries a value",
    );
}

/// The effective budget is clamped to `[0, 10]` on both the per-PR override
/// and the product-default fallback paths.
#[test]
fn effective_ci_budget_clamps_to_zero_and_ten() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "budget-clamp");

    // A per-PR override above the hard cap clamps to 10.
    let high = create_test_chore(&db, product_id.clone(), "high").id;
    set_task_ci_budget_raw(&db, &high, Some(50));
    assert_eq!(
        db.effective_ci_budget(&high).unwrap(),
        10,
        "override above 10 clamps to the hard cap",
    );

    // A negative override clamps to 0.
    let low = create_test_chore(&db, product_id.clone(), "low").id;
    set_task_ci_budget_raw(&db, &low, Some(-5));
    assert_eq!(
        db.effective_ci_budget(&low).unwrap(),
        0,
        "negative override clamps to 0"
    );

    // A misconfigured *product* default is clamped on the fallback path too.
    set_product_ci_budget(&db, &product_id, Some(99));
    let via_product = create_test_chore(&db, product_id.clone(), "via-product").id;
    assert_eq!(
        db.effective_ci_budget(&via_product).unwrap(),
        10,
        "product default above 10 clamps on the fallback path",
    );
}

/// An unknown task falls through to the documented default of 3.
#[test]
fn effective_ci_budget_returns_three_for_unknown_task() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    assert_eq!(
        db.effective_ci_budget("chr_does_not_exist").unwrap(),
        3,
        "a missing task yields the documented default of 3",
    );
}

/// `active_blocked_signals` returns only `cleared_at IS NULL` rows for the
/// given work item, ordered `created_at ASC` then `reason ASC`. Cleared rows
/// and rows belonging to other work items are excluded.
#[test]
fn active_blocked_signals_filters_cleared_and_orders() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "active-signals");
    let chore = create_test_chore(&db, product_id.clone(), "signals").id;
    let other = create_test_chore(&db, product_id.clone(), "other").id;

    // Two active rows planted out of created_at order; a same-created_at row
    // to exercise the reason ASC tiebreak; a cleared row (excluded); and an
    // active row on a different work item (excluded).
    insert_signal(&db, &chore, "ci_failure", "300", None);
    insert_signal(&db, &chore, "merge_conflict", "100", None);
    insert_signal(&db, &chore, "dependency", "100", None); // same created_at → reason ASC first
    insert_signal(&db, &chore, "review_feedback", "200", Some("250")); // cleared → excluded
    insert_signal(&db, &other, "ci_failure", "050", None); // other work item → excluded

    let got: Vec<(String, Option<String>)> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| (s.reason, s.cleared_at))
        .collect();
    assert_eq!(
        got,
        vec![
            ("dependency".to_owned(), None),
            ("merge_conflict".to_owned(), None),
            ("ci_failure".to_owned(), None),
        ],
        "only active rows for this work item, ordered created_at ASC then reason ASC",
    );
}

/// `ci_budget_snapshot` reports every field and clamps only `effective`; the
/// raw per-PR override is echoed unclamped.
#[test]
fn ci_budget_snapshot_reports_all_fields_and_clamps_effective() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "snapshot-fields");
    set_product_ci_budget(&db, &product_id, Some(4));
    let chore = create_test_chore(&db, product_id.clone(), "snap").id;
    set_task_ci_budget_raw(&db, &chore, Some(50));
    set_task_ci_used(&db, &chore, 2);
    set_blocking_state(&db, &chore, "blocked", Some("ci_failure"), Some(&pr(1)), "100");

    let snap = db
        .ci_budget_snapshot(&chore)
        .unwrap()
        .expect("snapshot for a live task");
    assert_eq!(snap.work_item_id, chore);
    assert_eq!(
        snap.per_pr_override,
        Some(50),
        "the raw stored override is echoed unclamped"
    );
    assert_eq!(snap.product_default, 4);
    assert_eq!(snap.effective, 10, "effective clamps the override to the hard cap");
    assert_eq!(snap.used, 2);
    assert_eq!(snap.blocked_reason.as_deref(), Some("ci_failure"));
}

/// The snapshot's `blocked_reason` is populated only when the task's status
/// is actually `blocked`; a stale reason on a non-blocked row is suppressed.
#[test]
fn ci_budget_snapshot_blocked_reason_only_when_blocked() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "snapshot-reason");
    let chore = create_test_chore(&db, product_id.clone(), "reason").id;

    // A stale blocked_reason on an in_review row must not leak into the snapshot.
    set_blocking_state(&db, &chore, "in_review", Some("ci_failure"), Some(&pr(1)), "100");
    let snap = db.ci_budget_snapshot(&chore).unwrap().expect("snapshot exists");
    assert_eq!(
        snap.blocked_reason, None,
        "blocked_reason is suppressed unless status is blocked",
    );
    // No override, product default NULL → 3; used defaults to 0.
    assert_eq!(snap.per_pr_override, None);
    assert_eq!(snap.product_default, 3);
    assert_eq!(snap.effective, 3);
    assert_eq!(snap.used, 0);

    // Once genuinely blocked, the reason surfaces.
    set_blocking_state(&db, &chore, "blocked", Some("merge_conflict"), Some(&pr(1)), "100");
    let snap = db.ci_budget_snapshot(&chore).unwrap().unwrap();
    assert_eq!(snap.blocked_reason.as_deref(), Some("merge_conflict"));
}

/// The snapshot is `None` for an unknown id and for a soft-deleted task.
#[test]
fn ci_budget_snapshot_none_for_missing_and_deleted() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "snapshot-missing");
    assert!(db.ci_budget_snapshot("chr_does_not_exist").unwrap().is_none());

    let chore = create_test_chore(&db, product_id.clone(), "deleted").id;
    soft_delete(&db, &chore);
    assert!(
        db.ci_budget_snapshot(&chore).unwrap().is_none(),
        "a soft-deleted task yields no snapshot",
    );
}

/// `rearm_blocked_merge_conflict_signal` returns true and arms a fresh
/// `merge_conflict` signal when the parent is `blocked: merge_conflict`.
#[test]
fn rearm_blocked_merge_conflict_signal_true_and_arms() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "rearm-mc");
    let chore = create_test_chore(&db, product_id.clone(), "mc").id;
    // Blocked on merge_conflict with NO signal row (the stale-clear shape).
    set_blocking_state(&db, &chore, "blocked", Some("merge_conflict"), Some(&pr(1)), "100");
    assert!(active_signal_reasons(&db, &chore).is_empty(), "no signal before re-arm");

    assert!(
        db.rearm_blocked_merge_conflict_signal(&chore).unwrap(),
        "task is blocked: merge_conflict → true",
    );
    assert_eq!(
        active_signal_reasons(&db, &chore),
        vec!["merge_conflict"],
        "the signal is armed active",
    );
}

/// A previously-cleared signal row is re-armed (its `cleared_at` reset to
/// NULL) rather than duplicated — the T230 premature-clear recovery.
#[test]
fn rearm_blocked_merge_conflict_signal_rearms_a_cleared_row() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "rearm-mc-cleared");
    let chore = create_test_chore(&db, product_id.clone(), "mc").id;
    set_blocking_state(&db, &chore, "blocked", Some("merge_conflict"), Some(&pr(1)), "100");
    insert_signal(&db, &chore, "merge_conflict", "100", Some("150"));
    assert!(
        signal_cleared_at(&db, &chore, "merge_conflict").is_some(),
        "the row starts cleared",
    );

    assert!(db.rearm_blocked_merge_conflict_signal(&chore).unwrap());
    assert_eq!(
        signal_cleared_at(&db, &chore, "merge_conflict"),
        None,
        "the cleared row is re-armed to active",
    );
}

/// The merge-conflict re-arm returns false and arms nothing when the parent
/// is not blocked, or is blocked on a different reason.
#[test]
fn rearm_blocked_merge_conflict_signal_false_on_wrong_state() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "rearm-mc-false");

    // Not blocked (in_review) → false.
    let not_blocked = create_test_chore(&db, product_id.clone(), "in-review").id;
    set_blocking_state(&db, &not_blocked, "in_review", None, Some(&pr(1)), "100");
    assert!(!db.rearm_blocked_merge_conflict_signal(&not_blocked).unwrap());
    assert!(active_signal_reasons(&db, &not_blocked).is_empty());

    // Blocked on ci_failure (wrong reason) → false.
    let wrong_reason = create_test_chore(&db, product_id.clone(), "ci").id;
    set_blocking_state(&db, &wrong_reason, "blocked", Some("ci_failure"), Some(&pr(2)), "100");
    assert!(!db.rearm_blocked_merge_conflict_signal(&wrong_reason).unwrap());
    assert!(active_signal_reasons(&db, &wrong_reason).is_empty());
}

/// `rearm_blocked_ci_failure_signal` returns true for both `ci_failure` and
/// `ci_failure_exhausted` parents, arming a signal whose reason mirrors the
/// parent's exact CI blocked reason.
#[test]
fn rearm_blocked_ci_failure_signal_true_for_failure_and_exhausted() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "rearm-ci");

    let failing = create_test_chore(&db, product_id.clone(), "failing").id;
    set_blocking_state(&db, &failing, "blocked", Some("ci_failure"), Some(&pr(1)), "100");
    assert!(db.rearm_blocked_ci_failure_signal(&failing).unwrap());
    assert_eq!(active_signal_reasons(&db, &failing), vec!["ci_failure"]);

    let exhausted = create_test_chore(&db, product_id.clone(), "exhausted").id;
    set_blocking_state(
        &db,
        &exhausted,
        "blocked",
        Some("ci_failure_exhausted"),
        Some(&pr(2)),
        "100",
    );
    assert!(db.rearm_blocked_ci_failure_signal(&exhausted).unwrap());
    assert_eq!(
        active_signal_reasons(&db, &exhausted),
        vec!["ci_failure_exhausted"],
        "the armed signal mirrors the parent's exact CI reason",
    );
}

/// The CI re-arm returns false and arms nothing when the parent is not
/// blocked, or is blocked on a non-CI reason.
#[test]
fn rearm_blocked_ci_failure_signal_false_on_wrong_state() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "rearm-ci-false");

    // Not blocked (in_review) → false.
    let not_blocked = create_test_chore(&db, product_id.clone(), "in-review").id;
    set_blocking_state(&db, &not_blocked, "in_review", None, Some(&pr(1)), "100");
    assert!(!db.rearm_blocked_ci_failure_signal(&not_blocked).unwrap());
    assert!(active_signal_reasons(&db, &not_blocked).is_empty());

    // Blocked on merge_conflict (a non-CI reason) → false.
    let wrong_reason = create_test_chore(&db, product_id.clone(), "merge-conflict").id;
    set_blocking_state(
        &db,
        &wrong_reason,
        "blocked",
        Some("merge_conflict"),
        Some(&pr(2)),
        "100",
    );
    assert!(!db.rearm_blocked_ci_failure_signal(&wrong_reason).unwrap());
    assert!(active_signal_reasons(&db, &wrong_reason).is_empty());
}

// ── retarget_blocked_ci_failure_to_merge_conflict ───────────────────────
//
// Coverage for `WorkDb::retarget_blocked_ci_failure_to_merge_conflict` — the
// foreign-bucket takeover (T2381/PR#1861) that re-buckets a chore currently
// `blocked: ci_failure` / `ci_failure_exhausted` into `blocked:
// merge_conflict`, clearing the CI-family signals and arming a
// merge_conflict signal. Unlike its siblings this path overwrites *another*
// watcher's scalar reason, so the tests pin the observable parent state
// (returned Task + `active_blocked_signals`), not raw SQL internals.

/// Attempt id stamped onto `tasks.blocked_attempt_id` by the seed helper so
/// the retarget's NULL-out of that column is observable.
const RETARGET_ATTEMPT_ID: &str = "cir_retarget_seed";

/// Stand up a product-with-repo + a chore driven through the public flips
/// into `blocked: ci_failure` (or `ci_failure_exhausted` when `exhausted`),
/// against `pr_url`, with `blocked_attempt_id` stamped and the CI signal(s)
/// armed. Mirrors the arrangement `ci_watch` produces in production. Returns
/// the db and the chore id.
fn seed_blocked_ci(label: &str, pr_url: &str, exhausted: bool) -> (WorkDb, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), format!("Chore {label}"));
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    // in_review → blocked: ci_failure, stamping blocked_attempt_id and arming
    // the ci_failure signal.
    db.mark_chore_blocked_ci_failure(&chore.id, pr_url, Some(RETARGET_ATTEMPT_ID))
        .unwrap()
        .expect("in_review row flips to blocked: ci_failure");
    if exhausted {
        // ci_failure → ci_failure_exhausted (keeps blocked_attempt_id, arms the
        // exhausted signal alongside the still-active ci_failure one).
        db.mark_chore_blocked_ci_failure_exhausted(&chore.id, pr_url)
            .unwrap()
            .expect("ci_failure row flips to ci_failure_exhausted");
    }
    (db, chore.id)
}

/// Happy path from `ci_failure`: the parent is re-bucketed to `blocked:
/// merge_conflict`, `blocked_attempt_id` is NULLed, `last_status_actor`
/// becomes `engine`, and the returned Task reflects all of it.
#[test]
fn retarget_flips_ci_failure_to_merge_conflict() {
    let pr_url = pr(1);
    let (db, chore_id) = seed_blocked_ci("retarget-cf", &pr_url, false);

    // Precondition: blocked on ci_failure, attempt-id stamped, signal armed.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("ci_failure"));
    assert_eq!(attempt_id.as_deref(), Some(RETARGET_ATTEMPT_ID));
    assert_eq!(active_signal_reasons(&db, &chore_id), vec!["ci_failure"]);

    let updated = db
        .retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
        .unwrap()
        .expect("a blocked: ci_failure row on the matching pr_url must be retargeted");

    // The returned Task reflects the flip.
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(updated.blocked_attempt_id, None, "blocked_attempt_id is cleared");
    assert_eq!(updated.last_status_actor, "engine");

    // …and so does the persisted row and the signal set.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(attempt_id, None);
    assert_eq!(
        active_signal_reasons(&db, &chore_id),
        vec!["merge_conflict"],
        "the ci_failure signal is cleared and a merge_conflict signal is armed",
    );
}

/// Same takeover from the budget-exhausted bucket: a `ci_failure_exhausted`
/// parent is retargeted just like a `ci_failure` one.
#[test]
fn retarget_flips_ci_failure_exhausted_to_merge_conflict() {
    let pr_url = pr(2);
    let (db, chore_id) = seed_blocked_ci("retarget-exhausted", &pr_url, true);

    // Precondition: blocked on ci_failure_exhausted with the exhausted signal
    // armed (the earlier ci_failure signal is still active too).
    let (_, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));
    assert_eq!(attempt_id.as_deref(), Some(RETARGET_ATTEMPT_ID));
    assert!(
        active_signal_reasons(&db, &chore_id).contains(&"ci_failure_exhausted".to_owned()),
        "the exhausted signal must be armed before the retarget",
    );

    let updated = db
        .retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
        .unwrap()
        .expect("a blocked: ci_failure_exhausted row must also be retargeted");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(updated.blocked_attempt_id, None);
    assert_eq!(updated.last_status_actor, "engine");

    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(attempt_id, None);
    assert_eq!(active_signal_reasons(&db, &chore_id), vec!["merge_conflict"]);
}

/// The signal-clearing contract: every active CI-family signal
/// (`ci_failure`, `ci_failure_exhausted`, `ci_flaky_retriggered`) is stamped
/// `cleared_at`, and afterwards the only active signal is `merge_conflict`.
#[test]
fn retarget_clears_all_ci_signals_and_arms_merge_conflict() {
    let pr_url = pr(3);
    let (db, chore_id) = seed_blocked_ci("retarget-signals", &pr_url, true);
    // Plant the third CI-family signal the takeover must also clear.
    insert_signal(&db, &chore_id, "ci_flaky_retriggered", "500", None);

    // Precondition: all three CI-family signals are active.
    let before = active_signal_reasons(&db, &chore_id);
    for reason in ["ci_failure", "ci_failure_exhausted", "ci_flaky_retriggered"] {
        assert!(
            before.contains(&reason.to_owned()),
            "{reason} must be armed before the retarget",
        );
    }

    db.retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
        .unwrap()
        .expect("retarget must land");

    // Every CI-family signal is now cleared; only merge_conflict is active.
    for reason in ["ci_failure", "ci_failure_exhausted", "ci_flaky_retriggered"] {
        assert!(
            signal_cleared_at(&db, &chore_id, reason).is_some(),
            "the {reason} signal must be stamped cleared_at",
        );
    }
    assert_eq!(active_signal_reasons(&db, &chore_id), vec!["merge_conflict"]);
    assert!(
        signal_cleared_at(&db, &chore_id, "merge_conflict").is_none(),
        "the merge_conflict signal must be armed after the retarget",
    );
}

/// A `pr_url` mismatch is a guarded no-op: the PR was re-pointed under the
/// engine, so the row is not ours to retarget. `Ok(None)`, and the parent
/// plus its armed signal are untouched.
#[test]
fn retarget_noop_on_pr_url_mismatch() {
    let pr_url = pr(4);
    let (db, chore_id) = seed_blocked_ci("retarget-pr-miss", &pr_url, false);

    assert!(
        db.retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr(999))
            .unwrap()
            .is_none(),
        "a mismatched pr_url must not retarget the row",
    );
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("ci_failure"));
    assert_eq!(attempt_id.as_deref(), Some(RETARGET_ATTEMPT_ID));
    assert_eq!(active_signal_reasons(&db, &chore_id), vec!["ci_failure"]);
}

/// A parent that is not in `status='blocked'` (e.g. a human moved it back to
/// `in_review`) is a no-op even on the right pr_url and a CI reason.
#[test]
fn retarget_noop_when_not_blocked() {
    let pr_url = pr(5);
    let (db, chore_id) = seed_blocked_ci("retarget-not-blocked", &pr_url, false);
    // Move the parent out of `blocked` while keeping its reason / pr_url.
    set_blocking_state(&db, &chore_id, "in_review", Some("ci_failure"), Some(&pr_url), "100");

    assert!(
        db.retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
            .unwrap()
            .is_none(),
        "a non-blocked parent must not be retargeted",
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "in_review");
    assert_eq!(reason.as_deref(), Some("ci_failure"));
}

/// A parent blocked on a *non-CI* reason (e.g. already `merge_conflict`) is
/// a no-op — the WHERE guard only claims `ci_failure` / `ci_failure_exhausted`
/// rows, so a concurrent clear/human move is respected.
#[test]
fn retarget_noop_on_non_ci_blocked_reason() {
    let pr_url = pr(6);
    let (db, chore_id) = seed_blocked_ci("retarget-non-ci", &pr_url, false);
    set_blocking_state(&db, &chore_id, "blocked", Some("merge_conflict"), Some(&pr_url), "100");

    assert!(
        db.retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
            .unwrap()
            .is_none(),
        "a non-CI blocked reason must not be retargeted",
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}

/// A soft-deleted row (stamped `deleted_at`) is invisible to the takeover —
/// the `deleted_at IS NULL` guard makes it a no-op.
#[test]
fn retarget_noop_on_soft_deleted_row() {
    let pr_url = pr(7);
    let (db, chore_id) = seed_blocked_ci("retarget-deleted", &pr_url, false);
    soft_delete(&db, &chore_id);

    assert!(
        db.retarget_blocked_ci_failure_to_merge_conflict(&chore_id, &pr_url)
            .unwrap()
            .is_none(),
        "a soft-deleted row must not be retargeted",
    );
    // The reason is untouched behind the soft-delete.
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("ci_failure"));
}

// ── record_merge_conflict_in_flight ─────────────────────────────────────
//
// Coverage for `WorkDb::record_merge_conflict_in_flight` — the conflict
// analogue of `record_ci_failure_in_flight` (covered in the CI lifecycle
// tests). It arms the `merge_conflict` signal WITHOUT moving the parent to
// `status='blocked'` (the in_review-with-revision model), stamping the
// attempt id, and is idempotent: a repeat call re-arms the same row rather
// than inserting a duplicate.

/// `record_merge_conflict_in_flight` upserts an active `merge_conflict`
/// signal (carrying the attempt id) while the parent stays `in_review`, and
/// a repeat call after a signal-only clear re-arms the same row rather than
/// duplicating it.
#[test]
fn record_merge_conflict_in_flight_arms_signal_without_blocking() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "record-mc-inflight");
    let pr_url = pr(1);
    let chore = make_in_review_chore(&db, &product_id, &pr_url);

    db.record_merge_conflict_in_flight(&chore, "crz_in_flight_1").unwrap();

    // The parent is NOT flipped to blocked — only the side-table signal arms.
    let (status, reason, _) = parent_state(&db, &chore);
    assert_eq!(
        status, "in_review",
        "record_*_in_flight must leave the parent in_review, not flip it to blocked",
    );
    assert_eq!(reason, None);

    let signals = db.active_blocked_signals(&chore).unwrap();
    assert_eq!(signals.len(), 1, "exactly one active signal after the in-flight record");
    assert_eq!(signals[0].reason, "merge_conflict");
    assert_eq!(
        signals[0].attempt_id.as_deref(),
        Some("crz_in_flight_1"),
        "the in-flight attempt id must be stamped on the armed signal",
    );

    // A signal-only clear deactivates it (the premature polymorphic-clear shape)...
    assert!(db.clear_merge_conflict_signal_only(&chore).unwrap());
    assert!(active_signal_reasons(&db, &chore).is_empty());

    // ...and a repeat record re-arms the SAME row (cleared_at back to NULL),
    // not a duplicate — the ON CONFLICT(work_item_id, reason) upsert.
    db.record_merge_conflict_in_flight(&chore, "crz_in_flight_2").unwrap();
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);
    assert!(
        signal_cleared_at(&db, &chore, "merge_conflict").is_none(),
        "the re-record must re-arm the existing signal row",
    );
    let row_count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_blocked_signals
              WHERE work_item_id = ?1 AND reason = 'merge_conflict'",
            params![chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        row_count, 1,
        "record_*_in_flight must re-arm rather than insert a duplicate signal row",
    );
}

// ── mark_chore_blocked_ci_failure_exhausted ─────────────────────────────
//
// Coverage for `WorkDb::mark_chore_blocked_ci_failure_exhausted` — the
// budget-exhausted exit. Other tests use it only as a setup step; these pin
// its own transitions: `in_review` → `ci_failure_exhausted` (first failure
// with the budget already spent), `ci_failure` → `ci_failure_exhausted`, and
// the WHERE-guard misses (already exhausted / wrong pr_url).

/// From `in_review` the exhausted flip blocks the parent as
/// `ci_failure_exhausted`, arms the matching signal, and returns the updated
/// task; a second call is a guarded no-op (the row is no longer `in_review`
/// nor `ci_failure`).
#[test]
fn mark_ci_failure_exhausted_from_in_review_then_idempotent() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "exhausted-in-review");
    let pr_url = pr(1);
    let chore = make_in_review_chore(&db, &product_id, &pr_url);

    let updated = db
        .mark_chore_blocked_ci_failure_exhausted(&chore, &pr_url)
        .unwrap()
        .expect("in_review row flips straight to blocked: ci_failure_exhausted");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("ci_failure_exhausted"));
    assert_eq!(updated.last_status_actor, "engine");
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure_exhausted"]);

    // Idempotent: already exhausted → the WHERE guard misses (blocked, but the
    // reason is no longer ci_failure), so Ok(None) and nothing changes.
    assert!(
        db.mark_chore_blocked_ci_failure_exhausted(&chore, &pr_url)
            .unwrap()
            .is_none(),
        "a second exhausted flip on an already-exhausted row is a no-op",
    );
    let (status, reason, _) = parent_state(&db, &chore);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));
}

/// From an active `blocked: ci_failure` the exhausted flip re-buckets the
/// scalar reason to `ci_failure_exhausted` and arms the exhausted signal
/// alongside the still-active `ci_failure` one (the multi-signal projection).
#[test]
fn mark_ci_failure_exhausted_from_ci_failure_arms_alongside() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "exhausted-from-ci");
    let pr_url = pr(1);
    let chore = make_in_review_chore(&db, &product_id, &pr_url);

    // in_review → blocked: ci_failure (arms the ci_failure signal).
    db.mark_chore_blocked_ci_failure(&chore, &pr_url, None)
        .unwrap()
        .expect("flip to blocked: ci_failure");
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);

    // ci_failure → exhausted: the scalar reason is re-bucketed and the
    // exhausted signal is armed while the earlier ci_failure signal stays active.
    let updated = db
        .mark_chore_blocked_ci_failure_exhausted(&chore, &pr_url)
        .unwrap()
        .expect("ci_failure row flips to ci_failure_exhausted");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("ci_failure_exhausted"));

    let mut reasons = active_signal_reasons(&db, &chore);
    reasons.sort();
    assert_eq!(
        reasons,
        vec!["ci_failure".to_owned(), "ci_failure_exhausted".to_owned()],
        "the exhausted signal is armed alongside the still-active ci_failure signal",
    );
}

/// A `pr_url` mismatch is a guarded no-op even from a genuine `in_review`
/// row — the PR was re-pointed under the engine, so the row is not ours to
/// exhaust. `Ok(None)` and the parent (plus its empty signal set) is untouched.
#[test]
fn mark_ci_failure_exhausted_noop_on_pr_url_mismatch() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "exhausted-pr-miss");
    let pr_url = pr(1);
    let chore = make_in_review_chore(&db, &product_id, &pr_url);

    assert!(
        db.mark_chore_blocked_ci_failure_exhausted(&chore, &pr(999))
            .unwrap()
            .is_none(),
        "a mismatched pr_url must not exhaust the row",
    );
    let (status, reason, _) = parent_state(&db, &chore);
    assert_eq!(status, "in_review");
    assert_eq!(reason, None);
    assert!(active_signal_reasons(&db, &chore).is_empty());
}
