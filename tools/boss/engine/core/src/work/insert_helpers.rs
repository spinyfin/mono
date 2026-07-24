use super::*;

/// Insert a `ci_failure_suppressions` row for the work item, keyed by
/// the head sha of the most recent `ci_remediations` attempt. Called
/// from `update_task` when a human moves a chore out of `blocked:
/// ci_failure` (or `ci_failure_exhausted`) — see design §Q5 ("Manual
/// override (CI)") and Phase 12 #38. The function is best-effort:
/// when no `ci_remediations` row exists (the chore was manually moved
/// without the engine having ever recorded an attempt — e.g. a budget=0
/// `notify only` flow) we leave the table alone — the engine has no
/// head sha to suppress against and the next probe will simply
/// re-observe the failure.
///
/// We also reset `ci_attempts_used` so the next CI failure (on a new
/// head sha — the suppression has expired by then) starts with a
/// fresh budget; mirrors the manual `boss engine ci retry` reset rule.
/// The reset happens on both paths, including the no-attempt one.
pub(crate) fn record_ci_failure_suppression_in_tx(conn: &Connection, work_item_id: &str, now: &str) -> Result<()> {
    // The most recent `ci_remediations` row carries the head sha the
    // engine was reacting to. Prefer the latest attempt regardless of
    // status — the user may be moving off `ci_failure_exhausted`, in
    // which case the row is terminal but its head sha is still what
    // we should suppress.
    let head_sha: Option<String> = conn
        .query_row(
            "SELECT head_sha_at_trigger FROM ci_remediations
              WHERE work_item_id = ?1
              ORDER BY created_at DESC, id DESC
              LIMIT 1",
            params![work_item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(head_sha) = head_sha {
        conn.execute(
            "INSERT OR REPLACE INTO ci_failure_suppressions
                 (work_item_id, head_sha, created_at)
             VALUES (?1, ?2, ?3)",
            params![work_item_id, head_sha, now],
        )?;
    } else {
        tracing::debug!(
            work_item_id,
            "record_ci_failure_suppression_in_tx: no ci_remediations row; skipping suppression insert",
        );
    }
    // Reset the per-PR budget so a future fresh-head failure starts
    // clean. The reset is unconditional within this code path —
    // pulling a row out of `ci_failure` is itself an override of the
    // budget logic.
    conn.execute(
        "UPDATE tasks
            SET ci_attempts_used = 0
          WHERE id = ?1
            AND deleted_at IS NULL",
        params![work_item_id],
    )?;
    Ok(())
}

/// Upsert the multi-signal side table for a `(work_item_id, reason)`
/// pair. The PK collapses repeat observations to one row; we reset
/// `cleared_at` to NULL on re-observation so the same signal flapping
/// in and out lands as one row with the latest `created_at`.
///
/// `attempt_id` is the soft FK that the design's §Q2 stores so the UI
/// can navigate from a signal back to its attempt row; `None` for
/// `'dependency'` (which has no attempt table) and for the
/// `'ci_failure_exhausted'` signal (which is the *absence* of an
/// engine-managed attempt — the engine has stopped trying).
pub(crate) fn upsert_task_blocked_signal(
    conn: &Connection,
    work_item_id: &str,
    reason: &str,
    attempt_id: Option<&str>,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO task_blocked_signals
             (work_item_id, reason, attempt_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(work_item_id, reason) DO UPDATE SET
             attempt_id = COALESCE(excluded.attempt_id, task_blocked_signals.attempt_id),
             cleared_at = NULL",
        params![work_item_id, reason, attempt_id, now],
    )?;
    Ok(())
}

/// Check whether a non-deleted task/chore with the same trimmed name
/// exists in the same product and was created within `DUPLICATE_GUARD_WINDOW_SECS`.
/// Returns `Some(DuplicateTaskError)` when the guard fires, `None` otherwise.
pub(crate) fn check_recent_duplicate(
    conn: &Connection,
    product_id: &str,
    name: &str,
) -> Result<Option<DuplicateTaskError>> {
    let trimmed = name.trim();
    let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let cutoff = now_secs - DUPLICATE_GUARD_WINDOW_SECS;

    let row: Option<(String, Option<i64>, i64)> = conn
        .query_row(
            "SELECT id, short_id, CAST(created_at AS INTEGER)
             FROM tasks
             WHERE product_id = ?1
               AND trim(name) = ?2
               AND deleted_at IS NULL
               AND CAST(created_at AS INTEGER) >= ?3
             ORDER BY CAST(created_at AS INTEGER) DESC
             LIMIT 1",
            params![product_id, trimmed, cutoff],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    Ok(
        row.map(|(existing_id, existing_short_id, created_at)| DuplicateTaskError {
            existing_id,
            existing_short_id: existing_short_id.unwrap_or(0),
            name: trimmed.to_owned(),
            age_secs: now_secs - created_at,
        }),
    )
}

pub(crate) fn insert_task_in_tx(conn: &Connection, input: CreateTaskInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    ensure_project_belongs_to_product(conn, &input.project_id, &input.product_id)?;

    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, &input.project_id)?;
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let deferred_value: i64 = if input.deferred { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "task");
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, deferred)
         VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![id, input.product_id, input.project_id, input.name, description, ordinal, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, deferred_value],
    )?;

    apply_create_time_dependencies(conn, &id, &input.depends_on, &now)?;
    query_task(conn, &id)?.with_context(|| format!("missing task after insert: {id}"))
}

