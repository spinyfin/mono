//! Boothby schema + pre/post-image journalling.
//!
//! Three invariants carry most of the weight here:
//!
//!   * a mutation by any actor other than Boothby writes no
//!     `boothby_actions` row and is otherwise byte-identical to what it
//!     did before this feature existed;
//!   * a Boothby mutation journals the touched columns — and only the
//!     touched columns — inside the same transaction as the write; and
//!   * a Boothby mutation that *cannot* be journalled is refused outright.
//!
//! Design: `tools/boss/docs/designs/boothby.md` §"Audit & undo data model".

use super::*;

/// One `boothby_actions` row, as the assertions below want to read it.
struct ActionRow {
    verb: String,
    target_kind: String,
    target_id: String,
    seq: i64,
    images: ActionImages,
}

/// A journalled action's `pre_image` / `post_image`, parsed. One field on
/// [`ActionRow`] rather than two: they are always read together.
struct ActionImages {
    pre: Option<serde_json::Value>,
    post: Option<serde_json::Value>,
}

impl ActionRow {
    /// Both images, for the reversible actions that carry a `pre_image`.
    fn images(&self) -> (&serde_json::Value, &serde_json::Value) {
        (
            self.images.pre.as_ref().expect("reversible action has a pre_image"),
            self.images.post.as_ref().expect("every action has a post_image"),
        )
    }
}

fn actions(db: &WorkDb) -> Vec<ActionRow> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT verb, target_kind, target_id, seq, pre_image, post_image
             FROM boothby_actions ORDER BY seq ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            let pre: Option<String> = row.get(4)?;
            let post: Option<String> = row.get(5)?;
            Ok(ActionRow {
                verb: row.get(0)?,
                target_kind: row.get(1)?,
                target_id: row.get(2)?,
                seq: row.get(3)?,
                images: ActionImages {
                    pre: pre.map(|s| serde_json::from_str(&s).unwrap()),
                    post: post.map(|s| serde_json::from_str(&s).unwrap()),
                },
            })
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// The single action journalled by a test that expects exactly one.
fn only_action(db: &WorkDb) -> ActionRow {
    let mut rows = actions(db);
    assert_eq!(rows.len(), 1, "expected exactly one journalled action");
    rows.pop().unwrap()
}

/// Open a pass so journalled actions have something to attribute to.
fn open_pass(db: &WorkDb, id: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO boothby_passes (id, trigger, started_at) VALUES (?1, 'schedule', ?2)",
        rusqlite::params![id, now_string()],
    )
    .unwrap();
}

/// The context an executor would arm for a reversible taxonomy verb.
fn ctx(verb: &str) -> BoothbyActionContext {
    BoothbyActionContext::builder()
        .verb(verb)
        .rationale("no activity in 90 days and no PR")
        .reversibility(boss_protocol::BOOTHBY_REVERSIBILITY_REVERSIBLE)
        .build()
}

fn status_patch(status: &str) -> WorkItemPatch {
    WorkItemPatch {
        status: Some(status.to_owned()),
        ..Default::default()
    }
}

// ── schema ────────────────────────────────────────────────────────────

#[test]
fn migration_creates_all_four_boothby_tables() {
    let db = WorkDb::open(temp_db_path("boothby-schema")).unwrap();
    let conn = db.connect().unwrap();
    for table in [
        "boothby_passes",
        "boothby_actions",
        "boothby_findings",
        "boothby_cursors",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{table} was not created");
    }
}

/// `WorkDb::open` runs every migration on each open, so re-opening the
/// same database must not trip over its own DDL.
#[test]
fn boothby_migration_is_idempotent_across_reopen() {
    let (_dir, path) = disk_db_path("boothby-idempotent");
    let db = WorkDb::open(path.clone()).unwrap();
    open_pass(&db, "bp_survives");
    drop(db);

    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM boothby_passes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "re-open must preserve rows, not recreate the table");
}

