use super::*;

/// Dispatch priority class for a `ready` execution, per the operator
/// directive: revisions before tasks/chores, ordered by revision kind
/// (merge-conflict fixes first, then CI fixes, then automated-PR-review
/// follow-ups, then any other revision, then everything else). Lower
/// discriminant dispatches first.
///
/// The full ready-queue sort key — documented once, here, as the single
/// source of truth — is:
///
/// ```text
/// (DispatchClass ASC, work_executions.priority DESC, created_at ASC, id ASC)
/// ```
///
/// [`WorkDb::list_ready_executions`] applies this key via an `ORDER BY
/// CASE …` clause built from the same `CREATED_VIA_*` prefixes this
/// classifier uses, and [`WorkDb::classify_work_item_for_dispatch`]
/// recomputes the class for dispatch-trace logging. Keep all three in
/// sync when a class changes.
///
/// # Scope: this key orders the queue, it does not allocate slots
///
/// The key above decides the order rows come off the ready queue. It does
/// NOT decide which pool's capacity a row may consume. The dispatcher
/// layers one rule on top of it that this key cannot express: automation
/// work ranks below ALL mainline work for an interactive slot, regardless
/// of dispatch class, priority, or arrival order. `drain_ready_queue`
/// enforces that structurally by walking this order twice — mainline and
/// review claim their pools in pass 1; only in pass 2 may automation that
/// missed its own pool spill into a Lower Decks slot. So an automation row
/// sorted first here still loses an interactive slot to a mainline row
/// sorted last. See `crate::dispatch_spillover` for the full priority
/// order (mainline > review > spilled automation) and the preemption rule.
///
/// Classification is keyed off the task's `kind` and `created_via`
/// provenance stamp — never off task name/title text. `created_via` is
/// the durable, machine-readable origin discriminator the engine already
/// stamps at creation time for every engine-triggered revision (see
/// `CREATED_VIA_MERGE_CONFLICT_PREFIX` / `CREATED_VIA_CI_FIX_PREFIX` /
/// `CREATED_VIA_PR_REVIEW_PREFIX` in `boss_protocol`); an operator-filed
/// or otherwise-provenanced revision falls through to `OtherRevision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DispatchClass {
    MergeConflictRevision = 1,
    CiFixRevision = 2,
    PrReviewRevision = 3,
    OtherRevision = 4,
    OtherWork = 5,
}

impl DispatchClass {
    /// `task_kind` / `created_via` are the raw `tasks.kind` /
    /// `tasks.created_via` column values. `task_kind = None` covers
    /// executions whose `work_item_id` doesn't name a `tasks` row at all
    /// (e.g. an `automation_triage` execution, which binds to an
    /// `automations.id`) — those classify as [`Self::OtherWork`], same as
    /// any non-revision task.
    pub(crate) fn classify(task_kind: Option<&str>, created_via: Option<&str>) -> Self {
        if task_kind != Some(TaskKind::Revision.as_str()) {
            return Self::OtherWork;
        }
        let created_via = created_via.unwrap_or_default();
        if created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
            Self::MergeConflictRevision
        } else if created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
            Self::CiFixRevision
        } else if created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX) {
            Self::PrReviewRevision
        } else {
            Self::OtherRevision
        }
    }

    pub(crate) fn as_ordinal(self) -> i64 {
        self as i64
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MergeConflictRevision => "merge_conflict_revision",
            Self::CiFixRevision => "ci_fix_revision",
            Self::PrReviewRevision => "pr_review_revision",
            Self::OtherRevision => "other_revision",
            Self::OtherWork => "other_work",
        }
    }
}

