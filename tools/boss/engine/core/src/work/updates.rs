use super::*;
use unicode_segmentation::UnicodeSegmentation;

/// Max length of `blocked_reason`, in grapheme clusters (not bytes, not
/// `char`s — `🚧 Future` must cost 9, not more). The pill title-cases and
/// hard-truncates at one line; anything longer belongs in `blocked_detail`
/// instead, which is unlimited and rendered verbatim as a tooltip.
///
/// Measured against the kanban card's blocked pill at its default board
/// column width: the reported bad input (`FUTURE — deferred scope per
/// design doc; requires explicit operator approval to start`, 84
/// graphemes) visibly truncates after 44 characters
/// (`Future — Deferred Scope Per Design Doc; Requ…`). 40 sits just below
/// that measured fit point — leaving a small margin for strings with
/// wider average character width than the measured sample — while
/// staying comfortably above every real label in use today (the longest
/// built-in discriminator is `ci_failure_exhausted` at 20 graphemes, and
/// the example custom tag `🚧 Future` is 8).
pub(crate) const BLOCKED_REASON_MAX_GRAPHEMES: usize = 40;

/// Reject a `blocked_reason` patch value that would render as a
/// truncated, unrecoverable pill. Validates the incoming value only —
/// never the value already stored on a row, so updating an unrelated
/// field on a row with a pre-existing over-long `blocked_reason` (from
/// before this limit existed) still succeeds.
fn validate_blocked_reason_length(reason: &str) -> Result<()> {
    let len = reason.graphemes(true).count();
    if len > BLOCKED_REASON_MAX_GRAPHEMES {
        bail!(
            "blocked_reason is too long ({len} graphemes, max {BLOCKED_REASON_MAX_GRAPHEMES}): \
             it renders as a short pill label and truncates anything longer. \
             Put the full explanation in --blocked-detail instead — it has no length limit \
             and renders verbatim as a tooltip on the pill."
        );
    }
    Ok(())
}

