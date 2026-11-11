//! Lifetime conflict limits and the repo owner's batched disposition decisions.

use super::*;
use rusqlite::Transaction;

pub const CONFLICT_INVENTORY_GROUP_PREFIX: &str = "conflict_inventory|";
pub const CONFLICT_INVENTORY_STOP_REASON: &str = "lifetime_conflict_limit";
pub const CONFLICT_INVENTORY_MERGE: &str = "Merge now";
pub const CONFLICT_INVENTORY_CLOSE: &str = "Close";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conflict_inventory_decisions (
            attempt_id TEXT PRIMARY KEY REFERENCES conflict_resolutions(id),
            attention_id TEXT NOT NULL UNIQUE REFERENCES attentions(id),
            pr_url TEXT NOT NULL,
            stopped_at TEXT NOT NULL,
            answer TEXT,
            answered_at TEXT,
            action_attempt_id TEXT REFERENCES conflict_resolutions(id),
            action_completed_at TEXT,
            action_error TEXT
        );
        CREATE INDEX IF NOT EXISTS conflict_inventory_pr ON conflict_inventory_decisions(pr_url);",
    )?;
    Ok(())
}

/// Limits apply to completed in-review resolutions and time since the first
/// observed in-review conflict, never to speculative or producer telemetry.
/// Three resolutions exceeds the 1.8-state mean for merged PRs; seven days
/// catches the standing inventory (96/108 older than a week) well before its
/// 68-day median. These are policy defaults, not measured optimal cutoffs
/// (spinyfin/mono#3558).
#[derive(Debug, Clone, Copy)]
pub struct ConflictInventoryLimits {
    pub resolutions: u64,
    pub days: u64,
}

impl Default for ConflictInventoryLimits {
    fn default() -> Self {
        Self {
            resolutions: 3,
            days: 7,
        }
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Hand the authorized merge to the existing Trunk queue retry path in
    /// one transaction, so a crash cannot strand an unsubmitted intent.
    pub fn queue_conflict_inventory_trunk_merge(&self, id: &str, input: TrunkMergeIntentInsertInput) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = super::trunk_merge_intents::query_active_trunk_merge_intent(&tx, &input.work_item_id)? {
            anyhow::ensure!(
                existing.pr_url == input.pr_url,
                "the active merge intent belongs to another PR"
            );
            let state = existing.last_trunk_state.as_deref().unwrap_or_default();
            anyhow::ensure!(
                !matches!(state, "failed" | "pending_failure" | "cancelled")
                    && (!state.starts_with("boss:") || state == TRUNK_INTENT_AWAITING_RESUBMIT),
                "the existing merge request needs attention: {state}"
            );
        }
        if let Some(intent) = super::trunk_merge_intents::insert_intent_in_tx(&tx, input)? {
            tx.execute(
                "UPDATE trunk_merge_intents SET last_trunk_state = ?2, submit_count = 0 WHERE id = ?1",
                params![intent.id, TRUNK_INTENT_AWAITING_RESUBMIT],
            )?;
        }
        tx.execute(
            "UPDATE conflict_inventory_decisions SET action_completed_at = ?2, action_error = NULL
            WHERE attempt_id = ?1 AND answer = 'Merge now'",
            params![id, now_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Durable external actions. A restart retries requests whose result has
    /// not yet been recorded; GitHub close and checked merge are idempotent.
    pub fn pending_conflict_inventory_actions(&self) -> Result<Vec<(String, String, ConflictResolution)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT d.attempt_id, d.answer FROM conflict_inventory_decisions d
            JOIN conflict_resolutions cr ON cr.id = d.action_attempt_id
            WHERE d.answer = 'Merge now' AND d.action_completed_at IS NULL AND cr.status = 'succeeded'
            UNION ALL SELECT attempt_id, answer FROM conflict_inventory_decisions
            WHERE answer = 'Close' AND action_completed_at IS NULL",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, answer)| {
                let attempt = query_conflict_resolution(&conn, &id).require("conflict", &id)?;
                Ok((id, answer, attempt))
            })
            .collect()
    }

