use super::*;

// `InsertAttentionMergeInput` is crate-internal (`work.rs` re-exports only the
// wire types from `attentions`), so name it by its in-crate path.
use crate::work::attentions::InsertAttentionMergeInput;

// Behaviour coverage for the `WorkDb` attention methods that the recovery
// sweeps, the external-tracker reconciler, the churn guard, and the dedup
// sweep drive in production but that had no direct test. Each test drives the
// public `WorkDb` API against an in-memory db and asserts on returned values
// and subsequently-observable state — never on SQL shape or private helpers.

/// A product plus one chore to hang work-item-scoped attention items off.
/// Returns `(db, product_id, chore_id)`.
fn chore_fixture(label: &str) -> (WorkDb, String, String) {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product = create_test_product_named(&db, label);
    let chore = create_test_chore(&db, product.id.clone(), "Do the thing");
    (db, product.id, chore.id)
}

/// An execution bound to a fresh chore under `product_id`, for the
/// execution-scoped worker-signal tests.
fn seed_execution_for_chore(db: &WorkDb, product_id: &str, label: &str) -> String {
    let chore = create_test_chore(db, product_id.to_owned(), label.to_owned());
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
    .id
}

/// File an open, execution-scoped attention item of `kind` through the public
/// create verb — the same seam `worker_escalation` uses when a worker emits a
/// marker.
fn open_execution_attention(db: &WorkDb, execution_id: &str, kind: &str) -> String {
    db.create_attention_item(CreateAttentionItemInput {
        body_markdown: "body".to_owned(),
        kind: kind.to_owned(),
        title: "title".to_owned(),
        execution_id: Some(execution_id.to_owned()),
        resolved_at: None,
        status: None,
        work_item_id: None,
    })
    .unwrap()
    .id
}

/// The single open item of `kind` on `work_item_id`, or `None`.
fn open_item_of_kind(db: &WorkDb, work_item_id: &str, kind: &str) -> Option<WorkAttentionItem> {
    db.list_attention_items_for_work_item(work_item_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == kind && item.status == "open")
}

fn status_of(db: &WorkDb, attention_id: &str) -> String {
    db.get_attention_item(attention_id).unwrap().status
}

const ESCALATION: &str = crate::worker_escalation::WORKER_ESCALATION_ATTENTION_KIND;
const BLOCKED: &str = crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND;

// ── upsert_work_item_attention ─────────────────────────────────────────────

/// The dedupe contract the transient-recovery sweep leans on: a second call
/// with the same `(work_item_id, kind)` folds into the open item rather than
/// piling up a duplicate row, and hands back that same id.
#[test]
fn upsert_work_item_attention_reuses_the_open_item_of_the_same_kind() {
    let (db, _product, chore) = chore_fixture("upsert-wi-dedupe");

    let first = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Stalled", "first body")
        .unwrap();
    let second = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Stalled again", "second body")
        .unwrap();

    assert_eq!(first, second, "same kind must fold into the open item");
    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1, "repeated sweep passes must not pile up rows");
    assert_eq!(items[0].status, "open");
}

/// Dedupe is scoped to the kind: a different failure mode on the same work
/// item earns its own item so the operator sees both.
#[test]
fn upsert_work_item_attention_creates_a_distinct_item_per_kind() {
    let (db, _product, chore) = chore_fixture("upsert-wi-kinds");

    let recovery = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Stalled", "body")
        .unwrap();
    let dead_pid = db
        .upsert_work_item_attention(&chore, "dead_pid", "Worker vanished", "body")
        .unwrap();

    assert_ne!(recovery, dead_pid, "distinct kinds are distinct items");
    let kinds: Vec<String> = db
        .list_attention_items_for_work_item(&chore)
        .unwrap()
        .into_iter()
        .map(|item| item.kind)
        .collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&"transient_recovery".to_owned()));
    assert!(kinds.contains(&"dead_pid".to_owned()));
}

