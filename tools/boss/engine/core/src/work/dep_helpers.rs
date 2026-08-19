use super::*;

pub(crate) fn slugify(name: &str) -> String {
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

pub(crate) enum ItemKind {
    Product,
    Project,
    Task,
    /// A `work_comments` row id (`cmt_…`). Answer-agent executions bind the
    /// comment they answer into `work_executions.work_item_id`; reconciliation
    /// must classify that id rather than fail with "unknown work item id
    /// format". Comments are not product/project/task work items — callers
    /// that need the row use [`WorkDb::get_comment`]; closedness for recon
    /// uses [`WorkDb::is_bound_work_item_closed`].
    Comment,
}

/// One candidate from a short-id lookup. Listed in full when a short id
/// is ambiguous across products so the caller can disambiguate.
/// Keep field count ≤5 (repo giant-struct rule); product identity is one
/// display string (`slug (name)`).
#[derive(Debug, Clone)]
pub(crate) struct ShortIdCandidate {
    pub id: String,
    pub product: String,
    pub name: String,
    pub status: String,
}

impl ShortIdCandidate {
    fn display_line(&self) -> String {
        format!(
            "  - {id}  product={product}  name={name:?}  status={status}",
            id = self.id,
            product = self.product,
            name = self.name,
            status = self.status,
        )
    }
}

/// Format an ambiguity error that lists every candidate (long id, product,
/// name, status). Never pick one — callers must disambiguate.
///
/// Guidance covers both surfaces that take `--product` and those that
/// do not (`create-revision`, `bossctl dispatch diagnose`, `work
/// executions`, `review show`): `slug/n` and the primary `task_…` /
/// `proj_…` id work everywhere.
pub(crate) fn format_short_id_ambiguous(input: &str, candidates: &[ShortIdCandidate]) -> String {
    let mut lines = vec![format!(
        "could not resolve id {input}: {} across {} products; \
         pass --product <slug>, use the product-scoped `slug/n` form, \
         or a primary id (`task_…` / `proj_…`). candidates:",
        boss_protocol::WORK_ITEM_ID_AMBIGUOUS_MARKER,
        candidates.len()
    )];
    for c in candidates {
        lines.push(c.display_line());
    }
    lines.join("\n")
}

/// If `id` looks like a friendly work-item selector (`T42`, `t42`, `P7`,
/// `p7`, `#42`, bare `42`, or `slug/42`), query the DB by short_id and
/// return the matching primary id.
///
/// Returns:
/// - `Ok(Some(primary))` when exactly one live row matches
/// - `Ok(None)` when `id` is not a friendly-id form, or when no row
///   matches (callers that want a hard not-found should check
///   [`boss_protocol::is_friendly_work_item_selector`] and error)
/// - `Err` when the short id matches more than one product — the error
///   message lists every candidate (long id, product, name, status).
///   Never silently picks one.
pub(crate) fn resolve_friendly_work_item_id(conn: &Connection, id: &str) -> Result<Option<String>> {
    resolve_friendly_work_item_id_inner(conn, id, false)
}

/// Variant of [`resolve_friendly_work_item_id`] that, when
/// `include_deleted` is true, resolves a `T<n>` short id even if its
/// task row carries a `deleted_at` tombstone. Only `restore` needs
/// this — every other resolution path wants the live-only view and
/// calls through the plain wrapper above.
pub(crate) fn resolve_friendly_work_item_id_inner(
    conn: &Connection,
    id: &str,
    include_deleted: bool,
) -> Result<Option<String>> {
    let selector = boss_protocol::parse_work_item_selector(id);
    let (short_id, product_scope) = match selector {
        boss_protocol::WorkItemSelector::ShortId(n) => (n, None),
        boss_protocol::WorkItemSelector::ProductShortId { product_slug, n } => {
            // Resolve slug → product_id first; unknown slug is not a match.
            let product_id: Option<String> = conn
                .query_row(
                    "SELECT id FROM products WHERE slug = ?1 OR id = ?1",
                    params![product_slug],
                    |row| row.get(0),
                )
                .optional()?;
            match product_id {
                Some(pid) => (n, Some(pid)),
                None => return Ok(None),
            }
        }
        boss_protocol::WorkItemSelector::PrimaryId(_) | boss_protocol::WorkItemSelector::Other(_) => {
            return Ok(None);
        }
    };
    let candidates = lookup_short_id_candidates(conn, short_id, product_scope.as_deref(), include_deleted)?;
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(candidates.into_iter().next().unwrap().id)),
        _ => bail!("{}", format_short_id_ambiguous(id, &candidates)),
    }
}

