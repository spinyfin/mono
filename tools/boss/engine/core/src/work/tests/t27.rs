use super::*;

// Behaviour coverage for the dispatch-failure recovery methods in
// `work/dispatch.rs` that `dispatch_failure_recovery_sweep`, the completion
// handler, and the runner drive but that had no direct test. Each test drives
// the public `WorkDb` API against a temp db and asserts on returned values and
// subsequently-observable state — never on SQL shape or private helpers.

/// Park `work_item_id` through the same public seam the dispatcher uses when a
/// pre-spawn failure exhausts its retries, then backdate `dispatch_failed_at`
/// by `age_secs` so the row reads as having sat through that much cooldown.
///
/// `bounce_dispatch_failed_to_backlog` stamps the wall clock itself and the
/// crate has no injectable clock, so the backdate is the one piece of fixture
/// state these tests plant via SQL. Everything asserted still goes through the
/// public API.
fn park_dispatch_failed(db: &WorkDb, work_item_id: &str, age_secs: i64) {
    assert!(
        db.bounce_dispatch_failed_to_backlog(work_item_id, "lease_failed", "cube: refusing to move backwards")
            .unwrap(),
        "bounce must park a todo work item"
    );
    let stamp = (boss_engine_utils::epoch_time::now_epoch_secs() - age_secs).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_at = ?2 WHERE id = ?1",
            rusqlite::params![work_item_id, stamp],
        )
        .unwrap();
}

/// Re-read a chore through the public getter.
fn chore_row(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(task) => task,
        _ => panic!("expected {id} to be a chore"),
    }
}

fn request_input(work_item_id: &str) -> RequestExecutionInput {
    RequestExecutionInput::builder().work_item_id(work_item_id).build()
}

/// Cooldown the recovery tests sweep with: any item parked longer than this is
/// past its cooldown, anything parked more recently is not.
const MIN_AGE: i64 = 300;

// ── list_dispatch_failed_recovery_candidates ───────────────────────────────

/// The candidate set is exactly the parked-and-cooled rows: an item bounced by
/// `bounce_dispatch_failed_to_backlog` whose cooldown has elapsed is returned,
/// while an ordinary un-parked `todo` sibling in the same product is not.
#[test]
fn list_dispatch_failed_recovery_candidates_returns_parked_items_past_cooldown() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-happy").id;
    let parked = create_test_chore(&db, product.clone(), "parked").id;
    let healthy = create_test_chore(&db, product, "never failed").id;
    park_dispatch_failed(&db, &parked, MIN_AGE * 2);

    let candidates = db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap();

    assert_eq!(candidates, vec![parked]);
    assert!(
        !candidates.contains(&healthy),
        "a work item that never failed dispatch is not a recovery candidate"
    );
}

/// The cooldown is a floor, not a hint: a freshly-parked item is withheld until
/// `min_age_secs` has elapsed, and the same row becomes a candidate once its
/// `dispatch_failed_at` ages past the cutoff.
#[test]
fn list_dispatch_failed_recovery_candidates_excludes_items_still_in_cooldown() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-cooldown").id;
    let chore = create_test_chore(&db, product, "just failed").id;
    park_dispatch_failed(&db, &chore, MIN_AGE / 10);

    assert!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap().is_empty(),
        "an item parked well inside its cooldown must not be swept yet"
    );

    // Same row, same parked state — only the elapsed cooldown differs.
    park_dispatch_failed(&db, &chore, MIN_AGE * 2);
    assert_eq!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap(),
        vec![chore],
        "the item becomes a candidate once its cooldown has elapsed"
    );
}

/// `autostart = 1` means a human already hit retry, so the sweep must keep its
/// hands off — the recovery path only owns rows it parked and nobody has
/// touched since.
#[test]
fn list_dispatch_failed_recovery_candidates_excludes_item_a_human_already_retried() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-human").id;
    let chore = create_test_chore(&db, product, "human retried").id;
    park_dispatch_failed(&db, &chore, MIN_AGE * 2);

    db.update_work_item(
        &chore,
        WorkItemPatch {
            autostart: Some(true),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap().is_empty(),
        "a human re-enabling autostart takes the item out of the sweep's hands"
    );
}

