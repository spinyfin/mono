use super::*;

use crate::test_support::*;
/// Returns the `:memory:` sentinel so `WorkDb::open` allocates a
/// per-test named shared-cache in-memory database. Each call to
/// `WorkDb::open(PathBuf::from(":memory:"))` gets a unique database;
/// the `label` parameter is kept for call-site readability only.
fn temp_db_path(_label: &str) -> PathBuf {
    PathBuf::from(":memory:")
}

/// Returns a real on-disk temp path. Use this only for tests that
/// open a raw `rusqlite::Connection` alongside the `WorkDb` (e.g.
/// schema-migration tests that pre-populate a legacy schema and then
/// re-open via `WorkDb::open`). All other tests should use
/// `temp_db_path` so the database stays in RAM.
///
/// The returned `TempDir` must be kept alive for as long as the path
/// is in use — dropping it deletes the backing file.
fn disk_db_path(label: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let file = format!("boss-{label}-{}.sqlite3", next_id("test"));
    let path = dir.path().join(file);
    (dir, path)
}

// ── legacy (pre-migration) schema construction ─────────────────────────
//
// The migration tests each stand up a hand-written "legacy" schema on
// disk and then re-open it through `WorkDb::open` so the migration
// chain runs against it. Those schemas are all the same shape — a
// `metadata` table carrying the `schema_version` stamp plus some
// historical prefix of the `products` / `projects` / `tasks` column
// sets — so the boilerplate lives here and each test declares only
// what makes its case interesting: the columns the migration under
// test is about, any extra tables, and its seed rows.

/// Pass to `LegacySchema::products` / `projects` / `tasks` when the
/// table should be created with only its baseline columns.
const NO_EXTRA_COLUMNS: &str = "";

/// `projects.last_status_actor`, added in schema v4.
const PROJECTS_V4_COLUMNS: &str = "last_status_actor TEXT NOT NULL DEFAULT 'human'";

/// `tasks.last_status_actor` + `tasks.priority`, added in schema v4.
const TASKS_V4_COLUMNS: &str = "last_status_actor TEXT NOT NULL DEFAULT 'human',
     priority TEXT NOT NULL DEFAULT 'medium'";

/// A drifted variant some legacy fixtures use: `priority` +
/// `created_via` but no `last_status_actor`. Kept distinct because
/// those tests only need a DB old enough to lack the column their
/// migration adds, and the missing actor column is immaterial there.
const TASKS_NO_ACTOR_COLUMNS: &str = "priority TEXT NOT NULL DEFAULT 'medium',
     created_via TEXT NOT NULL DEFAULT 'unknown'";

/// The v4 task columns plus `created_via`, added in schema v5.
const TASKS_V5_COLUMNS: &str = "last_status_actor TEXT NOT NULL DEFAULT 'human',
     priority TEXT NOT NULL DEFAULT 'medium',
     created_via TEXT NOT NULL DEFAULT 'unknown'";

/// The baseline (schema v3) column list for each legacy table. Later
/// versions only ever appended columns, so a call site describes its
/// schema as "baseline + these extras" and the appended order matches
/// the historical one — which positional `INSERT INTO t VALUES (...)`
/// seeds depend on.
const PRODUCTS_BASE_COLUMNS: &str = "id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
     description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
     status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL";

const PROJECTS_BASE_COLUMNS: &str = "id TEXT PRIMARY KEY, product_id TEXT NOT NULL, name TEXT NOT NULL,
     slug TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
     goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
     priority TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL";

const TASKS_BASE_COLUMNS: &str = "id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
     kind TEXT NOT NULL, name TEXT NOT NULL,
     description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
     ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
     autostart INTEGER NOT NULL DEFAULT 1";

/// Builder for a pre-migration schema. Emits `metadata` (stamped with
/// `version`), then whichever of `products` / `projects` / `tasks` the
/// caller asked for, then any extra DDL, then any seed rows — in that
/// order, as a single `execute_batch`.
struct LegacySchema {
    version: u32,
    tables: Vec<String>,
    seeds: Vec<String>,
}

impl LegacySchema {
    /// Start a legacy schema whose `metadata.schema_version` is `version`.
    fn new(version: u32) -> Self {
        Self {
            version,
            tables: Vec::new(),
            seeds: Vec::new(),
        }
    }

    /// Create `products` with the baseline columns plus `extra`
    /// (`NO_EXTRA_COLUMNS` for baseline only).
    fn products(self, extra: &str) -> Self {
        self.table("products", PRODUCTS_BASE_COLUMNS, extra)
    }