/// Declare each `--depends-on` prerequisite as a `blocks` edge in the
/// SAME transaction as the freshly-inserted work item. This is the fix
/// for the create→`depend add` race: by the time this transaction
/// commits, the row is already `blocked` (if any prerequisite is
/// unsatisfied), so the auto-dispatcher's reconcile sees the gate and
/// parks the execution in `waiting_dependency` instead of dispatching a
/// worker. `prerequisite_ids` are canonical work-item ids — the caller
/// (CLI) resolves selectors like `T42` before sending. A freshly
/// created row can't have a live worker, so the cancelled-execution
/// channel of [`add_dependency_edge_in_tx`] is always empty here.
pub(crate) fn apply_create_time_dependencies(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_ids: &[String],
    now: &str,
) -> Result<()> {
    for prerequisite_id in prerequisite_ids {
        let prerequisite_id = prerequisite_id.trim();
        if prerequisite_id.is_empty() {
            continue;
        }
        add_dependency_edge_in_tx(conn, dependent_id, prerequisite_id, RELATION_BLOCKS, now)
            .with_context(|| format!("declaring create-time dependency on `{prerequisite_id}`"))?;
    }
    Ok(())
}

pub(crate) fn insert_chore_in_tx(conn: &Connection, input: CreateChoreInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;

    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let deferred_value: i64 = if input.deferred { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let kind_str = input.kind_override.as_ref().map(|k| k.as_str()).unwrap_or("chore");
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, kind_str);
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, origin_task_short_id, origin_pr_number, deferred)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![id, input.product_id, kind_str, input.name, description, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, input.origin_task_short_id, input.origin_pr_number, deferred_value],
    )?;

    apply_create_time_dependencies(conn, &id, &input.depends_on, &now)?;
    query_task(conn, &id)?.with_context(|| format!("missing chore after insert: {id}"))
}