/// A parked row that is later soft-deleted or moved to a terminal status is no
/// longer schedulable, so re-dispatching it would mint an execution nothing can
/// ever run.
#[test]
fn list_dispatch_failed_recovery_candidates_excludes_soft_deleted_and_terminal_items() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-gone").id;
    let deleted = create_test_chore(&db, product.clone(), "deleted").id;
    let finished = create_test_chore(&db, product.clone(), "done").id;
    let live = create_test_chore(&db, product, "still schedulable").id;
    for id in [&deleted, &finished, &live] {
        park_dispatch_failed(&db, id, MIN_AGE * 2);
    }

    // All three are candidates while they are parked and schedulable.
    assert_eq!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap().len(),
        3,
        "precondition: every parked row starts out a candidate"
    );

    db.delete_work_item(&deleted).unwrap();
    db.update_work_item(
        &finished,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap(),
        vec![live],
        "soft-deleted and terminal rows drop out; the schedulable one remains"
    );
}

/// Oldest failure first, so the item that has waited longest gets the next
/// recovery attempt rather than being starved by newer arrivals.
#[test]
fn list_dispatch_failed_recovery_candidates_orders_by_dispatch_failed_at_ascending() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-order").id;
    let newest = create_test_chore(&db, product.clone(), "newest").id;
    let oldest = create_test_chore(&db, product.clone(), "oldest").id;
    let middle = create_test_chore(&db, product, "middle").id;

    // Park out of order so the result can only be sorted, not incidental.
    park_dispatch_failed(&db, &newest, MIN_AGE * 2);
    park_dispatch_failed(&db, &oldest, MIN_AGE * 10);
    park_dispatch_failed(&db, &middle, MIN_AGE * 5);

    assert_eq!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap(),
        vec![oldest, middle, newest],
        "candidates come back oldest-failure-first"
    );
}

/// Rows parked at the very same instant tie-break by id, so the sweep's order
/// is total and stable across passes rather than left to sqlite's whim.
///
/// Ids are minted by a monotonic counter, so the natural insertion (rowid)
/// order of two freshly-created rows always agrees with their id-ascending
/// order — asserting `expected.sort()` against insertion-order ids would pass
/// whether or not the query's `id ASC` tie-break is even present, since
/// sqlite's default rowid scan would already return them in that order. To
/// actually pin the tie-break, the ids are swapped after insertion so the row
/// inserted *first* (lower rowid) ends up with the *larger* id: only the
/// explicit `ORDER BY ... id ASC` can put the lower-id row first here.
#[test]
fn list_dispatch_failed_recovery_candidates_breaks_dispatch_failed_at_ties_by_id() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "dfr-tie").id;
    let first_inserted = create_test_chore(&db, product.clone(), "tie a").id;
    let second_inserted = create_test_chore(&db, product, "tie b").id;
    for id in [&first_inserted, &second_inserted] {
        park_dispatch_failed(&db, id, MIN_AGE * 2);
    }
    assert!(
        first_inserted < second_inserted,
        "test setup: the row inserted first must naturally mint the smaller id"
    );

    // Swap the id columns so the first-inserted row (lower rowid) ends up
    // with the lexicographically larger id, and vice versa, via a temp value.
    let conn = db.connect().unwrap();
    let tmp = format!("{first_inserted}-swap-tmp");
    conn.execute(
        "UPDATE tasks SET id = ?2 WHERE id = ?1",
        rusqlite::params![first_inserted, tmp],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET id = ?2 WHERE id = ?1",
        rusqlite::params![second_inserted, first_inserted],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET id = ?2 WHERE id = ?1",
        rusqlite::params![tmp, second_inserted],
    )
    .unwrap();
    drop(conn);
    let lower_id = first_inserted.clone(); // now holds the row inserted second
    let higher_id = second_inserted.clone(); // now holds the row inserted first

    assert!(
        lower_id < higher_id,
        "test setup: swapped ids must actually differ in sort order"
    );
    assert_eq!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap(),
        vec![lower_id, higher_id],
        "equal dispatch_failed_at stamps order by id, not by insertion order"
    );
}

