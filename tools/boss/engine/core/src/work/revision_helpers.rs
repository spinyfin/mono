use super::*;

impl WorkDb {
    /// For a `revision` task, walk the parent chain to the chain root and
    /// return the chain root's bound `pr_url`, or `None` if the chain root
    /// cannot be resolved or has no bound PR yet.
    ///
    /// This is the authoritative fallback for completion-handler code that
    /// needs the bound PR URL but cannot rely on `execution.pr_url` (e.g.
    /// executions created before `pr_url` was reliably stamped at dispatch
    /// time). It mirrors the lookup performed by `reconcile_revision_execution`
    /// so the completion handler and the dispatcher always agree on which PR
    /// the revision belongs to.
    pub(crate) fn get_revision_chain_root_pr_url(&self, task_id: &str) -> Option<String> {
        let conn = self.connect().ok()?;
        get_chain_root_task(&conn, task_id)
            .ok()
            .flatten()
            .and_then(|t| t.pr_url)
            .filter(|u| !u.is_empty())
    }

    /// For a `revision` task, walk the parent chain to the chain root and
    /// return the chain root task's id — the row the merge poller actually
    /// reads/writes `pr_mergeable_state` / `pr_merge_state_status` /
    /// `pr_head_sha` / `pr_state_polled_at` for, since only the chain root
    /// carries a bound `pr_url`. Used by `app::pr_status` so a revision
    /// worker's `boss pr status`/`--refresh` reads and persists against the
    /// same row the poller sweeps, instead of the revision task's own row
    /// (which never has PR-state columns populated).
    pub(crate) fn get_revision_chain_root_task_id(&self, task_id: &str) -> Option<String> {
        let conn = self.connect().ok()?;
        get_chain_root_task(&conn, task_id).ok().flatten().map(|t| t.id)
    }
}

/// Return the id of the most-recently-created non-done revision that is a
/// descendant of `root_id`, or `None` when the chain has no prior active
/// revision.
///
/// This is used by [`assert_parent_revisable_and_insert`] to find the
/// "tail" of the revision chain so the new revision can be automatically
/// gated on it, serialising back-to-back revisions targeting the same PR.
///
/// "Active" = status is not `'done'` (includes `todo`, `blocked`,
/// `in_progress`, `in_review`).  A done revision is already finished and
/// cannot race with the new one, so it does not need to gate it.
///
/// The recursive CTE walks `parent_task_id` links one level at a time,
/// starting from direct children of `root_id`.  Depth is capped at 64 by
/// the CTE's `UNION ALL` termination condition (no infinite loop in
/// well-formed data; the engine never creates cycles).
pub(crate) fn find_latest_active_revision_in_chain(conn: &Connection, root_id: &str) -> Result<Option<String>> {
    let id: Option<String> = conn
        .query_row(
            "WITH RECURSIVE chain(id) AS (
                SELECT id
                FROM tasks
                WHERE parent_task_id = ?1
                  AND kind = 'revision'
                  AND deleted_at IS NULL
              UNION ALL
                SELECT t.id
                FROM tasks t
                JOIN chain c ON t.parent_task_id = c.id
                WHERE t.kind = 'revision'
                  AND t.deleted_at IS NULL
            )
            SELECT c.id
            FROM chain c
            JOIN tasks t ON t.id = c.id
            WHERE t.status != 'done'
            ORDER BY c.id DESC
            LIMIT 1",
            params![root_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(id)
}

/// Run the create-time gate and, on success, insert a `kind = 'revision'`
/// task row atomically. This is the single point of truth for the invariant
/// "kind = revision ⇒ parent_task_id IS the chain root (non-revision) AND
/// that root has an open PR".
///
/// Gate order (per revision-tasks.md §Q4):
/// 1. Resolve `input.parent_task_id` to a real task; walk to chain root.
///    Callers may pass a revision (manual `create-revision --parent <rev>`,
///    PR-review / CI / conflict producers against a revision work item);
///    the stored `parent_task_id` is always the chain root so revision
///    chains stay flat. Sequencing of back-to-back revisions is
///    via dependency edges on the chain tail, not nested parents.
/// 2. If chain root has no `pr_url` → [`RevisionGateError::NoPr`].
/// 3. If chain root `status == "done"` → [`RevisionGateError::Merged`] (PR merged = task done).
/// 4. Otherwise call `pr_checker.check(pr_url)` for the live state:
///    `Merged` → merged error; `ClosedUnmerged` → closed error; `Open` → insert.
pub(crate) fn assert_parent_revisable_and_insert(
    pending: &mut PendingEvents,
    conn: &Connection,
    input: CreateRevisionInput,
    pr_checker: &dyn PrStateChecker,
) -> Result<Task> {
    // A review execution is the idempotency key for materialising its
    // findings. The finalizer normally reaches this function once, but a
    // repeated Stop/reconciliation path must not mint another work item from
    // the same result. Check before the parent/PR gate so a retry remains a
    // no-op even when the original materialisation has since been completed,
    // converted to a followup, or its parent PR has merged.
    if let Some(created_via) = input
        .created_via
        .as_deref()
        .map(str::trim)
        .filter(|value| value.starts_with(CREATED_VIA_PR_REVIEW_PREFIX))
    {
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id
                 FROM tasks
                 WHERE created_via = ?1
                 ORDER BY (deleted_at IS NULL) DESC, created_at ASC, id ASC
                 LIMIT 1",
                params![created_via],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing_id) = existing_id {
            let existing = query_task(conn, &existing_id)?
                .with_context(|| format!("pr_review materialisation {existing_id} disappeared during dedup"))?;
            tracing::warn!(
                created_via,
                existing_task_id = %existing.id,
                existing_kind = %existing.kind,
                existing_status = %existing.status,
                "pr_review findings materialisation already exists; duplicate mint is a no-op",
            );
            return Ok(existing);
        }
    }

    // ── 1. Resolve parent and chain root ────────────────────────────────────
    let parent_id = resolve_task_id_from_selector(conn, &input.parent_task_id)?;
    let root_id = chain_root(conn, &parent_id)?;
    let root = query_task(conn, &root_id)?.with_context(|| format!("chain root {root_id} not found"))?;

    // ── 2. No PR → reject ───────────────────────────────────────────────────
    let pr_url = match &root.pr_url {
        None => return Err(anyhow::Error::new(RevisionGateError::no_pr(&root))),
        Some(url) => url.clone(),
    };

    // ── 3. Cached: task done → PR merged ────────────────────────────────────
    if root.status == TaskStatus::Done {
        return Err(anyhow::Error::new(RevisionGateError::merged(&root, &pr_url)));
    }

    // ── 4. Live probe for Open vs ClosedUnmerged ────────────────────────────
    match pr_checker.check(&pr_url)? {
        PrOpenState::Merged => {
            return Err(anyhow::Error::new(RevisionGateError::merged(&root, &pr_url)));
        }
        PrOpenState::ClosedUnmerged => {
            return Err(anyhow::Error::new(RevisionGateError::closed(&root, &pr_url)));
        }
        PrOpenState::Open => {}
    }

    // ── 5. Find chain tail for auto-sequencing ──────────────────────────────
    // Snapshot the latest non-done revision for this chain root *before*
    // inserting the new one.  The new revision will be gated on this tail
    // so that back-to-back revisions targeting the same PR always execute
    // one-after-another rather than racing as concurrent workers.
    let chain_tail_id = find_latest_active_revision_in_chain(conn, &root_id)?;

    // ── 6. Insert revision ──────────────────────────────────────────────────
    // Always parent to the chain root (never to another revision). Callers
    // may have passed a revision as `--parent`; reparenting here is what
    // keeps every automatic and manual producer flat under the original
    // work item so the UI's direct-child rollup sees the whole chain.
    let now = now_string();
    let depends_on = input.depends_on.clone();
    let new_revision = insert_revision_in_tx(conn, input, &root_id, &root)?;

    // ── 7. Caller-supplied `--depends-on` gate ──────────────────────────────
    // Declared atomically with the row insert, same as `task create` /
    // `chore create` — this also performs the engine auto-block, so a
    // caller-supplied prerequisite gates the revision immediately.
    // `new_revision` was just inserted in this same transaction, so it can't
    // yet have a live execution attached — `add_dependency_edge_in_tx`'s
    // cancelled-execution channel is guaranteed empty here (mirrors the
    // create-time-dependency invariant documented on `insert_helpers::
    // apply_create_time_dependencies`).
    apply_create_time_dependencies(conn, &new_revision.id, &depends_on, &now)?;

    // ── 8. Auto-gate: block new revision on chain tail ───────────────────────
    // When a prior unfinished revision exists, the new one must wait for it
    // before the dispatcher can run it.  This prevents two workers from
    // committing to the same PR branch simultaneously.
    let has_chain_tail_gate = chain_tail_id.is_some();
    if let Some(tail_id) = chain_tail_id {
        deps::insert_edge(conn, &new_revision.id, &tail_id, RELATION_BLOCKS, &now)?;
        // `new_revision` was just inserted in this same transaction, so it
        // can't yet have a live execution attached — the cancelled-execution
        // channel here is guaranteed empty (same invariant as above).
        maybe_engine_block_dependent(pending, conn, &new_revision.id, &now)?;
    }

    // ── 9. Create the initial execution row now ─────────────────────────────
    // Every other creation path (`task create`, `chore create`, …) gets its
    // first execution from the caller's immediate follow-up `RequestExecution`
    // call (the macOS app and CLI both fire it right after the create RPC
    // returns). `create_revision` has no such follow-up for its engine-
    // triggered callers (conflict_watch, ci_watch): those spawn a revision
    // from a background sweep with no client on the other end to issue the
    // follow-up. Without this, the row sat in `todo` (or `blocked`) with zero
    // `work_executions` rows until the periodic dep-unblock sweep's stuck-
    // execution rescue happened to notice it on its next ≤30s pass — see
    // `dep_unblock_sweep.rs`'s Part B, which was never meant to be the
    // primary dispatch path. Reconciling here, inside the same transaction as
    // the insert and the gates above, closes that gap for every caller
    // (engine-triggered and human/CLI alike) instead of just the reported one.
    //
    // `reconcile_revision_execution` runs its own gating check
    // (`deps::gating_prereqs_for`), so this is safe to call unconditionally
    // once the gates above have been applied: a gated revision (chain-tail or
    // caller `--depends-on`) is born with a `waiting_dependency` execution —
    // promoted the normal way once its prerequisite clears — while an
    // ungated one is born `ready` immediately. `task_accepts_execution` keeps
    // this in step with `autostart = false`: that flag means "create the row
    // but do not auto-dispatch," so no execution is created here and the
    // caller must explicitly start it later, exactly as today.
    if task_accepts_execution(&new_revision) {
        let mut result = ExecutionReconcileResult::default();
        reconcile_revision_execution(pending, conn, &mut result, &new_revision)?;
    }

    if has_chain_tail_gate || !depends_on.is_empty() {
        // Re-read the row so the caller sees the updated status.
        return query_task(conn, &new_revision.id)?
            .with_context(|| format!("missing revision after auto-block: {}", new_revision.id));
    }

    Ok(new_revision)
}

