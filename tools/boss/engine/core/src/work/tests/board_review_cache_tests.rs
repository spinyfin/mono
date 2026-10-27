//! The board review cache must be indistinguishable from recomputing every
//! owner's review projection: each test mutates a review input, then asserts
//! the cached assembly equals an uncached assembly of the same database and
//! that exactly the touched family was recomputed.
use super::*;
use crate::population_timing::{PopulationTrace, segment};
use crate::work::board_caches::BoardCaches;
use std::time::Instant;

struct Board {
    db: WorkDb,
    product_id: String,
    caches: BoardCaches,
}

/// What one cached assembly cost: owners recomputed, and the statements the
/// review segment ran.
#[derive(Debug, PartialEq, Eq)]
struct Pass {
    misses: u64,
    queries: i64,
}

impl Board {
    fn new(label: &str) -> Self {
        let db = WorkDb::open(temp_db_path(label)).unwrap();
        let product_id = make_revision_product(&db, label);
        Self {
            db,
            product_id,
            caches: BoardCaches::default(),
        }
    }

    fn in_review_chore(&self, pr_number: u32) -> String {
        let chore = create_test_chore_manual(&self.db, self.product_id.clone(), format!("Chore {pr_number}"));
        self.sql(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            &[&chore.id, &format!("https://github.com/spinyfin/mono/pull/{pr_number}")],
        );
        chore.id
    }

    fn revision(&self, root: &str) -> String {
        insert_revision_row(&self.db, &self.product_id, root)
    }

    fn sql(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        self.db.connect().unwrap().execute(sql, params).unwrap();
    }

    fn observe(&self, root: &str, sha: &str, ci: &str, mergeable: &str) {
        self.sql(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = ?3, pr_mergeable_state = ?4 WHERE id = ?1",
            &[&root, &sha, &ci, &mergeable],
        );
    }

    fn execution(&self, work_item_id: &str, kind: ExecutionKind) -> String {
        self.db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(kind)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
            .id
    }

    fn verdict(&self, owner: &str, sha: &str, outcome: &'static str) -> String {
        let execution = self.execution(owner, ExecutionKind::PrReview);
        self.sql(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            &[&execution],
        );
        WorkDb::insert_review_verdict_in_tx(
            &self.db.connect().unwrap(),
            &execution,
            owner,
            &crate::work::ReviewVerdictInput {
                head_sha: Some(sha.to_owned()),
                findings_count: if outcome == "completed_clean" { 0 } else { 2 },
                revision_warranted: outcome == "completed_with_findings",
                gate_outcome: outcome,
            },
        )
        .unwrap();
        execution
    }

