use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct WorkDb {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub repo_remote_url: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: String,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub priority: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub product_id: String,
    pub project_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub ordinal: Option<i64>,
    pub pr_url: Option<String>,
    pub deleted_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkTree {
    pub product: Product,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub chores: Vec<Task>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "item_type", rename_all = "snake_case")]
pub enum WorkItem {
    Product(Product),
    Project(Project),
    Task(Task),
    Chore(Task),
}

#[derive(Debug, Deserialize)]
pub struct CreateProductInput {
    pub name: String,
    pub description: Option<String>,
    pub repo_remote_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateProjectInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
    pub goal: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskInput {
    pub product_id: String,
    pub project_id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateChoreInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkItemPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub goal: Option<String>,
    pub priority: Option<String>,
    pub repo_remote_url: Option<String>,
    pub pr_url: Option<String>,
    pub ordinal: Option<i64>,
}

impl WorkDb {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work db directory {}", parent.display())
            })?;
        }

        let db = Self { path };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list_products(&self) -> Result<Vec<Product>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
             FROM products
             ORDER BY name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], map_product)?;
        collect_rows(rows)
    }

    pub fn create_product(&self, input: CreateProductInput) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let id = next_id("prod");
        let now = now_string();
        let slug = unique_product_slug(&tx, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let repo_remote_url = normalize_optional_text(input.repo_remote_url);

        tx.execute(
            "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6)",
            params![id, input.name, slug, description, repo_remote_url, now],
        )?;

        let product = query_product(&tx, &id)?
            .with_context(|| format!("missing product after insert: {id}"))?;
        tx.commit()?;
        Ok(product)
    }

    pub fn list_projects(&self, product_id: &str) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
             FROM projects
             WHERE product_id = ?1
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([product_id], map_project)?;
        collect_rows(rows)
    }

    pub fn create_project(&self, input: CreateProjectInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("proj");
        let now = now_string();
        let slug = unique_project_slug(&tx, &input.product_id, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let goal = input.goal.unwrap_or_default();

        tx.execute(
            "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', 'medium', ?7, ?7)",
            params![id, input.product_id, input.name, slug, description, goal, now],
        )?;

        let project = query_project(&tx, &id)?
            .with_context(|| format!("missing project after insert: {id}"))?;
        tx.commit()?;
        Ok(project)
    }

    pub fn create_task(&self, input: CreateTaskInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;
        ensure_project_belongs_to_product(&tx, &input.project_id, &input.product_id)?;

        let id = next_id("task");
        let now = now_string();
        let ordinal = next_task_ordinal(&tx, &input.project_id)?;
        let description = input.description.unwrap_or_default();

        tx.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7)",
            params![id, input.product_id, input.project_id, input.name, description, ordinal, now],
        )?;

        let task =
            query_task(&tx, &id)?.with_context(|| format!("missing task after insert: {id}"))?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_chore(&self, input: CreateChoreInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("task");
        let now = now_string();
        let description = input.description.unwrap_or_default();

        tx.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at)
             VALUES (?1, ?2, NULL, 'chore', ?3, ?4, 'todo', NULL, NULL, NULL, ?5, ?5)",
            params![id, input.product_id, input.name, description, now],
        )?;

        let task =
            query_task(&tx, &id)?.with_context(|| format!("missing chore after insert: {id}"))?;
        tx.commit()?;
        Ok(task)
    }

    pub fn update_work_item(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        match classify_id(id)? {
            ItemKind::Product => self.update_product(id, patch),
            ItemKind::Project => self.update_project(id, patch),
            ItemKind::Task => self.update_task(id, patch),
        }
    }

    pub fn delete_work_item(&self, id: &str) -> Result<()> {
        match classify_id(id)? {
            ItemKind::Task => {
                let mut conn = self.connect()?;
                let tx = conn.transaction()?;
                let now = now_string();
                let rows = tx.execute(
                    "UPDATE tasks SET deleted_at = ?2, updated_at = ?2
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, now],
                )?;
                if rows == 0 {
                    bail!("unknown task: {id}");
                }
                tx.commit()?;
                Ok(())
            }
            ItemKind::Product => bail!("product deletion is not supported; archive it instead"),
            ItemKind::Project => bail!("project deletion is not supported; archive it instead"),
        }
    }

    pub fn get_work_tree(&self, product_id: &str) -> Result<WorkTree> {
        let conn = self.connect()?;
        let product = query_product(&conn, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;

        let projects = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
                 FROM projects
                 WHERE product_id = ?1
                 ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )?;
            let rows = stmt.query_map([product_id], map_project)?;
            collect_rows(rows)?
        };

        let tasks = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        let chores = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        Ok(WorkTree {
            product,
            projects,
            tasks,
            chores,
        })
    }

    pub fn reorder_project_tasks(&self, project_id: &str, task_ids: &[String]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_project_exists(&tx, project_id)?;

        let mut existing = {
            let mut stmt = tx.prepare(
                "SELECT id
                 FROM tasks
                 WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
            collect_rows(rows)?
        };
        let mut requested = task_ids.to_vec();
        existing.sort();
        requested.sort();
        if existing != requested {
            bail!("reorder request must include the full active task set for the project");
        }

        for (index, task_id) in task_ids.iter().enumerate() {
            tx.execute(
                "UPDATE tasks SET ordinal = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, (index as i64) + 1, now_string()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    fn init(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                repo_remote_url TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                goal TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
                ON projects(product_id, slug);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                project_id TEXT REFERENCES projects(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                ordinal INTEGER,
                pr_url TEXT,
                deleted_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS tasks_product_idx
                ON tasks(product_id, kind, deleted_at);

            CREATE INDEX IF NOT EXISTS tasks_project_idx
                ON tasks(project_id, deleted_at, ordinal);
            ",
        )?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("failed to open work db {}", self.path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product =
            query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_optional_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_text_patch(&mut product.status, patch.status);
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
            ],
        )?;

        let updated = query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    fn update_project(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut project =
            query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;

        apply_text_patch(&mut project.name, patch.name);
        apply_text_patch(&mut project.description, patch.description);
        apply_text_patch(&mut project.goal, patch.goal);
        apply_text_patch(&mut project.status, patch.status);
        apply_text_patch(&mut project.priority, patch.priority);
        project.slug =
            unique_project_slug_for_update(&tx, &project.product_id, id, &slugify(&project.name))?;
        project.updated_at = now_string();

        tx.execute(
            "UPDATE projects
             SET name = ?2, slug = ?3, description = ?4, goal = ?5, status = ?6, priority = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.description,
                project.goal,
                project.status,
                project.priority,
                project.updated_at,
            ],
        )?;

        let updated = query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Project(updated))
    }

    fn update_task(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut task = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot update a deleted task: {id}");
        }

        apply_text_patch(&mut task.name, patch.name);
        apply_text_patch(&mut task.description, patch.description);
        apply_text_patch(&mut task.status, patch.status);
        apply_optional_patch(&mut task.pr_url, patch.pr_url);
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                task.id,
                task.name,
                task.description,
                task.status,
                task.ordinal,
                task.pr_url,
                task.updated_at,
            ],
        )?;

        let updated = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        tx.commit()?;
        Ok(task_to_item(updated))
    }
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn map_product(row: &Row<'_>) -> rusqlite::Result<Product> {
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        repo_remote_url: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        slug: row.get(3)?,
        description: row.get(4)?,
        goal: row.get(5)?,
        status: row.get(6)?,
        priority: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        product_id: row.get(1)?,
        project_id: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        description: row.get(5)?,
        status: row.get(6)?,
        ordinal: row.get(7)?,
        pr_url: row.get(8)?,
        deleted_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn query_product(conn: &Connection, id: &str) -> Result<Option<Product>> {
    conn.query_row(
        "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
         FROM products
         WHERE id = ?1",
        [id],
        map_product,
    )
    .optional()
    .map_err(Into::into)
}

fn query_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    conn.query_row(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
         FROM projects
         WHERE id = ?1",
        [id],
        map_project,
    )
    .optional()
    .map_err(Into::into)
}