/// Look up work-item rows with the given short_id, optionally scoped to
/// one product. Live rows always match. When `include_deleted` is false,
/// archived-and-tombstoned rows also match so the friendly short-id
/// form agrees with primary-id retrieval; other soft-deleted rows stay
/// hidden. When
/// `include_deleted` is true (restore), every tombstone matches.
fn lookup_short_id_candidates(
    conn: &Connection,
    short_id: i64,
    product_id: Option<&str>,
    include_deleted: bool,
) -> Result<Vec<ShortIdCandidate>> {
    let mut out = Vec::new();

    let task_sql = match (product_id.is_some(), include_deleted) {
        (true, true) => {
            "SELECT t.id, t.product_id, p.slug, p.name, t.name, t.status
             FROM tasks t JOIN products p ON p.id = t.product_id
             WHERE t.short_id = ?1 AND t.product_id = ?2"
        }
        (true, false) => {
            "SELECT t.id, t.product_id, p.slug, p.name, t.name, t.status
             FROM tasks t JOIN products p ON p.id = t.product_id
             WHERE t.short_id = ?1 AND t.product_id = ?2
              AND (t.deleted_at IS NULL OR t.status = 'archived')"
        }
        (false, true) => {
            "SELECT t.id, t.product_id, p.slug, p.name, t.name, t.status
             FROM tasks t JOIN products p ON p.id = t.product_id
             WHERE t.short_id = ?1"
        }
        (false, false) => {
            "SELECT t.id, t.product_id, p.slug, p.name, t.name, t.status
             FROM tasks t JOIN products p ON p.id = t.product_id
             WHERE t.short_id = ?1
               AND (t.deleted_at IS NULL OR t.status = 'archived')"
        }
    };
    {
        let mut stmt = conn.prepare(task_sql)?;
        let rows = match product_id {
            Some(pid) => stmt.query_map(params![short_id, pid], map_short_id_candidate)?,
            None => stmt.query_map(params![short_id], map_short_id_candidate)?,
        };
        for row in rows {
            out.push(row?);
        }
    }

    let project_sql = if product_id.is_some() {
        "SELECT pr.id, pr.product_id, p.slug, p.name, pr.name, pr.status
         FROM projects pr JOIN products p ON p.id = pr.product_id
         WHERE pr.short_id = ?1 AND pr.product_id = ?2"
    } else {
        "SELECT pr.id, pr.product_id, p.slug, p.name, pr.name, pr.status
         FROM projects pr JOIN products p ON p.id = pr.product_id
         WHERE pr.short_id = ?1"
    };
    {
        let mut stmt = conn.prepare(project_sql)?;
        let rows = match product_id {
            Some(pid) => stmt.query_map(params![short_id, pid], map_short_id_candidate)?,
            None => stmt.query_map(params![short_id], map_short_id_candidate)?,
        };
        for row in rows {
            out.push(row?);
        }
    }

    Ok(out)
}

fn map_short_id_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShortIdCandidate> {
    let product_id: String = row.get(1)?;
    let product_slug: String = row.get(2)?;
    let product_name: String = row.get(3)?;
    Ok(ShortIdCandidate {
        id: row.get(0)?,
        product: format!("{product_slug} ({product_name}, {product_id})"),
        name: row.get(4)?,
        status: row.get(5)?,
    })
}

pub(crate) fn classify_id(id: &str) -> Result<ItemKind> {
    if id.starts_with("prod_") {
        return Ok(ItemKind::Product);
    }
    if id.starts_with("proj_") {
        return Ok(ItemKind::Project);
    }
    if id.starts_with("task_") {
        return Ok(ItemKind::Task);
    }
    if id.starts_with("cmt_") {
        return Ok(ItemKind::Comment);
    }
    bail!("unknown work item id format: {id}")
}