// ── reenable_and_request_execution_with_live_check ──────────────────────────

/// Happy path: the sweep's retry both un-parks the item (autostart back on, so
/// the card renders as dispatch-pending again instead of sitting silently in
/// Backlog) and mints the fresh `ready` execution that actually re-runs it.
#[test]
fn reenable_and_request_execution_reenables_autostart_and_creates_execution() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "reen-happy").id;
    let chore = create_test_chore_manual(&db, product, "retry me").id;
    park_dispatch_failed(&db, &chore, MIN_AGE * 2);

    let execution = db
        .reenable_and_request_execution_with_live_check(&chore, request_input(&chore), |_| true)
        .unwrap()
        .expect("a parked, eligible item must be re-enabled and re-dispatched");

    assert_eq!(execution.work_item_id, chore);
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(
        executions_for(&db, &chore),
        vec![(execution.id.clone(), "ready".to_owned())],
        "exactly one fresh ready execution is created"
    );

    let after = chore_row(&db, &chore);
    assert!(after.autostart, "autostart is flipped back on");
    assert_eq!(
        after.dispatch_failed_reason, None,
        "the stale failure is cleared so the card stops rendering it"
    );
    assert!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE).unwrap().is_empty(),
        "a recovered item is no longer a candidate for the next sweep pass"
    );
}

/// Raced a human retry: with no `dispatch_failed_reason` the item isn't the
/// sweep's to touch, so the call reports "nothing to do" and changes nothing
/// rather than minting a second execution alongside the human's.
#[test]
fn reenable_and_request_execution_returns_none_when_item_is_not_parked() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "reen-none").id;
    let chore = create_test_chore_manual(&db, product, "not parked").id;

    let outcome = db
        .reenable_and_request_execution_with_live_check(&chore, request_input(&chore), |_| true)
        .unwrap();

    assert!(outcome.is_none(), "an item with no dispatch failure is not eligible");
    assert!(
        executions_for(&db, &chore).is_empty(),
        "an ineligible item must not gain an execution"
    );
    assert!(
        !chore_row(&db, &chore).autostart,
        "autostart is left exactly as the human set it"
    );
}