/// Resolve a caller-supplied task selector (full `task_<hex>` id, `T<n>`
/// short id, or bare primary id) to the primary `tasks.id`. For now only
/// full ids are supported; short-id resolution requires the product scope
/// which the engine RPC can carry. Extended when needed.
pub(crate) fn resolve_task_id_from_selector(conn: &Connection, selector: &str) -> Result<String> {
    let trimmed = selector.trim();
    // Full typed id
    if trimmed.starts_with("task_") {
        if !row_exists(
            conn,
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND deleted_at IS NULL)",
            &[&trimmed],
        )? {
            bail!("unknown task: {trimmed}");
        }
        return Ok(trimmed.to_owned());
    }
    bail!(
        "unsupported selector {trimmed:?}; pass the full task id (task_<hex>). \
         Short-id (T<n>) resolution is done by the CLI before sending the RPC."
    )
}

// ── Revision projection helpers ─────────────────────────────────────────────

/// Derive `revision_seq` and `revision_parent_pr_url` for every revision task
/// in `tasks` and return the annotated list.
///
/// Algorithm:
/// 1. Build a lookup of all task IDs → (kind, parent_task_id, pr_url) from
///    both `tasks` and `chores` (the chain root can be a chore).
/// 2. For each revision, walk `parent_task_id` links until a non-revision
///    ancestor is reached — that is the chain root.
/// 3. Group revisions by chain-root ID; sort each group by `created_at ASC`
///    (creation order = R<n> order).
/// 4. Assign 1-based sequence numbers within each group and set
///    `revision_parent_pr_url` from the chain root's `pr_url`.
///
/// Capped at a chain depth of 20 to protect against cycles in corrupt data.
pub(crate) fn attach_revision_projections(mut tasks: Vec<Task>, chores: &[Task]) -> Vec<Task> {
    // Compact lookup: id → (kind, parent_task_id, pr_url)
    type Entry = (TaskKind, Option<String>, Option<String>);
    let mut lookup: std::collections::HashMap<String, Entry> = std::collections::HashMap::new();
    for t in tasks.iter().chain(chores.iter()) {
        lookup.insert(
            t.id.clone(),
            (t.kind.clone(), t.parent_task_id.clone(), t.pr_url.clone()),
        );
    }

    /// Walk parent_task_id links to the first non-revision ancestor.
    /// Returns `(root_id, root_pr_url)` or `None` when the chain is broken.
    fn chain_root(
        start: &str,
        lookup: &std::collections::HashMap<String, (TaskKind, Option<String>, Option<String>)>,
    ) -> Option<(String, Option<String>)> {
        let mut cur = start.to_owned();
        for _ in 0..20 {
            let (kind, parent_id, pr_url) = lookup.get(&cur)?;
            if *kind != TaskKind::Revision {
                return Some((cur, pr_url.clone()));
            }
            cur = parent_id.clone()?;
        }
        None // cycle or unexpectedly deep chain
    }

    // Find chain root for every revision, then group and sequence.
    // We work with indices into `tasks` so we can mutate them afterwards.
    let mut root_info: Vec<Option<(String, Option<String>)>> = tasks
        .iter()
        .map(|t| {
            if t.kind == TaskKind::Revision {
                chain_root(&t.id, &lookup)
            } else {
                None
            }
        })
        .collect();

    // Group revision indices by root_id, sorted by created_at.
    // Key: root_id → Vec<(created_at, index)> sorted by created_at.
    let mut by_root: std::collections::HashMap<String, Vec<(String, usize)>> = std::collections::HashMap::new();
    for (idx, t) in tasks.iter().enumerate() {
        if t.kind == TaskKind::Revision
            && let Some((root_id, _)) = &root_info[idx]
        {
            by_root
                .entry(root_id.clone())
                .or_default()
                .push((t.created_at.clone(), idx));
        }
    }
    for entries in by_root.values_mut() {
        entries.sort_by(|a, b| a.0.cmp(&b.0)); // stable sort by created_at
    }

    // Build seq map: task index → 1-based sequence number.
    let mut seq_map: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
    for entries in by_root.values() {
        for (seq_0, (_, idx)) in entries.iter().enumerate() {
            seq_map.insert(*idx, (seq_0 + 1) as i64);
        }
    }

    // Apply projections to the task list.
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.kind != TaskKind::Revision {
            continue;
        }
        if let Some((_, pr_url)) = root_info[idx].take() {
            task.revision_parent_pr_url = pr_url;
        }
        if let Some(seq) = seq_map.get(&idx) {
            task.revision_seq = Some(*seq);
        }
    }

    tasks
}