/// Folding into an open item leaves its text at the first trip's wording.
/// This pins the deliberate asymmetry with
/// `upsert_external_tracker_attention`, which *does* refresh — a caller that
/// wants the operator-visible text to track the latest trip must use that
/// verb, not this one.
#[test]
fn upsert_work_item_attention_keeps_the_original_title_and_body_on_a_fold() {
    let (db, _product, chore) = chore_fixture("upsert-wi-text");

    db.upsert_work_item_attention(&chore, "transient_recovery", "First title", "first body")
        .unwrap();
    let id = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Second title", "second body")
        .unwrap();

    let item = db.get_attention_item(&id).unwrap();
    assert_eq!(item.title, "First title");
    assert_eq!(item.body_markdown, "first body");
}

/// Dedupe only suppresses against an *open* item: once the prior item is
/// resolved, a fresh trip re-raises rather than staying silent — otherwise a
/// recurrence after a resolve would never reach the operator.
#[test]
fn upsert_work_item_attention_raises_a_fresh_item_after_the_prior_one_resolved() {
    let (db, _product, chore) = chore_fixture("upsert-wi-reraise");

    let first = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Stalled", "body")
        .unwrap();
    // The only public resolve-by-(work_item, kind) verb; it is kind-agnostic despite the name.
    db.resolve_external_tracker_attention(&chore, "transient_recovery")
        .unwrap();

    let second = db
        .upsert_work_item_attention(&chore, "transient_recovery", "Stalled again", "new body")
        .unwrap();

    assert_ne!(second, first, "a resolved item must not suppress a re-raise");
    assert_eq!(status_of(&db, &first), "resolved");
    assert_eq!(status_of(&db, &second), "open");
    assert_eq!(db.list_attention_items_for_work_item(&chore).unwrap().len(), 2);
}

/// A typo'd work item id is an error rather than a silently orphaned row.
#[test]
fn upsert_work_item_attention_rejects_an_unknown_work_item() {
    let (db, _product, _chore) = chore_fixture("upsert-wi-unknown");

    assert!(
        db.upsert_work_item_attention("task_nope", "transient_recovery", "t", "b")
            .is_err()
    );
}

// ── resolve_worker_signal_attentions_for_execution ─────────────────────────

/// A probe acks both worker-signal markers at once and reports how many it
/// closed.
#[test]
fn resolve_worker_signal_attentions_resolves_both_marker_kinds_and_returns_the_count() {
    let (db, product, _chore) = chore_fixture("resolve-signals-both");
    let execution = seed_execution_for_chore(&db, &product, "signals");
    let escalation = open_execution_attention(&db, &execution, ESCALATION);
    let blocked = open_execution_attention(&db, &execution, BLOCKED);

    let resolved = db.resolve_worker_signal_attentions_for_execution(&execution).unwrap();

    assert_eq!(resolved, 2);
    assert_eq!(status_of(&db, &escalation), "resolved");
    assert_eq!(status_of(&db, &blocked), "resolved");
}

/// Resolution is scoped to the probed execution — a sibling worker's open
/// escalation must survive, or one probe would silently ack everyone.
#[test]
fn resolve_worker_signal_attentions_leaves_other_executions_untouched() {
    let (db, product, _chore) = chore_fixture("resolve-signals-scope");
    let probed = seed_execution_for_chore(&db, &product, "probed");
    let other = seed_execution_for_chore(&db, &product, "other");
    let mine = open_execution_attention(&db, &probed, ESCALATION);
    let theirs = open_execution_attention(&db, &other, ESCALATION);

    let resolved = db.resolve_worker_signal_attentions_for_execution(&probed).unwrap();

    assert_eq!(resolved, 1, "only the probed execution's item counts");
    assert_eq!(status_of(&db, &mine), "resolved");
    assert_eq!(status_of(&db, &theirs), "open", "a sibling's escalation must survive");
}

/// A probe acks the worker-signal markers only. Other kinds on the same
/// execution (e.g. a `deferred_scope` item awaiting a human's call) stay open.
#[test]
fn resolve_worker_signal_attentions_leaves_other_kinds_untouched() {
    let (db, product, _chore) = chore_fixture("resolve-signals-kinds");
    let execution = seed_execution_for_chore(&db, &product, "kinds");
    let escalation = open_execution_attention(&db, &execution, ESCALATION);
    let deferred = open_execution_attention(&db, &execution, crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND);

    let resolved = db.resolve_worker_signal_attentions_for_execution(&execution).unwrap();

    assert_eq!(resolved, 1);
    assert_eq!(status_of(&db, &escalation), "resolved");
    assert_eq!(status_of(&db, &deferred), "open", "only worker-signal kinds are acked");
}