/// The single-open-pass index is what makes "resolve the open pass
/// in-transaction" well-defined, so it has to actually bite.
#[test]
fn at_most_one_pass_can_be_open() {
    let db = WorkDb::open(temp_db_path("boothby-one-pass")).unwrap();
    open_pass(&db, "bp_1");

    let conn = db.connect().unwrap();
    assert!(
        conn.execute(
            "INSERT INTO boothby_passes (id, trigger, started_at) VALUES ('bp_2', 'manual', '1')",
            [],
        )
        .is_err(),
        "a second open pass must be refused",
    );

    // Finishing the first frees the slot.
    conn.execute(
        "UPDATE boothby_passes SET outcome = 'completed', finished_at = '2' WHERE id = 'bp_1'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO boothby_passes (id, trigger, started_at) VALUES ('bp_2', 'manual', '3')",
        [],
    )
    .unwrap();
}

/// A pass is finished exactly when it has an outcome.
#[test]
fn pass_outcome_and_finished_at_must_agree() {
    let db = WorkDb::open(temp_db_path("boothby-pass-check")).unwrap();
    let conn = db.connect().unwrap();

    assert!(
        conn.execute(
            "INSERT INTO boothby_passes (id, trigger, started_at, finished_at)
             VALUES ('bp_bad', 'schedule', '1', '2')",
            [],
        )
        .is_err(),
        "a finished pass must carry an outcome",
    );
    assert!(
        conn.execute(
            "INSERT INTO boothby_passes (id, trigger, started_at, outcome)
             VALUES ('bp_bad2', 'schedule', '1', 'completed')",
            [],
        )
        .is_err(),
        "a pass with an outcome must carry finished_at",
    );
}

/// The design defines `trigger` as `'schedule' | 'event:<name>' |
/// 'manual'` with an open-ended event name, so the column must not be
/// CHECK-constrained to a closed set.
#[test]
fn pass_trigger_accepts_open_ended_event_names() {
    let db = WorkDb::open(temp_db_path("boothby-trigger")).unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO boothby_passes (id, trigger, started_at) VALUES ('bp_1', 'event:pr_merged', '1')",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE boothby_passes SET outcome = 'completed', finished_at = '2' WHERE id = 'bp_1'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO boothby_passes (id, trigger, started_at)
         VALUES ('bp_2', 'event:some_stage_nobody_has_written_yet', '3')",
        [],
    )
    .unwrap();
}

#[test]
fn finding_fingerprint_is_unique_so_recurrences_dedup() {
    let db = WorkDb::open(temp_db_path("boothby-finding-dedup")).unwrap();
    let conn = db.connect().unwrap();
    for id in ["bf_1", "bf_2"] {
        let inserted = conn.execute(
            "INSERT INTO boothby_findings (id, fingerprint, kind, subject, first_seen, last_seen, status)
             VALUES (?1, 'same-fingerprint', 'error', '{}', '1', '1', 'open')",
            [id],
        );
        if id == "bf_2" {
            assert!(inserted.is_err(), "a repeat fingerprint must not insert a second row");
        } else {
            inserted.unwrap();
        }
    }
}

/// `(pass_id, seq)` is the journal's read order and must be unique.
#[test]
fn action_seq_is_unique_within_a_pass() {
    let db = WorkDb::open(temp_db_path("boothby-seq")).unwrap();
    open_pass(&db, "bp_1");
    let conn = db.connect().unwrap();
    let insert = |id: &str| {
        conn.execute(
            "INSERT INTO boothby_actions
                (id, pass_id, seq, verb, target_kind, target_id, rationale, reversibility, created_at)
             VALUES (?1, 'bp_1', 1, 'v', 'task', 'task_1', 'why', 'reversible', '1')",
            [id],
        )
    };
    insert("ba_1").unwrap();
    assert!(insert("ba_2").is_err(), "seq must be unique within a pass");
}

// ── journalling is inert for non-Boothby actors ───────────────────────