/// Set `has_in_progress_revision = true` on every chain-root task that has
/// at least one descendant revision with status `todo` or `active`.
///
/// Called by `get_work_tree` after [`attach_revision_projections`]. Only
/// revisions in the `tasks` slice are inspected (revisions can only be
/// `kind = "revision"` tasks, never chores). The chain root can live in
/// either `tasks` or `chores`, so both slices are mutated.
///
/// Status rule: `todo` and `active` are the in-progress states. `in_review`
/// means the revision's commit has already landed on the PR branch — that is
/// NOT a merge blocker. `done` and deleted revisions likewise don't trigger
/// the flag.
pub(crate) fn attach_in_progress_revision_flag(tasks: &mut [Task], chores: &mut [Task]) {
    // Build a compact lookup: id → (kind, parent_task_id) for chain walking.
    let mut lookup: std::collections::HashMap<String, (TaskKind, Option<String>)> = std::collections::HashMap::new();
    for t in tasks.iter().chain(chores.iter()) {
        lookup.insert(t.id.clone(), (t.kind.clone(), t.parent_task_id.clone()));
    }

    /// Walk parent_task_id links to the first non-revision ancestor.
    /// Returns the root id or `None` when the chain is broken or cycles.
    fn walk_to_root(
        start: &str,
        lookup: &std::collections::HashMap<String, (TaskKind, Option<String>)>,
    ) -> Option<String> {
        let mut cur = start.to_owned();
        for _ in 0..20 {
            let (kind, parent_id) = lookup.get(&cur)?;
            if *kind != TaskKind::Revision {
                return Some(cur);
            }
            cur = parent_id.clone()?;
        }
        None
    }

    // Collect all root ids that have at least one in-progress revision.
    let mut in_progress_roots: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in tasks.iter() {
        if t.kind == TaskKind::Revision
            && (t.status == TaskStatus::Todo || t.status == TaskStatus::Active)
            && let Some(root_id) = walk_to_root(&t.id, &lookup)
        {
            in_progress_roots.insert(root_id);
        }
    }

    if in_progress_roots.is_empty() {
        return;
    }

    for task in tasks.iter_mut() {
        if task.kind != TaskKind::Revision && in_progress_roots.contains(&task.id) {
            task.has_in_progress_revision = true;
        }
    }
    for chore in chores.iter_mut() {
        if in_progress_roots.contains(&chore.id) {
            chore.has_in_progress_revision = true;
        }
    }
}

// ── Ready-for-review flag ────────────────────────────────────────────────────

/// Set `ready_for_review = true` on every Review-lane task that is waiting
/// on the operator and nothing else: an open PR (`status = "in_review"`,
/// `pr_url` set) with no active block, no in-progress descendant revision,
/// every required CI check green, and no merge conflict with the base
/// branch.
///
/// Called by `get_work_tree` after [`attach_in_progress_revision_flag`], so
/// `has_in_progress_revision` is already populated. Reads only fields
/// already loaded onto `Task` by the `get_work_tree` SQL — no extra query.
///
/// Deliberately keyed on the *raw* polled facts (`ci_required_state`,
/// `pr_mergeable_state`) rather than only on `blocked_reason`/`status`,
/// which reflect the last reconciliation pass over those facts and can lag
/// a fresher merge-poller sweep — a task can still read `status =
/// "in_review"` with `blocked_reason = None` while `pr_mergeable_state`
/// already reads `"conflicting"` from the most recent poll. Reading the
/// raw fact directly means this flag doesn't have to wait for the
/// reconciliation step to catch up before it stops calling a conflicted PR
/// ready. A still-running (`"in_progress"`) or not-yet-polled (`None`/
/// `"unknown"`) CI state is treated as NOT ready — there's nothing to
/// merge yet.
pub(crate) fn attach_ready_for_review_flag(tasks: &mut [Task], chores: &mut [Task]) {
    for task in tasks.iter_mut().chain(chores.iter_mut()) {
        task.ready_for_review = task.status == TaskStatus::InReview
            && task.pr_url.is_some()
            && task.blocked_reason.is_none()
            && !task.has_in_progress_revision
            && task.ci_required_state.as_deref() == Some("success")
            && task.pr_mergeable_state.as_deref() == Some("mergeable");
    }
}

// ── AI reviewing flag ────────────────────────────────────────────────────────

/// Set `ai_reviewing = true` on every task (and chore) that is currently held
/// in `active` (Doing) with a `pr_url` AND has a non-terminal `pr_review`
/// execution. Called from `get_work_tree` to surface the "Reviewing (AI)"
/// badge on kanban cards while the reviewer pass is in flight.
///
/// "In flight" means the `pr_review` execution is `running` — a reviewer agent
/// is actually reviewing. A `ready` execution (queued for a review-pool slot,
/// or stuck in the pre-start retry loop after a dispatch failure) is NOT
/// counted: nothing is reviewing yet, so claiming "AI reviewing" would be a
/// lie. The flag is derived — not a stored DB column — so it's always
/// accurate: a task that never had a reviewer, whose reviewer is still queued,
/// or whose reviewer has already finalised (or failed to dispatch) arrives
/// with `ai_reviewing = false` (the default).
pub(crate) fn attach_ai_reviewing_flag(
    conn: &Connection,
    tasks: &mut [Task],
    chores: &mut [Task],
) -> rusqlite::Result<()> {
    // Collect IDs of tasks currently in `active` with a `pr_url` — these are
    // the only candidates. If there are none we can skip the DB query entirely.
    let candidate_ids: Vec<&str> = tasks
        .iter()
        .chain(chores.iter())
        .filter(|t| t.status == TaskStatus::Active && t.pr_url.is_some())
        .map(|t| t.id.as_str())
        .collect();
    if candidate_ids.is_empty() {
        return Ok(());
    }

    // Find which of those candidates have a `pr_review` execution that is
    // actually `running` — i.e. a reviewer agent is in flight. We deliberately
    // do NOT count a merely-`ready` execution: a `pr_review` exec sits in
    // `ready` while queued for a review-pool slot AND while bouncing through the
    // pre-start retry loop after a dispatch failure (e.g. the jj-immutable-head
    // bug this badge was lying about). In neither case is anything reviewing,
    // so showing "AI reviewing" would be dishonest. Terminal states
    // (completed/failed/…) are likewise not in flight. Only `running` means an
    // agent is reviewing right now.
    let placeholders = candidate_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT DISTINCT we.work_item_id
         FROM work_executions we
         WHERE we.work_item_id IN ({})
           AND we.kind = 'pr_review'
           AND we.status = 'running'",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = candidate_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let reviewing: std::collections::HashSet<String> = stmt
        .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    if reviewing.is_empty() {
        return Ok(());
    }
    for task in tasks.iter_mut() {
        if reviewing.contains(&task.id) {
            task.ai_reviewing = true;
        }
    }
    for chore in chores.iter_mut() {
        if reviewing.contains(&chore.id) {
            chore.ai_reviewing = true;
        }
    }
    Ok(())
}

// ── Attachments (screenshot evidence) presence flag ─────────────────────────

/// Set `has_attachments = true` on every task/chore whose own
/// `work_attachments` are non-empty, and additionally on a chain-root task
/// whose direct revision child has attachments of its own — the kanban
/// card affordance to open the screenshot viewer.
///
/// Every task/chore id is a candidate — unlike [`attach_ai_reviewing_flag`],
/// there is no cheaper status-based pre-filter (any row could have evidence
/// attached). Rather than binding one placeholder per candidate id, this
/// runs an unparameterized `SELECT DISTINCT work_item_id FROM
/// work_attachments` (a covering index scan against
/// `work_attachments_work_item_idx`) and intersects it against the
/// in-memory candidate set — the `work_attachments` table is small relative
/// to the board, so scanning it is cheaper than an `IN (...)` list and
/// avoids `SQLITE_MAX_VARIABLE_NUMBER` at large board sizes.
///
/// The root/child roll-up exists because an `in_review`/`done` revision
/// never gets a standalone kanban card (it rolls up into its parent's card
/// instead — see `ChatViewModel.revealCardTarget`), so a chain root whose
/// own row has no attachments must still show the affordance when a
/// revision under it does, or that revision's evidence would be
/// unreachable from the board. A revision's own row reflects only its own
/// attachments: revisions never have revisions of their own to roll up
/// (`insert_revision_in_tx` always parents directly to the chain root).
pub(crate) fn attach_has_attachments_flag(
    conn: &Connection,
    tasks: &mut [Task],
    chores: &mut [Task],
) -> rusqlite::Result<()> {
    let candidate_ids: std::collections::HashSet<&str> =
        tasks.iter().chain(chores.iter()).map(|t| t.id.as_str()).collect();
    if candidate_ids.is_empty() {
        return Ok(());
    }

    // `work_attachments_work_item_idx` covers `(work_item_id, created_at)`,
    // so this is a covering index scan with zero bound parameters — cheaper
    // than an `IN (...)` built from every board id, and not exposed to
    // SQLITE_MAX_VARIABLE_NUMBER at large board sizes. Intersect against the
    // in-memory candidate set for the identical result.
    let mut stmt = conn.prepare("SELECT DISTINCT work_item_id FROM work_attachments")?;
    let own_attachments: std::collections::HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter(|id| candidate_ids.contains(id.as_str()))
        .collect();

    apply_has_attachments_flag(tasks, chores, &own_attachments);
    Ok(())
}