/// Re-probing is a no-op: already-resolved items are neither re-counted nor
/// reopened, and an execution that never signalled reports zero rather than
/// erroring.
#[test]
fn resolve_worker_signal_attentions_is_idempotent_and_reports_zero_when_none_are_open() {
    let (db, product, _chore) = chore_fixture("resolve-signals-idempotent");
    let quiet = seed_execution_for_chore(&db, &product, "quiet");
    assert_eq!(
        db.resolve_worker_signal_attentions_for_execution(&quiet).unwrap(),
        0,
        "no open signals is a normal zero, not an error"
    );

    let noisy = seed_execution_for_chore(&db, &product, "noisy");
    let escalation = open_execution_attention(&db, &noisy, ESCALATION);
    assert_eq!(db.resolve_worker_signal_attentions_for_execution(&noisy).unwrap(), 1);

    assert_eq!(
        db.resolve_worker_signal_attentions_for_execution(&noisy).unwrap(),
        0,
        "a second probe must not re-count the same item"
    );
    assert_eq!(status_of(&db, &escalation), "resolved", "and must not reopen it");
}

// ── upsert/resolve_external_tracker_attention ──────────────────────────────

/// The reconciler's round trip: repeated ticks for the same failure fold into
/// one open item, and a recovery clears it.
#[test]
fn external_tracker_attention_upsert_then_resolve_round_trips() {
    let (db, _product, chore) = chore_fixture("ext-roundtrip");

    db.upsert_external_tracker_attention(&chore, "external_tracker_sync", "Sync failed", "body")
        .unwrap();
    db.upsert_external_tracker_attention(&chore, "external_tracker_sync", "Sync failed", "body")
        .unwrap();
    assert_eq!(
        db.list_attention_items_for_work_item(&chore).unwrap().len(),
        1,
        "repeated ticks must not pile up rows"
    );

    db.resolve_external_tracker_attention(&chore, "external_tracker_sync")
        .unwrap();

    assert!(
        open_item_of_kind(&db, &chore, "external_tracker_sync").is_none(),
        "recovery must clear the failure item"
    );
}

/// Unlike `upsert_work_item_attention`, this verb refreshes the open item's
/// text so the operator sees the latest tick's failure detail rather than a
/// snapshot frozen at the first trip.
#[test]
fn external_tracker_attention_refreshes_the_open_items_title_and_body() {
    let (db, _product, chore) = chore_fixture("ext-refresh");

    db.upsert_external_tracker_attention(&chore, "external_tracker_sync", "1 failure", "first body")
        .unwrap();
    db.upsert_external_tracker_attention(&chore, "external_tracker_sync", "7 failures", "latest body")
        .unwrap();

    let item = open_item_of_kind(&db, &chore, "external_tracker_sync").expect("item stays open");
    assert_eq!(item.title, "7 failures");
    assert_eq!(item.body_markdown, "latest body");
}

/// Resolution is scoped to `(work_item_id, kind)`: another work item's item of
/// the same kind, and another kind on the same work item, both survive.
#[test]
fn resolve_external_tracker_attention_only_clears_the_matching_work_item_and_kind() {
    let (db, product, chore) = chore_fixture("ext-scope");
    let sibling = create_test_chore(&db, product, "Sibling chore").id;
    db.upsert_external_tracker_attention(&chore, "external_tracker_sync", "t", "b")
        .unwrap();
    db.upsert_external_tracker_attention(&chore, "other_kind", "t", "b")
        .unwrap();
    db.upsert_external_tracker_attention(&sibling, "external_tracker_sync", "t", "b")
        .unwrap();

    db.resolve_external_tracker_attention(&chore, "external_tracker_sync")
        .unwrap();

    assert!(open_item_of_kind(&db, &chore, "external_tracker_sync").is_none());
    assert!(
        open_item_of_kind(&db, &chore, "other_kind").is_some(),
        "a different kind on the same work item must survive"
    );
    assert!(
        open_item_of_kind(&db, &sibling, "external_tracker_sync").is_some(),
        "a sibling work item's item must survive"
    );
}