    /// Create `projects` with the baseline columns plus `extra`.
    fn projects(self, extra: &str) -> Self {
        self.table("projects", PROJECTS_BASE_COLUMNS, extra)
    }

    /// Create `tasks` with the baseline columns plus `extra`.
    fn tasks(self, extra: &str) -> Self {
        self.table("tasks", TASKS_BASE_COLUMNS, extra)
    }

    /// Append verbatim DDL (a table this builder has no baseline for).
    fn ddl(mut self, sql: &str) -> Self {
        self.tables.push(sql.trim().to_owned());
        self
    }

    /// Append verbatim seed statements, run after all DDL.
    fn seed(mut self, sql: &str) -> Self {
        self.seeds.push(sql.trim().to_owned());
        self
    }

    fn table(mut self, name: &str, base: &str, extra: &str) -> Self {
        let columns = if extra.trim().is_empty() {
            base.to_owned()
        } else {
            format!("{base},\n     {}", extra.trim())
        };
        self.tables.push(format!("CREATE TABLE {name} (\n     {columns});"));
        self
    }

    /// Execute the assembled schema against `conn`.
    fn create(self, conn: &rusqlite::Connection) {
        let mut sql = String::from("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);\n");
        for table in &self.tables {
            sql.push_str(table);
            sql.push('\n');
        }
        sql.push_str(&format!(
            "INSERT INTO metadata(key, value) VALUES ('schema_version', '{}');\n",
            self.version
        ));
        for seed in &self.seeds {
            sql.push_str(seed);
            sql.push('\n');
        }
        conn.execute_batch(&sql)
            .unwrap_or_else(|e| panic!("legacy schema v{} failed: {e}\n{sql}", self.version));
    }
}

/// The `products` seed row every legacy schema plants so the child
/// rows have a parent. Timestamps are the suite-wide `1700000000`.
fn legacy_product_seed(id: &str, name: &str, slug: &str) -> String {
    format!(
        "INSERT INTO products(id, name, slug, status, created_at, updated_at)
         VALUES ('{id}', '{name}', '{slug}', 'active', '1700000000', '1700000000');"
    )
}

/// The matching `projects` seed row.
fn legacy_project_seed(id: &str, product_id: &str, name: &str, slug: &str) -> String {
    format!(
        "INSERT INTO projects(id, product_id, name, slug, status, priority, created_at, updated_at)
         VALUES ('{id}', '{product_id}', '{name}', '{slug}', 'planned', 'medium', '1700000000', '1700000000');"
    )
}

/// Project creation auto-spawns a `kind = 'design'` task, which
/// otherwise sits at the head of the project's task chain and
/// holds the dispatcher's `ready` slot. Most legacy tests pre-date
/// the design task and want to test the project_task ordering in
/// isolation, so they call this helper to mark the design as
/// already done — the rest of the chain then behaves exactly as it
/// did before.
fn complete_design_for_project(db: &WorkDb, project_id: &str) {
    let project = db.get_project(project_id).unwrap();
    let tasks = db
        .list_tasks(&project.product_id, Some(project_id), None, false)
        .unwrap();
    let design = tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("project should have an auto-created design task");
    db.update_work_item(
        &design.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
}

/// Helper: create a product (optionally with a worker branch prefix),
/// a project, a task, and an execution under it. Returns the stored
/// product and execution so prefix denormalisation can be asserted.
#[cfg(test)]
fn product_task_execution_with_prefix(db: &WorkDb, worker_branch_prefix: Option<&str>) -> (Product, WorkExecution) {
    let product = db
        .create_product(CreateProductInput {
            name: "Prefix Co".to_owned(),
            description: Some("desc".to_owned()),
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: worker_branch_prefix.map(str::to_owned),
        })
        .unwrap();
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("T")
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    (product, execution)
}

// ── T8 WorkDb external-ref method tests ─────────────────────────────────

/// Helper: create a product and a chore in a fresh in-memory db.
/// Returns `(db, product_id, chore_id)`.
fn setup_product_and_chore() -> (WorkDb, String, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "TestProduct");
    let chore = create_test_chore(&db, product.id.clone(), "Fix thing");
    (db, product.id, chore.id)
}