impl WorkDb {
    pub(crate) fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product = query_product(&tx, id).require("product", id)?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_repo_remote_url_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_repo_remote_url_patch(&mut product.design_repo, patch.design_repo);
        apply_repo_remote_url_patch(&mut product.docs_repo, patch.docs_repo);
        apply_text_patch(&mut product.status, patch.status);
        apply_optional_string_patch(&mut product.default_model, patch.default_model);
        apply_optional_string_patch(&mut product.default_driver, patch.default_driver);
        apply_optional_string_patch(&mut product.dispatch_preamble, patch.dispatch_preamble);
        apply_optional_string_patch(&mut product.worker_branch_prefix, patch.worker_branch_prefix);
        // Re-canonicalise so a patched (or pre-existing) prefix always
        // carries its trailing `/`; idempotent on already-canonical
        // values and on `None`.
        product.worker_branch_prefix = canonicalize_worker_branch_prefix(product.worker_branch_prefix.take());
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7, default_model = ?8, dispatch_preamble = ?9, design_repo = ?10, worker_branch_prefix = ?11, docs_repo = ?12, default_driver = ?13
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
                product.default_model,
                product.dispatch_preamble,
                product.design_repo,
                product.worker_branch_prefix,
                product.docs_repo,
                product.default_driver,
            ],
        )?;

        let updated = query_product(&tx, id).require("product", id)?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    pub(crate) fn update_project(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut project = query_project(&tx, id).require("project", id)?;
        // Pre-image for the Boothby audit trail. Cloned unconditionally
        // rather than behind the actor gate: `project` is mutated in place
        // by the patch application below, so by the time the gate is
        // consulted the original is gone. One clone of a small struct on a
        // path that already does several SQL round-trips.
        let before = project.clone();
        let previous_status = project.status;
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut project.name, patch.name);
        apply_text_patch(&mut project.description, patch.description);
        apply_text_patch(&mut project.goal, patch.goal);
        if let Some(status_str) = patch.status {
            project.status = status_str.parse::<ProjectStatus>().map_err(|e| anyhow::anyhow!(e))?;
        }
        apply_text_patch(&mut project.priority, patch.priority);
        project.slug = unique_project_slug_for_update(&tx, &project.product_id, id, &slugify(&project.name))?;
        project.updated_at = now_string();

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, previous_status.as_str(), project.status.as_str())?;
        }
        let actor_stamp = if status_changed && previous_status != project.status {
            actor
        } else {
            ""
        };

        tx.execute(
            "UPDATE projects
             SET name = ?2, slug = ?3, description = ?4, goal = ?5, status = ?6, priority = ?7, updated_at = ?8,
                 last_status_actor = CASE WHEN ?9 = '' THEN last_status_actor ELSE ?9 END
             WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.description,
                project.goal,
                project.status.as_str(),
                project.priority,
                project.updated_at,
                actor_stamp,
            ],
        )?;

        if status_changed && previous_status != project.status {
            cascade_dependents_after_prereq_status_change(&tx, id, project.status.as_str(), &project.updated_at)?;
        }

        let updated = query_project(&tx, id).require("project", id)?;
        // Audit inside `tx`: the action row and the write it describes
        // commit together or not at all. Inert unless `actor` is Boothby.
        boothby::capture_project_update(&tx, self, actor, &before, &updated, &project.updated_at)?;
        tx.commit()?;
        Ok(WorkItem::Project(updated))
    }

    pub(crate) fn update_task(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
        // Fail fast, before opening a transaction: validate the incoming
        // value only (never a value already sitting in a row from before
        // this limit existed — see `validate_blocked_reason_length`).
        if let Some(reason) = patch.blocked_reason.as_deref() {
            validate_blocked_reason_length(reason)?;
        }
        let blocked_detail_patch_requests_set = patch.blocked_detail.as_deref().is_some_and(|s| !s.is_empty());
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut task = query_task(&tx, id).require("task", id)?;
        let previous_status = task.status.clone();
        // Check this before the generic tombstone bail below: an
        // archived-and-tombstoned moot revision (see
        // `block_pending_revisions_on_parent_close`) is also a deleted
        // task, so without this ordering every attempt to reopen one would
        // surface the generic "cannot update a deleted task" message
        // instead of the specific, actionable explanation.
        if let Some(status_str) = patch.status.as_deref() {
            let requested_status = status_str.parse::<TaskStatus>().map_err(|e| anyhow::anyhow!(e))?;
            refuse_manual_move_off_archived_moot_revision(&tx, id, &task.kind, &previous_status, &requested_status)?;
        }
        if task.deleted_at.is_some() {
            bail!("cannot update a deleted task: {id}");
        }
        // Pre-image for the Boothby audit trail — see the note in
        // `update_project`. Taken after the two refusal checks above so a
        // rejected patch never reaches the audit path at all.
        let before = task.clone();
        let previous_blocked_reason = task.blocked_reason.clone();
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut task.name, patch.name);
        apply_text_patch(&mut task.description, patch.description);
        if let Some(status_str) = patch.status {
            task.status = status_str.parse::<TaskStatus>().map_err(|e| anyhow::anyhow!(e))?;
        }
        apply_optional_patch(&mut task.pr_url, patch.pr_url);
        // Reject non-empty repo override when the product has its own repo.
        if let Some(ref repo_patch) = patch.repo_remote_url
            && !repo_patch.trim().is_empty()
        {
            let product = query_product(&tx, &task.product_id)?
                .with_context(|| format!("orphan task {id}: parent product {} missing", task.product_id))?;
            if let Some(product_repo) = product.repo_remote_url.as_deref() {
                bail!(
                    "cannot set per-task repo override on product `{}`: \
                         product has its own repo (`{}`). \
                         Clear the product's repo first, or omit --repo to inherit.",
                    product.slug,
                    product_repo,
                );
            }
        }
        apply_repo_remote_url_patch(&mut task.repo_remote_url, patch.repo_remote_url);
        if let Some(priority_patch) = patch.priority {
            task.priority = normalize_priority(Some(&priority_patch))?;
        }
        if let Some(effort_patch) = patch.effort_level {
            // Empty string clears the column; anything else must
            // parse as one of the five allowed levels. Invalid
            // values reject the whole patch — no half-updates.
            let trimmed = effort_patch.trim();
            task.effort_level = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.parse::<EffortLevel>().map_err(|e| anyhow::anyhow!(e))?)
            };
        }
        apply_optional_string_patch(&mut task.model_override, patch.model_override);
        apply_optional_string_patch(&mut task.driver, patch.driver);
        apply_optional_string_patch(&mut task.blocked_reason, patch.blocked_reason);
        apply_optional_string_patch(&mut task.blocked_detail, patch.blocked_detail);
        if let Some(autostart) = patch.autostart {
            task.autostart = autostart;
        }
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        // Invariant: blocked_reason and blocked_attempt_id must be NULL for any
        // non-blocked status. Enforce this here so every write path honours it,
        // not just the engine's targeted CI/conflict-resolution helpers.
        if task.status != TaskStatus::Blocked {
            task.blocked_reason = None;
            task.blocked_attempt_id = None;
        }
        // Mirror invariant for archived_reason: it only ever documents why
        // the row is *currently* archived, so it must not linger once the
        // row leaves that status.
        if task.status != TaskStatus::Archived {
            task.archived_reason = None;
        }
        // Invariant: blocked_detail cannot outlive blocked_reason — a
        // detail with nothing to explain is meaningless. Clear it
        // whenever the row ends up with no reason, whether that's because
        // this patch cleared blocked_reason directly or because it moved
        // the status off `blocked` (which already nulled blocked_reason
        // above).
        if task.blocked_reason.is_none() {
            task.blocked_detail = None;
        }
        // Reject rather than silently drop: if this patch explicitly asked
        // to set a non-empty blocked_detail but the row has no
        // blocked_reason to attach it to (the clear above just fired),
        // that's very likely the caller forgetting --blocked-reason, not
        // an intentional no-op.
        if blocked_detail_patch_requests_set && task.blocked_reason.is_none() {
            bail!(
                "cannot set blocked_detail without a blocked_reason on {id}: \
                 pass --blocked-reason alongside --blocked-detail (or update a task \
                 that already has a blocked_reason set)"
            );
        }

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, previous_status.as_str(), task.status.as_str())?;
        }
        let actor_stamp = if status_changed && previous_status != task.status {
            actor
        } else {
            ""
        };

        let effort_level_value = task.effort_level.map(|level| level.as_str().to_owned());

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7,
                 priority = ?9, repo_remote_url = ?10,
                 effort_level = ?11, model_override = ?12, autostart = ?13,
                 blocked_reason = ?14, blocked_attempt_id = ?15, driver = ?16,
                 archived_reason = ?17, blocked_detail = ?18,
                 last_status_actor = CASE WHEN ?8 = '' THEN last_status_actor ELSE ?8 END,
                 completed_at = CASE
                     WHEN ?4 IN ('done', 'archived', 'cancelled') THEN COALESCE(completed_at, ?7)
                     ELSE NULL
                 END
             WHERE id = ?1",
            params![
                task.id,
                task.name,
                task.description,
                task.status.as_str(),
                task.ordinal,
                task.pr_url,
                task.updated_at,
                actor_stamp,
                task.priority,
                task.repo_remote_url,
                effort_level_value,
                task.model_override,
                task.autostart as i64,
                task.blocked_reason,
                task.blocked_attempt_id,
                task.driver,
                task.archived_reason,
                task.blocked_detail,
            ],
        )?;

        if status_changed && previous_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, id, task.status.as_str(), &task.updated_at)?;
        }

        // Manual-override suppression for `blocked: ci_failure` /
        // `ci_failure_exhausted` (design §Q5 / Phase 12 #38). A human
        // pulling a chore out of the CI-failure column is a signal that
        // the engine should keep its hands off the current head sha —
        // otherwise the very next probe re-observes the failure and
        // immediately re-flips the row. We honour the override by:
        //   1) inserting a `ci_failure_suppressions` row keyed on the
        //      head_sha of the most recent CI attempt (a fresh push
        //      changes the key and naturally invalidates suppression),
        //   2) resetting `ci_attempts_used` so a future probe (on a
        //      new head) starts with a fresh budget — mirrors the
        //      `boss engine ci retry` reset rule.
        if status_changed
            && previous_status == TaskStatus::Blocked
            && task.status != TaskStatus::Blocked
            && matches!(
                previous_blocked_reason.as_deref(),
                Some("ci_failure") | Some("ci_failure_exhausted")
            )
        {
            record_ci_failure_suppression_in_tx(&tx, id, &task.updated_at)?;
        }

        let updated = query_task(&tx, id).require("task", id)?;
        // Audit inside `tx`: the action row and the write it describes
        // commit together or not at all. Inert unless `actor` is Boothby.
        boothby::capture_task_update(&tx, self, actor, &before, &updated, &task.updated_at)?;
        tx.commit()?;
        Ok(task_to_item(updated))
    }
}