/// Like [`attach_has_attachments_flag`], but queries only the supplied
/// candidate rows. This is for single-row projections, whose revision-chain
/// slices are small enough that keyed lookups beat a board-wide index scan.
fn attach_has_attachments_flag_for_ids(
    conn: &Connection,
    tasks: &mut [Task],
    chores: &mut [Task],
) -> rusqlite::Result<()> {
    let candidate_ids: Vec<&str> = tasks.iter().chain(chores.iter()).map(|task| task.id.as_str()).collect();
    if candidate_ids.is_empty() {
        return Ok(());
    }

    let placeholders = candidate_ids
        .iter()
        .enumerate()
        .map(|(index, _)| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT DISTINCT work_item_id FROM work_attachments WHERE work_item_id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = candidate_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let own_attachments: std::collections::HashSet<String> = stmt
        .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
        .filter_map(|row| row.ok())
        .collect();

    apply_has_attachments_flag(tasks, chores, &own_attachments);
    Ok(())
}

fn apply_has_attachments_flag(
    tasks: &mut [Task],
    chores: &mut [Task],
    own_attachments: &std::collections::HashSet<String>,
) {
    let mut roots_with_child_attachments: std::collections::HashSet<String> = std::collections::HashSet::new();
    for task in tasks.iter() {
        if task.kind == TaskKind::Revision
            && own_attachments.contains(&task.id)
            && let Some(parent_id) = &task.parent_task_id
        {
            roots_with_child_attachments.insert(parent_id.clone());
        }
    }

    for task in tasks.iter_mut() {
        task.has_attachments = own_attachments.contains(&task.id) || roots_with_child_attachments.contains(&task.id);
    }
    for chore in chores.iter_mut() {
        chore.has_attachments = own_attachments.contains(&chore.id) || roots_with_child_attachments.contains(&chore.id);
    }
}

// ── AI review state ──────────────────────────────────────────────────────────

/// Wire values for `Task.ai_review_state`. Plain string constants — not a
/// dedicated protocol enum — matching the existing convention for polled
/// PR-state fields (`ci_required_state`, `pr_mergeable_state`, ...).
pub(crate) const AI_REVIEW_STATE_REVIEWING: &str = "reviewing";
pub(crate) const AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS: &str = "reviewed_with_findings";
pub(crate) const AI_REVIEW_STATE_REVIEWED_ALL_CLEAR: &str = "reviewed_all_clear";
pub(crate) const AI_REVIEW_STATE_REVIEW_NOT_REQUIRED: &str = "review_not_required";

/// Resolve and set `ai_review_state` (+ `ai_review_findings_revision_id`) on
/// every task and chore — the single AI-review badge state a kanban card
/// should show. Must run after [`attach_revision_projections`] (reads
/// `revision_seq`) and after [`attach_ai_reviewing_flag`] (reads
/// `ai_reviewing`) in `get_work_tree`.
///
/// Precedence, evaluated top to bottom for every row — a row can satisfy
/// more than one of these at once (e.g. reopened to Doing after an earlier
/// completed review), so this order is load-bearing, not incidental:
///
/// 1. **Kind-excluded** ([`task_kind_excluded_from_ai_review`]) → always
///    `review_not_required`, regardless of status or revision history.
///    Deliberate simplification: a `design`/`design_postmortem`/
///    `investigation` chain root never gets an *initial* AI review
///    (`should_enqueue_reviewer_for_primary` excludes it), but a revision
///    under it CAN still be reviewed via the separate, kind-independent
///    revision-review trigger — this rule does not look through to that; the
///    chain root's own card still reads `review_not_required`.
/// 2. **Active (Doing)** → `reviewing` when [`attach_ai_reviewing_flag`]
///    already set `ai_reviewing` on this row, else no badge ("not reviewed
///    yet"). Any older verdict is ignored here: a row back in Doing has
///    fresh, not-yet-reviewed work in flight, so a stale `reviewed_*` badge
///    would misrepresent the current head.
/// 3. **In Review or Done** → resolve the most recent *informative* verdict
///    (never `gave_up`/`dropped_duplicate_head` — see
///    [`query_latest_informative_review_verdicts`]; a give-up or dropped
///    duplicate is treated exactly like "no verdict at all," per the
///    deliberate absence of a "review failed" state), preferring the last
///    completed (`in_review`/`done`) direct-child revision's own id when one
///    exists, and falling back to the row's own id when that preferred
///    target has no informative verdict of its own (e.g. the terminal
///    revision was never reviewed, but the chain root itself was).
///    `completed_clean` → `reviewed_all_clear`. `completed_with_findings` /
///    `revision_creation_failed` → `reviewed_with_findings`, plus the
///    verdict's `revision_task_id` (the follow-up revision carrying those
///    review comments — `None` when revision creation itself failed, so
///    there is nothing to reveal). No informative verdict at either the
///    preferred target or the fallback → no badge.
/// 4. Anything else (backlog/blocked/cancelled/archived) → no badge.
pub(crate) fn attach_ai_review_state(conn: &Connection, tasks: &mut [Task], chores: &mut [Task]) -> Result<()> {
    // The last completed (in_review/done) direct-child revision per parent
    // id, keyed by the highest `revision_seq`. Revisions always parent
    // directly to the chain root (never to another revision — see
    // `insert_revision_in_tx`), so a single non-recursive group-by answers
    // "last completed revision" with no chain walk.
    let mut last_completed_by_parent: std::collections::HashMap<String, (i64, String)> =
        std::collections::HashMap::new();
    for t in tasks.iter() {
        if t.kind != TaskKind::Revision || !matches!(t.status, TaskStatus::InReview | TaskStatus::Done) {
            continue;
        }
        let parent_id = match t.parent_task_id.clone() {
            Some(p) => p,
            None => continue,
        };
        let seq = t.revision_seq.unwrap_or(0);
        last_completed_by_parent
            .entry(parent_id)
            .and_modify(|(best_seq, best_id)| {
                if seq > *best_seq {
                    *best_seq = seq;
                    *best_id = t.id.clone();
                }
            })
            .or_insert((seq, t.id.clone()));
    }

    // Which id's verdict actually answers "is THIS card reviewed": the
    // row's own id, unless it's a chain root in Review/Done with a last
    // completed revision, in which case that revision's own id (verdicts
    // are recorded against whichever row actually produced the reviewed
    // push — see `pr_review_verdicts.work_item_id` — so a revision's own
    // review is never recorded on the chain root).
    let target_id = |row: &Task| -> String {
        if row.kind != TaskKind::Revision
            && matches!(row.status, TaskStatus::InReview | TaskStatus::Done)
            && let Some((_, revision_id)) = last_completed_by_parent.get(&row.id)
        {
            return revision_id.clone();
        }
        row.id.clone()
    };

    // Only rows whose badge might depend on a verdict lookup — kind-
    // reviewable and in Review/Done — need one; Active and everything-else
    // rows resolve from `ai_reviewing`/kind alone. Collecting just those ids
    // keeps the batched query no larger than it needs to be.
    let mut lookup_ids: Vec<String> = tasks
        .iter()
        .chain(chores.iter())
        .filter(|row| {
            !task_kind_excluded_from_ai_review(&row.kind)
                && matches!(row.status, TaskStatus::InReview | TaskStatus::Done)
        })
        .flat_map(|row| [target_id(row), row.id.clone()])
        .collect();
    lookup_ids.sort_unstable();
    lookup_ids.dedup();
    let verdicts = query_latest_informative_review_verdicts(conn, &lookup_ids)?;

    // The redirect to the last completed revision's verdict is a
    // preference, not a hard cutover: when that target has no
    // informative verdict of its own, fall back to the row's own
    // verdict rather than rendering nothing. Without this fallback, a
    // chain root whose terminal revision was never reviewed would
    // render no badge despite holding its own verdict — a shape that
    // occurs disproportionately once a card reaches Done and its last
    // revision flips to `done`.
    let resolve = |row: &Task| -> (Option<&'static str>, Option<String>) {
        if task_kind_excluded_from_ai_review(&row.kind) {
            return (Some(AI_REVIEW_STATE_REVIEW_NOT_REQUIRED), None);
        }
        match row.status {
            TaskStatus::Active => {
                if row.ai_reviewing {
                    (Some(AI_REVIEW_STATE_REVIEWING), None)
                } else {
                    (None, None)
                }
            }
            TaskStatus::InReview | TaskStatus::Done => {
                let target = target_id(row);
                let verdict = verdicts
                    .get(&target)
                    .or_else(|| if target != row.id { verdicts.get(&row.id) } else { None });
                match verdict {
                    None => (None, None),
                    Some(v) => match v.gate_outcome.as_str() {
                        REVIEW_GATE_OUTCOME_COMPLETED_CLEAN => (Some(AI_REVIEW_STATE_REVIEWED_ALL_CLEAR), None),
                        REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS | REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED => {
                            (Some(AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS), v.revision_task_id.clone())
                        }
                        // Not reachable: `query_latest_informative_review_verdicts`
                        // already restricts to these three outcomes.
                        _ => (None, None),
                    },
                }
            }
            _ => (None, None),
        }
    };

    for task in tasks.iter_mut() {
        let (state, revision_id) = resolve(task);
        task.ai_review_state = state.map(str::to_owned);
        task.ai_review_findings_revision_id = revision_id;
    }
    for chore in chores.iter_mut() {
        let (state, revision_id) = resolve(chore);
        chore.ai_review_state = state.map(str::to_owned);
        chore.ai_review_findings_revision_id = revision_id;
    }
    Ok(())
}

/// Populate engine-derived `Task` projection fields for a single row so
/// `WorkItemUpdated` payloads are complete rather than carrying mapper
/// defaults. Mirrors the attach_* pipeline `get_work_tree` runs over the
/// full product, loading just this row's revision chain.
///
/// `attach_ai_reviewing_flag` / `attach_ai_review_state` /
/// `attach_has_attachments_flag_for_ids` stay non-fatal here the same way they
/// are in `get_work_tree` — a side-query failure must not take down
/// `GetWorkItem`. Chain loading errors (BFS / point reads) propagate:
/// those are the same class of failure as the row read itself.
pub(crate) fn attach_task_derived_projections(conn: &Connection, task: &mut Task) -> Result<()> {
    let (mut tasks, mut chores) = load_chain_slices_for_projection(conn, task)?;
    tasks = attach_revision_projections(tasks, &chores);
    attach_in_progress_revision_flag(&mut tasks, &mut chores);
    attach_ready_for_review_flag(&mut tasks, &mut chores);
    if let Err(err) = attach_ai_reviewing_flag(conn, &mut tasks, &mut chores) {
        tracing::warn!(
            ?err,
            task_id = %task.id,
            "attach_task_derived_projections: ai_reviewing failed; ignoring"
        );
    }
    if let Err(err) = attach_ai_review_state(conn, &mut tasks, &mut chores) {
        tracing::warn!(
            ?err,
            task_id = %task.id,
            "attach_task_derived_projections: ai_review_state failed; ignoring"
        );
    }
    if let Err(err) = attach_has_attachments_flag_for_ids(conn, &mut tasks, &mut chores) {
        tracing::warn!(
            ?err,
            task_id = %task.id,
            "attach_task_derived_projections: has_attachments failed; ignoring"
        );
    }
    if let Some(projected) = tasks.iter().chain(chores.iter()).find(|t| t.id == task.id) {
        copy_derived_projection_fields(task, projected);
    }
    Ok(())
}

fn copy_derived_projection_fields(dst: &mut Task, src: &Task) {
    dst.revision_seq = src.revision_seq;
    dst.revision_parent_pr_url = src.revision_parent_pr_url.clone();
    dst.has_in_progress_revision = src.has_in_progress_revision;
    dst.has_attachments = src.has_attachments;
    dst.ai_reviewing = src.ai_reviewing;
    dst.ai_review_state = src.ai_review_state.clone();
    dst.ai_review_findings_revision_id = src.ai_review_findings_revision_id.clone();
    dst.ready_for_review = src.ready_for_review;
}

fn push_projection_row(task: Task, tasks: &mut Vec<Task>, chores: &mut Vec<Task>) {
    match task.kind {
        TaskKind::Chore | TaskKind::Followup => chores.push(task),
        _ => tasks.push(task),
    }
}

/// Load the requested row plus its revision chain so the in-memory
/// attach_* helpers see the same sibling/root shape `get_work_tree`
/// builds. Chain roots go in `chores` when they are chore-like; every
/// revision lives in `tasks` (revisions are never chores).
fn load_chain_slices_for_projection(conn: &Connection, task: &Task) -> Result<(Vec<Task>, Vec<Task>)> {
    let mut tasks = Vec::new();
    let mut chores = Vec::new();

    let root_id = if task.kind == TaskKind::Revision {
        match get_chain_root_task(conn, &task.id)? {
            Some(root) => {
                let id = root.id.clone();
                if root.id != task.id {
                    push_projection_row(root, &mut tasks, &mut chores);
                }
                id
            }
            None => task.id.clone(),
        }
    } else {
        task.id.clone()
    };

    for rev_id in collect_chain_revision_ids(conn, &root_id)? {
        if rev_id == task.id {
            continue;
        }
        if let Some(rev) = query_task(conn, &rev_id)? {
            push_projection_row(rev, &mut tasks, &mut chores);
        }
    }

    push_projection_row(task.clone(), &mut tasks, &mut chores);
    Ok((tasks, chores))
}

/// Default `effort_level` for a revision the caller didn't supply one for.
///
/// Design-family chain roots (`design`/`investigation`/`design_postmortem`)
/// default to `large` (→ Opus) rather than the ordinary `small` default: a
/// revision to a design or investigation doc is judgment-heavy, low-volume
/// work whose errors compound into every downstream implementation task, so
/// it warrants the higher tier by default rather than only when explicitly
/// requested. Every other chain root keeps the original `small` default
/// (revision-tasks.md §Q7).
fn default_revision_effort_level(root_kind: &TaskKind) -> &'static str {
    match root_kind {
        TaskKind::Design | TaskKind::Investigation | TaskKind::DesignPostmortem => "large",
        _ => "small",
    }
}