impl WorkDb {
    /// Look up the [`DispatchClass`] for `work_item_id`, for dispatch-trace
    /// logging at pickup time. Mirrors the classification
    /// [`WorkDb::list_ready_executions`] applies via SQL; a `work_item_id`
    /// with no matching `tasks` row (automation triage) classifies as
    /// [`DispatchClass::OtherWork`], matching that query's `LEFT JOIN`
    /// fallback.
    pub(crate) fn classify_work_item_for_dispatch(&self, work_item_id: &str) -> Result<DispatchClass> {
        let conn = self.connect()?;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT kind, created_via FROM tasks WHERE id = ?1",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((kind, created_via)) => DispatchClass::classify(Some(&kind), Some(&created_via)),
            None => DispatchClass::OtherWork,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a fresh per-test in-memory `WorkDb`. Mirrors the `open_db`
    /// convention used across `work/*.rs` test modules (e.g.
    /// `dispatch_helpers.rs`): each `WorkDb::open(":memory:")` allocates a
    /// unique shared-cache db.
    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    /// Create a product with the given repo default. Returns the product id.
    fn product_with_repo(db: &WorkDb, repo: Option<&str>) -> String {
        db.create_product(
            CreateProductInput::builder()
                .name("Boss")
                .maybe_repo_remote_url(repo.map(str::to_owned))
                .build(),
        )
        .unwrap()
        .id
    }

    #[test]
    fn classifies_revisions_by_created_via_prefix() {
        assert_eq!(
            DispatchClass::classify(Some("revision"), Some("merge-conflict:crz_1")),
            DispatchClass::MergeConflictRevision,
        );
        assert_eq!(
            DispatchClass::classify(Some("revision"), Some("ci-fix:cir_1")),
            DispatchClass::CiFixRevision,
        );
        assert_eq!(
            DispatchClass::classify(Some("revision"), Some("pr_review:exec_1")),
            DispatchClass::PrReviewRevision,
        );
    }

    #[test]
    fn operator_filed_and_unknown_revisions_are_other_revision() {
        for created_via in ["cli", "bossctl", "mac_app", "unknown", "engine_auto", "attention"] {
            assert_eq!(
                DispatchClass::classify(Some("revision"), Some(created_via)),
                DispatchClass::OtherRevision,
                "created_via={created_via} should classify as OtherRevision",
            );
        }
        assert_eq!(
            DispatchClass::classify(Some("revision"), None),
            DispatchClass::OtherRevision,
        );
    }

    #[test]
    fn non_revision_kinds_are_other_work_regardless_of_created_via() {
        for kind in ["chore", "task", "followup", "design", "investigation", "project_task"] {
            assert_eq!(
                DispatchClass::classify(Some(kind), Some("pr_review:exec_1")),
                DispatchClass::OtherWork,
                "kind={kind} should never inherit a revision class from created_via alone",
            );
        }
    }

    #[test]
    fn missing_task_row_is_other_work() {
        assert_eq!(DispatchClass::classify(None, None), DispatchClass::OtherWork);
    }

    #[test]
    fn ordering_matches_operator_directive() {
        let mut classes = [
            DispatchClass::OtherWork,
            DispatchClass::OtherRevision,
            DispatchClass::PrReviewRevision,
            DispatchClass::CiFixRevision,
            DispatchClass::MergeConflictRevision,
        ];
        classes.sort();
        assert_eq!(
            classes,
            [
                DispatchClass::MergeConflictRevision,
                DispatchClass::CiFixRevision,
                DispatchClass::PrReviewRevision,
                DispatchClass::OtherRevision,
                DispatchClass::OtherWork,
            ],
        );
    }

    // ── list_ready_executions dispatch ordering (integration) ──────────────

    /// Raw-insert a task row with full control over `kind` and
    /// `created_via`, bypassing the create-time revision/chain-root
    /// invariants (mirrors `insert_raw_task` in `work/tests/t03.rs`) so
    /// these tests can exercise `list_ready_executions`'s ordering in
    /// isolation, without standing up a whole revision chain + open PR.
    fn insert_raw_task_with_created_via(conn: &Connection, product_id: &str, kind: &str, created_via: &str) -> String {
        let id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
             VALUES (?1, ?2, NULL, ?3, 'Raw', '', 'todo', NULL, NULL, NULL, ?4, ?4, 1, 'medium', ?5)",
            params![id, product_id, kind, now, created_via],
        )
        .unwrap();
        id
    }