/// ATOMICITY: both steps share one transaction, so a `request_execution` bail
/// must roll the autostart re-enable back with it.
///
/// The failure driven here is the one the doc comment names — a gating
/// dependency reappeared: the prereq was `done` when the edge was added (so the
/// dependent was never auto-blocked) and then reopened. Were the re-enable
/// committed separately, the item would be left `autostart = 1` with
/// `dispatch_failed_reason` still set and no execution: invisible to
/// `list_dispatch_failed_recovery_candidates` (needs `autostart = 0`) and to
/// `rescan_active_dispatch` (needs `status = 'active'`) — permanently stranded.
#[test]
fn reenable_and_request_execution_rolls_back_autostart_when_request_execution_fails() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "reen-atomic").id;
    let prereq = create_test_chore_manual(&db, product.clone(), "prereq").id;
    let chore = create_test_chore_manual(&db, product, "gated").id;

    // Satisfy the prereq before adding the edge, so the dependent is never
    // auto-blocked and stays an ordinary schedulable row.
    db.update_work_item(
        &prereq,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.add_dependency(AddDependencyInput {
        dependent: chore.clone(),
        prerequisite: prereq.clone(),
        relation: None,
    })
    .unwrap();
    park_dispatch_failed(&db, &chore, MIN_AGE * 2);

    let before = chore_row(&db, &chore);
    assert!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE)
            .unwrap()
            .contains(&chore),
        "precondition: the parked item is a recovery candidate"
    );

    // The prereq reopens: the edge gates the dependent again, so the
    // request_execution step inside the recovery call now bails.
    db.update_work_item(
        &prereq,
        WorkItemPatch {
            status: Some("todo".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let err = db
        .reenable_and_request_execution_with_live_check(&chore, request_input(&chore), |_| true)
        .expect_err("a gated work item must fail the request_execution step");
    assert!(
        err.to_string().contains("gated by"),
        "expected the gating refusal, got: {err}"
    );

    let after = chore_row(&db, &chore);
    assert!(
        !after.autostart,
        "the autostart re-enable must roll back with the failed request_execution"
    );
    assert_eq!(
        after.dispatch_failed_reason, before.dispatch_failed_reason,
        "the parked failure reason survives the rolled-back attempt"
    );
    assert_eq!(
        after.dispatch_failed_at, before.dispatch_failed_at,
        "the parked failure timestamp survives the rolled-back attempt"
    );
    assert_eq!(after.status, before.status, "the item's status is untouched");
    assert!(
        executions_for(&db, &chore).is_empty(),
        "no execution is left behind by the failed attempt"
    );
    assert!(
        db.list_dispatch_failed_recovery_candidates(MIN_AGE)
            .unwrap()
            .contains(&chore),
        "the item is left exactly as it was — still a candidate for the next sweep pass"
    );
}

// ── execution_superseded_in_workspace ──────────────────────────────────────

/// Create a `status` execution for `work_item_id` bound to `workspace_id`.
fn execution_in_workspace(
    db: &WorkDb,
    work_item_id: &str,
    workspace_id: Option<&str>,
    status: ExecutionStatus,
) -> WorkExecution {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(status)
                .maybe_cube_workspace_id(workspace_id.map(str::to_owned))
                .build(),
        )
        .unwrap();
    // `created_at` has whole-second resolution, so same-second siblings would
    // tie-break by id. Stamp a distinct, increasing second per execution so the
    // "newer" relation under test is unambiguous rather than incidental.
    let stamp = (boss_engine_utils::epoch_time::now_epoch_secs() + next_created_at_offset()).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, stamp],
        )
        .unwrap();
    execution
}

/// Strictly-increasing per-call offset for the `created_at` stamps above, so
/// every execution these tests mint is unambiguously newer than the last.
fn next_created_at_offset() -> i64 {
    static OFFSET: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
    OFFSET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// An execution with no workspace can't have been displaced out of one — there
/// is nothing for a successor to reuse.
#[test]
fn execution_superseded_in_workspace_false_without_a_workspace_id() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-nows").id;
    let chore = create_test_chore_manual(&db, product, "no workspace").id;
    let execution = execution_in_workspace(&db, &chore, None, ExecutionStatus::Running);

    assert!(!db.execution_superseded_in_workspace(&execution).unwrap());
}

/// An empty workspace id is treated as absent, not as a workspace literally
/// named `""` that other empty-id rows could supersede one another within.
///
/// `create_execution` normalizes `Some("")` to `NULL`, so the empty id is
/// planted at the row level (and on the passed-in struct, which is what the
/// guard actually reads) to reproduce the shape the guard defends against.
/// Both siblings carry `""`, so an implementation without the emptiness filter
/// would match the newer one and wrongly report the older as superseded — the
/// filter is the only reason this is `false`.
#[test]
fn execution_superseded_in_workspace_false_for_an_empty_workspace_id() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-empty").id;
    let chore = create_test_chore_manual(&db, product, "empty workspace").id;
    let mut stale = execution_in_workspace(&db, &chore, None, ExecutionStatus::Running);
    let newer = execution_in_workspace(&db, &chore, None, ExecutionStatus::Running);
    for id in [&stale.id, &newer.id] {
        set_workspace_id_raw(&db, id, "");
    }
    stale.cube_workspace_id = Some(String::new());

    assert!(
        !db.execution_superseded_in_workspace(&stale).unwrap(),
        "an empty workspace id is absent, not a workspace others can supersede within"
    );
}

/// Force `cube_workspace_id` on an existing row, bypassing
/// `create_execution`'s empty-string-to-NULL normalization.
fn set_workspace_id_raw(db: &WorkDb, execution_id: &str, workspace_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_workspace_id = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, workspace_id],
        )
        .unwrap();
}