// ── revision name helpers ────────────────────────────────────────────────────

/// Extract a short display name from a revision description.
///
/// Returns the first non-empty, non-blank line of `description`, trimmed.
/// If that first line exceeds 120 characters it is hard-truncated at the
/// nearest word boundary below 120 and an ellipsis is appended. The full
/// description is stored separately in `tasks.description`; the `name`
/// column is just the compact card title.
pub(crate) fn revision_name_from_description(description: &str) -> String {
    for line in description.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return if trimmed.len() <= 120 {
            trimmed.to_owned()
        } else {
            // Walk back to the largest char boundary <= 120 so slicing never
            // splits a multi-byte UTF-8 scalar (a naive `&trimmed[..120]` panics
            // when byte 120 lands mid-character).
            let end = boss_engine_utils::string_clip::floor_char_boundary(trimmed, 120);
            let cutoff = &trimmed[..end];
            match cutoff.rfind(' ') {
                Some(pos) => format!("{}…", &cutoff[..pos]),
                None => format!("{cutoff}…"),
            }
        };
    }
    // Fallback: should not reach here — insert_revision_in_tx enforces non-empty.
    description.trim().to_owned()
}

/// Insert a `kind = 'revision'` task row. Called only after the gate passes.
///
/// `parent_id` is the chain root (the non-revision work item that owns the
/// PR). Callers of [`assert_parent_revisable_and_insert`] may pass a revision
/// as the create-time parent selector; that helper resolves to the root and
/// passes the root id here so `parent_task_id` never points at another
/// revision. `root` is the same row, already loaded for inherit fields.
pub(crate) fn insert_revision_in_tx(
    conn: &Connection,
    input: CreateRevisionInput,
    parent_id: &str,
    root: &Task,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let description = input.description.trim().to_owned();
    if description.is_empty() {
        bail!("revision description must be non-empty");
    }
    let priority = normalize_priority(input.priority.as_deref())?;
    let effort_level = Some(
        input
            .effort_level
            .map(|l| l.as_str().to_owned())
            .unwrap_or_else(|| default_revision_effort_level(&root.kind).to_owned()),
    );
    let (description, effort_matched_rule, effort_reasons) = crate::effort::resolve_effort_provenance_for_create(
        description,
        input.effort_matched_rule,
        input.effort_reasons,
    );
    let model_override = normalize_model_override(input.model_override);
    // Inherit the chain root's capability signal when the caller did not name
    // one: a revision to an investigation is investigation-shaped, a revision
    // to a plain chore is not. When the root itself is unclassified (a row
    // predating the column) the revision stays unclassified too, so it keeps
    // resolving the legacy way rather than being silently re-modelled.
    let reasoning = input.reasoning.or(root.reasoning).map(|mode| mode.as_str().to_owned());
    let driver = normalize_model_override(input.driver);
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "revision");
    // Inherit product, project, and repo from the chain root. A revision
    // by definition lands a follow-up commit on the chain root's PR, which
    // lives in exactly one repo — the root's — so the revision must target
    // the same repo.
    //
    // Copying `root.repo_remote_url` verbatim preserves the per-task repo
    // override invariant (see `enforce_task_repo_invariant`): the root row
    // carries a non-NULL `repo_remote_url` only for multi-repo products
    // whose `product.repo_remote_url` is NULL, so the revision mirrors the
    // same shape. When the product owns the repo, the root's column is NULL
    // and the revision stays NULL too — `resolve_repo_for_work_item` then
    // falls back to the product for both rows.
    //
    // Without this copy, a revision under a multi-repo product had a NULL
    // repo on both the revision row and the (repo-less) product, so
    // `resolve_repo_for_work_item` returned None and the autostarted
    // execution died pre-start with no workspace (issue #840).
    let product_id = &root.product_id;
    let project_id = root.project_id.as_deref();
    let repo_remote_url = root.repo_remote_url.as_deref();
    let short_id = allocate_short_id(conn, product_id)?;
    // `name` is the compact one-line card title. When the coordinator supplies
    // `input.name`, use it verbatim (after trimming); otherwise fall back to
    // deriving the name from the first non-empty line of `description`.
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| revision_name_from_description(&description));
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, \
         pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, \
         effort_level, model_override, reasoning, driver, short_id, parent_task_id, repo_remote_url, effort_matched_rule, effort_reasons) \
         VALUES (?1, ?2, ?3, 'revision', ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, \
         ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            id,
            product_id,
            project_id,
            name,
            description,
            now,
            autostart_value,
            priority,
            created_via,
            effort_level,
            model_override,
            reasoning,
            driver,
            short_id,
            parent_id,
            repo_remote_url,
            effort_matched_rule,
            effort_reasons,
        ],
    )?;
    // `query_task` reads the trailing `parent_task_id` column (via
    // `map_task_with_parent`), so the returned revision row already carries
    // its parent linkage — callers (`create-revision --json`) can verify it
    // without a second lookup.
    query_task(conn, &id)?.with_context(|| format!("missing revision after insert: {id}"))
}