#[test]
fn human_task_update_journals_nothing() {
    let db = WorkDb::open(temp_db_path("boothby-human-task")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    db.update_work_item(&chore.id, status_patch("archived")).unwrap();

    assert!(actions(&db).is_empty(), "a human update must not be journalled");
}

#[test]
fn engine_and_boss_task_updates_journal_nothing() {
    let db = WorkDb::open(temp_db_path("boothby-other-actors")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    open_pass(&db, "bp_1");

    for (actor, status) in [
        (boss_protocol::LAST_STATUS_ACTOR_ENGINE, "in_review"),
        (boss_protocol::LAST_STATUS_ACTOR_BOSS, "archived"),
    ] {
        let chore = create_test_chore_manual(&db, &product.id, format!("chore for {actor}"));
        db.update_work_item_as_actor(&chore.id, status_patch(status), actor)
            .unwrap();
    }

    assert!(actions(&db).is_empty(), "only Boothby writes boothby_actions");
}

/// An armed context must not leak onto another actor's write.
#[test]
fn an_armed_context_does_not_journal_a_human_update() {
    let db = WorkDb::open(temp_db_path("boothby-armed-human")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    db.update_work_item(&chore.id, status_patch("archived")).unwrap();

    assert!(actions(&db).is_empty(), "the actor gate, not the context, decides");
}

/// The delete/restore wrappers keep their pre-Boothby behaviour: they
/// route through the actor-aware body as `human` and journal nothing.
#[test]
fn actorless_delete_and_restore_wrappers_journal_nothing() {
    let db = WorkDb::open(temp_db_path("boothby-wrapper-delete")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    db.delete_work_item(&chore.id).unwrap();
    db.restore_work_item(&chore.id).unwrap();

    assert!(actions(&db).is_empty());
    let restored = db.get_work_item(&chore.id).unwrap();
    assert!(matches!(restored, WorkItem::Chore(t) if t.deleted_at.is_none()));
}

// ── journalling for Boothby ───────────────────────────────────────────

#[test]
fn boothby_task_update_journals_only_the_touched_columns() {
    let db = WorkDb::open(temp_db_path("boothby-task-journal")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a stale chore");
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    assert_eq!(
        row.verb, "close_stale_task",
        "verb comes from the executor, not a guess"
    );
    assert_eq!(row.target_kind, BOOTHBY_TARGET_TASK);
    assert_eq!(row.target_id, chore.id);
    assert_eq!(row.seq, 1);

    let (pre, post) = row.images();
    assert_eq!(pre["status"], "todo");
    assert_eq!(post["status"], "archived");
    // The actor moved too, and undo needs it to hand the row back.
    assert_eq!(pre["last_status_actor"], "human");
    assert_eq!(post["last_status_actor"], "boothby");
    // `name` never moved, so it must appear in neither image — an undo
    // that rewrote it would clobber a column Boothby never touched.
    assert!(pre.get("name").is_none(), "untouched column leaked into pre_image");
    assert!(post.get("name").is_none(), "untouched column leaked into post_image");
    // `updated_at` is bookkeeping, not a decision.
    assert!(pre.get("updated_at").is_none());
}

#[test]
fn boothby_action_records_rationale_and_reversibility_from_the_context() {
    let db = WorkDb::open(temp_db_path("boothby-ctx-fields")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    let _guard = db
        .arm_boothby_action(
            BoothbyActionContext::builder()
                .verb("close_duplicate_task")
                .rationale("duplicate of T12")
                .reversibility(boss_protocol::BOOTHBY_REVERSIBILITY_REVERSIBLE)
                .params(r#"{"canonical":"T12"}"#)
                .build(),
        )
        .unwrap();
    db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let conn = db.connect().unwrap();
    let (rationale, reversibility, params, undo_state, pass_id): (String, String, Option<String>, String, String) =
        conn.query_row(
            "SELECT rationale, reversibility, params, undo_state, pass_id FROM boothby_actions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!(rationale, "duplicate of T12");
    assert_eq!(reversibility, "reversible");
    assert_eq!(params.as_deref(), Some(r#"{"canonical":"T12"}"#));
    assert_eq!(undo_state, "none", "undo_state lifecycle belongs to the undo engine");
    assert_eq!(pass_id, "bp_1", "journalled against the open pass");
}

/// An irreversible verb journals `params` + evidence instead of a
/// restorable pre-image, per the design.
#[test]
fn an_irreversible_verb_journals_no_pre_image() {
    let db = WorkDb::open(temp_db_path("boothby-irreversible")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    let _guard = db
        .arm_boothby_action(
            BoothbyActionContext::builder()
                .verb("force_release_lease")
                .rationale("holder pid is dead")
                .reversibility(boss_protocol::BOOTHBY_REVERSIBILITY_IRREVERSIBLE)
                .build(),
        )
        .unwrap();
    db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    assert!(row.images.pre.is_none(), "I-class actions carry no pre_image");
    assert!(row.images.post.is_some(), "but still record what they did");
}

#[test]
fn boothby_no_op_update_journals_nothing() {
    let db = WorkDb::open(temp_db_path("boothby-noop")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    // Patch the status to the value it already has: the write happens, but
    // no column moves, so there is no decision worth journalling.
    db.update_work_item_as_actor(&chore.id, status_patch("todo"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    assert!(actions(&db).is_empty(), "a no-op must not append an action row");
}

#[test]
fn boothby_project_update_journals_the_status_move() {
    let db = WorkDb::open(temp_db_path("boothby-project")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product.id)
                .name("An empty project")
                .build(),
        )
        .unwrap();
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("archive_empty_project")).unwrap();
    db.update_work_item_as_actor(&project.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    assert_eq!(row.verb, "archive_empty_project");
    assert_eq!(row.target_kind, BOOTHBY_TARGET_PROJECT);
    assert_eq!(row.target_id, project.id);
    let (pre, post) = row.images();
    assert_eq!(post["status"], "archived");
    assert_eq!(pre["last_status_actor"], "human");
    assert_eq!(post["last_status_actor"], "boothby");
}

#[test]
fn boothby_delete_journals_the_tombstone_as_the_undo_payload() {
    let db = WorkDb::open(temp_db_path("boothby-delete")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a duplicate chore");
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_duplicate_task")).unwrap();
    db.delete_work_item_as_actor(&chore.id, LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    assert_eq!(row.target_kind, BOOTHBY_TARGET_TASK);
    let (pre, post) = row.images();
    // `deleted_at` NULL -> a timestamp is the whole delete, and restoring
    // the pre-image's null is exactly the undo.
    assert_eq!(pre["deleted_at"], serde_json::Value::Null);
    assert!(post["deleted_at"].is_string());
}

#[test]
fn boothby_restore_journals_clearing_the_tombstone() {
    let db = WorkDb::open(temp_db_path("boothby-restore")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    db.delete_work_item(&chore.id).unwrap();
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("restore_task")).unwrap();
    db.restore_work_item_as_actor(&chore.id, LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    let (pre, post) = row.images();
    assert!(pre["deleted_at"].is_string());
    assert_eq!(post["deleted_at"], serde_json::Value::Null);
}

#[test]
fn boothby_attention_dismiss_journals_the_state_move() {
    let db = WorkDb::open(temp_db_path("boothby-attention")).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);
    let (_attention, group) = db
        .create_attention(
            CreateAttentionInput::builder()
                .kind("question")
                .source_kind("design_doc")
                .association_project_id(&project.id)
                .source_doc_path("docs/d.md")
                .question_type("prompt")
                .prompt_text("which way?")
                .build(),
        )
        .unwrap();
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("dismiss_attention")).unwrap();
    db.dismiss_attention_as_actor(&group.id, None, LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    let row = only_action(&db);
    assert_eq!(row.verb, "dismiss_attention");
    assert_eq!(row.target_kind, BOOTHBY_TARGET_ATTENTION);
    assert_eq!(row.target_id, group.id);
    let (pre, post) = row.images();
    assert_eq!(pre["state"], "open");
    assert_eq!(post["state"], "dismissed");
    assert_eq!(pre["dismissed_at"], serde_json::Value::Null);
    assert!(post["dismissed_at"].is_string());
}

/// Dismissing an already-dismissed group is an idempotent no-op that
/// returns early — it must not append a second action row.
#[test]
fn boothby_repeat_attention_dismiss_journals_once() {
    let db = WorkDb::open(temp_db_path("boothby-attention-twice")).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);
    let (_attention, group) = db
        .create_attention(
            CreateAttentionInput::builder()
                .kind("question")
                .source_kind("design_doc")
                .association_project_id(&project.id)
                .source_doc_path("docs/d.md")
                .question_type("prompt")
                .prompt_text("which way?")
                .build(),
        )
        .unwrap();
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("dismiss_attention")).unwrap();
    db.dismiss_attention_as_actor(&group.id, None, LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();
    db.dismiss_attention_as_actor(&group.id, None, LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    assert_eq!(actions(&db).len(), 1, "the idempotent re-dismiss must not re-journal");
}

#[test]
fn seq_increments_across_actions_in_a_pass() {
    let db = WorkDb::open(temp_db_path("boothby-seq-inc")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    for label in ["one", "two", "three"] {
        let chore = create_test_chore_manual(&db, &product.id, format!("chore {label}"));
        db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
            .unwrap();
    }

    assert_eq!(actions(&db).iter().map(|r| r.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    let conn = db.connect().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT actions_count FROM boothby_passes WHERE id = 'bp_1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 3, "the pass's denormalised counter tracks the journal");
}

// ── no journal, no mutation ───────────────────────────────────────────

/// An autonomous mutation that cannot be explained must not happen at
/// all: with no armed context there is no verb or rationale to record, so
/// the write is refused rather than escaping the audit trail.
#[test]
fn a_boothby_mutation_without_an_armed_context_is_refused() {
    let db = WorkDb::open(temp_db_path("boothby-unarmed")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    let refused = db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY);
    assert!(refused.is_err(), "an unjournalled Boothby mutation must be refused");

    // And the refusal rolled the mutation back.
    let after = db.get_work_item(&chore.id).unwrap();
    assert!(matches!(after, WorkItem::Chore(t) if t.status == TaskStatus::Todo));
    assert!(actions(&db).is_empty());
}

/// Every action belongs to a pass, so a Boothby mutation outside one is
/// equally refused.
#[test]
fn a_boothby_mutation_without_an_open_pass_is_refused() {
    let db = WorkDb::open(temp_db_path("boothby-no-pass")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    let refused = db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY);
    assert!(refused.is_err(), "no open pass ⇒ no journal ⇒ no mutation");

    let after = db.get_work_item(&chore.id).unwrap();
    assert!(matches!(after, WorkItem::Chore(t) if t.status == TaskStatus::Todo));
}

/// A finished pass does not collect actions raised after it closed.
#[test]
fn a_boothby_mutation_after_the_pass_closed_is_refused() {
    let db = WorkDb::open(temp_db_path("boothby-closed-pass")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE boothby_passes SET outcome = 'completed', finished_at = '1' WHERE id = 'bp_1'",
            [],
        )
        .unwrap();
    }

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    assert!(
        db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
            .is_err(),
    );
}

/// The guard disarms on drop, so a verb cannot leak onto the next
/// mutation and mislabel it.
#[test]
fn the_action_context_disarms_when_the_guard_drops() {
    let db = WorkDb::open(temp_db_path("boothby-disarm")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let first = create_test_chore_manual(&db, &product.id, "first");
    let second = create_test_chore_manual(&db, &product.id, "second");
    open_pass(&db, "bp_1");

    {
        let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
        db.update_work_item_as_actor(&first.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
            .unwrap();
    }

    assert!(
        db.update_work_item_as_actor(&second.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
            .is_err(),
        "the dropped guard must not leave `close_stale_task` armed",
    );
    assert_eq!(actions(&db).len(), 1);
}

/// The journal row and the write it describes share one transaction, so a
/// refused mutation must leave no trace at all.
#[test]
fn a_refused_boothby_update_journals_nothing() {
    let db = WorkDb::open(temp_db_path("boothby-refused")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    db.delete_work_item(&chore.id).unwrap();
    open_pass(&db, "bp_1");

    let _guard = db.arm_boothby_action(ctx("close_stale_task")).unwrap();
    // Updating a tombstoned task is refused outright.
    assert!(
        db.update_work_item_as_actor(&chore.id, status_patch("archived"), LAST_STATUS_ACTOR_BOOTHBY)
            .is_err()
    );
    assert!(actions(&db).is_empty(), "a rejected mutation must not be journalled");
}

// ── actor vocabulary ──────────────────────────────────────────────────

/// Pins the decision the exhaustive-match audit made: Boothby sits with
/// `human` / `boss` on the "deliberate, hands off" side of the
/// dep-unblock rule, not with `engine`.
#[test]
fn only_engine_counts_as_an_engine_cascade() {
    assert!(StatusActor::Engine.is_engine_cascade());
    for actor in [StatusActor::Human, StatusActor::Boss, StatusActor::Boothby] {
        assert!(
            !actor.is_engine_cascade(),
            "{actor} is a deliberate actor; the dep sweep must not reverse its block",
        );
    }
}

#[test]
fn status_actor_round_trips_through_its_string_form() {
    for actor in StatusActor::ALL {
        assert_eq!(actor.as_str().parse::<StatusActor>().unwrap(), *actor);
    }
    assert_eq!(
        LAST_STATUS_ACTOR_BOOTHBY.parse::<StatusActor>().unwrap(),
        StatusActor::Boothby
    );
    assert!("groundskeeper".parse::<StatusActor>().is_err());
}

/// A Boothby-blocked task must not be auto-unblocked by the dependency
/// sweep once its prereqs clear — the block was a judgement, not cascade
/// bookkeeping. Behavioural counterpart to
/// `only_engine_counts_as_an_engine_cascade`.
#[test]
fn dep_sweep_leaves_a_boothby_blocked_task_alone() {
    let db = WorkDb::open(temp_db_path("boothby-dep-sweep")).unwrap();
    let product = create_test_product_named(&db, "Boss");
    let chore = create_test_chore_manual(&db, &product.id, "a chore");
    open_pass(&db, "bp_1");

    // Blocked, with no blocked_reason, owned by Boothby — the shape that
    // routes to the actor check rather than the `dependency` fast path.
    let _guard = db.arm_boothby_action(ctx("park_task")).unwrap();
    db.update_work_item_as_actor(&chore.id, status_patch("blocked"), LAST_STATUS_ACTOR_BOOTHBY)
        .unwrap();

    assert!(
        !db.try_unblock_dependency_if_resolved(&chore.id).unwrap(),
        "a Boothby block must stick, exactly as a human's does",
    );
    let after = db.get_work_item(&chore.id).unwrap();
    assert!(matches!(after, WorkItem::Chore(t) if t.status == TaskStatus::Blocked));
}

// ── created_via ───────────────────────────────────────────────────────

#[test]
fn boothby_created_via_prefix_is_a_known_source() {
    assert!(is_known_created_via(&format!("{CREATED_VIA_BOOTHBY_PREFIX}bp_abc123")));
    // Bare prefix with no pass id still parses as Boothby-sourced.
    assert!(is_known_created_via(CREATED_VIA_BOOTHBY_PREFIX));
    assert!(!is_known_created_via("boothby"), "the prefix requires its colon");
}