/// Insert a `kind = 'investigation'` task. Mirrors `insert_chore_in_tx`
/// but uses `investigation` kind and accepts an optional `project_id`.
/// The repo stored on the task row is the investigation deliverable repo
/// (product `docs_repo` or `BOSS_USER_DOCS_REPO`), not the product's
/// code repo — `enforce_task_repo_invariant` is NOT called so the
/// override can point at a docs-only repo without triggering the
/// same-product check.
pub(crate) fn insert_investigation_in_tx(
    conn: &Connection,
    input: boss_protocol::CreateInvestigationInput,
) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    if let Some(ref pid) = input.project_id {
        ensure_project_belongs_to_product(conn, pid, &input.product_id)?;
    }
    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }
    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let deferred_value: i64 = if input.deferred { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "investigation");
    let repo_remote_url = input.repo_remote_url.filter(|s| !s.is_empty());
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, deferred)
         VALUES (?1, ?2, ?3, 'investigation', ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            id, input.product_id, input.project_id, input.name, description, now,
            autostart_value, priority, created_via, repo_remote_url,
            effort_level, model_override, driver, short_id, deferred_value
        ],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing investigation after insert: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_product_named, open_db};
    use boss_engine_utils::epoch_time::now_epoch_secs;

    // ── helpers ─────────────────────────────────────────────────────────────

    /// Create a chore through the public surface, keeping the `Result` so
    /// tests can assert on the duplicate guard firing.
    fn try_chore(db: &WorkDb, product_id: &str, name: &str) -> Result<Task> {
        db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
    }

    /// Create a chore that bypasses the guard — used to plant a second
    /// same-named row that the guard is then asked to match against.
    fn forced_chore(db: &WorkDb, product_id: &str, name: &str) -> Task {
        db.create_chore(
            CreateChoreInput::builder()
                .product_id(product_id)
                .name(name)
                .force_duplicate(true)
                .build(),
        )
        .unwrap()
    }

    /// The guard reads wall-clock time via `now_epoch_secs()`, so tests
    /// control a row's age by rewriting `created_at` rather than sleeping.
    fn backdate(db: &WorkDb, task_id: &str, secs_ago: i64) {
        let conn = db.connect().unwrap();
        let ts = (now_epoch_secs() - secs_ago).to_string();
        conn.execute("UPDATE tasks SET created_at = ?2 WHERE id = ?1", params![task_id, ts])
            .unwrap();
    }

    /// Unwrap the guard's error as the typed `DuplicateTaskError`.
    fn expect_duplicate(err: anyhow::Error) -> DuplicateTaskError {
        err.downcast::<DuplicateTaskError>()
            .expect("the guard must report a typed DuplicateTaskError")
    }

    // ── check_recent_duplicate ──────────────────────────────────────────────

    /// The core guard: a same-named, non-deleted sibling created inside
    /// the window blocks the insert and names the row it collided with.
    #[test]
    fn same_name_inside_window_is_reported_as_duplicate() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let first = try_chore(&db, &product.id, "Ship the thing").unwrap();

        let err = try_chore(&db, &product.id, "Ship the thing").expect_err("guard must fire");
        let dup = expect_duplicate(err);

        assert_eq!(dup.existing_id, first.id);
        assert_eq!(dup.name, "Ship the thing");
        assert_eq!(dup.existing_short_id, first.short_id.unwrap());
    }

    /// The window is a sliding one: once the sibling ages past
    /// `DUPLICATE_GUARD_WINDOW_SECS` the same name is allowed again.
    #[test]
    fn same_name_outside_window_is_not_a_duplicate() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let first = try_chore(&db, &product.id, "Ship the thing").unwrap();
        backdate(&db, &first.id, DUPLICATE_GUARD_WINDOW_SECS + 5);

        let second = try_chore(&db, &product.id, "Ship the thing")
            .expect("a sibling older than the window must not block the insert");
        assert_ne!(second.id, first.id);
    }

    /// A soft-deleted row is invisible to the guard — re-filing a task
    /// you just deleted must not be reported as a duplicate of it.
    #[test]
    fn soft_deleted_sibling_does_not_trigger_the_guard() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let first = try_chore(&db, &product.id, "Ship the thing").unwrap();
        db.delete_work_item(&first.id).unwrap();

        let second =
            try_chore(&db, &product.id, "Ship the thing").expect("a soft-deleted sibling must not block the insert");
        assert_ne!(second.id, first.id);
    }

    /// The guard is scoped per product: two products may each hold a
    /// task with the same name at the same instant.
    #[test]
    fn guard_is_scoped_per_product() {
        let (_dir, db) = open_db();
        let product_a = create_test_product_named(&db, "A");
        let product_b = create_test_product_named(&db, "B");
        try_chore(&db, &product_a.id, "Ship the thing").unwrap();

        try_chore(&db, &product_b.id, "Ship the thing").expect("the same name in a different product must be allowed");
    }

    /// Names are compared trimmed on both sides, so surrounding
    /// whitespace cannot smuggle a duplicate past the guard. The
    /// reported `name` is the trimmed form.
    #[test]
    fn names_are_matched_after_trimming_on_both_sides() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        // Stored untrimmed; the guard trims the column as well as the input.
        let first = try_chore(&db, &product.id, "  Ship the thing  ").unwrap();

        let err = try_chore(&db, &product.id, "Ship the thing").expect_err("guard must fire");
        let dup = expect_duplicate(err);
        assert_eq!(dup.existing_id, first.id);
        assert_eq!(dup.name, "Ship the thing");

        // ...and symmetrically, a padded *input* matches a stored bare name.
        let err = try_chore(&db, &product.id, " Ship the thing ").expect_err("guard must fire");
        assert_eq!(expect_duplicate(err).name, "Ship the thing");
    }

    /// With several in-window matches the guard reports the most recent
    /// one — that's the row the user most likely just created by mistake.
    #[test]
    fn most_recent_match_is_reported_when_several_exist() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let older = try_chore(&db, &product.id, "Ship the thing").unwrap();
        backdate(&db, &older.id, 50);
        let newer = forced_chore(&db, &product.id, "Ship the thing");
        backdate(&db, &newer.id, 10);

        let dup = expect_duplicate(try_chore(&db, &product.id, "Ship the thing").expect_err("guard must fire"));
        assert_eq!(
            dup.existing_id, newer.id,
            "the most recently created in-window match must be the one reported",
        );
    }

    /// `age_secs` is derived from the matched row's `created_at`, not
    /// from the guard window — it is what the CLI shows the operator.
    #[test]
    fn age_secs_reflects_the_matched_rows_created_at() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let first = try_chore(&db, &product.id, "Ship the thing").unwrap();
        backdate(&db, &first.id, 30);

        let dup = expect_duplicate(try_chore(&db, &product.id, "Ship the thing").expect_err("guard must fire"));
        // Bounded rather than exact: the clock may tick mid-test.
        assert!(
            (30..=32).contains(&dup.age_secs),
            "age_secs should track the backdated created_at, got {}",
            dup.age_secs,
        );
    }

    /// Legacy rows predating the short-id column report `T0` rather
    /// than failing the insert path with a NULL decode error.
    #[test]
    fn existing_short_id_falls_back_to_zero_when_null() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let first = try_chore(&db, &product.id, "Ship the thing").unwrap();
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET short_id = NULL WHERE id = ?1", params![first.id])
            .unwrap();
        drop(conn);

        let dup = expect_duplicate(try_chore(&db, &product.id, "Ship the thing").expect_err("guard must fire"));
        assert_eq!(dup.existing_short_id, 0);
    }

    // ── record_ci_failure_suppression_in_tx ─────────────────────────────────
    //
    // The happy paths — a suppression row is written on a manual move
    // out of `ci_failure` / `ci_failure_exhausted`, is scoped to one
    // head sha, and non-CI moves write nothing — are covered in
    // `work/tests/t04.rs`. These cover the branches that file leaves
    // open: attempt selection across several rows, the no-attempt
    // fallback, and repeat overrides on one head sha.

    /// Count suppression rows for a work item — the observable outcome
    /// of the helper, without asserting on how the row got there.
    fn suppression_rows(db: &WorkDb, work_item_id: &str) -> i64 {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM ci_failure_suppressions WHERE work_item_id = ?1",
                params![work_item_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Drive a chore to `in_review` with `pr_url` set — the state
    /// `mark_chore_blocked_ci_failure`'s WHERE guard requires.
    fn chore_in_review(db: &WorkDb, product_id: &str, name: &str, pr_url: &str) -> Task {
        let chore = try_chore(db, product_id, name).unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        chore
    }

    fn insert_attempt(db: &WorkDb, product_id: &str, chore_id: &str, pr_url: &str, head_sha: &str) -> String {
        db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number: 1,
            head_branch: "feature".into(),
            head_sha_at_trigger: head_sha.to_owned(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("attempt insert must not collide")
        .id
    }

    fn backdate_attempt(db: &WorkDb, attempt_id: &str, secs_ago: i64) {
        let conn = db.connect().unwrap();
        let ts = (now_epoch_secs() - secs_ago).to_string();
        conn.execute(
            "UPDATE ci_remediations SET created_at = ?2 WHERE id = ?1",
            params![attempt_id, ts],
        )
        .unwrap();
    }

    /// The human moves the chore out of the blocked column.
    fn manual_override_to_in_review(db: &WorkDb, chore_id: &str) {
        db.update_work_item(
            chore_id,
            WorkItemPatch {
                status: Some("in_review".into()),
                blocked_reason: Some("".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    }

    /// Attempt selection is by recency alone. The newest attempt here
    /// is terminal (`succeeded`) while an older one is still pending —
    /// suppression must still key to the newest attempt's head sha,
    /// because that is the head the engine was last reacting to.
    #[test]
    fn suppression_keys_to_the_latest_attempt_regardless_of_its_status() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let pr_url = "https://github.com/spinyfin/mono/pull/1";
        let chore = chore_in_review(&db, &product.id, "ci chore", pr_url);

        let older = insert_attempt(&db, &product.id, &chore.id, pr_url, "head-older");
        backdate_attempt(&db, &older, 120);
        let newer = insert_attempt(&db, &product.id, &chore.id, pr_url, "head-newer");
        backdate_attempt(&db, &newer, 10);
        // Newest attempt is terminal; the older one stays pending.
        db.mark_ci_remediation_succeeded(&newer, None).unwrap();

        db.mark_chore_blocked_ci_failure(&chore.id, pr_url, None).unwrap();
        manual_override_to_in_review(&db, &chore.id);

        assert!(
            db.is_ci_failure_suppressed(&chore.id, "head-newer").unwrap(),
            "suppression must key to the latest attempt even though it is terminal",
        );
        assert!(
            !db.is_ci_failure_suppressed(&chore.id, "head-older").unwrap(),
            "an older attempt's head sha must not be suppressed",
        );
    }

    /// A chore can reach `blocked: ci_failure` with no attempt row at
    /// all (e.g. a budget=0 notify-only flow). The override must then
    /// be a no-op on the suppression table rather than an error — and
    /// the budget reset must still happen.
    #[test]
    fn override_with_no_attempt_row_writes_no_suppression_but_still_resets_budget() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let pr_url = "https://github.com/spinyfin/mono/pull/2";
        let chore = chore_in_review(&db, &product.id, "ci chore", pr_url);

        db.mark_chore_blocked_ci_failure(&chore.id, pr_url, None).unwrap();
        db.increment_ci_attempts_used(&chore.id).unwrap();
        assert_eq!(db.get_ci_attempts_used(&chore.id).unwrap(), 1);

        // No `ci_remediations` row exists — this must not raise.
        manual_override_to_in_review(&db, &chore.id);

        assert_eq!(
            suppression_rows(&db, &chore.id),
            0,
            "with no attempt row there is no head sha to suppress against",
        );
        assert_eq!(
            db.get_ci_attempts_used(&chore.id).unwrap(),
            0,
            "the budget reset is unconditional, even when no suppression is written",
        );
    }

    /// Repeat overrides against one head sha collapse to a single row
    /// (the table's PK plus `INSERT OR REPLACE`), so a human bouncing
    /// the card in and out cannot accumulate duplicates.
    #[test]
    fn repeat_override_on_the_same_head_sha_collapses_to_one_row() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "P");
        let pr_url = "https://github.com/spinyfin/mono/pull/3";
        let chore = chore_in_review(&db, &product.id, "ci chore", pr_url);
        insert_attempt(&db, &product.id, &chore.id, pr_url, "head-aaa");

        for _ in 0..2 {
            assert!(
                db.mark_chore_blocked_ci_failure(&chore.id, pr_url, None)
                    .unwrap()
                    .is_some(),
                "the chore must be re-blockable from in_review",
            );
            manual_override_to_in_review(&db, &chore.id);
        }

        assert_eq!(
            suppression_rows(&db, &chore.id),
            1,
            "a second override on the same head sha must replace, not append",
        );
        assert!(db.is_ci_failure_suppressed(&chore.id, "head-aaa").unwrap());
    }
}