/// Cheap existence check for a typed primary id (`task_` / `proj_` /
/// `prod_`). Used by the shared resolver so verifying a primary id does
/// not pay for a full row read + doc-link attach (GetWorkItem fetches
/// once after resolve).
pub(crate) fn typed_work_item_exists(conn: &Connection, id: &str) -> Result<bool> {
    match classify_id(id)? {
        ItemKind::Task => Ok(conn
            .query_row(
                // Same inspectable-archive predicate as `get_work_item`:
                // live rows, plus archived rows even when tombstoned.
                "SELECT 1 FROM tasks
                 WHERE id = ?1 AND (deleted_at IS NULL OR status = 'archived')",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()),
        ItemKind::Project => Ok(conn
            .query_row("SELECT 1 FROM projects WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
        ItemKind::Product => Ok(conn
            .query_row("SELECT 1 FROM products WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
        ItemKind::Comment => Ok(conn
            .query_row("SELECT 1 FROM work_comments WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
    }
}

/// Like [`typed_work_item_exists`], but a tombstoned task row still counts
/// as existing. Used by resolvers that must see archived work items
/// (restore, the dependency verbs) so a typo'd id is still reported as
/// "no matching work item" instead of a silent no-op downstream.
pub(crate) fn typed_work_item_exists_including_deleted(conn: &Connection, id: &str) -> Result<bool> {
    match classify_id(id)? {
        ItemKind::Task => Ok(conn
            .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
        ItemKind::Project => Ok(conn
            .query_row("SELECT 1 FROM projects WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
        ItemKind::Product => Ok(conn
            .query_row("SELECT 1 FROM products WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
        ItemKind::Comment => Ok(conn
            .query_row("SELECT 1 FROM work_comments WHERE id = ?1", params![id], |_| Ok(()))
            .optional()?
            .is_some()),
    }
}

/// Resolve a single edge endpoint into its [`DependencyEdge`] view.
/// `peer_id` is the *other* end of the edge (the prerequisite when
/// the edge sits in the prerequisites list, the dependent when it
/// sits in the dependents list). Looks up the row's status / name /
/// kind so the view is fully self-contained. A peer that no longer
/// resolves (soft-deleted task; concurrent delete) renders as
/// `kind = "unknown"` with empty name and `status = "missing"` —
/// the human renderer surfaces it instead of dropping the row, so
/// the user can spot dangling edges and clean them up.
pub(crate) fn resolve_dependency_edge(conn: &Connection, peer_id: &str, relation: &str) -> Result<DependencyEdge> {
    if peer_id.starts_with("proj_") {
        if let Some(project) = query_project(conn, peer_id)? {
            return Ok(DependencyEdge {
                id: project.id,
                relation: relation.to_owned(),
                kind: "project".to_owned(),
                name: project.name,
                status: project.status.to_string(),
                archived_by: None,
                archived_at: None,
                archived_reason: None,
            });
        }
    } else if peer_id.starts_with("task_")
        && let Some(task) = query_task(conn, peer_id)?
    {
        let kind = match task.kind {
            TaskKind::Chore | TaskKind::Followup => "chore",
            _ => "task",
        };
        return Ok(DependencyEdge {
            id: task.id,
            relation: relation.to_owned(),
            kind: kind.to_owned(),
            name: task.name,
            status: task.status.to_string(),
            archived_by: task.archived_by,
            archived_at: task.archived_at,
            archived_reason: task.archived_reason,
        });
    }
    Ok(DependencyEdge {
        id: peer_id.to_owned(),
        relation: relation.to_owned(),
        kind: "unknown".to_owned(),
        name: String::new(),
        status: "missing".to_owned(),
        archived_by: None,
        archived_at: None,
        archived_reason: None,
    })
}

/// Mutate `items` in place to retain only the rows that match
/// `filter`. The closure pair lets the same helper drive task,
/// chore, and project lists — they all key on `id` and `status`,
/// just on different row types.
///
/// `Unblocked` and `BlockedByDeps` need the full set of gated ids
/// for the open product, computed once via a pair of joins (see
/// [`compute_gated_work_item_ids`]). `PrerequisitesOf` and
/// `DependentsOf` need only the edge listing for the named row, so
/// they walk the existing dep helpers directly.
pub(crate) fn apply_dep_filter<T, F, G>(
    conn: &Connection,
    filter: &DependencyFilter,
    id_of: F,
    status_of: G,
    items: &mut Vec<T>,
) -> Result<()>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> &str,
{
    match filter {
        DependencyFilter::PrerequisitesOf { id } => {
            let edges = deps::prerequisites_of(conn, id, None)?;
            let allowed: HashSet<String> = edges.into_iter().map(|edge| edge.prerequisite_id).collect();
            items.retain(|item| allowed.contains(id_of(item)));
        }
        DependencyFilter::DependentsOf { id } => {
            let edges = deps::dependents_of(conn, id, None)?;
            let allowed: HashSet<String> = edges.into_iter().map(|edge| edge.dependent_id).collect();
            items.retain(|item| allowed.contains(id_of(item)));
        }
        DependencyFilter::Unblocked => {
            let gated = compute_gated_work_item_ids(conn)?;
            items.retain(|item| status_of(item) == "todo" && !gated.contains(id_of(item)));
        }
        DependencyFilter::BlockedByDeps => {
            let gated = compute_gated_work_item_ids(conn)?;
            items.retain(|item| gated.contains(id_of(item)));
        }
    }
    Ok(())
}

/// Set of work item ids that have at least one `blocks` edge to a
/// prerequisite that has not reached a satisfied status. `done` and
/// `archived` satisfy for every work item (Q4 / Q10). Computed via two SQL joins so the helper
/// does one round-trip regardless of the dependent count.
pub(crate) fn compute_gated_work_item_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut ids: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.dependent_id
         FROM work_item_dependencies d
         JOIN tasks t ON t.id = d.prerequisite_id
         WHERE d.relation = 'blocks'
           AND t.deleted_at IS NULL
           AND t.status NOT IN ('done', 'archived')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        ids.insert(row?);
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.dependent_id
         FROM work_item_dependencies d
         JOIN projects p ON p.id = d.prerequisite_id
         WHERE d.relation = 'blocks'
           AND p.status NOT IN ('done', 'archived')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(ids)
}

/// Stamp a dependent's status to `blocked` and `last_status_actor`
/// to `'engine'` if (a) the dependent is currently in a status
/// other than `blocked`, `done`, `archived`, and (b) it has at least
/// one unmet gating prereq. No-op otherwise.
///
/// Used at edge-creation time (`add_dependency`): a brand-new edge
/// that introduces a gating prereq must move its dependent to
/// `blocked` so the kanban and dispatcher reflect the new gate.
/// The reverse (cascade-on-prereq-regression) deliberately does NOT
/// call this — see the comment on
/// [`cascade_dependents_after_prereq_status_change`].
pub(crate) fn maybe_engine_block_dependent(
    pending: &mut PendingEvents,
    conn: &Connection,
    dependent_id: &str,
    now_epoch: &str,
) -> Result<Option<WorkExecution>> {
    let gating = deps::gating_prereqs_for(conn, dependent_id)?;
    if gating.is_empty() {
        return Ok(None);
    }
    let current_status = deps::lookup_work_item_status(conn, dependent_id)?;
    let Some(current) = current_status else {
        return Ok(None);
    };
    if matches!(current.as_str(), "blocked" | "done" | "archived") {
        return Ok(None);
    }
    if dependent_id.starts_with("proj_") {
        write_engine_project_status(conn, dependent_id, ProjectStatus::Blocked, now_epoch)?;
    } else if dependent_id.starts_with("task_") {
        write_engine_task_status(conn, dependent_id, TaskStatus::Blocked, now_epoch)?;
    }
    // Stamp blocked_reason so the user-override path in
    // request_execution_in_tx_with_live_check can identify and clear
    // stale dependency blocks consistently (the backfill migration
    // covered pre-existing rows; this covers new auto-blocks).
    if dependent_id.starts_with("task_") {
        conn.execute(
            "UPDATE tasks SET blocked_reason = 'dependency'
             WHERE id = ?1 AND status = 'blocked' AND deleted_at IS NULL",
            [dependent_id],
        )?;
        // Invariant: a `blocked` work item must never have a live
        // execution. If a worker is actively running this dependent —
        // i.e. a `blocks` edge just landed on an already-`active`
        // task — cancel that execution in the *same transaction* as
        // the status flip so the DB can never expose the "blocked but
        // executing" state the operator caught. The caller releases
        // the worker's pane + cube lease out-of-band (async side
        // effects that cannot live inside a DB transaction);
        // returning the cancelled row tells it which execution to
        // reap. Projects (`proj_…`) have no executions, so this only
        // applies to tasks.
        return cancel_live_execution_for_block_in_tx(pending, conn, dependent_id, now_epoch);
    }
    Ok(None)
}

/// Cancel the live (`running` / `waiting_human`) execution attached to
/// `work_item_id` as part of an engine block, returning the cancelled
/// row (or `None` when no worker was attached).
///
/// Unlike [`WorkDb::cancel_execution`], this deliberately does NOT
/// reset the task to `todo`: the caller has just set it to `blocked`
/// and that status must stick. It only marks the execution `cancelled`
/// and stamps `finished_at`. Releasing the pane / cube lease is the
/// caller's job (it needs the engine app's completion handler, which
/// the DB layer can't reach).
fn cancel_live_execution_for_block_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    work_item_id: &str,
    now_epoch: &str,
) -> Result<Option<WorkExecution>> {
    let Some(live) = query_live_execution_for_work_item(conn, work_item_id)? else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE work_executions
         SET status = 'cancelled',
             finished_at = ?2
         WHERE id = ?1",
        params![live.id, now_epoch],
    )?;
    let updated = query_execution(conn, &live.id)?
        .with_context(|| format!("execution vanished after block-cancel: {}", live.id))?;
    stage_execution_terminal(pending, conn, &live.id, work_item_id)?;
    tracing::info!(
        work_item_id,
        execution_id = %live.id,
        "engine: cancelled live execution because its dependent transitioned to blocked",
    );
    // Canonical terminalization trace — see `WorkDb::mark_execution_orphaned`
    // in executions_runs.rs. This cancels a genuinely *live* execution, so
    // it is exactly the shape of transition the ack-timeout / stale-reap
    // contradiction needs traced to be attributable.
    tracing::warn!(
        execution_id = %live.id,
        work_item_id = %work_item_id,
        from_status = %live.status,
        to_status = %updated.status,
        reason = "dependent transitioned to blocked",
        "execution terminalized: block cancel",
    );
    Ok(Some(updated))
}

/// Declare a single `relation` edge from `dependent_id` to
/// `prerequisite_id` **inside an existing transaction**, applying the
/// same validation [`WorkDb::add_dependency`] does (same-product, no
/// self-edge, no cycle) plus the engine auto-block. Returns the new
/// edge and any execution that had to be cancelled because the new
/// gate pushed an actively-running dependent into `blocked`.
///
/// Shared by the public `add_dependency` method and the create-time
/// `--depends-on` path (`insert_task_in_tx` / `insert_chore_in_tx`),
/// so a dependency declared atomically at create time gates dispatch
/// identically to one added afterwards.
pub(crate) fn add_dependency_edge_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
    now_epoch: &str,
) -> Result<(WorkItemDependency, Option<WorkExecution>)> {
    if dependent_id == prerequisite_id {
        bail!("a work item cannot depend on itself: {dependent_id}");
    }
    // All of the validation below — the archived-prerequisite rejection,
    // the same-product check, and the cycle check — only makes sense for a
    // genuinely NEW edge. `WorkDb::add_dependency` documents idempotent
    // re-add of an existing edge, and the materializer re-applies a design
    // proposal's full edge set (including ones a deduped handle already
    // established) on every populate; running e.g. `product_id_for_work_item`
    // against a prerequisite that has since been archived (tombstoned) would
    // itself fail with "unknown task" even though the edge needs no new
    // validation at all. Skipping validation entirely for a pre-existing
    // edge keeps both call sites a clean no-op.
    let edge_already_exists = deps::query_edge(conn, dependent_id, prerequisite_id, relation)?.is_some();
    if !edge_already_exists {
        if matches!(
            deps::lookup_work_item_status_for_gating(conn, prerequisite_id)?.as_deref(),
            Some("archived")
        ) {
            bail!(
                "cannot add dependency on archived work item {prerequisite_id}: archived work items cannot be prerequisites"
            );
        }
        // Both ids must resolve and live in the same product. Cross-
        // product edges are tracked separately (see proj_18a2bbe20fc03718_8).
        let dependent_product = product_id_for_work_item(conn, dependent_id)?;
        let prerequisite_product = product_id_for_work_item(conn, prerequisite_id)?;
        if dependent_product != prerequisite_product {
            bail!(
                "dependency edges must stay within a single product; cross-product edges are tracked in proj_18a2bbe20fc03718_8"
            );
        }
        if deps::would_create_cycle(conn, dependent_id, prerequisite_id)? {
            bail!("creating this edge would form a cycle: {prerequisite_id} → … → {dependent_id}");
        }
    }
    let (edge, _outcome): (WorkItemDependency, EdgeInsertOutcome) =
        deps::insert_edge(conn, dependent_id, prerequisite_id, relation, now_epoch)?;
    // Auto-block (Q4): if the new edge introduces a gating prereq, flip
    // the dependent to `blocked` and (defect #2) reap any live worker.
    let reaped = maybe_engine_block_dependent(pending, conn, dependent_id, now_epoch)?;
    Ok((edge, reaped))
}

/// Flip a dependent off `blocked` if (a) its current status is
/// `blocked`, (b) the block was engine-owned — either
/// `blocked_reason = 'dependency'` (the authoritative signal set by
/// [`maybe_engine_block_dependent`]) or, for items that pre-date that
/// column, `blocked_reason IS NULL AND last_status_actor = 'engine'`
/// — and (c) no gating prereqs remain. Items blocked for other reasons
/// (merge_conflict, ci_failure) or manually by a human are left alone.
///
/// Returns `true` when an unblock was written, `false` when the item
/// was skipped (not blocked, not engine-owned, or still gated). This
/// lets callers (and the periodic dep-unblock sweep) distinguish a
/// real action from a no-op without scanning the DB a second time.
///
/// Emits a `tracing::info!` line on each successful unblock so the
/// chain `prereq → done → dependent unblocked` is visible after the
/// fact in the engine log — without it, an auto-unblock that races
/// past a sleeping observer is invisible and the next bug report
/// degenerates into "did the cascade fire or not?".
pub(crate) fn maybe_engine_unblock_dependent(
    pending: &mut PendingEvents,
    conn: &Connection,
    dependent_id: &str,
    now_epoch: &str,
) -> Result<bool> {
    let current = match deps::lookup_work_item_status(conn, dependent_id)? {
        Some(s) => s,
        None => return Ok(false),
    };
    if current != "blocked" {
        return Ok(false);
    }
    // Guard: only auto-unblock if the engine was responsible for the block.
    // For tasks, `blocked_reason = 'dependency'` is the canonical signal —
    // it is set atomically by `maybe_engine_block_dependent` and never set
    // by any human-facing update path.  Accept `blocked_reason IS NULL AND
    // last_status_actor = 'engine'` as a fallback for rows that were
    // auto-blocked before the blocked_reason column existed.
    // For projects (no blocked_reason column), fall back to the actor check.
    //
    // The actor test goes through `StatusActor::is_engine_cascade` rather
    // than a bare `== "engine"` so the rule lives in one place and adding a
    // fifth actor fails to compile there until someone decides which side
    // it belongs on. `'boothby'` deliberately answers `false`, alongside
    // `'human'` / `'boss'`: a Boothby block is a per-row judgement, not
    // cascade bookkeeping, so this sweep must leave it alone exactly as it
    // leaves a human's alone. (In practice the branch is unreachable for
    // Boothby anyway — `write_engine_project_status` / `write_engine_task_status`
    // are the only auto-blockers and both hardcode `'engine'`.)
    //
    // An actor outside the known vocabulary fails to parse and is treated
    // as not-engine-owned, preserving the exact behaviour of the `==
    // "engine"` compare this replaced.
    let actor = lookup_last_status_actor(conn, dependent_id)?;
    let engine_owned = actor
        .as_deref()
        .and_then(|a| a.parse::<StatusActor>().ok())
        .is_some_and(StatusActor::is_engine_cascade);
    let eligible = if dependent_id.starts_with("task_") {
        match lookup_blocked_reason(conn, dependent_id)?.as_deref() {
            Some("dependency") => true,
            None => engine_owned,
            _ => false, // merge_conflict, ci_failure, etc. — different cascade owners
        }
    } else {
        engine_owned
    };
    if !eligible {
        return Ok(false);
    }
    let gating = deps::gating_prereqs_for(conn, dependent_id)?;
    if !gating.is_empty() {
        return Ok(false);
    }
    if dependent_id.starts_with("proj_") {
        write_engine_project_status(conn, dependent_id, ProjectStatus::Planned, now_epoch)?;
    } else if dependent_id.starts_with("task_") {
        write_engine_task_status(conn, dependent_id, TaskStatus::Todo, now_epoch)?;
    }
    // Clear blocked_reason so it doesn't linger on a todo row.
    if dependent_id.starts_with("task_") {
        conn.execute(
            "UPDATE tasks SET blocked_reason = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            [dependent_id],
        )?;
    }
    tracing::info!(
        dependent_id,
        "engine: auto-unblocked dependent — all gating prereqs satisfied",
    );
    // Atomically create or promote the execution to `ready` so the
    // coordinator can dispatch this task on the next kick. Without
    // this, the `waiting_dependency` execution that was created when
    // the chore was first blocked would never be promoted to `ready`
    // unless an external event (frontend request, reconciler kick)
    // happened to trigger `reconcile_product_executions`. Only applies
    // to task_ ids; projects don't have `work_executions` rows.
    //
    // Guard: if a live execution (running/waiting_human) is already
    // attached to this work item — possible when a worker was dispatched
    // during the gated window via a timing race — skip the reconcile.
    // Creating or promoting a `ready` execution on top of a live one
    // would let the dispatcher spawn a second worker for the same row.
    if dependent_id.starts_with("task_") {
        let live = query_live_execution_for_work_item(conn, dependent_id)?;
        if live.is_none() {
            let kind = execution_kind_for_work_item(conn, dependent_id)?;
            let mut reconcile_result = ExecutionReconcileResult::default();
            if kind == ExecutionKind::RevisionImplementation {
                // Route through the revision-aware reconciler instead of
                // blindly minting a `ready` row: a revision whose chain
                // root's PR already merged/closed while this dependency
                // gate was still blocking it is moot, and
                // `reconcile_revision_execution`'s dispatch-time catch-up
                // gate is what catches that and archives the revision
                // (with a stray-execution cleanup of its own) instead of
                // leaving a `ready` execution stranded on a row that's
                // about to be archived out from under it.
                if let Some(task) = query_task(conn, dependent_id)? {
                    reconcile_revision_execution(pending, conn, &mut reconcile_result, &task)?;
                }
            } else {
                reconcile_work_item_execution(conn, &mut reconcile_result, dependent_id, kind, ExecutionStatus::Ready)?;
            }
        } else {
            tracing::info!(
                dependent_id,
                live_execution_id = %live.as_ref().map(|e| e.id.as_str()).unwrap_or(""),
                "gate-clear: skipping ready-promotion — live execution already attached",
            );
        }
    }
    Ok(true)
}

/// Walk every `blocks` dependent of `prereq_id` and run the
/// auto-unblock check when the prereq has just reached a satisfied
/// status. Non-satisfying transitions (e.g. a prereq dragged from
/// `done` back to `backlog`) intentionally do *not* re-block the
/// dependent: a row that has already been unblocked may be running
/// or in `in_review`, and yanking it back to `blocked` from under
/// a worker would lose state. The dispatcher's `gating_prereqs_for`
/// check is the safety net — a regressed prereq immediately re-gates
/// any future dispatch of its dependents — so the cascade can stay
/// purely additive.
pub(crate) fn cascade_dependents_after_prereq_status_change(
    pending: &mut PendingEvents,
    conn: &Connection,
    prereq_id: &str,
    new_prereq_status: &str,
    now_epoch: &str,
) -> Result<()> {
    // Fire the cascade when the prereq reaches any status that *might*
    // satisfy at least one class of dependent:
    //   - `done` / `archived` satisfy all dependents (standard rule).
    //   - `in_review` satisfies revision dependents specifically
    //     (the PR is open; the revision can push to it).
    //
    // `maybe_engine_unblock_dependent` re-evaluates each dependent's
    // full gating list via `gating_prereqs_for`, which is revision-
    // aware, so non-revision dependents are not inadvertently unblocked
    // by an `in_review` transition.
    let might_satisfy = deps::status_satisfies(new_prereq_status) || new_prereq_status == "in_review";
    if !might_satisfy {
        return Ok(());
    }
    let dependents = deps::dependents_of(conn, prereq_id, Some("blocks"))?;
    for edge in dependents {
        maybe_engine_unblock_dependent(pending, conn, &edge.dependent_id, now_epoch)?;
    }
    Ok(())
}

/// Merge-time hook for the non-blocking `merge_order` relation: when a PR
/// merges, order the pair for every in-flight `merge_order` sibling of the
/// just-merged item. Those siblings become the "later" PRs of their pairing
/// and, when they next forward-port onto the moved base, must do so
/// preservingly — the forward-port brief stamps a sibling-specific
/// preservation clause (see [`crate::runner`]) and the both-parents deletion
/// tripwire ([`crate::merge_parent_deletion`]) verifies it.
///
/// This is observability-only (no status change, never gates dispatch): the
/// durable "contract" is the `merge_order` edge itself plus the merged
/// sibling's `done` status. Emitting the ordering decision at merge time
/// makes it visible after the fact in the engine log. Returns the count of
/// in-flight siblings that now owe a preserving forward-port.
pub(crate) fn record_merge_order_on_merge(conn: &Connection, merged_id: &str) -> Result<usize> {
    let mut later_count = 0usize;
    for sibling in deps::merge_order_siblings(conn, merged_id)? {
        let status = deps::lookup_work_item_status(conn, &sibling.sibling_id)?;
        let in_flight = matches!(status.as_deref(), Some(s) if s != "done" && s != "archived");
        if in_flight {
            later_count += 1;
            tracing::info!(
                merged_first = merged_id,
                later_sibling = %sibling.sibling_id,
                "merge_order: overlap partner merged; sibling PR is now the later side and must forward-port preservingly",
            );
        }
    }
    Ok(later_count)
}

/// Internal write used by project auto-block / unblock paths. It stamps the
/// current actor/basis and appends the matching status audit in the caller's
/// transaction. `done` is deliberately outside this writer's authority.
///
/// Takes a typed [`ProjectStatus`] rather than a bare `&str` — the
/// engine previously routed both task and project auto-block/unblock
/// writes through one shared `&str`-typed function keyed off id
/// prefix, and nothing stopped a `TaskStatus` literal (`"todo"`) from
/// being handed to the `proj_` branch. That produced a real incident:
/// `maybe_engine_unblock_dependent` wrote `"todo"` into
/// `projects.status`, which isn't a valid [`ProjectStatus`], and every
/// read of that product's projects thereafter hard-failed. Splitting
/// the writer by table means the *type* passed in is constrained to
/// the target table's vocabulary at the call site, not just validated
/// after the fact.
pub(crate) fn write_engine_project_status(
    conn: &Connection,
    project_id: &str,
    new_status: ProjectStatus,
    now_epoch: &str,
) -> Result<()> {
    if new_status == ProjectStatus::Done {
        bail!(
            "engine dependency cascades cannot mark projects done; completion requires an explicit operator or audited agent action"
        );
    }
    let Some(project) = query_project(conn, project_id)? else {
        return Ok(());
    };
    if project.status == new_status {
        return Ok(());
    }
    let basis = match new_status {
        ProjectStatus::Blocked => "dependency cascade found an unmet prerequisite",
        ProjectStatus::Planned => "dependency cascade found every prerequisite satisfied",
        ProjectStatus::Active | ProjectStatus::Archived => "engine dependency cascade status transition",
        ProjectStatus::Done => unreachable!("done rejected above"),
    };
    conn.execute(
        "UPDATE projects
         SET status = ?2, last_status_actor = 'engine', updated_at = ?3, status_basis = ?4
         WHERE id = ?1",
        params![project_id, new_status.as_str(), now_epoch, basis],
    )?;
    record_project_status_audit(
        conn,
        project_id,
        project.status,
        new_status,
        boss_protocol::LAST_STATUS_ACTOR_ENGINE,
        basis,
        now_epoch,
    )?;
    Ok(())
}

/// Task-table counterpart of [`write_engine_project_status`]. See that
/// function's doc comment for why this is split by table and typed by
/// [`TaskStatus`] rather than a bare `&str`.
pub(crate) fn write_engine_task_status(
    conn: &Connection,
    task_id: &str,
    new_status: TaskStatus,
    now_epoch: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE tasks
         SET status = ?2, last_status_actor = 'engine', updated_at = ?3
         WHERE id = ?1 AND deleted_at IS NULL",
        params![task_id, new_status.as_str(), now_epoch],
    )?;
    Ok(())
}

/// Q4 case 1: refuse a manual move from `blocked` to anything else
/// while the row still has at least one unmet `blocks` prereq. The
/// alternative — letting the user override and run anyway —
/// recreates the original ambiguous "blocked" flag, which the design
/// explicitly rejects.
///
/// Manual moves *into* `blocked`, and any move when no edges gate
/// the row, are allowed.
pub(crate) fn refuse_manual_move_off_blocked_while_gated(
    conn: &Connection,
    work_item_id: &str,
    previous_status: &str,
    new_status: &str,
) -> Result<()> {
    if previous_status != "blocked" || new_status == "blocked" || new_status == "archived" {
        return Ok(());
    }
    let gating = deps::gating_prereqs_for(conn, work_item_id)?;
    if gating.is_empty() {
        return Ok(());
    }
    let names = gating.join(", ");
    bail!("cannot move {work_item_id} to {new_status}: gated by [{names}] (use `boss <kind> depend rm` to remove)");
}

pub(crate) fn lookup_blocked_reason(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT blocked_reason FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(Into::into)
            .map(|opt| opt.flatten());
    }
    Ok(None)
}

pub(crate) fn lookup_last_status_actor(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    if work_item_id.starts_with("proj_") {
        return conn
            .query_row(
                "SELECT last_status_actor FROM projects WHERE id = ?1",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT last_status_actor FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── slugify ─────────────────────────────────────────────────────────────

    #[test]
    fn slugify_lowercases_and_collapses_separators() {
        assert_eq!(slugify("  Hello,  World!! "), "hello-world");
    }

    #[test]
    fn slugify_simple_word_passes_through_lowercased() {
        assert_eq!(slugify("Dispatcher"), "dispatcher");
    }

    #[test]
    fn slugify_collapses_runs_of_non_alphanumerics_to_single_dash() {
        assert_eq!(slugify("a___b---c   d"), "a-b-c-d");
    }

    #[test]
    fn slugify_trims_leading_and_trailing_dashes() {
        assert_eq!(slugify("--foo--"), "foo");
        assert_eq!(slugify("!!!bar???"), "bar");
    }

    #[test]
    fn slugify_keeps_internal_digits() {
        assert_eq!(slugify("Boss Engine v2"), "boss-engine-v2");
    }

    #[test]
    fn slugify_all_punctuation_yields_empty_string() {
        assert_eq!(slugify("!!! ??? ..."), "");
    }

    // ── classify_id ─────────────────────────────────────────────────────────

    #[test]
    fn classify_id_recognises_each_prefix() {
        assert!(matches!(classify_id("prod_abc").unwrap(), ItemKind::Product));
        assert!(matches!(classify_id("proj_abc").unwrap(), ItemKind::Project));
        assert!(matches!(classify_id("task_abc").unwrap(), ItemKind::Task));
        assert!(matches!(classify_id("cmt_abc").unwrap(), ItemKind::Comment));
    }

    #[test]
    fn classify_id_rejects_unknown_prefix() {
        // `ItemKind` has no `Debug`, so match rather than `unwrap_err()`.
        match classify_id("exec_abc") {
            Ok(_) => panic!("expected an error for an unknown prefix"),
            Err(err) => assert!(
                err.to_string().contains("unknown work item id format"),
                "unexpected error: {err}"
            ),
        }
        assert!(classify_id("").is_err());
    }
}