/// The newest execution is never its own predecessor, so its own Stop still
/// finalizes it.
#[test]
fn execution_superseded_in_workspace_false_for_the_newest_live_occupant() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-newest").id;
    let chore = create_test_chore_manual(&db, product, "newest").id;
    execution_in_workspace(&db, &chore, Some("ws-newest"), ExecutionStatus::Running);
    let newest = execution_in_workspace(&db, &chore, Some("ws-newest"), ExecutionStatus::Running);

    assert!(
        !db.execution_superseded_in_workspace(&newest).unwrap(),
        "the newest live occupant is not superseded by the older one it replaced"
    );
}

/// The guard's reason for existing: a stale prior occupant of a re-leased
/// workspace, whose leaked Stop hook must not be allowed to finalize the run
/// that now holds the workspace.
#[test]
fn execution_superseded_in_workspace_true_when_a_newer_live_execution_claims_it() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-true").id;
    let chore = create_test_chore_manual(&db, product, "stale occupant").id;
    let stale = execution_in_workspace(&db, &chore, Some("ws-reused"), ExecutionStatus::Running);
    execution_in_workspace(&db, &chore, Some("ws-reused"), ExecutionStatus::WaitingHuman);

    assert!(
        db.execution_superseded_in_workspace(&stale).unwrap(),
        "a newer waiting_human execution in the same workspace supersedes the stale one"
    );
}

/// Only live (`running` / `waiting_human`) successors count: a newer execution
/// that already reached a terminal status has released the workspace, so the
/// older row's Stop is still its own.
#[test]
fn execution_superseded_in_workspace_false_when_the_newer_execution_is_terminal() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-terminal").id;
    let chore = create_test_chore_manual(&db, product, "terminal successor").id;
    let subject = execution_in_workspace(&db, &chore, Some("ws-term"), ExecutionStatus::Running);
    execution_in_workspace(&db, &chore, Some("ws-term"), ExecutionStatus::Completed);

    assert!(
        !db.execution_superseded_in_workspace(&subject).unwrap(),
        "a completed successor does not supersede a still-live execution"
    );
}

/// A live execution in a *different* workspace is irrelevant — the guard is
/// scoped to the workspace being reused.
#[test]
fn execution_superseded_in_workspace_false_when_the_newer_live_execution_is_elsewhere() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "sup-other-ws").id;
    let chore = create_test_chore_manual(&db, product, "other workspace").id;
    let subject = execution_in_workspace(&db, &chore, Some("ws-mine"), ExecutionStatus::Running);
    execution_in_workspace(&db, &chore, Some("ws-theirs"), ExecutionStatus::Running);

    assert!(!db.execution_superseded_in_workspace(&subject).unwrap());
}

// ── get_prior_orphaned_execution ───────────────────────────────────────────

/// Create an execution for `work_item_id` with `status` and an optional
/// `pr_url`, ordered after every previously-created one.
fn prior_execution(db: &WorkDb, work_item_id: &str, status: ExecutionStatus, pr_url: Option<&str>) -> WorkExecution {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(status)
                .maybe_pr_url(pr_url.map(str::to_owned))
                .build(),
        )
        .unwrap();
    let stamp = (boss_engine_utils::epoch_time::now_epoch_secs() + next_created_at_offset()).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, stamp],
        )
        .unwrap();
    execution
}

/// Nothing to resume when this is the item's first execution.
#[test]
fn get_prior_orphaned_execution_none_when_there_are_no_priors() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-first").id;
    let chore = create_test_chore_manual(&db, product, "first run").id;
    let current = prior_execution(&db, &chore, ExecutionStatus::Ready, None);

    assert!(db.get_prior_orphaned_execution(&chore, &current.id).unwrap().is_none());
}

/// Priors that ended cleanly left no mid-flight branch to resume.
#[test]
fn get_prior_orphaned_execution_none_when_all_priors_are_non_orphaned() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-clean").id;
    let chore = create_test_chore_manual(&db, product, "clean priors").id;
    prior_execution(&db, &chore, ExecutionStatus::Completed, None);
    prior_execution(&db, &chore, ExecutionStatus::Failed, None);
    prior_execution(&db, &chore, ExecutionStatus::Abandoned, None);
    let current = prior_execution(&db, &chore, ExecutionStatus::Ready, None);

    assert!(db.get_prior_orphaned_execution(&chore, &current.id).unwrap().is_none());
}

