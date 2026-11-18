use super::*;

// Behaviour coverage for the `WorkDb` dispatch / merge-queue / merge-order /
// blocked-signal methods that each had exactly one production call site and no
// direct test. Every test drives a real in-memory `WorkDb` through the public
// API and asserts on observable outcomes — the returned value, the resulting
// row state re-read through another public method — never on SQL text or
// internal implementation details.
//
// The recurring shape each method shares is a WHERE-guarded write: the happy
// path transitions and reports `true`/`Some`, and a guard miss (wrong status,
// unknown id, already-applied) is a silent no-op reporting `false`/`None`.
// Both halves are covered for each method.

// ── downgrade_ready_to_waiting_dependency (work/dispatch.rs) ────────────────

/// Happy path: a `ready` execution the dispatcher finds still gated by an
/// unmet prereq is moved back to `waiting_dependency` and reports `true`.
#[test]
fn downgrade_ready_to_waiting_dependency_moves_a_ready_execution_back() {
    let db = WorkDb::open(temp_db_path("downgrade-happy")).unwrap();
    let product = create_test_product_named(&db, "Boss-downgrade-happy");
    let chore = create_test_chore(&db, product.id.clone(), "downgrade-happy");
    let exec = create_ready_chore_execution(&db, chore.id.clone());

    assert!(
        db.downgrade_ready_to_waiting_dependency(&exec.id).unwrap(),
        "a ready execution must be downgraded"
    );
    assert_eq!(
        db.get_execution(&exec.id).unwrap().status,
        ExecutionStatus::WaitingDependency,
        "the row must actually carry the downgraded status"
    );
}

/// Guard miss: the `status = 'ready'` guard rejects an execution that is no
/// longer ready (here, one this method already downgraded) and an id that
/// matches no row at all. Both are no-ops reporting `false`, and the
/// already-downgraded row keeps its state.
#[test]
fn downgrade_ready_to_waiting_dependency_no_ops_on_non_ready_and_unknown() {
    let db = WorkDb::open(temp_db_path("downgrade-guard")).unwrap();
    let product = create_test_product_named(&db, "Boss-downgrade-guard");
    let chore = create_test_chore(&db, product.id.clone(), "downgrade-guard");
    let exec = create_ready_chore_execution(&db, chore.id.clone());

    assert!(db.downgrade_ready_to_waiting_dependency(&exec.id).unwrap());

    // The row is `waiting_dependency` now — the guard must reject a repeat.
    assert!(
        !db.downgrade_ready_to_waiting_dependency(&exec.id).unwrap(),
        "a second downgrade finds no ready row and reports false"
    );
    assert_eq!(
        db.get_execution(&exec.id).unwrap().status,
        ExecutionStatus::WaitingDependency,
        "the guard miss must leave the existing state untouched"
    );

    assert!(
        !db.downgrade_ready_to_waiting_dependency("exec-does-not-exist").unwrap(),
        "an unknown execution id reports false"
    );
}

/// Scoping: the downgrade targets exactly the named execution — a sibling
/// `ready` execution on another work item is untouched.
#[test]
fn downgrade_ready_to_waiting_dependency_leaves_other_executions_alone() {
    let db = WorkDb::open(temp_db_path("downgrade-scope")).unwrap();
    let product = create_test_product_named(&db, "Boss-downgrade-scope");
    let target_chore = create_test_chore(&db, product.id.clone(), "downgrade-target");
    let other_chore = create_test_chore(&db, product.id.clone(), "downgrade-other");
    let target = create_ready_chore_execution(&db, target_chore.id.clone());
    let other = create_ready_chore_execution(&db, other_chore.id.clone());

    assert!(db.downgrade_ready_to_waiting_dependency(&target.id).unwrap());

    assert_eq!(
        db.get_execution(&other.id).unwrap().status,
        ExecutionStatus::Ready,
        "an unrelated ready execution must not be downgraded"
    );
}

// ── retire_stale_revision_before_dispatch (work/dispatch.rs) ────────────────

/// Seed a `todo` revision task under a fresh chore root plus the `ready`
/// `revision_implementation` execution the dispatcher would be about to spawn.
/// Returns `(revision_task_id, execution_id)`.
fn seed_ready_revision(db: &WorkDb, label: &str) -> (String, String) {
    let product = make_revision_product(db, label);
    let root = make_chore_root(db, &product, label);
    let revision = insert_revision_row(db, &product, &root);
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    (revision, exec.id)
}