fn query_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    conn.query_row(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
         FROM tasks
         WHERE id = ?1",
        [id],
        map_task,
    )
    .optional()
    .map_err(Into::into)
}

fn ensure_product_exists(conn: &Connection, product_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE id = ?1)",
        [product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown product: {product_id}");
    }
    Ok(())
}

fn ensure_project_exists(conn: &Connection, project_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown project: {project_id}");
    }
    Ok(())
}

fn ensure_project_belongs_to_product(
    conn: &Connection,
    project_id: &str,
    product_id: &str,
) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND product_id = ?2)",
        params![project_id, product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("project {project_id} does not belong to product {product_id}");
    }
    Ok(())
}

fn next_task_ordinal(conn: &Connection, project_id: &str) -> Result<i64> {
    let current = conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) FROM tasks
             WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(current + 1)
}

fn unique_product_slug(conn: &Connection, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1)",
        [candidate.as_str()],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_product_slug_for_update(conn: &Connection, id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1 AND id != ?2)",
        params![candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug(conn: &Connection, product_id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2)",
        params![product_id, candidate],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug_for_update(
    conn: &Connection,
    product_id: &str,
    id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2 AND id != ?3)",
        params![product_id, candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn default_slug(base_slug: &str) -> String {
    if base_slug.is_empty() {
        "item".to_owned()
    } else {
        base_slug.to_owned()
    }
}

fn next_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos:x}_{counter:x}")
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn apply_text_patch(target: &mut String, patch: Option<String>) {
    if let Some(value) = patch {
        *target = value;
    }
}

fn apply_optional_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = normalize_optional_text(Some(value));
    }
}