/// Stand up a fresh product + project against `path` so the
/// design-doc pointer tests don't all open-code the same
/// boilerplate. Returns the project id; the product's repo URL
/// is the standard `mono` git@ form the rest of the suite uses.
fn seed_project_for_design_doc(db: &WorkDb) -> (Product, Project) {
    let product = create_test_product(db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project design doc pointer".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    (product, project)
}

/// Stand up a product + a project-less investigation task so the
/// per-task doc-pointer tests don't open-code the boilerplate. The
/// product carries the standard `mono` repo so doc resolution has a
/// repo to fall back to and classifies `SameProduct`.
fn seed_investigation_for_doc(db: &WorkDb) -> (Product, Task) {
    let product = create_test_product(db);
    let investigation = db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Investigate the thing")
                .build(),
        )
        .unwrap();
    (product, investigation)
}

/// Convenience: rebuild a `set_project_design_doc` input with
/// just the project id and path filled in. Most pointer tests
/// only care about the path; defaulting the rest keeps signal
/// high.
fn set_design_doc_input(project_id: &str, path: &str) -> SetProjectDesignDocInput {
    SetProjectDesignDocInput {
        project_id: project_id.to_owned(),
        design_doc_repo_remote_url: None,
        design_doc_branch: None,
        design_doc_path: Some(path.to_owned()),
        unset: false,
    }
}

/// Helper: stand up an execution attached to the given project so
/// the conflict-surfacing test has a foreign key it can attach
/// the attention item to.
fn seed_execution_for(db: &WorkDb, product_id: &str, project_id: &str) -> WorkExecution {
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name("Schema")
                .build(),
        )
        .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(task.id)
            .kind(ExecutionKind::TaskImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

/// Shared scaffold for the `resolve_repo_for_work_item` tests: a
/// product (with `product_repo`) carrying a project + one task
/// whose own `repo_remote_url` is left `NULL`. Tests plant the
/// override they want via `set_task_repo` and then exercise the
/// helper.
fn make_resolve_scaffold(
    label: &str,
    product_repo: Option<&str>,
) -> (tempfile::TempDir, PathBuf, WorkDb, String, String) {
    // disk_db_path so that resolve_repo_errors_when_parent_product_is_missing
    // can open a second raw connection to the same database file.
    let (dir, path) = disk_db_path(label);
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", product_repo);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();
    // When the product has no repo, `create_task` now rejects a
    // None override (multi-repo products require a row-level repo).
    // These resolver tests need to probe pre-existing legacy rows
    // that have both task and product repo = NULL, so we bypass the
    // creation-time validation and insert directly via SQL.
    let task_id = if product_repo.is_none() {
        let conn = db.connect().unwrap();
        let id = next_id("task");
        let now = now_string();
        conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
                 VALUES (?1, ?2, ?3, 'project_task', 'Task', '', 'todo', 1, NULL, NULL, ?4, ?4, 0, 'medium', 'test')",
                params![id, product.id, project.id, now],
            ).unwrap();
        id
    } else {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Task")
                .autostart(false)
                .build(),
        )
        .unwrap()
        .id
    };
    (dir, path, db, product.id, task_id)
}

/// Resolver tests plant the override directly via SQL so they can
/// probe arbitrary combinations (including legacy rows that violate
/// the new invariant). Using `db.connect()` keeps the WAL /
/// busy-timeout pragmas consistent with the helper's read path.
fn set_task_repo(db: &WorkDb, task_id: &str, value: Option<&str>) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
        params![task_id, value],
    )
    .unwrap();
}

// ── Bug A: double-spawn guard ───────────────────────────────────────────

fn make_waiting_human_chore(db: &WorkDb, label: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, &format!("Prod-{label}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), format!("Chore-{label}"));
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:foo/bar.git")
                .build(),
        )
        .unwrap();
    let (exec, run) = db
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&exec.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    (product.id, chore.id, exec.id)
}

// ── Bug B: late PR detection ────────────────────────────────────────────

fn make_abandoned_chore_with_workspace(db: &WorkDb, label: &str) -> (String, String, String) {
    let (product_id, chore_id, exec_id) = make_waiting_human_chore(db, label);
    // Simulate the orphan sweep abandoning exec_a.
    db.mark_execution_redundant(&exec_id).unwrap();
    (product_id, chore_id, exec_id)
}

// ── Revision tasks Phase 1: schema + chain_root ────────────────────────

/// Helper: create a minimal product for revision tests.
fn make_revision_product(db: &WorkDb, label: &str) -> String {
    create_test_product_named(db, &format!("Boss-{label}")).id
}

/// Helper: create a chore (non-revision root) and return its id.
fn make_chore_root(db: &WorkDb, product_id: &str, label: &str) -> String {
    create_test_chore_manual(db, product_id, format!("Root chore {label}")).id
}