/// Trim and reduce an empty model slug to `None`. The CLI uses
/// `--model ""` to clear a stored override on update verbs; the
/// engine treats the same shape consistently on create so callers
/// don't have to special-case empty strings. Non-empty strings pass
/// through verbatim — claude is the source of truth on slug
/// resolution (design §Q3).
pub(crate) fn normalize_model_override(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Insert a `kind = 'design'` task as the first row under
/// `project_id`. Used by `create_project` and the migration that
/// backfills design tasks for projects predating this column. The
/// design task always has `ordinal = 0` so it sorts ahead of every
/// `project_task` (which start at `ordinal = 1`) and the dispatcher
/// picks it up first via the existing first-incomplete chain.
///
/// `created_via` is always `engine_auto`: the user did not file the
/// design task directly, the engine added it as a side-effect of
/// project creation (or backfill). That distinction is the entire
/// point of the column — manual chores and engine-spawned ones must
/// be tellable apart in one query.
pub(crate) fn insert_design_task_for_project_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    project_name: &str,
    autostart: bool,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let autostart_value: i64 = if autostart { 1 } else { 0 };
    let name = format!("Design {project_name}");
    let short_id = allocate_short_id(conn, product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
         VALUES (?1, ?2, ?3, 'design', ?7, '', 'todo', 0, NULL, NULL, ?4, ?4, ?5, 'medium', ?6, ?8)",
        params![id, product_id, project_id, now, autostart_value, CREATED_VIA_ENGINE_AUTO, name, short_id],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing design task after insert: {id}"))
}

/// Resolve the caller-supplied `created_via` to a stored string. A
/// `None` input lands as `unknown` (the engine app should normally
/// have already substituted a transport-layer hint by the time the
/// row reaches this insert; falling through to `unknown` here is the
/// last-resort safety net). Values outside the documented set are
/// stored verbatim but logged so we can spot undocumented sources
/// sneaking in. `id_for_log` and `kind_for_log` exist only to make
/// the warning useful — they don't affect the stored value.
pub(crate) fn canonicalize_created_via(raw: Option<&str>, id_for_log: &str, kind_for_log: &str) -> String {
    let value = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CREATED_VIA_UNKNOWN);
    if !is_known_created_via(value) {
        tracing::warn!(
            id = %id_for_log,
            kind = %kind_for_log,
            created_via = %value,
            "created_via not in documented set; storing as-is",
        );
    }
    value.to_owned()
}

/// Validate a caller-supplied priority and return the canonical
/// lower-case value. `None`, the empty string, and pure whitespace
/// resolve to the schema default (`medium`) so callers never have
/// to type `--priority medium` explicitly. Anything outside
/// `low` / `medium` / `high` is rejected up-front so the engine
/// stays the single source of truth for the vocabulary.
pub fn normalize_priority(value: Option<&str>) -> Result<String> {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        return Ok("medium".to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "low" | "medium" | "high" => Ok(lower),
        other => bail!("invalid priority `{other}`; expected one of low, medium, high"),
    }
}