/// An orphan that got as far as opening a PR is the `task.pr_url` resume path's
/// business, not the branch-resume path's.
#[test]
fn get_prior_orphaned_execution_none_when_the_latest_orphan_has_a_pr_url() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-pr").id;
    let chore = create_test_chore_manual(&db, product, "orphan with pr").id;
    prior_execution(
        &db,
        &chore,
        ExecutionStatus::Orphaned,
        Some("https://github.com/spinyfin/mono/pull/7"),
    );
    let current = prior_execution(&db, &chore, ExecutionStatus::Ready, None);

    assert!(db.get_prior_orphaned_execution(&chore, &current.id).unwrap().is_none());
}

/// Discriminates the doc's third `None` bullet from the SQL's actual filter:
/// an OLDER pr-less orphan exists, but the *latest* orphan already has a
/// `pr_url`. The doc says the latest-orphan-has-pr_url case stops the search
/// entirely (that orphan is the `task.pr_url` resume path's business), so the
/// older pr-less orphan must never be offered up as a stale, superseded
/// branch to resume.
#[test]
fn get_prior_orphaned_execution_none_when_a_newer_orphan_has_a_pr_url_even_if_an_older_one_does_not() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-pr-newer").id;
    let chore = create_test_chore_manual(&db, product, "newer orphan has pr").id;
    prior_execution(&db, &chore, ExecutionStatus::Orphaned, None);
    prior_execution(
        &db,
        &chore,
        ExecutionStatus::Orphaned,
        Some("https://github.com/spinyfin/mono/pull/9"),
    );
    let current = prior_execution(&db, &chore, ExecutionStatus::Ready, None);

    assert!(
        db.get_prior_orphaned_execution(&chore, &current.id).unwrap().is_none(),
        "a newer orphan already carrying a pr_url must stop the search, not fall through to an older pr-less orphan"
    );
}

/// The execution being dispatched is excluded, so a currently-orphaned row
/// never matches itself.
#[test]
fn get_prior_orphaned_execution_excludes_the_current_execution() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-self").id;
    let chore = create_test_chore_manual(&db, product, "self").id;
    let current = prior_execution(&db, &chore, ExecutionStatus::Orphaned, None);

    assert!(
        db.get_prior_orphaned_execution(&chore, &current.id).unwrap().is_none(),
        "the current execution must not match itself even when it is orphaned"
    );
}

/// With several resumable orphans, the newest wins — its branch is the one
/// carrying the most work.
#[test]
fn get_prior_orphaned_execution_returns_the_most_recent_match() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-recent").id;
    let chore = create_test_chore_manual(&db, product, "several orphans").id;
    prior_execution(&db, &chore, ExecutionStatus::Orphaned, None);
    prior_execution(&db, &chore, ExecutionStatus::Completed, None);
    let newest_orphan = prior_execution(&db, &chore, ExecutionStatus::Orphaned, None);
    let current = prior_execution(&db, &chore, ExecutionStatus::Ready, None);

    let found = db
        .get_prior_orphaned_execution(&chore, &current.id)
        .unwrap()
        .expect("a pr-less orphaned prior must be found");

    assert_eq!(found.id, newest_orphan.id, "the most recent orphan is returned");
    assert_eq!(found.status, ExecutionStatus::Orphaned);
}

/// Orphans belonging to a different work item are never offered up for resume.
#[test]
fn get_prior_orphaned_execution_is_scoped_to_the_work_item() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "orph-scope").id;
    let mine = create_test_chore_manual(&db, product.clone(), "mine").id;
    let theirs = create_test_chore_manual(&db, product, "theirs").id;
    prior_execution(&db, &theirs, ExecutionStatus::Orphaned, None);
    let current = prior_execution(&db, &mine, ExecutionStatus::Ready, None);

    assert!(db.get_prior_orphaned_execution(&mine, &current.id).unwrap().is_none());
}