    fn epoch(&self, work_item_id: &str) -> i64 {
        self.db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COALESCE((SELECT epoch FROM board_review_epoch WHERE work_item_id = ?1), 0)",
                [work_item_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Assemble through the caches and without them, assert every card is
    /// identical, and return what the cached review pass cost.
    fn assert_equivalent(&self) -> Pass {
        let misses_before = self.caches.reviews.stats().misses;
        let mut trace = PopulationTrace::new(self.product_id.clone(), "review-cache", None, Instant::now());
        let cached = self
            .db
            .get_work_tree_cached(&self.product_id, &mut trace, &self.caches)
            .unwrap();
        let uncached = self.db.get_work_tree(&self.product_id).unwrap();
        let cards = |tree: &boss_protocol::WorkTree| serde_json::to_value((&tree.tasks, &tree.chores)).unwrap();
        assert_eq!(
            cards(&cached),
            cards(&uncached),
            "cached board diverged from the uncached board"
        );
        let (_, _, queries) = trace.segment_counts(segment::DB_AI_REVIEW_STATE).unwrap();
        Pass {
            misses: self.caches.reviews.stats().misses - misses_before,
            queries: queries.unwrap(),
        }
    }

    fn card(&self, id: &str) -> Task {
        let tree = self.db.get_work_tree(&self.product_id).unwrap();
        tree.tasks
            .into_iter()
            .chain(tree.chores)
            .find(|task| task.id == id)
            .unwrap()
    }
}

const PAYLOAD: &str = r#"{"batch_id":"batch-1","verdict":{"batch_id":"batch-1","pr_url":"https://github.com/spinyfin/mono/pull/1","target_sha":"h2","phase":"pre_merge","summary":"One high-severity defect.","revision_warranted":true,"findings":[{"severity":"high","category":"correctness","confidence":"high","file":"src/lib.rs","title":"Unchecked index","detail":"Out of bounds read.","sources":["claude"]}],"contradictions":[]}}"#;

/// Every input of the owner projection, written one at a time: each write
/// recomputes exactly the family it belongs to, a non-input write recomputes
/// nothing, and the cached board never differs from the uncached one.
#[test]
fn cached_board_matches_uncached_through_every_review_input() {
    let board = Board::new("review-cache-inputs");
    let root = board.in_review_chore(1);
    let revision = board.revision(&root);
    let other = board.in_review_chore(2);
    board.observe(&root, "h1", "success", "mergeable");
    board.observe(&other, "o1", "success", "mergeable");
    board.verdict(&root, "h1", "completed_clean");
    board.verdict(&other, "o1", "completed_clean");

    // Cold: both owners computed; the warm pass reads only the epochs plus
    // the live queued-review probe.
    assert_eq!(board.assert_equivalent().misses, 2);
    assert_eq!(board.assert_equivalent(), Pass { misses: 0, queries: 2 });
    assert_eq!(
        board.card(&root).ai_review_state.as_deref(),
        Some("reviewed_clean_pending")
    );

    // Non-inputs: poll bookkeeping on the root and a run under its revision.
    board.sql(
        "UPDATE tasks SET pr_state_polled_at = '1', updated_at = '2', pr_updated_at = '3' WHERE id = ?1",
        &[&root],
    );
    let execution = board.execution(&revision, ExecutionKind::RevisionImplementation);
    board
        .db
        .create_run(CreateRunInput {
            execution_id: execution.clone(),
            agent_id: "agent".to_owned(),
            status: Some("active".to_owned()),
            error_text: None,
            result_summary: None,
            transcript_path: None,
            artifacts_path: None,
            started_at: Some("100".to_owned()),
            finished_at: None,
        })
        .unwrap();
    // The execution insert itself is an input (its completion can deliver a
    // findings revision), so it was recomputed once; the run was not.
    assert_eq!(board.assert_equivalent(), Pass { misses: 1, queries: 4 });
    assert_eq!(board.assert_equivalent(), Pass { misses: 0, queries: 2 });

    // The observed head moved: the clean verdict no longer matches.
    board.observe(&root, "h2", "success", "mergeable");
    assert_eq!(board.assert_equivalent().misses, 1);
    assert_eq!(board.card(&root).ai_review_state.as_deref(), Some("not_reviewed"));

    // A findings verdict on the new head, then linked to its revision.
    let findings = board.verdict(&root, "h2", "completed_with_findings");
    assert_eq!(board.assert_equivalent().misses, 1);
    board
        .db
        .set_review_verdict_revision_task_id(&findings, &revision)
        .unwrap();
    assert_eq!(board.assert_equivalent().misses, 1);
    let card = board.card(&root);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewed_with_findings"));
    assert_eq!(card.ai_review_findings_revision_id.as_deref(), Some(revision.as_str()));

    // The applied supervisor proposal supplies the findings markdown.
    board.sql(
        "INSERT INTO worker_proposals (id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at)
         VALUES ('proposal-1', ?1, ?2, 'review_verdict', ?3, 'key-1', 'proposed', '1')",
        &[&findings, &root, &PAYLOAD],
    );
    board.sql(
        "UPDATE pr_review_verdicts SET proposal_id = 'proposal-1' WHERE execution_id = ?1",
        &[&findings],
    );
    assert_eq!(board.assert_equivalent().misses, 1);
    // Not applied yet: the legacy fallback shows the revision's own brief.
    let markdown = board.card(&root).ai_review_badge.unwrap().findings_markdown;
    assert!(!markdown.unwrap_or_default().contains("Unchecked index"));
    board.sql(
        "UPDATE worker_proposals SET state = 'applied' WHERE id = 'proposal-1'",
        &[],
    );
    assert_eq!(board.assert_equivalent().misses, 1);
    let markdown = board.card(&root).ai_review_badge.unwrap().findings_markdown.unwrap();
    assert!(markdown.contains("Unchecked index"));

    // The revision's own columns feed the tooltip and pending-work rules.
    board.sql(
        "UPDATE tasks SET name = 'Fix the index', status = 'active' WHERE id = ?1",
        &[&revision],
    );
    assert_eq!(board.assert_equivalent().misses, 1);
    assert!(
        board
            .card(&root)
            .ai_review_badge
            .unwrap()
            .tooltip
            .contains("Findings revision: Fix the index (Doing).")
    );

    // Delivering the revision without moving the head resolves the findings.
    board.sql(
        "UPDATE work_executions SET status = 'completed', finished_at = '5', pr_head_after = 'h2' WHERE id = ?1",
        &[&execution],
    );
    board.sql("UPDATE tasks SET status = 'in_review' WHERE id = ?1", &[&revision]);
    assert_eq!(board.assert_equivalent().misses, 1);
    let badge = board.card(&root).ai_review_badge.unwrap();
    assert_eq!(badge.label, "Findings addressed");

    // Removing the revision from the board changes the family.
    board.sql("UPDATE tasks SET deleted_at = '9' WHERE id = ?1", &[&revision]);
    assert_eq!(board.assert_equivalent().misses, 1);
    assert_eq!(
        board.card(&root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );

    // A brand-new revision joins the family through the insert trigger.
    let newer = board.revision(&root);
    assert_eq!(board.assert_equivalent().misses, 1);
    board.sql("UPDATE tasks SET status = 'blocked' WHERE id = ?1", &[&newer]);
    assert_eq!(board.assert_equivalent().misses, 1);

    // Closing the root keeps only a verdict that matches its last head.
    board.sql("UPDATE tasks SET status = 'done' WHERE id = ?1", &[&root]);
    assert_eq!(board.assert_equivalent().misses, 1);
    assert_eq!(
        board.card(&root).ai_review_state.as_deref(),
        Some("reviewed_with_findings")
    );
    assert!(board.card(&root).ai_review_badge.is_none());

    // Throughout, the untouched owner (clean, green, no revisions) never
    // left the cache.
    assert_eq!(
        board.card(&other).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );
    assert_eq!(board.assert_equivalent(), Pass { misses: 0, queries: 2 });
}

/// Review liveness is layered per pass, never cached: a queued or running
/// review changes the state without any epoch moving.
#[test]
fn live_review_executions_change_the_state_without_a_recompute() {
    let board = Board::new("review-cache-live");
    let root = board.in_review_chore(3);
    board.observe(&root, "h1", "success", "mergeable");
    board.verdict(&root, "h1", "completed_clean");
    assert_eq!(board.assert_equivalent().misses, 1);
    let review = board.execution(&root, ExecutionKind::PrReview);
    // The insert bumps the owner (an execution is an input), so one miss;
    // the state itself comes from the live probe.
    assert_eq!(board.assert_equivalent().misses, 1);
    assert_eq!(board.card(&root).ai_review_state.as_deref(), Some("review_queued"));
    board.sql(
        "UPDATE work_executions SET status = 'running' WHERE id = ?1",
        &[&review],
    );
    assert_eq!(board.assert_equivalent().misses, 1);
    assert_eq!(board.card(&root).ai_review_state.as_deref(), Some("reviewing"));
    // A run under the reviewer moves nothing the projection reads.
    board
        .db
        .create_run(CreateRunInput {
            execution_id: review.clone(),
            agent_id: "reviewer".to_owned(),
            status: Some("active".to_owned()),
            error_text: None,
            result_summary: None,
            transcript_path: None,
            artifacts_path: None,
            started_at: Some("100".to_owned()),
            finished_at: None,
        })
        .unwrap();
    assert_eq!(board.assert_equivalent(), Pass { misses: 0, queries: 2 });
}

/// A chain whose root is off the board (archived) cannot be keyed in memory;
/// it takes the card-keyed SQL walk and still matches the uncached board.
#[test]
fn chain_leaving_the_board_falls_back_to_the_sql_walk() {
    let board = Board::new("review-cache-fallback");
    let root = board.in_review_chore(4);
    let revision = board.revision(&root);
    board.observe(&root, "h1", "success", "mergeable");
    board.verdict(&root, "h1", "completed_clean");
    board.sql("UPDATE tasks SET status = 'archived' WHERE id = ?1", &[&root]);
    board.sql("UPDATE tasks SET status = 'active' WHERE id = ?1", &[&revision]);
    // No owner on the board: skip the epoch read. Only the card-keyed
    // state and history queries and the live probe run.
    assert_eq!(board.assert_equivalent(), Pass { misses: 0, queries: 3 });
    let card = board.card(&revision);
    assert_eq!(card.kind, TaskKind::Revision);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewed_clean_pending"));
}

/// The schema 53 triggers: every projection input bumps the written item and
/// its parent inside the write, and non-input columns bump nothing.
#[test]
fn review_epoch_triggers_cover_every_projection_input() {
    let board = Board::new("review-epoch-triggers");
    let root = board.in_review_chore(5);
    let root_after_create = board.epoch(&root);
    assert!(root_after_create >= 1, "task insert and in-review update bump the row");

    let revision = board.revision(&root);
    assert_eq!(board.epoch(&revision), 1, "a new revision starts at one");
    assert_eq!(
        board.epoch(&root),
        root_after_create + 1,
        "a new revision bumps its root"
    );

    let before = (board.epoch(&root), board.epoch(&revision));
    board.sql(
        "UPDATE tasks SET updated_at = 'x', pr_state_polled_at = 'y', ordinal = 7 WHERE id = ?1",
        &[&revision],
    );
    assert_eq!(
        (board.epoch(&root), board.epoch(&revision)),
        before,
        "non-input columns are silent"
    );

    for column in [
        "status",
        "pr_head_sha",
        "ci_required_state",
        "pr_mergeable_state",
        "last_reviewed_sha",
        "name",
        "description",
        "deleted_at",
        "pr_url",
    ] {
        let before = (board.epoch(&root), board.epoch(&revision));
        board.sql(
            &format!("UPDATE tasks SET {column} = {column} WHERE id = ?1"),
            &[&revision],
        );
        assert_eq!(
            (board.epoch(&root), board.epoch(&revision)),
            (before.0 + 1, before.1 + 1),
            "{column} is an input"
        );
    }

    let before = (board.epoch(&root), board.epoch(&revision));
    board.verdict(&revision, "h1", "completed_clean");
    // Execution insert, execution status update and verdict insert: three
    // bumps each on the revision and, through its parent, the root.
    assert_eq!(
        (board.epoch(&root), board.epoch(&revision)),
        (before.0 + 3, before.1 + 3)
    );

    let before = (board.epoch(&root), board.epoch(&revision));
    board.sql(
        "INSERT INTO worker_proposals (id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at)
         SELECT 'proposal-5', id, ?1, 'review_verdict', '{}', 'key-5', 'proposed', '1' FROM work_executions WHERE work_item_id = ?1",
        &[&revision],
    );
    board.sql(
        "UPDATE worker_proposals SET state = 'applied' WHERE id = 'proposal-5'",
        &[],
    );
    board.sql(
        "UPDATE worker_proposals SET decided_by = 'engine' WHERE id = 'proposal-5'",
        &[],
    );
    assert_eq!(
        (board.epoch(&root), board.epoch(&revision)),
        (before.0 + 2, before.1 + 2)
    );

    let before = (board.epoch(&root), board.epoch(&revision));
    board.sql("DELETE FROM worker_proposals WHERE id = 'proposal-5'", &[]);
    board.sql("DELETE FROM pr_review_verdicts WHERE work_item_id = ?1", &[&revision]);
    board.sql("DELETE FROM work_executions WHERE work_item_id = ?1", &[&revision]);
    assert_eq!(
        (board.epoch(&root), board.epoch(&revision)),
        (before.0 + 3, before.1 + 3)
    );

    let before = board.epoch(&root);
    board.sql("DELETE FROM tasks WHERE id = ?1", &[&revision]);
    assert_eq!(board.epoch(&root), before + 1, "losing a member bumps the root");
}

#[test]
fn deleting_revision_cannot_alias_a_cached_family_key() {
    for member_epoch in [1, 4] {
        let board = Board::new(&format!("review-cache-delete-{member_epoch}"));
        let root = board.in_review_chore(20);
        board.observe(&root, "h1", "success", "mergeable");
        board.verdict(&root, "h1", "completed_clean");
        let revision = board.revision(&root);
        for _ in 1..member_epoch {
            board.sql("UPDATE tasks SET name = name WHERE id = ?1", &[&revision]);
        }
        assert_eq!(board.epoch(&revision), member_epoch);
        assert_eq!(board.assert_equivalent().misses, 1);
        assert_eq!(board.assert_equivalent().misses, 0);
        assert_eq!(
            board.card(&root).ai_review_state.as_deref(),
            Some("reviewed_clean_pending")
        );
        board.sql("UPDATE tasks SET deleted_at = '9' WHERE id = ?1", &[&revision]);
        for _ in 1..member_epoch {
            board.sql("UPDATE tasks SET name = name WHERE id = ?1", &[&root]);
        }
        assert_eq!(board.assert_equivalent().misses, 1);
        assert_eq!(board.card(&root).ai_review_state.as_deref(), Some("reviewed_all_clear"));
    }
}

#[test]
fn nested_off_board_revision_inputs_invalidate_both_owners_on_reparent() {
    for column in ["status = 'archived'", "deleted_at = '9'"] {
        let board = Board::new("review-cache-off-board");
        let root = board.in_review_chore(21);
        let other = board.in_review_chore(22);
        board.observe(&root, "h1", "success", "mergeable");
        board.observe(&other, "h1", "success", "mergeable");
        let intermediate = board.revision(&root);
        let nested = board.revision(&intermediate);
        board.sql("UPDATE tasks SET status = 'in_review' WHERE id = ?1", &[&nested]);
        board.sql(&format!("UPDATE tasks SET {column} WHERE id = ?1"), &[&intermediate]);
        assert_eq!(board.assert_equivalent().misses, 2);
        assert_eq!(board.assert_equivalent().misses, 0);
        board.verdict(&nested, "h1", "completed_clean");
        assert_eq!(board.assert_equivalent().misses, 1);
        board.sql(
            "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1",
            &[&intermediate, &other],
        );
        assert_eq!(board.assert_equivalent().misses, 2);
    }
}

#[test]
fn single_item_trace_counts_review_cards_and_queries() {
    let board = Board::new("review-single-trace");
    let root = board.in_review_chore(23);
    board.observe(&root, "h1", "success", "mergeable");
    board.verdict(&root, "h1", "completed_clean");
    let mut card = board.card(&root);
    let mut trace = PopulationTrace::new(&board.product_id, "single-test", None, Instant::now());
    crate::work::revision_helpers::attach_task_derived_projections_traced(
        &board.db.connect().unwrap(),
        &mut card,
        &mut trace,
    )
    .unwrap();
    let (duration, cards, queries) = trace.segment_counts("single_item.ai_review_state").unwrap();
    assert!(duration >= 0.0);
    assert_eq!(cards, Some(1));
    assert_eq!(queries, Some(3));
}
