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
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("Chore {label}"))
                .autostart(false)
                .build(),
        )
        .unwrap();
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