/// Resolving when nothing is open is a no-op, not an error — the reconciler
/// calls it unconditionally on every healthy tick.
#[test]
fn resolve_external_tracker_attention_is_a_no_op_when_nothing_is_open() {
    let (db, _product, chore) = chore_fixture("ext-noop");

    assert!(
        db.resolve_external_tracker_attention(&chore, "external_tracker_sync")
            .is_ok()
    );
    assert!(db.list_attention_items_for_work_item(&chore).unwrap().is_empty());
}

// ── file_churn_guard_parked_attention ──────────────────────────────────────

/// A guard trip leaves an open, operator-visible item naming the failure count
/// and the executions to look at.
#[test]
fn file_churn_guard_parked_attention_opens_an_item_naming_the_failures() {
    let (db, _product, chore) = chore_fixture("churn-open");
    let failing = vec!["exec_aaa".to_owned(), "exec_bbb".to_owned()];

    db.file_churn_guard_parked_attention(&chore, "orphan_sweep", 4, &failing);

    let item = open_item_of_kind(&db, &chore, CHURN_GUARD_PARKED_ATTENTION_KIND).expect("guard files an open item");
    assert!(
        item.title.contains('4'),
        "title names the failure count: {}",
        item.title
    );
    assert!(item.body_markdown.contains("orphan_sweep"), "body names the sweep");
    assert!(item.body_markdown.contains("exec_aaa") && item.body_markdown.contains("exec_bbb"));
}

/// The guard re-files on every ~60s sweep pass while the item is parked: those
/// passes must fold into the one open item and refresh it to the latest count,
/// not pile up a row per pass.
#[test]
fn file_churn_guard_parked_attention_folds_repeat_trips_and_refreshes_the_count() {
    let (db, _product, chore) = chore_fixture("churn-fold");

    db.file_churn_guard_parked_attention(&chore, "orphan_sweep", 4, &["exec_aaa".to_owned()]);
    db.file_churn_guard_parked_attention(&chore, "orphan_sweep", 9, &["exec_ccc".to_owned()]);

    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1, "repeated trips must not pile up rows");
    assert!(
        items[0].title.contains('9'),
        "the open item tracks the latest count: {}",
        items[0].title
    );
    assert!(items[0].body_markdown.contains("exec_ccc"));
}

/// Best-effort by contract: an unknown work item is swallowed (the caller has
/// already logged the trip) rather than panicking the sweep pass.
#[test]
fn file_churn_guard_parked_attention_swallows_an_unknown_work_item() {
    let (db, _product, _chore) = chore_fixture("churn-unknown");

    db.file_churn_guard_parked_attention("task_nope", "orphan_sweep", 4, &[]);
}

// ── attention_merges ledger ────────────────────────────────────────────────

/// Build a canonical `Attention` to hang merge rows off (the ledger's
/// `canonical_attention_id` is a real foreign key).
fn seed_attention(db: &WorkDb, product_id: &str, prompt: &str) -> String {
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product_id.to_owned())
                .name("Merges")
                .goal("goal")
                .build(),
        )
        .unwrap();
    db.create_attention(
        CreateAttentionInput::builder()
            .kind("question")
            .association_project_id(project.id)
            .source_kind("design_doc")
            .source_doc_path(format!("docs/{prompt}.md"))
            .question_type("prompt")
            .prompt_text(prompt)
            .build(),
    )
    .unwrap()
    .0
    .id
}

/// A `WorkItemDup` fold: recorded against a canonical work item.
fn work_item_merge(product_id: &str, summary: &str, work_item_id: &str) -> InsertAttentionMergeInput {
    InsertAttentionMergeInput::builder()
        .product_id(product_id)
        .trigger("sweep")
        .candidate_summary(summary)
        .model("claude-opus-4-8")
        .canonical_work_item_id(work_item_id)
        .build()
}

