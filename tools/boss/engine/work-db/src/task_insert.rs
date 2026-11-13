//! One crate-private INSERT for production `tasks` rows.
//!
//! Callers still own validation, identity allocation, transactions, and
//! domain-specific values. This module owns the column list and the defaults
//! applied when a creator leaves a field unset; builder defaults equal the
//! schema defaults (`autostart=1`, `deferred`/`human_driven`/`xhigh=0`,
//! `priority=medium`, `created_via=unknown`, nullable columns NULL).
//!
//! Values each production creator sets:
//!
//! | Creator | Distinct values | Left to writer defaults |
//! | --- | --- | --- |
//! | project_task / chore / investigation | computed autostart/deferred/human_driven; seeded reasoning; effort provenance | `design_reasoning_effort_xhigh=0`; chore also writes origin columns |
//! | revision | parent, inherited repo, optional reasoning inherit | `deferred=0`, `human_driven=0`, `design_reasoning_effort_xhigh=0` |
//! | project-seeded design | `ordinal=0`, `kind=design`, `created_via=engine_auto`, xhigh flag | `deferred=0`, `human_driven=0`, effort/reasoning/repo NULL |
//! | attention-seeded design | next ordinal, `created_via=attention` | `deferred=0`, `human_driven=0`, `design_reasoning_effort_xhigh=0`, effort/reasoning/repo NULL |
//! | design postmortem | `effort_level=medium`, `created_via=engine_auto` | `deferred=0`, `human_driven=0`, reasoning/repo NULL |
//!
//! Every creator writes `status='todo'`, `pr_url=NULL`, `deleted_at=NULL`,
//! `priority` medium unless the caller named one, and leaves `last_status_actor`,
//! `tags`, `ci_attempts_used`, and `review_cycle` on their schema defaults.

use rusqlite::{Connection, params};

use crate::CREATED_VIA_UNKNOWN;
use crate::task_status_audit::WriteAttribution;

/// Shared INSERT column list. `pr_url` and `deleted_at` stay NULL: no
/// production creator writes them at insert time. `updated_at` is bound to
/// the same `created_at` value. Unlisted columns keep their schema defaults
/// (`last_status_actor='human'`, `tags='[]'`, `ci_attempts_used=0`,
/// `review_cycle=0`, …).
const INSERT_TASK_SQL: &str = "INSERT INTO tasks (
        id, product_id, project_id, kind, name, description, status, ordinal,
        pr_url, deleted_at, created_at, updated_at, autostart, deferred, human_driven,
        design_reasoning_effort_xhigh, priority, created_via, repo_remote_url,
        effort_level, model_override, reasoning, driver, short_id,
        parent_task_id, origin_task_short_id, origin_pr_number,
        effort_matched_rule, effort_reasons
     ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
        NULL, NULL, ?9, ?9, ?10, ?11, ?12,
        ?13, ?14, ?15, ?16,
        ?17, ?18, ?19, ?20, ?21,
        ?22, ?23, ?24,
        ?25, ?26
     )";

/// Builder-backed insertion value. Required fields are identity and the
/// values every production creator always supplies. Optional fields default
/// to the schema (or the universal production) value so a new defaulted
/// storage field can be added here without touching every caller.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct TaskInsert {
    pub id: String,
    pub product_id: String,
    pub kind: String,
    pub name: String,
    pub created_at: String,
    pub short_id: i64,

    pub project_id: Option<String>,
    #[builder(default)]
    pub description: String,
    #[builder(default = "todo".to_owned())]
    pub status: String,
    pub ordinal: Option<i64>,
    #[builder(default = true)]
    pub autostart: bool,
    #[builder(default)]
    pub deferred: bool,
    #[builder(default)]
    pub human_driven: bool,
    #[builder(default)]
    pub design_reasoning_effort_xhigh: bool,
    #[builder(default = "medium".to_owned())]
    pub priority: String,
    #[builder(default = CREATED_VIA_UNKNOWN.to_owned())]
    pub created_via: String,
    pub repo_remote_url: Option<String>,
    pub effort_level: Option<String>,
    pub model_override: Option<String>,
    pub reasoning: Option<String>,
    pub driver: Option<String>,
    pub parent_task_id: Option<String>,
    pub origin_task_short_id: Option<i64>,
    pub origin_pr_number: Option<i64>,
    pub effort_matched_rule: Option<String>,
    pub effort_reasons: Option<String>,
}