    fn ready_execution_for(db: &WorkDb, work_item_id: &str, kind: ExecutionKind) -> String {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap()
        .id
    }

    /// Acceptance test (chore spec §7): eligible rows spanning all five
    /// dispatch classes, competing for a single free slot, must dispatch
    /// in exactly class order — even though they're inserted here in the
    /// REVERSE of that order, so a plain FIFO-by-creation-time queue (the
    /// pre-existing behaviour) would get every pairing backwards.
    #[test]
    fn list_ready_executions_orders_by_class_across_all_five_kinds() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

        // Inserted oldest-first in class-5..1 order (worst class first).
        let other_work = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "chore", "cli");
        let other_revision = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "bossctl");
        let pr_review_revision =
            insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "pr_review:exec_findings");
        let ci_fix_revision =
            insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "ci-fix:cir_1");
        let merge_conflict_revision =
            insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "merge-conflict:crz_1");

        let other_work_exec = ready_execution_for(&db, &other_work, ExecutionKind::ChoreImplementation);
        let other_revision_exec = ready_execution_for(&db, &other_revision, ExecutionKind::RevisionImplementation);
        let pr_review_exec = ready_execution_for(&db, &pr_review_revision, ExecutionKind::RevisionImplementation);
        let ci_fix_exec = ready_execution_for(&db, &ci_fix_revision, ExecutionKind::RevisionImplementation);
        let merge_conflict_exec =
            ready_execution_for(&db, &merge_conflict_revision, ExecutionKind::RevisionImplementation);

        let ready_ids: Vec<String> = db.list_ready_executions().unwrap().into_iter().map(|e| e.id).collect();

        assert_eq!(
            ready_ids,
            vec![
                merge_conflict_exec,
                ci_fix_exec,
                pr_review_exec,
                other_revision_exec,
                other_work_exec,
            ],
            "dispatch order must be class 1..5, reversing the creation order these rows were inserted in",
        );
    }

    /// Within a single class, ties still break FIFO by creation time (then
    /// `id`) — the composite key's second/third/fourth components must
    /// survive the new class-ASC primary key unchanged.
    #[test]
    fn list_ready_executions_is_fifo_within_a_class() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

        let first_task =
            insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "merge-conflict:crz_1");
        let second_task =
            insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "revision", "merge-conflict:crz_2");

        let first_exec = ready_execution_for(&db, &first_task, ExecutionKind::RevisionImplementation);
        let second_exec = ready_execution_for(&db, &second_task, ExecutionKind::RevisionImplementation);

        let ready_ids: Vec<String> = db.list_ready_executions().unwrap().into_iter().map(|e| e.id).collect();

        assert_eq!(
            ready_ids,
            vec![first_exec, second_exec],
            "same-class rows must stay FIFO by creation order",
        );
    }

    /// Classification must key off durable provenance (`created_via`
    /// prefix + task kind), never off the task's human-editable name —
    /// two merge-conflict revisions named to sort in the "wrong" order
    /// alphabetically must still land by class/FIFO, not by name.
    #[test]
    fn list_ready_executions_ignores_task_name_text() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
        let conn = db.connect().unwrap();

        // "AAA" would sort first alphabetically; it's the OLDER row here
        // and must still dispatch after "ZZZ" is never inserted — this
        // just confirms name text plays no role by using a name that
        // would flip the outcome if the classifier ever read it.
        let chore_id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
             VALUES (?1, ?2, NULL, 'chore', 'AAA sorts first alphabetically', '', 'todo', NULL, NULL, NULL, ?3, ?3, 1, 'medium', 'cli')",
            params![chore_id, product, now],
        )
        .unwrap();
        let revision_id = insert_raw_task_with_created_via(&conn, &product, "revision", "merge-conflict:crz_1");
        drop(conn);

        let chore_exec = ready_execution_for(&db, &chore_id, ExecutionKind::ChoreImplementation);
        let revision_exec = ready_execution_for(&db, &revision_id, ExecutionKind::RevisionImplementation);

        let ready_ids: Vec<String> = db.list_ready_executions().unwrap().into_iter().map(|e| e.id).collect();

        assert_eq!(
            ready_ids,
            vec![revision_exec, chore_exec],
            "the merge-conflict revision must win despite its name sorting after the chore's",
        );
    }

    // ── merge_order dispatch stagger ────────────────────────────────────────

    /// Wire a canonical `merge_order` edge `later` (dependent) → `first`
    /// (prerequisite).
    fn wire_merge_order(db: &WorkDb, later: &str, first: &str) {
        let conn = db.connect().unwrap();
        deps::insert_edge(&conn, later, first, deps::RELATION_MERGE_ORDER, &now_string()).unwrap();
    }

    fn set_status(db: &WorkDb, task_id: &str, status: &str) {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![task_id, status])
            .unwrap();
    }

    fn dispatch_not_before(db: &WorkDb, execution_id: &str) -> Option<String> {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT dispatch_not_before FROM work_executions WHERE id = ?1",
                params![execution_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
    }

    #[test]
    fn stagger_defers_later_side_once_when_first_side_in_flight() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
        let first = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        let later = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        wire_merge_order(&db, &later, &first);
        let exec = ready_execution_for(&db, &later, ExecutionKind::TaskImplementation);

        // First side (`first`) is `todo` — in flight — so the later side is
        // staggered exactly once.
        let stamped = db.maybe_stagger_merge_order_dispatch(&exec, &later, 60).unwrap();
        assert!(
            stamped.is_some(),
            "later side must be staggered when first is in flight"
        );
        assert!(
            dispatch_not_before(&db, &exec).is_some(),
            "dispatch_not_before must be stamped on the deferred execution",
        );

        // One-shot: a second pass must NOT re-defer.
        assert!(
            db.maybe_stagger_merge_order_dispatch(&exec, &later, 60)
                .unwrap()
                .is_none(),
            "stagger is one-shot — never re-delays an already-deferred execution",
        );
    }

    #[test]
    fn stagger_never_delays_the_first_side() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
        let first = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        let later = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        wire_merge_order(&db, &later, &first);
        let first_exec = ready_execution_for(&db, &first, ExecutionKind::TaskImplementation);

        // Only the canonical "later" (dependent) side is ever staggered.
        assert!(
            db.maybe_stagger_merge_order_dispatch(&first_exec, &first, 60)
                .unwrap()
                .is_none(),
            "the first side of a merge_order pair is never delayed",
        );
        assert!(dispatch_not_before(&db, &first_exec).is_none());
    }

    #[test]
    fn stagger_is_a_noop_when_disabled_or_first_side_done() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
        let first = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        let later = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        wire_merge_order(&db, &later, &first);
        let exec = ready_execution_for(&db, &later, ExecutionKind::TaskImplementation);

        // secs = 0 → disabled, no stagger.
        assert!(
            db.maybe_stagger_merge_order_dispatch(&exec, &later, 0)
                .unwrap()
                .is_none()
        );
        assert!(dispatch_not_before(&db, &exec).is_none());

        // First side already merged (`done`) → no concurrency to break up.
        set_status(&db, &first, "done");
        assert!(
            db.maybe_stagger_merge_order_dispatch(&exec, &later, 60)
                .unwrap()
                .is_none(),
            "no stagger once the first side is done",
        );
        assert!(dispatch_not_before(&db, &exec).is_none());
    }

    #[test]
    fn stagger_is_a_noop_without_a_merge_order_edge() {
        let db = open_db();
        let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
        let solo = insert_raw_task_with_created_via(&db.connect().unwrap(), &product, "task", "cli");
        let exec = ready_execution_for(&db, &solo, ExecutionKind::TaskImplementation);
        assert!(
            db.maybe_stagger_merge_order_dispatch(&exec, &solo, 60)
                .unwrap()
                .is_none(),
            "a task with no merge_order edge is never staggered",
        );
    }
}