pub(crate) fn insert_execution(conn: &Connection, input: CreateExecutionInput) -> Result<WorkExecution> {
    let repo_remote_url = resolve_execution_repo_remote_url(
        conn,
        &input.work_item_id,
        normalize_optional_text(input.repo_remote_url),
    )?;
    let id = next_id("exec");
    let now = now_string();
    let status = input.status.unwrap_or_default();
    let cube_repo_id = normalize_optional_text(input.cube_repo_id);
    let cube_lease_id = normalize_optional_text(input.cube_lease_id);
    let cube_workspace_id = normalize_optional_text(input.cube_workspace_id);
    let workspace_path = normalize_optional_text(input.workspace_path);
    let priority = input.priority.unwrap_or(0);
    let preferred_workspace_id = normalize_optional_text(input.preferred_workspace_id);
    let started_at = normalize_optional_text(input.started_at);
    let finished_at = normalize_optional_text(input.finished_at);
    let prefer_is_soft: i64 = if input.prefer_is_soft { 1 } else { 0 };
    let allow_dirty: i64 = if input.allow_dirty { 1 } else { 0 };
    let pr_url = normalize_optional_text(input.pr_url);
    // Freeze the owning product's worker branch prefix onto the execution row,
    // mirroring `repo_remote_url`. Kept for backward compatibility.
    let worker_branch_prefix = resolve_execution_worker_branch_prefix(conn, &input.work_item_id)?;
    // Snapshot the branch-naming strategy from the product's editorial_rules
    // at spawn time so the detector can always reconstruct the expected branch
    // name from state.db alone, even after the product rule changes later.
    let branch_naming = resolve_execution_branch_naming(conn, &input.work_item_id)?;
    let branch_naming_json = serde_json::to_string(&branch_naming).unwrap_or_default();

    conn.execute(
        "INSERT INTO work_executions (
            id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
            cube_workspace_id, workspace_path, priority, preferred_workspace_id,
            created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
            allow_dirty, branch_naming
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            id,
            input.work_item_id,
            input.kind.as_str(),
            status.as_str(),
            repo_remote_url,
            cube_repo_id,
            cube_lease_id,
            cube_workspace_id,
            workspace_path,
            priority,
            preferred_workspace_id,
            now,
            started_at,
            finished_at,
            prefer_is_soft,
            pr_url,
            worker_branch_prefix,
            allow_dirty,
            branch_naming_json,
        ],
    )?;

    // Decide (and durably record) which driver governs this execution —
    // explicit row override, Codex-percentage roll, or no override at all
    // — before returning. Every `insert_execution` call site funnels
    // through here, so this is the single place a routing decision is
    // made; the two real dispatch paths (`driver_lookup::get_execution_driver_slug`
    // for the events socket, `runner::worker_spawn` for the actual spawn)
    // both read the recorded decision back rather than re-deriving it.
    let decision = decide_execution_driver(conn, &input.work_item_id, input.kind)?;
    record_execution_driver_decision(conn, &id, &input.work_item_id, &decision, &now)?;

    query_execution(conn, &id)?.with_context(|| format!("missing execution after insert: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── revision_name_from_description ──────────────────────────────────────

    #[test]
    fn revision_name_skips_leading_blank_and_whitespace_lines() {
        let desc = "\n   \n\t\nFix the flaky retry loop\nmore detail";
        assert_eq!(revision_name_from_description(desc), "Fix the flaky retry loop");
    }

    #[test]
    fn revision_name_short_single_line_passes_through_trimmed() {
        assert_eq!(
            revision_name_from_description("  Tidy up the dispatcher  "),
            "Tidy up the dispatcher"
        );
    }

    #[test]
    fn revision_name_exactly_120_chars_is_verbatim() {
        let line = "a".repeat(120);
        assert_eq!(revision_name_from_description(&line), line);
    }

    #[test]
    fn revision_name_long_line_with_space_truncates_at_word_boundary() {
        // 130 'a's, a space, then a tail word. The cutoff at 120 bytes lands in
        // the run of 'a's; rfind(' ') finds no space before 120, so it hard
        // cuts. Use a layout where a space *does* fall below 120 to exercise the
        // word-boundary branch.
        let head = "word ".repeat(30); // 150 bytes, spaces every 5 chars
        let out = revision_name_from_description(&head);
        // Truncated at the last space at or before byte 120 (byte 119 here:
        // "word " * 24 = 120 bytes, last space at index 119).
        assert!(out.ends_with('…'), "expected ellipsis, got {out:?}");
        assert!(!out.contains("  "), "should cut cleanly at a space: {out:?}");
        // The kept prefix must be whole words only (no trailing partial 'word').
        let kept = out.trim_end_matches('…');
        assert!(kept.split(' ').all(|w| w.is_empty() || w == "word"));
        assert!(kept.len() <= 120);
    }

    #[test]
    fn revision_name_long_line_without_space_hard_cuts() {
        let line = "x".repeat(200);
        let out = revision_name_from_description(&line);
        assert_eq!(out, format!("{}…", "x".repeat(120)));
    }

    #[test]
    fn revision_name_multibyte_straddling_120_byte_boundary_does_not_panic() {
        // One ASCII byte followed by 3-byte scalars: char boundaries fall at
        // bytes 1, 4, 7, ... = 1 + 3k. Byte 120 is *not* a boundary (119 is not
        // divisible by 3), so a naive `&trimmed[..120]` byte-slice would panic.
        let line = format!("a{}", "世".repeat(50)); // 1 + 150 = 151 bytes
        let out = revision_name_from_description(&line);
        // No spaces → hard-cut branch; must end with the ellipsis and stay valid.
        assert!(out.ends_with('…'), "expected ellipsis, got {out:?}");
        let kept = out.trim_end_matches('…');
        // Cut at the largest char boundary <= 120, i.e. byte 118 (1 + 3*39).
        assert_eq!(kept, &line[..118]);
        // Sanity: the kept text is whole characters (String guarantees validity).
        assert!(line.starts_with(kept));
    }

    // ── normalize_priority ──────────────────────────────────────────────────

    #[test]
    fn normalize_priority_defaults_to_medium() {
        assert_eq!(normalize_priority(None).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("")).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("   \t ")).unwrap(), "medium");
    }

    #[test]
    fn normalize_priority_canonicalizes_case_and_whitespace() {
        assert_eq!(normalize_priority(Some("  LOW ")).unwrap(), "low");
        assert_eq!(normalize_priority(Some("Medium")).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("HIGH")).unwrap(), "high");
    }

    #[test]
    fn normalize_priority_rejects_unknown_value() {
        let err = normalize_priority(Some("urgent")).unwrap_err();
        assert!(err.to_string().contains("invalid priority"), "unexpected error: {err}");
    }

    // ── normalize_model_override ────────────────────────────────────────────

    #[test]
    fn normalize_model_override_none_and_blank_collapse_to_none() {
        assert_eq!(normalize_model_override(None), None);
        assert_eq!(normalize_model_override(Some(String::new())), None);
        assert_eq!(normalize_model_override(Some("   \t".to_owned())), None);
    }

    #[test]
    fn normalize_model_override_trims_and_passes_through() {
        assert_eq!(
            normalize_model_override(Some("  opus  ".to_owned())),
            Some("opus".to_owned())
        );
        assert_eq!(
            normalize_model_override(Some("claude-sonnet-4-6".to_owned())),
            Some("claude-sonnet-4-6".to_owned())
        );
    }

    // ── canonicalize_created_via ────────────────────────────────────────────

    #[test]
    fn canonicalize_created_via_blank_falls_back_to_unknown() {
        assert_eq!(
            canonicalize_created_via(None, "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
        assert_eq!(
            canonicalize_created_via(Some(""), "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
        assert_eq!(
            canonicalize_created_via(Some("   "), "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
    }

    #[test]
    fn canonicalize_created_via_known_values_returned_verbatim() {
        assert_eq!(
            canonicalize_created_via(Some(CREATED_VIA_ENGINE_AUTO), "task_x", "revision"),
            CREATED_VIA_ENGINE_AUTO
        );
        let merge_conflict = "merge-conflict:crz_abc123";
        assert_eq!(
            canonicalize_created_via(Some(merge_conflict), "task_x", "revision"),
            merge_conflict
        );
    }

    // ── attach_has_attachments_flag ─────────────────────────────────────────

    fn attachments_test_image() -> boss_engine_attachments::IngestedImage {
        boss_engine_attachments::IngestedImage {
            content_digest: "deadbeef".to_owned(),
            media_type: boss_protocol::AttachmentMediaType::Png,
            pixel_width: 10,
            pixel_height: 10,
            size_bytes: 128,
            source_name: "shot.png".to_owned(),
        }
    }

    fn attachments_test_task(id: &str, kind: TaskKind, parent_task_id: Option<&str>) -> Task {
        Task::builder()
            .id(id)
            .product_id("prod_1")
            .kind(kind)
            .name("n")
            .description("")
            .status(TaskStatus::Todo)
            .created_at("")
            .updated_at("")
            .maybe_parent_task_id(parent_task_id)
            .build()
    }

    #[test]
    fn flags_only_rows_with_their_own_attachments() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let with_evidence = crate::test_support::create_test_chore(&db, &product.id, "has evidence");
        let without_evidence = crate::test_support::create_test_chore(&db, &product.id, "no evidence");
        let execution = crate::test_support::create_ready_chore_execution(&db, with_evidence.id.clone());
        db.submit_work_attachment(crate::work::SubmitAttachmentInput {
            execution_id: &execution.id,
            work_item_id: &with_evidence.id,
            image: &attachments_test_image(),
            caption: "",
        })
        .unwrap()
        .unwrap();

        let mut tasks = vec![
            attachments_test_task(&with_evidence.id, TaskKind::Chore, None),
            attachments_test_task(&without_evidence.id, TaskKind::Chore, None),
        ];
        let conn = db.connect().unwrap();
        attach_has_attachments_flag(&conn, &mut tasks, &mut []).unwrap();

        assert!(
            tasks[0].has_attachments,
            "the row with its own evidence must be flagged"
        );
        assert!(!tasks[1].has_attachments, "a row with no evidence must not be flagged");
    }

    #[test]
    fn chain_root_rolls_up_a_revision_childs_attachments() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let root = crate::test_support::create_test_chore(&db, &product.id, "chain root, no evidence of its own");
        let revision_id = "task_revision_flag_test";
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 VALUES (?1, ?2, 'revision', 'R1', '', 'todo', '1700000000', '1700000000', ?3)",
                rusqlite::params![revision_id, product.id, root.id],
            )
            .unwrap();
        }
        let revision_execution = crate::test_support::create_ready_chore_execution(&db, revision_id);
        db.submit_work_attachment(crate::work::SubmitAttachmentInput {
            execution_id: &revision_execution.id,
            work_item_id: revision_id,
            image: &attachments_test_image(),
            caption: "",
        })
        .unwrap()
        .unwrap();

        let mut tasks = vec![
            attachments_test_task(&root.id, TaskKind::Chore, None),
            attachments_test_task(revision_id, TaskKind::Revision, Some(root.id.as_str())),
        ];
        let conn = db.connect().unwrap();
        attach_has_attachments_flag(&conn, &mut tasks, &mut []).unwrap();

        assert!(
            tasks[0].has_attachments,
            "the chain root must be flagged even though its OWN row has no evidence"
        );
        assert!(
            tasks[1].has_attachments,
            "the revision itself must also be flagged for its own evidence"
        );
    }

    #[test]
    fn empty_task_and_chore_slices_are_a_no_op() {
        let (_dir, db) = crate::test_support::open_db();
        let conn = db.connect().unwrap();
        attach_has_attachments_flag(&conn, &mut [], &mut []).unwrap();
    }

    #[test]
    fn canonicalize_created_via_trims_and_stores_undocumented_value_as_is() {
        // Surrounding whitespace is trimmed; an undocumented value is still
        // stored verbatim (logged, not rejected).
        assert_eq!(
            canonicalize_created_via(Some("  some-future-source  "), "task_x", "revision"),
            "some-future-source"
        );
    }

    // ── attach_ready_for_review_flag ─────────────────────────────────────────

    fn review_task(id: &str) -> Task {
        Task::builder()
            .id(id)
            .product_id("prod_1")
            .kind(TaskKind::Chore)
            .name("n")
            .description("")
            .status(TaskStatus::InReview)
            .created_at("")
            .updated_at("")
            .pr_url("https://github.com/org/repo/pull/1")
            .build()
    }

    #[test]
    fn ready_when_open_unblocked_ci_green_and_mergeable() {
        let mut t = review_task("t1");
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        let mut tasks = vec![t];
        attach_ready_for_review_flag(&mut tasks, &mut []);
        assert!(tasks[0].ready_for_review);
    }

    #[test]
    fn not_ready_when_blocked() {
        let mut t = review_task("t1");
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        t.blocked_reason = Some("merge_conflict".to_owned());
        let mut tasks = vec![t];
        attach_ready_for_review_flag(&mut tasks, &mut []);
        assert!(!tasks[0].ready_for_review);
    }

    #[test]
    fn not_ready_when_in_progress_revision() {
        let mut t = review_task("t1");
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        t.has_in_progress_revision = true;
        let mut tasks = vec![t];
        attach_ready_for_review_flag(&mut tasks, &mut []);
        assert!(!tasks[0].ready_for_review);
    }

    #[test]
    fn not_ready_when_ci_failing_pending_unknown_or_missing() {
        for ci_state in [Some("fail"), Some("in_progress"), Some("unknown"), None] {
            let mut t = review_task("t1");
            t.ci_required_state = ci_state.map(str::to_owned);
            t.pr_mergeable_state = Some("mergeable".to_owned());
            let mut tasks = vec![t];
            attach_ready_for_review_flag(&mut tasks, &mut []);
            assert!(
                !tasks[0].ready_for_review,
                "ci_required_state={ci_state:?} should not be ready"
            );
        }
    }

    #[test]
    fn not_ready_when_pr_mergeable_state_conflicting_unknown_or_missing() {
        // mono#2357/mono#2356 regression: a card can show a clean CI check
        // and no badges while GitHub already reports the PR CONFLICTING
        // because the status/blocked_reason reconciliation pass hasn't
        // caught up with the latest merge-poller sweep yet. The flag must
        // read pr_mergeable_state directly rather than trusting the absence
        // of blocked_reason.
        for mergeable_state in [Some("conflicting"), Some("unknown"), None] {
            let mut t = review_task("t1");
            t.ci_required_state = Some("success".to_owned());
            t.pr_mergeable_state = mergeable_state.map(str::to_owned);
            let mut tasks = vec![t];
            attach_ready_for_review_flag(&mut tasks, &mut []);
            assert!(
                !tasks[0].ready_for_review,
                "pr_mergeable_state={mergeable_state:?} should not be ready"
            );
        }
    }

    #[test]
    fn not_ready_when_no_pr_url() {
        let mut t = review_task("t1");
        t.pr_url = None;
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        let mut tasks = vec![t];
        attach_ready_for_review_flag(&mut tasks, &mut []);
        assert!(!tasks[0].ready_for_review);
    }

    #[test]
    fn not_ready_when_not_in_review() {
        let mut t = review_task("t1");
        t.status = TaskStatus::Active;
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        let mut tasks = vec![t];
        attach_ready_for_review_flag(&mut tasks, &mut []);
        assert!(!tasks[0].ready_for_review);
    }

    /// Regression fixture: the operator's worked example from a live Review
    /// column (2026-07-25), with the underlying PR facts as GitHub actually
    /// reported them once the state was checked directly (the companion
    /// staleness chore, see `attach_ready_for_review_flag`'s doc comment).
    /// Two of the eight cards rendered clean — green CI, no badge, no pill —
    /// while GitHub reported `CONFLICTING`; a naive "no badge, no lock"
    /// implementation would have called three of these ready. With correct
    /// facts, `ready_for_review` must be `false` for every row here.
    #[test]
    fn worked_example_zero_of_eight_are_ready() {
        // mono#2357 — looked clean, PR actually CONFLICTING.
        let mut t3529 = review_task("t3529");
        t3529.ci_required_state = Some("success".to_owned());
        t3529.pr_mergeable_state = Some("conflicting".to_owned());

        // mono#2356 — looked clean, PR actually CONFLICTING.
        let mut t3528 = review_task("t3528");
        t3528.ci_required_state = Some("success".to_owned());
        t3528.pr_mergeable_state = Some("conflicting".to_owned());

        // `in revision` badge (descendant revision still todo/active).
        let in_revision_ids = ["t3519", "t3513", "t3540", "t3537"];
        let mut in_revision_tasks: Vec<Task> = in_revision_ids
            .iter()
            .map(|id| {
                let mut t = review_task(id);
                t.ci_required_state = Some("success".to_owned());
                t.pr_mergeable_state = Some("mergeable".to_owned());
                t.has_in_progress_revision = true;
                t
            })
            .collect();

        // mono#2261 — `Merge Conflict` pill correctly shown.
        let mut t3231 = review_task("t3231");
        t3231.blocked_reason = Some("merge_conflict".to_owned());
        t3231.status = TaskStatus::Blocked;
        t3231.ci_required_state = Some("success".to_owned());
        t3231.pr_mergeable_state = Some("conflicting".to_owned());

        // "Idle detection..." — no badge, no pill, but CI is broken.
        let mut idle_detection = review_task("t_idle_detection");
        idle_detection.ci_required_state = Some("fail".to_owned());
        idle_detection.pr_mergeable_state = Some("mergeable".to_owned());

        let mut tasks = vec![t3529, t3528, t3231, idle_detection];
        tasks.append(&mut in_revision_tasks);
        attach_ready_for_review_flag(&mut tasks, &mut []);

        for task in &tasks {
            assert!(
                !task.ready_for_review,
                "expected {} to be NOT ready, got ready_for_review=true",
                task.id
            );
        }
    }
}