    /// Preserve an actionable failure on the stopped ledger row. Success here
    /// means the requested action was accepted, never that conflicts are clean.
    pub fn record_conflict_inventory_action(&self, id: &str, error: Option<&str>) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE conflict_inventory_decisions SET action_error = ?2,
            action_completed_at = CASE WHEN ?2 IS NULL THEN ?3 ELSE NULL END WHERE attempt_id = ?1",
            params![id, error, now_string()],
        )?;
        conn.execute(
            "UPDATE conflict_resolutions SET conflict_diagnosis = ?2 WHERE id = ?1",
            params![id, error.map(|e| format!("Owner decision could not be completed: {e}"))],
        )?;
        Ok(())
    }

    /// Stop before any automatic rebase/worker. The decision, stopped ledger
    /// row and question commit together. Repeated observations return the same
    /// stop, even after dismissal, a restart or a different head SHA.
    pub fn stop_long_conflicting_pr(
        &self,
        input: &ConflictResolutionInsertInput,
        limits: ConflictInventoryLimits,
    ) -> Result<Option<(Attention, AttentionGroup, bool)>> {
        anyhow::ensure!(
            limits.resolutions > 0 && limits.days > 0,
            "conflict limits must be positive"
        );
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let held: Option<String> = tx
            .query_row(
                "SELECT attention_id FROM conflict_inventory_decisions
             WHERE pr_url = ?1 AND (answer IS NULL OR answer = 'Close')
             ORDER BY CAST(stopped_at AS INTEGER) DESC LIMIT 1",
                [&input.pr_url],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = held {
            let (attention, group) = super::attentions::attention_with_group(&tx, &id)?;
            return Ok(Some((attention, group, false)));
        }
        // An explicit answer authorizes one resolution/port. Do not interrupt
        // that work or an already-running automatic writer.
        let live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conflict_resolutions cr
             LEFT JOIN conflict_inventory_decisions d ON d.action_attempt_id = cr.id
             WHERE cr.pr_url = ?1 AND cr.status IN ('pending', 'running')
               AND (cr.revision_task_id IS NOT NULL OR cr.mechanical_rung_in_flight IS NOT NULL
                    OR d.attempt_id IS NOT NULL))",
            [&input.pr_url],
            |r| r.get(0),
        )?;
        if live {
            return Ok(None);
        }
        let (count, first): (i64, Option<i64>) = tx.query_row(
            "SELECT COALESCE(SUM(status = 'succeeded'), 0), MIN(CAST(created_at AS INTEGER))
             FROM conflict_resolutions WHERE pr_url = ?1 AND event_source = 'review_watch'
               AND COALESCE(failure_reason, '') != ?2",
            params![input.pr_url, CONFLICT_INVENTORY_STOP_REASON],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let now = now_string();
        let now_secs: i64 = now.parse()?;
        let age_days = first
            .map(|first| now_secs.saturating_sub(first).max(0) as u64 / 86400)
            .unwrap_or(0);
        if (count as u64) < limits.resolutions && age_days < limits.days {
            return Ok(None);
        }
        let task = query_task(&tx, &input.work_item_id).require("task", &input.work_item_id)?;
        if task.pr_url.as_deref() != Some(&input.pr_url)
            || task.deleted_at.is_some()
            || !(task.status == TaskStatus::InReview
                || (task.status == TaskStatus::Blocked && task.blocked_reason.as_deref() == Some("merge_conflict")))
        {
            return Ok(None);
        }
        let id = next_id("crz");
        // A new conflict needs a fresh disposition. A previous Merge now
        // authorization must not fire later after the owner chooses rewrite
        // or close for this recurrence.
        tx.execute(
            "UPDATE conflict_inventory_decisions SET action_completed_at = ?2,
             action_error = 'Superseded by a new conflict decision'
             WHERE pr_url = ?1 AND answer = 'Merge now' AND action_completed_at IS NULL",
            params![input.pr_url, now],
        )?;
        // This records a stop, not a rebase attempt against a known fetched
        // base. NULL base keeps it separate from the completed observation.
        tx.execute(
            "INSERT INTO conflict_resolutions
             (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
              head_sha_before, status, failure_reason, created_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'abandoned', ?9, ?10, ?10)",
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.head_branch,
                input.base_branch,
                input.head_sha_before,
                CONFLICT_INVENTORY_STOP_REASON,
                now
            ],
        )?;
        tx.execute(
            "UPDATE conflict_resolutions SET status = 'abandoned', failure_reason = ?2, finished_at = ?3
             WHERE pr_url = ?1 AND status = 'pending' AND revision_task_id IS NULL",
            params![input.pr_url, CONFLICT_INVENTORY_STOP_REASON, now],
        )?;
        let prompt = format!(
            "How should PR #{} proceed?\n\n{}\n\nAutomatic conflict resolution has stopped after {count} completed resolutions and {age_days} days since its first recorded conflict (limits: {} resolutions or {} days).\n\nMerge now: resolve once more and request the normal checked merge. Close: close this PR without merging. Rewrite onto the new base: port the change onto the current base in a larger revision, preserving its behaviour and review fixes. Skipping or dismissing leaves automatic resolution stopped.",
            input.pr_number, input.pr_url, limits.resolutions, limits.days,
        );
        let (attention, group) = super::attentions::create_attention_in_tx(
            &tx,
            CreateAttentionInput::builder()
                .kind("question")
                .group_key(format!("{CONFLICT_INVENTORY_GROUP_PREFIX}{}", input.product_id))
                .association_task_id(&input.work_item_id)
                .source_kind("manual")
                .source_anchor(&id)
                .question_type("multiple_choice")
                .prompt_text(prompt)
                .choice_options(serde_json::to_string(&[
                    CONFLICT_INVENTORY_MERGE,
                    CONFLICT_INVENTORY_CLOSE,
                    boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE,
                ])?)
                .build(),
        )?;
        tx.execute(
            "INSERT INTO conflict_inventory_decisions (attempt_id, attention_id, pr_url, stopped_at)
                    VALUES (?1, ?2, ?3, ?4)",
            params![id, attention.id, input.pr_url, now],
        )?;
        crate::task_status_audit::execute(
            &tx,
            "conflict_inventory_stop",
            "engine",
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict',
             blocked_attempt_id = ?2, blocked_detail = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                input.work_item_id,
                id,
                "Automatic conflict resolution stopped; awaiting merge, close or rewrite decision",
                now
            ],
        )?;
        upsert_task_blocked_signal(&tx, &input.work_item_id, "merge_conflict", Some(&id), &now)?;
        tx.commit()?;
        Ok(Some((attention, group, true)))
    }
}