/// Insert one `tasks` row through [`crate::task_status_audit::execute`].
///
/// `operation` is the existing helper name stored as the ledger basis
/// (`insert_task_in_tx_as_actor`, `insert_revision_in_tx`, …) so provenance
/// strings stay identical. Does not open a connection, commit, allocate
/// identities, or publish events.
pub(crate) fn insert_task_row(
    conn: &Connection,
    row: &TaskInsert,
    attribution: WriteAttribution<'_>,
    operation: &str,
) -> rusqlite::Result<usize> {
    let basis = attribution.basis(operation);
    crate::task_status_audit::execute(
        conn,
        &basis,
        attribution.actor,
        INSERT_TASK_SQL,
        params![
            row.id,
            row.product_id,
            row.project_id,
            row.kind,
            row.name,
            row.description,
            row.status,
            row.ordinal,
            row.created_at,
            row.autostart as i64,
            row.deferred as i64,
            row.human_driven as i64,
            row.design_reasoning_effort_xhigh as i64,
            row.priority,
            row.created_via,
            row.repo_remote_url,
            row.effort_level,
            row.model_override,
            row.reasoning,
            row.driver,
            row.short_id,
            row.parent_task_id,
            row.origin_task_short_id,
            row.origin_pr_number,
            row.effort_matched_rule,
            row.effort_reasons,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_product_named, open_db};
    use crate::{
        CreateAttentionInput, CreateChoreInput, CreateProjectInput, CreateRevisionInput, CreateTaskInput,
        FakePrStateChecker, LAST_STATUS_ACTOR_HUMAN, PrOpenState, TaskKind, TaskStatus, WorkDb, allocate_short_id,
        insert_task_in_tx, now_string, query_task,
    };
    use rusqlite::{Connection, OptionalExtension, params};

    struct StoredDefaults {
        kind: String,
        project_id: Option<String>,
        status: String,
        ordinal: Option<i64>,
        pr_url: Option<String>,
        deleted_at: Option<String>,
        autostart: i64,
        deferred: i64,
        human_driven: i64,
        design_reasoning_effort_xhigh: i64,
        priority: String,
        created_via: String,
        repo_remote_url: Option<String>,
        effort_level: Option<String>,
        model_override: Option<String>,
        reasoning: Option<String>,
        driver: Option<String>,
        parent_task_id: Option<String>,
        origin_task_short_id: Option<i64>,
        origin_pr_number: Option<i64>,
        effort_matched_rule: Option<String>,
        effort_reasons: Option<String>,
        last_status_actor: String,
        tags: String,
        ci_attempts_used: i64,
        review_cycle: i64,
    }

    fn stored_defaults(conn: &Connection, id: &str) -> StoredDefaults {
        conn.query_row(
            "SELECT kind, project_id, status, ordinal, pr_url, deleted_at, autostart, deferred, human_driven,
                    design_reasoning_effort_xhigh, priority, created_via, repo_remote_url,
                    effort_level, model_override, reasoning, driver, parent_task_id,
                    origin_task_short_id, origin_pr_number, effort_matched_rule, effort_reasons,
                    last_status_actor, tags, ci_attempts_used, review_cycle
             FROM tasks WHERE id = ?1",
            [id],
            |row| {
                Ok(StoredDefaults {
                    kind: row.get(0)?,
                    project_id: row.get(1)?,
                    status: row.get(2)?,
                    ordinal: row.get(3)?,
                    pr_url: row.get(4)?,
                    deleted_at: row.get(5)?,
                    autostart: row.get(6)?,
                    deferred: row.get(7)?,
                    human_driven: row.get(8)?,
                    design_reasoning_effort_xhigh: row.get(9)?,
                    priority: row.get(10)?,
                    created_via: row.get(11)?,
                    repo_remote_url: row.get(12)?,
                    effort_level: row.get(13)?,
                    model_override: row.get(14)?,
                    reasoning: row.get(15)?,
                    driver: row.get(16)?,
                    parent_task_id: row.get(17)?,
                    origin_task_short_id: row.get(18)?,
                    origin_pr_number: row.get(19)?,
                    effort_matched_rule: row.get(20)?,
                    effort_reasons: row.get(21)?,
                    last_status_actor: row.get(22)?,
                    tags: row.get(23)?,
                    ci_attempts_used: row.get(24)?,
                    review_cycle: row.get(25)?,
                })
            },
        )
        .unwrap()
    }

    fn insert_ledger(conn: &Connection, id: &str, property: &str) -> (Option<String>, Option<String>, String, String) {
        conn.query_row(
            "SELECT old_value, new_value, actor, basis FROM task_status_audit
             WHERE work_item_id = ?1 AND property = ?2 AND old_value IS NULL
             ORDER BY id ASC LIMIT 1",
            params![id, property],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    }

    fn audit_count(conn: &Connection, id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn bind_open_pr(db: &WorkDb, task_id: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                params![task_id, "https://github.com/spinyfin/mono/pull/42"],
            )
            .unwrap();
    }

    fn schema_default_columns(row: &StoredDefaults) {
        assert_eq!(row.status, "todo");
        assert_eq!(row.pr_url, None);
        assert_eq!(row.deleted_at, None);
        assert_eq!(row.last_status_actor, "human");
        assert_eq!(row.tags, "[]");
        assert_eq!(row.ci_attempts_used, 0);
        assert_eq!(row.review_cycle, 0);
        assert_eq!(row.priority, "medium");
    }

    #[test]
    fn writer_builder_defaults_match_sqlite_schema_defaults() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Writer defaults");
        let conn = db.connect().unwrap();
        let id = crate::next_id("task");
        let now = now_string();
        let short_id = allocate_short_id(&conn, &product.id).unwrap();
        insert_task_row(
            &conn,
            &TaskInsert::builder()
                .id(&id)
                .product_id(&product.id)
                .kind("chore")
                .name("Minimal")
                .created_at(&now)
                .short_id(short_id)
                .build(),
            WriteAttribution {
                actor: "engine",
                provenance: None,
            },
            "insert_chore_with_project_in_tx_as_actor",
        )
        .unwrap();

        let sqlite_id = crate::next_id("task");
        let sqlite_short_id = allocate_short_id(&conn, &product.id).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at, short_id)
             VALUES (?1, ?2, 'chore', 'SQLite omitted', 'todo', ?3, ?3, ?4)",
            params![sqlite_id, product.id, now, sqlite_short_id],
        )
        .unwrap();

        let row = stored_defaults(&conn, &id);
        let sqlite = stored_defaults(&conn, &sqlite_id);
        schema_default_columns(&row);
        schema_default_columns(&sqlite);
        assert_eq!(row.kind, "chore");
        assert_eq!(row.ordinal, None);
        assert_eq!(row.autostart, sqlite.autostart);
        assert_eq!(row.deferred, sqlite.deferred);
        assert_eq!(row.human_driven, sqlite.human_driven);
        assert_eq!(row.design_reasoning_effort_xhigh, sqlite.design_reasoning_effort_xhigh);
        assert_eq!(row.priority, sqlite.priority);
        assert_eq!(row.created_via, sqlite.created_via);
        assert_eq!(row.repo_remote_url, sqlite.repo_remote_url);
        assert_eq!(row.effort_level, sqlite.effort_level);
        assert_eq!(row.model_override, sqlite.model_override);
        assert_eq!(row.reasoning, sqlite.reasoning);
        assert_eq!(row.driver, sqlite.driver);
        assert_eq!(row.parent_task_id, sqlite.parent_task_id);
        assert_eq!(row.origin_task_short_id, sqlite.origin_task_short_id);
        assert_eq!(row.origin_pr_number, sqlite.origin_pr_number);
        assert_eq!(row.effort_matched_rule, sqlite.effort_matched_rule);
        assert_eq!(row.effort_reasons, sqlite.effort_reasons);
        assert_eq!(row.autostart, 1);
        assert_eq!(row.deferred, 0);
        assert_eq!(row.human_driven, 0);
        assert_eq!(row.design_reasoning_effort_xhigh, 0);
        assert_eq!(row.created_via, CREATED_VIA_UNKNOWN);
        assert_eq!(audit_count(&conn, &id), 5);
        assert_eq!(
            insert_ledger(&conn, &id, "status"),
            (
                None,
                Some("todo".into()),
                "engine".into(),
                "insert_chore_with_project_in_tx_as_actor".into()
            )
        );
        assert_eq!(
            insert_ledger(&conn, &id, "autostart"),
            (
                None,
                Some("1".into()),
                "engine".into(),
                "insert_chore_with_project_in_tx_as_actor".into()
            )
        );
        assert_eq!(
            insert_ledger(&conn, &id, "deferred"),
            (
                None,
                Some("0".into()),
                "engine".into(),
                "insert_chore_with_project_in_tx_as_actor".into()
            )
        );
    }

    #[test]
    fn writer_preserves_actor_basis_and_explicit_overrides() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Writer overrides");
        let conn = db.connect().unwrap();
        let id = crate::next_id("task");
        let now = now_string();
        let short_id = allocate_short_id(&conn, &product.id).unwrap();
        insert_task_row(
            &conn,
            &TaskInsert::builder()
                .id(&id)
                .product_id(&product.id)
                .kind("followup")
                .name("Follow-up")
                .created_at(&now)
                .short_id(short_id)
                .description("body")
                .autostart(false)
                .deferred(true)
                .human_driven(true)
                .priority("high")
                .created_via("attention")
                .origin_task_short_id(7)
                .origin_pr_number(99)
                .parent_task_id("task_parent")
                .build(),
            WriteAttribution {
                actor: LAST_STATUS_ACTOR_HUMAN,
                provenance: Some("rpc:create_chore peer=socket_client"),
            },
            "insert_chore_with_project_in_tx_as_actor",
        )
        .unwrap();

        let row = stored_defaults(&conn, &id);
        assert_eq!(row.autostart, 0);
        assert_eq!(row.deferred, 1);
        assert_eq!(row.human_driven, 1);
        assert_eq!(row.priority, "high");
        assert_eq!(row.created_via, "attention");
        assert_eq!(row.origin_task_short_id, Some(7));
        assert_eq!(row.origin_pr_number, Some(99));
        assert_eq!(row.parent_task_id.as_deref(), Some("task_parent"));
        let expected_basis = "insert_chore_with_project_in_tx_as_actor rpc:create_chore peer=socket_client";
        assert_eq!(
            insert_ledger(&conn, &id, "deferred"),
            (
                None,
                Some("1".into()),
                LAST_STATUS_ACTOR_HUMAN.into(),
                expected_basis.into()
            )
        );
    }

    #[test]
    fn writer_rolls_back_with_the_enclosing_transaction() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Writer rollback");
        let mut conn = db.connect().unwrap();
        let id = crate::next_id("task");
        {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let short_id = allocate_short_id(&tx, &product.id).unwrap();
            insert_task_row(
                &tx,
                &TaskInsert::builder()
                    .id(&id)
                    .product_id(&product.id)
                    .kind("chore")
                    .name("Rolled back")
                    .created_at(now_string())
                    .short_id(short_id)
                    .build(),
                WriteAttribution {
                    actor: "engine",
                    provenance: None,
                },
                "insert_chore_with_project_in_tx_as_actor",
            )
            .unwrap();
            assert!(query_task(&tx, &id).unwrap().is_some());
            assert!(
                tx.execute("INSERT INTO products (id) VALUES (NULL)", []).is_err(),
                "later statement in the same transaction must fail"
            );
        }
        let leftover: Option<String> = conn
            .query_row("SELECT id FROM tasks WHERE id = ?1", [&id], |row| row.get(0))
            .optional()
            .unwrap();
        assert_eq!(leftover, None);
        assert_eq!(audit_count(&conn, &id), 0);
    }

    #[test]
    fn project_task_chore_and_investigation_public_creates_keep_effective_defaults() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Public creates");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Hold tasks")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();

        let task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Project task")
                    .build(),
            )
            .unwrap();
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Chore")
                    .build(),
            )
            .unwrap();
        let investigation = db
            .create_investigation(
                boss_protocol::CreateInvestigationInput::builder()
                    .product_id(product.id.clone())
                    .name("Investigate")
                    .build(),
            )
            .unwrap();

        let conn = db.connect().unwrap();
        let task_row = stored_defaults(&conn, &task.id);
        schema_default_columns(&task_row);
        assert_eq!(task_row.kind, "project_task");
        assert_eq!(task_row.autostart, 1);
        assert_eq!(task_row.deferred, 0);
        assert_eq!(task_row.human_driven, 0);
        assert_eq!(task_row.design_reasoning_effort_xhigh, 0);
        assert_eq!(task_row.reasoning.as_deref(), Some("standard"));
        assert_eq!(task_row.origin_task_short_id, None);
        assert_eq!(insert_ledger(&conn, &task.id, "status").3, "insert_task_in_tx_as_actor");

        let chore_row = stored_defaults(&conn, &chore.id);
        schema_default_columns(&chore_row);
        assert_eq!(chore_row.kind, "chore");
        assert_eq!(chore_row.project_id, None);
        assert_eq!(chore_row.ordinal, None);
        assert_eq!(chore_row.reasoning.as_deref(), Some("standard"));
        assert_eq!(
            insert_ledger(&conn, &chore.id, "status").3,
            "insert_chore_with_project_in_tx_as_actor"
        );

        let inv_row = stored_defaults(&conn, &investigation.id);
        schema_default_columns(&inv_row);
        assert_eq!(inv_row.kind, "investigation");
        assert_eq!(inv_row.ordinal, None);
        assert_eq!(inv_row.reasoning.as_deref(), Some("investigation"));
        assert_eq!(
            insert_ledger(&conn, &investigation.id, "status").3,
            "insert_investigation_in_tx_as_actor"
        );
    }

    #[test]
    fn revision_design_seed_and_postmortem_keep_effective_defaults() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Domain creates");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .build(),
            )
            .unwrap();
        let design_id: String = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
                [&project.id],
                |row| row.get(0),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            let design_row = stored_defaults(&conn, &design_id);
            schema_default_columns(&design_row);
            assert_eq!(design_row.kind, "design");
            assert_eq!(design_row.ordinal, Some(0));
            assert_eq!(design_row.autostart, 1);
            assert_eq!(design_row.deferred, 0);
            assert_eq!(design_row.human_driven, 0);
            assert_eq!(design_row.design_reasoning_effort_xhigh, 0);
            assert_eq!(design_row.created_via, crate::CREATED_VIA_ENGINE_AUTO);
            assert_eq!(design_row.effort_level, None);
            assert_eq!(design_row.reasoning, None);
            assert_eq!(
                insert_ledger(&conn, &design_id, "status").3,
                "insert_design_task_for_project_in_tx"
            );
        }

        let parent = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Parent")
                    .build(),
            )
            .unwrap();
        bind_open_pr(&db, &parent.id);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(&parent.id)
                    .description("Fix the gate")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            let revision_row = stored_defaults(&conn, &revision.id);
            schema_default_columns(&revision_row);
            assert_eq!(revision_row.kind, "revision");
            assert_eq!(revision_row.ordinal, None);
            assert_eq!(revision_row.deferred, 0);
            assert_eq!(revision_row.human_driven, 0);
            assert_eq!(revision_row.design_reasoning_effort_xhigh, 0);
            assert_eq!(revision_row.parent_task_id.as_deref(), Some(parent.id.as_str()));
            assert_eq!(revision_row.effort_level.as_deref(), Some("small"));
            assert_eq!(revision_row.reasoning.as_deref(), Some("standard"));
            assert_eq!(insert_ledger(&conn, &revision.id, "status").3, "insert_revision_in_tx");
        }

        let postmortem = db
            .create_design_postmortem(&product.id, &project.id, &project.name, "review merged PRs".to_owned())
            .unwrap();
        let conn = db.connect().unwrap();
        let postmortem_row = stored_defaults(&conn, &postmortem.id);
        schema_default_columns(&postmortem_row);
        assert_eq!(postmortem_row.kind, "design_postmortem");
        assert_eq!(postmortem_row.ordinal, None);
        assert_eq!(postmortem_row.autostart, 1);
        assert_eq!(postmortem_row.deferred, 0);
        assert_eq!(postmortem_row.effort_level.as_deref(), Some("medium"));
        assert_eq!(postmortem_row.reasoning, None);
        assert_eq!(postmortem_row.created_via, crate::CREATED_VIA_ENGINE_AUTO);
        assert_eq!(
            insert_ledger(&conn, &postmortem.id, "status").3,
            "insert_design_postmortem_in_tx"
        );
    }

    #[test]
    fn attention_seeded_design_keeps_effective_defaults() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Seeded design");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Hold")
                    .build(),
            )
            .unwrap();
        let (attention, group) = db
            .create_attention(
                CreateAttentionInput::builder()
                    .kind("question")
                    .association_project_id(&project.id)
                    .source_kind("design_doc")
                    .source_doc_path("docs/ledger.md")
                    .question_type("prompt")
                    .prompt_text("keep the table?")
                    .build(),
            )
            .unwrap();
        db.answer_attention(&attention.id, Some("yes".to_owned()), false, false)
            .unwrap();
        let actioned = db
            .action_attention_group(
                &group.id,
                false,
                &Default::default(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        assert_eq!(actioned.produced_work_item_ids.len(), 1);
        let seeded_id = &actioned.produced_work_item_ids[0];
        let conn = db.connect().unwrap();
        let seeded = stored_defaults(&conn, seeded_id);
        schema_default_columns(&seeded);
        assert_eq!(seeded.kind, "design");
        assert_eq!(seeded.autostart, 1);
        assert_eq!(seeded.deferred, 0);
        assert_eq!(seeded.human_driven, 0);
        assert_eq!(seeded.design_reasoning_effort_xhigh, 0);
        assert_eq!(seeded.created_via, crate::CREATED_VIA_ATTENTION);
        assert_eq!(seeded.effort_level, None);
        assert_eq!(seeded.reasoning, None);
        assert!(
            seeded.ordinal.unwrap() > 0,
            "seeded design uses the next project ordinal, not 0"
        );
        assert_eq!(
            insert_ledger(&conn, seeded_id, "status").3,
            "insert_seeded_design_task_in_tx"
        );
    }

    #[test]
    fn in_tx_create_rolls_back_when_a_later_statement_fails() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "In-tx rollback");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Hold")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        let mut conn = db.connect().unwrap();
        let inserted_id;
        {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let task = insert_task_in_tx(
                &tx,
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Should vanish")
                    .build(),
            )
            .unwrap();
            inserted_id = task.id.clone();
            assert_eq!(task.kind, TaskKind::ProjectTask);
            assert_eq!(task.status, TaskStatus::Todo);
            assert!(tx.execute("INSERT INTO products (id) VALUES (NULL)", []).is_err());
        }
        let leftover: Option<String> = conn
            .query_row("SELECT id FROM tasks WHERE id = ?1", [&inserted_id], |row| row.get(0))
            .optional()
            .unwrap();
        assert_eq!(leftover, None);
        assert_eq!(audit_count(&conn, &inserted_id), 0);
    }
}