/// Helper: directly INSERT a revision task row (kind = 'revision') with
/// the given parent_task_id. Phase 2 will add `insert_revision_in_tx`;
/// for Phase 1 tests we bypass the API to keep the test self-contained.
fn insert_revision_row(db: &WorkDb, product_id: &str, parent_task_id: &str) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'Test revision', '', 'todo', ?3, ?3, ?4)",
        rusqlite::params![id, product_id, now, parent_task_id],
    )
    .unwrap();
    id
}

// ── Revision tasks Phase 2: CLI create-revision gate + insert ──────────

/// Helper: create a chore and set its pr_url (to simulate "in review").
fn make_in_review_chore(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let chore = create_test_chore_manual(db, product_id, "Chore for revision tests");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        rusqlite::params![chore.id, pr_url],
    )
    .unwrap();
    chore.id
}

/// Helper: create a chore whose status is `done` (simulates merged PR).
fn make_done_chore(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let id = make_in_review_chore(db, product_id, pr_url);
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", rusqlite::params![id])
        .unwrap();
    id
}

/// Helper: build a minimal `CreateRevisionInput` for the given parent id.
fn revision_input(parent_id: &str) -> CreateRevisionInput {
    CreateRevisionInput::builder()
        .parent_task_id(parent_id)
        .description("test revision ask")
        .build()
}

/// Insert a `kind=revision` task linked to a CI-fix attempt. Mirrors what
/// `ci_watch` produces for a CI remediation: `created_via =
/// "ci-fix:<ci_remediations.id>"`, parent = the chore, and (as in the
/// steady-state loop) the row already flipped to `active`.
fn insert_ci_fix_revision_row(db: &WorkDb, product_id: &str, parent_task_id: &str, rem_id: &str) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}{rem_id}");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Fix failing CI on PR', '', 'active', ?3, ?3, 0, ?4, ?5)",
        rusqlite::params![id, product_id, now, created_via, parent_task_id],
    )
    .unwrap();
    id
}

/// (id, status) of every execution bound to `work_item_id`, oldest first.
fn executions_for(db: &WorkDb, work_item_id: &str) -> Vec<(String, String)> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, status FROM work_executions
             WHERE work_item_id = ?1 ORDER BY created_at ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![work_item_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn task_status(db: &WorkDb, task_id: &str) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
}

// ── attach_revision_projections ─────────────────────────────────────────

/// Build a minimal Task with enough fields for `attach_revision_projections`.
fn make_bare_task(id: &str, kind: &str, parent: Option<&str>, pr: Option<&str>, ts: &str) -> Task {
    Task {
        id: id.to_owned(),
        short_id: None,
        product_id: "p".to_owned(),
        project_id: None,
        kind: kind.parse().expect("invalid task kind in make_bare_task"),
        name: "n".to_owned(),
        description: "d".to_owned(),
        status: TaskStatus::Todo,
        ordinal: None,
        pr_url: pr.map(str::to_owned),
        deleted_at: None,
        created_at: ts.to_owned(),
        updated_at: ts.to_owned(),
        autostart: true,
        last_status_actor: "human".to_owned(),
        priority: "medium".to_owned(),
        created_via: "cli".to_owned(),
        repo_remote_url: None,
        archived_reason: None,
        blocked_reason: None,
        blocked_attempt_id: None,
        blocked_signals: vec![],
        effort_level: None,
        model_override: None,
        driver: None,
        ci_attempt_budget: None,
        ci_attempts_used: 0,
        ci_required_state: None,
        ci_required_detail: None,
        review_required_state: None,
        review_required_detail: None,
        pr_state_polled_at: None,
        merge_queue_state: None,
        merge_queue_detail: None,
        external_ref: None,
        parent_task_id: parent.map(str::to_owned),
        revision_seq: None,
        revision_parent_pr_url: None,
        has_in_progress_revision: false,
        source_automation_id: None,
        review_cycle: 0,
        last_reviewed_sha: None,
        ai_reviewing: false,
        doc_link_state: None,
        origin_task_short_id: None,
        origin_pr_number: None,
        completed_at: None,
        dispatch_failed_reason: None,
        dispatch_failed_error: None,
        dispatch_failed_at: None,
    }
}

mod t01;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
mod t08;
mod t09;
mod t10;
mod t11;
mod t12;
mod t13;
mod t14;
mod t15;
mod t16;
mod t17;
mod t18;
mod t19;
mod t20;
mod t21;
mod t22;
mod t23;
mod t24;
mod t25;
mod t26;
mod t27;
mod t28;
mod t29;
mod t30;