/// Happy path: a revision whose conflict-fix vehicle became unnecessary before
/// dispatch is retired — the execution is abandoned and the still-`todo`
/// revision task advances to `in_review`, the same terminal state a normal
/// successful revision reaches. Reports `true` because the task transitioned.
#[test]
fn retire_stale_revision_before_dispatch_abandons_execution_and_advances_task() {
    let db = WorkDb::open(temp_db_path("retire-happy")).unwrap();
    let (revision, exec_id) = seed_ready_revision(&db, "retire-happy");

    assert!(
        db.retire_stale_revision_before_dispatch(&exec_id, &revision).unwrap(),
        "a todo revision must be retired and report the transition"
    );
    assert_eq!(
        task_status(&db, &revision),
        "in_review",
        "the retired revision leaves Doing/Backlog for in_review"
    );
    assert_eq!(
        db.get_execution(&exec_id).unwrap().status,
        ExecutionStatus::Abandoned,
        "the never-dispatched execution is abandoned"
    );
}

/// The task-status guard accepts `active` as well as `todo`: a revision that
/// already flipped to `active` (a worker started before the retire raced in)
/// is still retired.
#[test]
fn retire_stale_revision_before_dispatch_accepts_an_active_revision() {
    let db = WorkDb::open(temp_db_path("retire-active")).unwrap();
    let (revision, exec_id) = seed_ready_revision(&db, "retire-active");
    db.update_work_item(
        &revision,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert!(
        db.retire_stale_revision_before_dispatch(&exec_id, &revision).unwrap(),
        "the guard accepts an active revision, not just a todo one"
    );
    assert_eq!(task_status(&db, &revision), "in_review");
}

/// Guard miss: a revision a concurrent path already moved out of
/// `todo`/`active` (here, already `in_review`) is left alone — the method
/// reports `false` and the task keeps its status.
#[test]
fn retire_stale_revision_before_dispatch_leaves_a_non_stale_revision_alone() {
    let db = WorkDb::open(temp_db_path("retire-guard")).unwrap();
    let (revision, exec_id) = seed_ready_revision(&db, "retire-guard");
    db.update_work_item(
        &revision,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert!(
        !db.retire_stale_revision_before_dispatch(&exec_id, &revision).unwrap(),
        "a revision already out of todo/active must not be re-transitioned"
    );
    assert_eq!(
        task_status(&db, &revision),
        "in_review",
        "the guard miss leaves the task status untouched"
    );
}

/// Idempotency: a second retire of the same revision reports `false` — the
/// first call moved the task to `in_review`, so the guard now misses.
#[test]
fn retire_stale_revision_before_dispatch_is_idempotent() {
    let db = WorkDb::open(temp_db_path("retire-idem")).unwrap();
    let (revision, exec_id) = seed_ready_revision(&db, "retire-idem");

    assert!(db.retire_stale_revision_before_dispatch(&exec_id, &revision).unwrap());
    assert!(
        !db.retire_stale_revision_before_dispatch(&exec_id, &revision).unwrap(),
        "a repeat retire is a no-op reporting false"
    );
    assert_eq!(task_status(&db, &revision), "in_review");
}

// ── list_queued_merge_queue_members (work/pr_flow.rs) ───────────────────────

/// Drive `task_id` into GitHub's merge queue through the same public poll-state
/// seam the merge poller uses, stamping `merge_queue_detail` with `detail`.
fn enqueue_in_merge_queue(db: &WorkDb, task_id: &str, detail: &str) {
    db.update_task_pr_poll_state(
        task_id,
        PrPollStateInput {
            ci_required_state: "pending",
            review_required_state: "approved",
            merge_queue_state: Some("queued"),
            merge_queue_detail: Some(detail),
            ..Default::default()
        },
    )
    .unwrap();
}

/// Ids of the queued members for `product_id`, sorted so assertions do not
/// depend on the (unspecified) row order.
fn queued_member_ids(db: &WorkDb, product_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = db
        .list_queued_merge_queue_members(product_id)
        .unwrap()
        .into_iter()
        .map(|m| m.task_id)
        .collect();
    ids.sort();
    ids
}

/// Happy path: every queued task in the product is returned, carrying the
/// `merge_queue_detail` blob the poller stamped, and nothing else.
#[test]
fn list_queued_merge_queue_members_returns_queued_members_with_detail() {
    let db = WorkDb::open(temp_db_path("mq-list-happy")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-happy");
    let first = create_test_chore(&db, product.id.clone(), "mq-first").id;
    let second = create_test_chore(&db, product.id.clone(), "mq-second").id;
    enqueue_in_merge_queue(&db, &first, "{\"position\":1}");
    enqueue_in_merge_queue(&db, &second, "{\"position\":2}");

    let members = db.list_queued_merge_queue_members(&product.id).unwrap();
    assert_eq!(members.len(), 2, "both queued tasks are members");

    let mut by_id: Vec<(String, Option<String>)> =
        members.into_iter().map(|m| (m.task_id, m.merge_queue_detail)).collect();
    by_id.sort();
    let mut expected = vec![
        (first, Some("{\"position\":1}".to_owned())),
        (second, Some("{\"position\":2}".to_owned())),
    ];
    expected.sort();
    assert_eq!(by_id, expected, "each member carries its stamped detail blob");
}

/// Scoping: a task that never entered the queue is excluded, and so is a task
/// the poller observed leaving it (`merge_queue_state` back to NULL).
#[test]
fn list_queued_merge_queue_members_excludes_non_queued_tasks() {
    let db = WorkDb::open(temp_db_path("mq-list-nonqueued")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-nonqueued");
    let queued = create_test_chore(&db, product.id.clone(), "mq-queued").id;
    let never_queued = create_test_chore(&db, product.id.clone(), "mq-never").id;
    let left_queue = create_test_chore(&db, product.id.clone(), "mq-left").id;

    enqueue_in_merge_queue(&db, &queued, "{\"position\":1}");
    enqueue_in_merge_queue(&db, &left_queue, "{\"position\":2}");
    // The next poll observes `left_queue` out of the queue entirely.
    db.update_task_pr_poll_state(
        &left_queue,
        PrPollStateInput {
            ci_required_state: "pending",
            review_required_state: "approved",
            ..Default::default()
        },
    )
    .unwrap();

    // The equality carries both exclusions: neither the never-queued task nor
    // the one that left the queue survives into the member list.
    assert_eq!(
        queued_member_ids(&db, &product.id),
        vec![queued],
        "only the still-queued task is a member; {never_queued} (never queued) \
         and {left_queue} (left the queue) must both be excluded"
    );
}

/// Scoping: the listing is per-product — another product's queued member never
/// leaks into this product's renumbering pass.
#[test]
fn list_queued_merge_queue_members_excludes_other_products() {
    let db = WorkDb::open(temp_db_path("mq-list-scope")).unwrap();
    let mine = create_test_product_named(&db, "Boss-mq-mine");
    let theirs = create_test_product_named(&db, "Boss-mq-theirs");
    let my_task = create_test_chore(&db, mine.id.clone(), "mq-mine").id;
    let their_task = create_test_chore(&db, theirs.id.clone(), "mq-theirs").id;
    enqueue_in_merge_queue(&db, &my_task, "{\"position\":1}");
    enqueue_in_merge_queue(&db, &their_task, "{\"position\":1}");

    assert_eq!(
        queued_member_ids(&db, &mine.id),
        vec![my_task],
        "only this product's queued member is listed"
    );
    assert_eq!(
        queued_member_ids(&db, &theirs.id),
        vec![their_task],
        "the other product lists its own member, not ours"
    );
}

/// Empty case: a product with tasks but nothing in the queue returns no
/// members (rather than erroring), as does an unknown product id.
#[test]
fn list_queued_merge_queue_members_is_empty_when_nothing_is_queued() {
    let db = WorkDb::open(temp_db_path("mq-list-empty")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-empty");
    create_test_chore(&db, product.id.clone(), "mq-idle");

    assert!(
        db.list_queued_merge_queue_members(&product.id).unwrap().is_empty(),
        "no queued tasks → no members"
    );
    assert!(
        db.list_queued_merge_queue_members("prod-does-not-exist")
            .unwrap()
            .is_empty(),
        "an unknown product id yields an empty member list"
    );
}

// ── update_task_merge_queue_detail (work/pr_flow.rs) ────────────────────────

/// The stored `merge_queue_detail` for `task_id`, read back through the public
/// member listing rather than a raw column read.
fn queued_detail(db: &WorkDb, product_id: &str, task_id: &str) -> Option<String> {
    db.list_queued_merge_queue_members(product_id)
        .unwrap()
        .into_iter()
        .find(|m| m.task_id == task_id)
        .and_then(|m| m.merge_queue_detail)
}

/// Happy path: a renumbering pass overwrites a queued member's detail; the new
/// blob is persisted and readable back, and the call reports `true` so the
/// caller knows to emit a change event.
#[test]
fn update_task_merge_queue_detail_persists_the_new_detail() {
    let db = WorkDb::open(temp_db_path("mq-detail-happy")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-detail-happy");
    let task = create_test_chore(&db, product.id.clone(), "mq-detail").id;
    enqueue_in_merge_queue(&db, &task, "{\"position\":3}");

    assert!(
        db.update_task_merge_queue_detail(&task, "{\"position\":1}").unwrap(),
        "a changed detail on a queued row reports true"
    );
    assert_eq!(
        queued_detail(&db, &product.id, &task),
        Some("{\"position\":1}".to_owned()),
        "the renumbered detail is what the next pass reads back"
    );
}

/// No-change / unknown-id: re-writing the value already stored reports `false`
/// (nothing to broadcast) without disturbing it, and an unknown task id
/// matches no row and likewise reports `false`.
#[test]
fn update_task_merge_queue_detail_reports_false_when_unchanged_or_unknown() {
    let db = WorkDb::open(temp_db_path("mq-detail-noop")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-detail-noop");
    let task = create_test_chore(&db, product.id.clone(), "mq-detail-noop").id;
    enqueue_in_merge_queue(&db, &task, "{\"position\":1}");

    assert!(
        !db.update_task_merge_queue_detail(&task, "{\"position\":1}").unwrap(),
        "re-writing the stored value is not a change"
    );
    assert_eq!(
        queued_detail(&db, &product.id, &task),
        Some("{\"position\":1}".to_owned()),
        "the no-op must leave the stored detail intact"
    );

    assert!(
        !db.update_task_merge_queue_detail("task-does-not-exist", "{\"position\":1}")
            .unwrap(),
        "an unknown task id reports false"
    );
}

/// Guard miss: a row that exited the queue between the member listing's read
/// and this write is left untouched — the `merge_queue_state = 'queued'` guard
/// rejects it and the call reports `false`. This is the exact race the guard
/// exists for: the renumbering pass listed the task while it was queued, then
/// a poll observed it leave before the recomputed position was written back.
#[test]
fn update_task_merge_queue_detail_skips_a_row_that_left_the_queue() {
    let db = WorkDb::open(temp_db_path("mq-detail-guard")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-detail-guard");
    let task = create_test_chore(&db, product.id.clone(), "mq-detail-guard").id;
    enqueue_in_merge_queue(&db, &task, "{\"position\":2}");
    // The renumbering pass has its member list; now the task leaves the queue.
    db.update_task_pr_poll_state(
        &task,
        PrPollStateInput {
            ci_required_state: "pending",
            review_required_state: "approved",
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        !db.update_task_merge_queue_detail(&task, "{\"position\":1}").unwrap(),
        "a task that left the queue is no longer a live member"
    );
    assert!(
        db.list_queued_merge_queue_members(&product.id).unwrap().is_empty(),
        "the guard miss must not resurrect the row as a queue member"
    );
}

/// Guard miss: a task that never entered the queue at all is likewise not a
/// live member — the write is refused rather than stamping a merge-queue
/// position onto an unrelated task.
#[test]
fn update_task_merge_queue_detail_skips_a_never_queued_task() {
    let db = WorkDb::open(temp_db_path("mq-detail-never")).unwrap();
    let product = create_test_product_named(&db, "Boss-mq-detail-never");
    let task = create_test_chore(&db, product.id.clone(), "mq-detail-never").id;

    assert!(
        !db.update_task_merge_queue_detail(&task, "{\"position\":1}").unwrap(),
        "a task that was never queued is not a live queue member"
    );
    assert_eq!(
        queued_detail(&db, &product.id, &task),
        None,
        "the guard miss must not stamp a detail onto a non-member"
    );
}

// ── merge_order_merged_siblings (work/workitems.rs) ─────────────────────────

/// Pair `later` and `first` with a canonical `merge_order` edge: the
/// prerequisite side is the "first" (earlier-merging) task, the dependent side
/// is the "later" one. Mirrors what the materializer inserts for an overlap
/// pair; `add_dependency` only ships `blocks` in v1, so the edge helper the
/// materializer itself uses is the seam here.
fn pair_merge_order(db: &WorkDb, later: &str, first: &str) {
    let conn = db.connect().unwrap();
    deps::insert_edge(&conn, later, first, deps::RELATION_MERGE_ORDER, &now_string()).unwrap();
}

/// Happy path: an item in a merge-order chain sees its already-merged
/// (`done`) sibling, carrying the PR url the forward-port brief names.
#[test]
fn merge_order_merged_siblings_returns_merged_siblings_with_pr_urls() {
    let db = WorkDb::open(temp_db_path("mo-happy")).unwrap();
    let product = make_revision_product(&db, "mo-happy");
    let merged_pr = "https://github.com/spinyfin/mono/pull/701";
    let merged = make_done_chore(&db, &product, merged_pr);
    let later = create_test_chore(&db, product.clone(), "mo-later").id;
    pair_merge_order(&db, &later, &merged);

    let siblings = db.merge_order_merged_siblings(&later).unwrap();
    assert_eq!(siblings.len(), 1, "the one merged partner is returned");
    assert_eq!(siblings[0].task_id, merged);
    assert_eq!(
        siblings[0].pr_url.as_deref(),
        Some(merged_pr),
        "the sibling's PR url is carried so the brief can name it"
    );
}

/// The pairing is undirected: a merged sibling is found from either end of the
/// edge, so the "first" side of a pair also sees a merged "later" partner.
#[test]
fn merge_order_merged_siblings_resolves_the_pair_from_either_side() {
    let db = WorkDb::open(temp_db_path("mo-direction")).unwrap();
    let product = make_revision_product(&db, "mo-direction");
    let merged_pr = "https://github.com/spinyfin/mono/pull/702";
    // The *dependent* ("later") side is the one that merged first in practice.
    let merged_later = make_done_chore(&db, &product, merged_pr);
    let first = create_test_chore(&db, product.clone(), "mo-first").id;
    pair_merge_order(&db, &merged_later, &first);

    let siblings = db.merge_order_merged_siblings(&first).unwrap();
    assert_eq!(siblings.len(), 1, "the prerequisite side also sees its merged peer");
    assert_eq!(siblings[0].task_id, merged_later);
}

/// Exclusion: only merged (`done`) siblings count — an in-flight partner that
/// has not landed yet is not a preservation constraint and is filtered out.
#[test]
fn merge_order_merged_siblings_excludes_unmerged_siblings() {
    let db = WorkDb::open(temp_db_path("mo-unmerged")).unwrap();
    let product = make_revision_product(&db, "mo-unmerged");
    let merged_pr = "https://github.com/spinyfin/mono/pull/703";
    let merged = make_done_chore(&db, &product, merged_pr);
    // An in-flight partner: its PR is open but has not landed yet.
    let in_review = create_test_chore(&db, product.clone(), "mo-in-review").id;
    db.update_work_item(
        &in_review,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let todo = create_test_chore(&db, product.clone(), "mo-todo").id;
    let later = create_test_chore(&db, product.clone(), "mo-later-mixed").id;

    // Three merge_order partners, only one of which has actually merged.
    pair_merge_order(&db, &later, &merged);
    pair_merge_order(&db, &later, &in_review);
    pair_merge_order(&db, &later, &todo);

    let siblings = db.merge_order_merged_siblings(&later).unwrap();
    assert_eq!(
        siblings.iter().map(|s| s.task_id.clone()).collect::<Vec<_>>(),
        vec![merged],
        "only the done sibling is a merged overlap partner"
    );
}

/// A `blocks` edge is not a `merge_order` pairing: the gating graph must never
/// leak into the soft merge-sequencing read.
#[test]
fn merge_order_merged_siblings_ignores_blocks_edges() {
    let db = WorkDb::open(temp_db_path("mo-blocks")).unwrap();
    let product = make_revision_product(&db, "mo-blocks");
    let merged = make_done_chore(&db, &product, "https://github.com/spinyfin/mono/pull/705");
    let later = create_test_chore(&db, product.clone(), "mo-later-blocks").id;
    db.add_dependency(AddDependencyInput {
        dependent: later.clone(),
        prerequisite: merged.clone(),
        relation: Some(RELATION_BLOCKS.to_owned()),
    })
    .unwrap();

    assert!(
        db.merge_order_merged_siblings(&later).unwrap().is_empty(),
        "a merged blocks-prereq is not a merge_order sibling"
    );
}

/// Empty case: an item with no merge-order chain at all has no merged
/// siblings, so the brief composer stamps no preservation clause.
#[test]
fn merge_order_merged_siblings_is_empty_without_a_chain() {
    let db = WorkDb::open(temp_db_path("mo-empty")).unwrap();
    let product = make_revision_product(&db, "mo-empty");
    let lonely = create_test_chore(&db, product.clone(), "mo-lonely").id;

    assert!(
        db.merge_order_merged_siblings(&lonely).unwrap().is_empty(),
        "an item with no merge_order edge has no merged siblings"
    );
}

// ── promote_todo_autostart_stuck_executions (work/workitems.rs) ─────────────

/// Create a `waiting_dependency` chore execution for `work_item_id` — the
/// stale state the Part B recovery sweep exists to unstick.
fn create_waiting_dependency_execution(db: &WorkDb, work_item_id: &str) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id.to_owned())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::WaitingDependency)
            .build(),
    )
    .unwrap()
}

/// `true` when `work_item_id` has at least one `ready` execution.
fn has_ready_execution(db: &WorkDb, work_item_id: &str) -> bool {
    executions_for(db, work_item_id)
        .iter()
        .any(|(_, status)| status == "ready")
}

/// Happy path: a `todo`, autostart item left with a stale
/// `waiting_dependency` execution and no unmet prereqs is promoted to `ready`
/// and its id reported.
#[test]
fn promote_todo_autostart_stuck_executions_promotes_a_stuck_item() {
    let db = WorkDb::open(temp_db_path("promote-happy")).unwrap();
    let product = create_test_product_named(&db, "Boss-promote-happy");
    let chore = create_test_chore(&db, product.id.clone(), "promote-happy");
    create_waiting_dependency_execution(&db, &chore.id);

    let promoted = db.promote_todo_autostart_stuck_executions().unwrap();
    assert_eq!(promoted, vec![chore.id.clone()], "the stuck item's id is reported");
    assert!(
        has_ready_execution(&db, &chore.id),
        "the stale waiting_dependency execution is promoted to ready"
    );
}

/// Scoping: a manual (`autostart = false`) item is never auto-promoted, even
/// when its execution is stuck in exactly the same way — starting it is the
/// human's call.
#[test]
fn promote_todo_autostart_stuck_executions_skips_non_autostart_items() {
    let db = WorkDb::open(temp_db_path("promote-manual")).unwrap();
    let product = create_test_product_named(&db, "Boss-promote-manual");
    let chore = create_test_chore_manual(&db, product.id.clone(), "promote-manual");
    create_waiting_dependency_execution(&db, &chore.id);

    assert!(
        db.promote_todo_autostart_stuck_executions().unwrap().is_empty(),
        "a manual-start item is not swept"
    );
    assert!(
        !has_ready_execution(&db, &chore.id),
        "the manual item's execution must stay waiting_dependency"
    );
}

/// Scoping: an item that is not stuck — its latest execution is already
/// `ready` — needs no promotion and is not reported.
#[test]
fn promote_todo_autostart_stuck_executions_skips_already_ready_items() {
    let db = WorkDb::open(temp_db_path("promote-ready")).unwrap();
    let product = create_test_product_named(&db, "Boss-promote-ready");
    let chore = create_test_chore(&db, product.id.clone(), "promote-ready");
    create_ready_chore_execution(&db, chore.id.clone());

    assert!(
        db.promote_todo_autostart_stuck_executions().unwrap().is_empty(),
        "an item already ready is not stuck and is not reported"
    );
}

/// Scoping: a `todo` autostart item that still has an unmet `blocks` prereq is
/// left waiting — promoting it would defeat the dependency gate.
///
/// Reaching this branch takes care: `add_dependency` auto-blocks the dependent,
/// and a `blocked` row is already excluded by the sweep's `status = 'todo'`
/// filter, so the prereq check would never be reached. The row must therefore
/// be `todo` *while its gate is still unmet* — the stale legacy state the sweep
/// exists to handle (an item unblocked to `todo` before the atomic-unblock fix
/// landed). `update_work_item` deliberately refuses that move ("cannot move …
/// to todo: gated by …"), so the status is planted directly via SQL, as the
/// suite's other legacy-row probes do. Without this the test would pass on the
/// status filter and never exercise `gating_prereqs_for` at all.
#[test]
fn promote_todo_autostart_stuck_executions_skips_items_with_unmet_prereqs() {
    let db = WorkDb::open(temp_db_path("promote-gated")).unwrap();
    let product = create_test_product_named(&db, "Boss-promote-gated");
    let gate = create_test_chore(&db, product.id.clone(), "promote-gate");
    let gated = create_test_chore(&db, product.id.clone(), "promote-gated");
    db.add_dependency(AddDependencyInput {
        dependent: gated.id.clone(),
        prerequisite: gate.id.clone(),
        relation: Some(RELATION_BLOCKS.to_owned()),
    })
    .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'todo' WHERE id = ?1", params![gated.id])
        .unwrap();
    create_waiting_dependency_execution(&db, &gated.id);
    // Precondition: the row is a sweep candidate on every axis except its
    // still-unmet prereq, so a skip can only come from the prereq check.
    assert_eq!(task_status(&db, &gated.id), "todo");
    assert!(
        !db.gating_prereqs_for(&gated.id).unwrap().is_empty(),
        "the gate must still be unmet for this test to mean anything"
    );

    let promoted = db.promote_todo_autostart_stuck_executions().unwrap();
    assert!(
        !promoted.contains(&gated.id),
        "an item with an unmet gating prereq must stay waiting"
    );
    assert!(
        !has_ready_execution(&db, &gated.id),
        "the gated item must not gain a ready execution"
    );
}

/// Idempotency: a second sweep straight after a successful one finds nothing
/// left to promote — the first pass already moved the item to `ready`.
#[test]
fn promote_todo_autostart_stuck_executions_is_idempotent() {
    let db = WorkDb::open(temp_db_path("promote-idem")).unwrap();
    let product = create_test_product_named(&db, "Boss-promote-idem");
    let chore = create_test_chore(&db, product.id.clone(), "promote-idem");
    create_waiting_dependency_execution(&db, &chore.id);

    assert_eq!(db.promote_todo_autostart_stuck_executions().unwrap(), vec![chore.id]);
    assert!(
        db.promote_todo_autostart_stuck_executions().unwrap().is_empty(),
        "a repeat sweep finds nothing stuck"
    );
}

// ── mark_chore_blocked_deletion_signoff (work/blocking.rs) ──────────────────

/// Stand up a chore already flipped into the `blocked: merge_conflict` state
/// the deletion-signoff halt requires as its entry condition. Returns
/// `(chore_id, pr_url)`.
fn seed_chore_blocked_on_merge_conflict(db: &WorkDb, label: &str, pr_url: &str) -> String {
    let product = make_revision_product(db, label);
    let chore = make_in_review_chore(db, &product, pr_url);
    db.mark_chore_blocked_merge_conflict(&chore, pr_url)
        .unwrap()
        .expect("in_review chore must flip to blocked: merge_conflict");
    chore
}

/// `true` when an uncleared `reason` signal is present on `work_item_id`.
fn has_active_signal(db: &WorkDb, work_item_id: &str, reason: &str) -> bool {
    db.active_blocked_signals(work_item_id)
        .unwrap()
        .iter()
        .any(|s| s.reason == reason)
}

/// Happy path: the ladder's mechanical rung produced a resolution the
/// both-parents deletion tripwire rejected, so the chore halts on
/// `deletion_signoff` — the returned task carries the new blocked reason, the
/// PR url, and the engine as the status actor.
#[test]
fn mark_chore_blocked_deletion_signoff_flips_a_merge_conflict_chore() {
    let db = WorkDb::open(temp_db_path("signoff-happy")).unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/801";
    let chore = seed_chore_blocked_on_merge_conflict(&db, "signoff-happy", pr_url);

    let updated = db
        .mark_chore_blocked_deletion_signoff(&chore, pr_url)
        .unwrap()
        .expect("a blocked: merge_conflict chore must flip to deletion_signoff");

    assert_eq!(updated.status, TaskStatus::Blocked, "the chore stays blocked");
    assert_eq!(
        updated.blocked_reason.as_deref(),
        Some("deletion_signoff"),
        "the halt reason is the deletion signoff"
    );
    assert_eq!(updated.pr_url.as_deref(), Some(pr_url), "the PR url is stamped");
    assert_eq!(
        updated.blocked_attempt_id, None,
        "the merge-conflict attempt id is cleared with the reason"
    );
    assert_eq!(updated.last_status_actor, "engine");
}

/// Leaving the merge-conflict state clears its side-table signal, mirroring
/// `clear_chore_blocked_merge_conflict` — otherwise the stale signal would
/// keep the chore in the merge-conflict sweep's candidate set.
#[test]
fn mark_chore_blocked_deletion_signoff_clears_the_merge_conflict_signal() {
    let db = WorkDb::open(temp_db_path("signoff-signal")).unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/802";
    let chore = seed_chore_blocked_on_merge_conflict(&db, "signoff-signal", pr_url);
    assert!(
        has_active_signal(&db, &chore, "merge_conflict"),
        "the merge_conflict flip must arm its signal up front"
    );

    db.mark_chore_blocked_deletion_signoff(&chore, pr_url)
        .unwrap()
        .expect("the flip lands");

    assert!(
        !has_active_signal(&db, &chore, "merge_conflict"),
        "the merge_conflict signal is cleared on leaving that state"
    );
}

/// Guard miss: the flip requires the chore to still be in the
/// `blocked: merge_conflict` state the caller left it in. A chore that is
/// merely `in_review` — never flipped, or moved back by a human — is left
/// alone and the call returns `Ok(None)`.
#[test]
fn mark_chore_blocked_deletion_signoff_no_ops_on_an_unmarkable_chore() {
    let db = WorkDb::open(temp_db_path("signoff-guard")).unwrap();
    let product = make_revision_product(&db, "signoff-guard");
    let pr_url = "https://github.com/spinyfin/mono/pull/803";
    let chore = make_in_review_chore(&db, &product, pr_url);

    assert!(
        db.mark_chore_blocked_deletion_signoff(&chore, pr_url)
            .unwrap()
            .is_none(),
        "a chore not blocked on merge_conflict must not be flipped"
    );
    assert_eq!(
        task_status(&db, &chore),
        "in_review",
        "the guard miss leaves the chore's status untouched"
    );
}

/// Guard miss: a chore blocked for some *other* reason (here
/// `ci_failure_exhausted`) is not a deletion-signoff candidate — the
/// `blocked_reason = 'merge_conflict'` half of the guard rejects it, so a
/// blocked chore is not indiscriminately re-pointed at the signoff halt.
#[test]
fn mark_chore_blocked_deletion_signoff_no_ops_on_a_differently_blocked_chore() {
    let db = WorkDb::open(temp_db_path("signoff-other-reason")).unwrap();
    let product = make_revision_product(&db, "signoff-other-reason");
    let pr_url = "https://github.com/spinyfin/mono/pull/804";
    let chore = make_in_review_chore(&db, &product, pr_url);
    db.mark_chore_blocked_ci_failure_exhausted(&chore, pr_url)
        .unwrap()
        .expect("in_review chore flips to blocked: ci_failure_exhausted");

    assert!(
        db.mark_chore_blocked_deletion_signoff(&chore, pr_url)
            .unwrap()
            .is_none(),
        "a chore blocked on a non-merge_conflict reason is not a signoff candidate"
    );
    assert_eq!(
        db.task_blocked_reason(&chore).unwrap().as_deref(),
        Some("ci_failure_exhausted"),
        "the guard miss leaves the existing blocked reason intact"
    );
}

/// Idempotency: a second flip finds the chore already on `deletion_signoff`
/// (no longer `merge_conflict`), so the guard misses and returns `Ok(None)`
/// without disturbing the halt.
#[test]
fn mark_chore_blocked_deletion_signoff_is_idempotent() {
    let db = WorkDb::open(temp_db_path("signoff-idem")).unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/805";
    let chore = seed_chore_blocked_on_merge_conflict(&db, "signoff-idem", pr_url);

    db.mark_chore_blocked_deletion_signoff(&chore, pr_url)
        .unwrap()
        .expect("the first flip lands");
    assert!(
        db.mark_chore_blocked_deletion_signoff(&chore, pr_url)
            .unwrap()
            .is_none(),
        "a repeat flip is a no-op returning Ok(None)"
    );
    assert_eq!(
        db.task_blocked_reason(&chore).unwrap().as_deref(),
        Some("deletion_signoff"),
        "the halt survives the repeat call"
    );
}