pub(super) fn action_group(
    pending: &mut PendingEvents,
    tx: &Transaction<'_>,
    members: &[Attention],
    checker: &dyn PrStateChecker,
    actor: &str,
) -> Result<Vec<String>> {
    anyhow::ensure!(actor == "human", "only the repo owner can decide stopped PRs");
    let mut touched = Vec::new();
    for member in members.iter().filter(|m| m.answer_state == "answered") {
        let choice = member.answer.as_deref().context("missing conflict decision")?;
        anyhow::ensure!(
            matches!(
                choice,
                CONFLICT_INVENTORY_MERGE | CONFLICT_INVENTORY_CLOSE | boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE
            ),
            "unknown conflict decision: {choice}"
        );
        let attempt_id: String = tx.query_row(
            "SELECT attempt_id FROM conflict_inventory_decisions WHERE attention_id = ?1 AND answer IS NULL",
            [&member.id],
            |r| r.get(0),
        )?;
        let attempt = query_conflict_resolution(tx, &attempt_id).require("conflict", &attempt_id)?;
        let parent = query_task(tx, &attempt.work_item_id).require("task", &attempt.work_item_id)?;
        anyhow::ensure!(
            parent.pr_url.as_deref() == Some(&attempt.pr_url) && parent.deleted_at.is_none(),
            "the PR binding changed; this decision is stale"
        );
        anyhow::ensure!(
            checker.check(&attempt.pr_url)? == PrOpenState::Open,
            "the PR is no longer open"
        );
        let now = now_string();
        tx.execute(
            "UPDATE conflict_inventory_decisions SET answer = ?2, answered_at = ?3 WHERE attempt_id = ?1",
            params![attempt_id, choice, now],
        )?;
        touched.push(parent.id.clone());
        if choice == CONFLICT_INVENTORY_CLOSE {
            continue;
        }
        // Keep the stop row immutable; the owner's one-shot authorization has
        // a new attempt with its own outcome and revision back-pointer.
        let approved_id = next_id("crz");
        tx.execute("INSERT INTO conflict_resolutions
            (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, status, created_at, mismatch_exit_barred)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, 1)",
            params![approved_id, attempt.product_id, parent.id, attempt.pr_url, attempt.pr_number,
                attempt.head_branch, attempt.base_branch, now])?;
        let rewrite = choice == boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE;
        let approved = query_conflict_resolution(tx, &approved_id).require("conflict", &approved_id)?;
        let input = if rewrite {
            super::conflict_mismatch::port_revision_input(
                &parent.id,
                &approved,
                member.prompt_text.as_deref().unwrap_or_default(),
                &now,
            )
        } else {
            CreateRevisionInput::builder().parent_task_id(&parent.id)
                .name(format!("Resolve and merge PR #{}", attempt.pr_number))
                .description(format!("Resolve PR #{} against the current {} branch, preserving its behaviour and all review fixes. The repo owner chose Merge now. Run the required checks and push to the existing PR. The engine will request the product's normal checked merge after the resolution and remaining revisions finish. This does not authorize bypassing checks or review.", attempt.pr_number, attempt.base_branch))
                .created_via(format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{approved_id}"))
                .effort_level(EffortLevel::Small).reasoning(boss_protocol::ReasoningMode::Standard).build()
        };
        let revision = assert_parent_revisable_and_insert_as_actor(pending, tx, input, checker, actor)?;
        super::conflict_mismatch::mark_operator_forced_admission_in_tx(tx, &revision.id)?;
        tx.execute(
            "UPDATE conflict_resolutions SET revision_task_id = ?2 WHERE id = ?1",
            params![approved_id, revision.id],
        )?;
        super::blocking::clear_blocked_merge_conflict_in_tx(tx, &parent.id, &attempt.pr_url, "human")?;
        upsert_task_blocked_signal(tx, &parent.id, "merge_conflict", Some(&approved_id), &now)?;
        tx.execute(
            "UPDATE conflict_inventory_decisions SET action_attempt_id = ?2,
            action_completed_at = CASE WHEN answer = ?3 THEN ?4 ELSE NULL END WHERE attempt_id = ?1",
            params![
                attempt_id,
                approved_id,
                boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE,
                now
            ],
        )?;
        touched.push(revision.id);
    }
    Ok(touched)
}