/// An `AttentionDup` fold: recorded against a canonical attention, optionally
/// naming the duplicate it folded (the pair the unique index guards).
fn attention_merge(
    product_id: &str,
    summary: &str,
    canonical_attention_id: &str,
    duplicate_attention_id: Option<&str>,
) -> InsertAttentionMergeInput {
    InsertAttentionMergeInput::builder()
        .product_id(product_id)
        .trigger("sweep")
        .candidate_summary(summary)
        .model("claude-opus-4-8")
        .canonical_attention_id(canonical_attention_id)
        .maybe_duplicate_attention_id(duplicate_attention_id)
        .build()
}

/// A work item's suppressed-duplicate count reflects the merges folded into
/// it, and does not leak merges recorded against another work item.
#[test]
fn count_attention_merges_by_work_item_counts_only_that_work_items_merges() {
    let (db, product, chore) = chore_fixture("merge-count");
    let sibling = create_test_chore(&db, product.clone(), "Sibling chore").id;
    assert_eq!(
        db.count_attention_merges_by_work_item(&chore).unwrap(),
        0,
        "a work item with no folds counts zero"
    );

    for summary in ["dup one", "dup two"] {
        db.insert_attention_merge(work_item_merge(&product, summary, &chore))
            .unwrap();
    }
    db.insert_attention_merge(work_item_merge(&product, "sibling dup", &sibling))
        .unwrap();

    assert_eq!(db.count_attention_merges_by_work_item(&chore).unwrap(), 2);
    assert_eq!(
        db.count_attention_merges_by_work_item(&sibling).unwrap(),
        1,
        "a sibling's folds must not leak into the count"
    );
}

/// The provenance affordance lists exactly the merges folded into the given
/// canonical attention, and returns the id `insert_attention_merge` handed
/// back for each.
#[test]
fn list_attention_merges_for_canonical_returns_only_that_canonicals_merges() {
    let (db, product, _chore) = chore_fixture("merge-list");
    let canonical = seed_attention(&db, &product, "canonical");
    let other = seed_attention(&db, &product, "other");

    let dup_one = seed_attention(&db, &product, "dup-one-src");
    let dup_two = seed_attention(&db, &product, "dup-two-src");
    let first = db
        .insert_attention_merge(attention_merge(&product, "dup one", &canonical, Some(&dup_one)))
        .unwrap();
    let second = db
        .insert_attention_merge(attention_merge(&product, "dup two", &canonical, Some(&dup_two)))
        .unwrap();
    db.insert_attention_merge(attention_merge(&product, "other dup", &other, None))
        .unwrap();

    let merges = db.list_attention_merges_for_canonical(&canonical).unwrap();

    // `created_at` is whole-second, so same-second inserts have no defined
    // order — assert on the set, not the sequence.
    let ids: Vec<&str> = merges.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(merges.len(), 2);
    assert!(ids.contains(&first.as_str()) && ids.contains(&second.as_str()));
    let summaries: Vec<&str> = merges.iter().map(|m| m.candidate_summary.as_str()).collect();
    assert!(
        !summaries.contains(&"other dup"),
        "another canonical's fold must not leak"
    );
    assert!(
        merges
            .iter()
            .all(|m| m.canonical_attention_id.as_deref() == Some(&canonical))
    );
    assert_eq!(
        db.list_attention_merges_for_canonical(&other).unwrap().len(),
        1,
        "the other canonical keeps its own fold"
    );
}

/// The pair-unique index is the sweep's idempotency backstop: re-folding the
/// same (canonical, duplicate) pair errors rather than silently double-counting
/// the score.
#[test]
fn insert_attention_merge_rejects_a_duplicate_canonical_duplicate_pair() {
    let (db, product, _chore) = chore_fixture("merge-pair-uq");
    let canonical = seed_attention(&db, &product, "canonical");
    let duplicate = seed_attention(&db, &product, "duplicate");

    let build = || attention_merge(&product, "dup", &canonical, Some(&duplicate));
    db.insert_attention_merge(build()).unwrap();

    assert!(
        db.insert_attention_merge(build()).is_err(),
        "re-folding the same pair must not double-count"
    );
    assert_eq!(db.list_attention_merges_for_canonical(&canonical).unwrap().len(), 1);
}