fn task_to_item(task: Task) -> WorkItem {
    if task.kind == "chore" {
        WorkItem::Chore(task)
    } else {
        WorkItem::Task(task)
    }
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    slug.trim_matches('-').to_owned()
}

enum ItemKind {
    Product,
    Project,
    Task,
}

fn classify_id(id: &str) -> Result<ItemKind> {
    if id.starts_with("prod_") {
        return Ok(ItemKind::Product);
    }
    if id.starts_with("proj_") {
        return Ok(ItemKind::Project);
    }
    if id.starts_with("task_") {
        return Ok(ItemKind::Task);
    }
    bail!("unknown work item id format: {id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(label: &str) -> PathBuf {
        let file = format!("boss-{label}-{}.sqlite3", next_id("test"));
        std::env::temp_dir().join(file)
    }

    #[test]
    fn creates_tree_and_soft_deletes_chores() {
        let path = temp_db_path("tree");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: Some("desc".to_owned()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Work taxonomy".to_owned(),
                description: None,
                goal: Some("goal".to_owned()),
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Backend schema".to_owned(),
                description: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.projects.len(), 1);
        assert_eq!(tree.tasks.len(), 1);
        assert_eq!(tree.tasks[0].id, task.id);
        assert_eq!(tree.chores.len(), 1);
        assert_eq!(tree.chores[0].id, chore.id);

        db.delete_work_item(&chore.id).unwrap();
        let tree = db.get_work_tree(&product.id).unwrap();
        assert!(tree.chores.is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reorders_project_tasks() {
        let path = temp_db_path("reorder");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Taxonomy".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let first = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "One".to_owned(),
                description: None,
            })
            .unwrap();
        let second = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Two".to_owned(),
                description: None,
            })
            .unwrap();

        db.reorder_project_tasks(&project.id, &[second.id.clone(), first.id.clone()])
            .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.tasks[0].id, second.id);
        assert_eq!(tree.tasks[1].id, first.id);

        let _ = std::fs::remove_file(path);
    }
}
