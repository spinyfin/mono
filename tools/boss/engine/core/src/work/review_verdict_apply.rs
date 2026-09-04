//! Asynchronous application of staged `review_verdict` proposals.
//!
//! Submission only stages the verdict (member reported, batch → `applying`,
//! proposal stays `proposed`). This module records one `pr_review_verdicts`
//! row per batch, increments the review cycle once, and materializes either
//! a revision (open origin), a follow-up against `main` (merged origin), or
//! clean advancement to human Review. The proposal id is the materialisation
//! idempotency key.

use super::*;
use boss_protocol::{
    CreateRevisionInput, ProposalKind, ProposalState, ReasoningMode, ReviewVerdictProposalPayload, WorkerProposal,
};

/// Count from one apply pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReviewVerdictApplyStats {
    pub applied: usize,
    pub failed: usize,
    pub created_work: usize,
}

impl WorkDb {
    /// Apply every still-`proposed` `review_verdict`. Safe to call
    /// redundantly: a proposal already applied is skipped, and the
    /// `pr_review:` created_via key plus the batch/proposal unique indexes
    /// make rematerialisation a no-op.
    pub fn apply_pending_review_verdicts(&self, pr_checker: &dyn PrStateChecker) -> Result<ReviewVerdictApplyStats> {
        let pending = self.list_proposed_review_verdicts()?;
        let mut stats = ReviewVerdictApplyStats::default();
        for proposal in pending {
            match self.apply_review_verdict_proposal(&proposal.id, pr_checker) {
                Ok(created) => {
                    stats.applied += 1;
                    if created.is_some() {
                        stats.created_work += 1;
                    }
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(
                        proposal_id = %proposal.id,
                        ?error,
                        "review-verdict apply failed; leaving the proposal proposed for retry",
                    );
                }
            }
        }
        Ok(stats)
    }

    /// Apply one staged `review_verdict`. Returns the remediation work-item
    /// id when findings warranted a revision or follow-up.
    pub fn apply_review_verdict_proposal(
        &self,
        proposal_id: &str,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<Option<String>> {
        let Some(proposal) = self.worker_proposal(proposal_id)? else {
            return Ok(None);
        };
        if proposal.kind != ProposalKind::ReviewVerdict || proposal.state != ProposalState::Proposed {
            return Ok(None);
        }
        let payload: ReviewVerdictProposalPayload = serde_json::from_str(&proposal.payload_json)?;
        let verdict: boss_pr_review::SupervisorVerdict = serde_json::from_value(payload.verdict.clone())?;
        let Some(batch) = self.review_batch(&payload.batch_id)? else {
            anyhow::bail!(
                "review-verdict {} names missing batch {}",
                proposal.id,
                payload.batch_id
            );
        };

        let review_result = verdict.to_review_result();
        // The engine gate is authoritative: the supervisor's
        // `revision_warranted = false` cannot suppress a critical/high
        // finding or a category that already forces remediation.
        let original_revision_warranted = crate::pr_review::passes_severity_gate(&review_result);
        let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", proposal.id);

        let mut duplicate_head = false;
        if let Ok((_, prior_sha)) = self.get_task_review_cycle_state(&batch.cycle_root_id) {
            duplicate_head = prior_sha.as_deref() == Some(verdict.target_sha.as_str());
        }
        let revision_warranted = original_revision_warranted && !duplicate_head;

        let origin_task = {
            let conn = self.connect()?;
            query_task(&conn, &batch.cycle_root_id)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "review batch {} cycle root {} is missing",
                    batch.id,
                    batch.cycle_root_id
                )
            })?
        };
        let origin = crate::pr_review::ReviewOrigin {
            task_short_id: origin_task.short_id,
            pr_number: Some(batch.pr_number),
        };
        let instructions = crate::pr_review::render_revision_instructions(&review_result, origin);
        let title = crate::pr_review::render_revision_title(origin, review_result.findings.len());

        // Prefer the existing materialisation keyed on this proposal. A reapply
        // after the cycle increment has landed would otherwise look like a
        // duplicate-head pass and drop the already-created revision/follow-up.
        let existing = {
            let conn = self.connect()?;
            existing_review_findings_work_item(&conn, &created_via)?
        };
        let remediating_task = if let Some(existing) = existing {
            Some(existing)
        } else if revision_warranted {
            Some(self.materialize_review_findings(
                &origin_task,
                &batch.cycle_root_id,
                &created_via,
                &title,
                &instructions,
                pr_checker,
            )?)
        } else {
            None
        };

        let gate_outcome = if duplicate_head && original_revision_warranted {
            REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD
        } else if remediating_task.is_some() {
            REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS
        } else {
            REVIEW_GATE_OUTCOME_COMPLETED_CLEAN
        };

        let applied_ref = self.commit_applied_review_verdict(
            &proposal,
            &batch,
            &verdict,
            ReviewVerdictInput {
                head_sha: Some(verdict.target_sha.clone()),
                findings_count: review_result.findings.len() as i64,
                revision_warranted: original_revision_warranted,
                gate_outcome,
            },
            remediating_task.as_ref().map(|task| task.id.as_str()),
        )?;

        Ok(applied_ref)
    }

    fn materialize_review_findings(
        &self,
        origin_task: &Task,
        cycle_root_id: &str,
        created_via: &str,
        title: &str,
        instructions: &str,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<Task> {
        match self.create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(cycle_root_id)
                .description(instructions)
                .name(title)
                .created_via(created_via)
                .reasoning(ReasoningMode::Standard)
                .build(),
            pr_checker,
        ) {
            Ok(task) => Ok(task),
            Err(error) if parent_no_longer_revisable(&error) => self.create_review_findings_followup(
                ReviewFindingsFollowupInsert::builder()
                    .product_id(origin_task.product_id.clone())
                    .name(title)
                    .created_via(created_via)
                    .description(instructions)
                    .chain_root_id(cycle_root_id)
                    .reasoning(ReasoningMode::Standard)
                    .build(),
            ),
            Err(error) => Err(error),
        }
    }

    fn commit_applied_review_verdict(
        &self,
        proposal: &WorkerProposal,
        batch: &boss_protocol::ReviewBatch,
        verdict: &boss_pr_review::SupervisorVerdict,
        input: ReviewVerdictInput,
        remediating_task_id: Option<&str>,
    ) -> Result<Option<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_state: Option<String> = tx
            .query_row(
                "SELECT state FROM worker_proposals WHERE id = ?1",
                params![proposal.id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current_state) = current_state else {
            return Ok(None);
        };
        if current_state != ProposalState::Proposed.as_str() {
            let applied_ref: Option<String> = tx.query_row(
                "SELECT applied_ref FROM worker_proposals WHERE id = ?1",
                params![proposal.id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            return Ok(applied_ref);
        }

        let existing_verdict_id: Option<String> = tx
            .query_row(
                "SELECT id FROM pr_review_verdicts WHERE batch_id = ?1",
                params![batch.id],
                |row| row.get(0),
            )
            .optional()?;
        let verdict_id = if let Some(existing_verdict_id) = existing_verdict_id {
            if let Some(remediating_task_id) = remediating_task_id {
                tx.execute(
                    "UPDATE pr_review_verdicts SET revision_task_id = ?2 WHERE id = ?1 AND revision_task_id IS NULL",
                    params![existing_verdict_id, remediating_task_id],
                )?;
            }
            existing_verdict_id
        } else {
            let inserted = Self::insert_batch_review_verdict_in_tx(
                &tx,
                &proposal.execution_id,
                &batch.cycle_root_id,
                &batch.id,
                &proposal.id,
                &input,
                remediating_task_id,
            )?;
            increment_review_cycle_once_in_tx(&tx, &batch.cycle_root_id, Some(verdict.target_sha.as_str()))?;
            inserted
        };

        let now = now_string();
        let mut pending = PendingEvents::new();
        tx.execute(
            "UPDATE pr_review_batches
             SET status = 'completed', completed_at = ?2, updated_at = ?2
             WHERE id = ?1 AND status = 'applying'",
            params![batch.id, now],
        )?;

        let applied_ref = remediating_task_id.unwrap_or(verdict_id.as_str()).to_owned();
        tx.execute(
            "UPDATE worker_proposals
             SET state = 'applied', applied_ref = ?2, decided_by = 'policy', decided_at = ?3
             WHERE id = ?1 AND state = 'proposed'",
            params![proposal.id, applied_ref, now],
        )?;

        // Open origin: the parent sits in human Review (clean, or beside
        // the autostarted revision). Merged origin: parent is already
        // `done`/`archived` and this no-ops.
        advance_cycle_root_to_in_review_in_tx(&mut pending, &tx, &batch.cycle_root_id, &now)?;

        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(Some(applied_ref).filter(|_| remediating_task_id.is_some()))
    }

    fn list_proposed_review_verdicts(&self) -> Result<Vec<WorkerProposal>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT id, execution_id, work_item_id, kind, payload_json,
                    idempotency_key, state, decided_by, decision_reason, applied_ref,
                    created_at, decided_at
             FROM worker_proposals
             WHERE kind = 'review_verdict' AND state = 'proposed'
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = statement
            .query_map([], map_listed_worker_proposal)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn worker_proposal(&self, proposal_id: &str) -> Result<Option<WorkerProposal>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, execution_id, work_item_id, kind, payload_json,
                    idempotency_key, state, decided_by, decision_reason, applied_ref,
                    created_at, decided_at
             FROM worker_proposals WHERE id = ?1",
            params![proposal_id],
            map_listed_worker_proposal,
        )
        .optional()
        .map_err(Into::into)
    }
}

fn map_listed_worker_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerProposal> {
    fn parse_column<T: std::str::FromStr<Err = String>>(raw: &str, index: usize) -> rusqlite::Result<T> {
        raw.parse::<T>()
            .map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into()))
    }
    let kind_raw: String = row.get(3)?;
    let state_raw: String = row.get(6)?;
    let decided_by_raw: Option<String> = row.get(7)?;
    Ok(WorkerProposal {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        kind: parse_column(&kind_raw, 3)?,
        payload_json: row.get(4)?,
        idempotency_key: row.get(5)?,
        state: parse_column(&state_raw, 6)?,
        decided_by: decided_by_raw.map(|raw| parse_column(&raw, 7)).transpose()?,
        decision_reason: row.get(8)?,
        applied_ref: row.get(9)?,
        created_at: row.get(10)?,
        decided_at: row.get(11)?,
    })
}

fn parent_no_longer_revisable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RevisionGateError>().is_some_and(|gate| {
        matches!(
            gate,
            RevisionGateError::Merged { .. } | RevisionGateError::ClosedUnmerged { .. }
        )
    })
}

fn increment_review_cycle_once_in_tx(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    last_reviewed_sha: Option<&str>,
) -> Result<()> {
    let sha = last_reviewed_sha.filter(|value| !value.is_empty());
    tx.execute(
        "UPDATE tasks
         SET review_cycle      = review_cycle + 1,
             last_reviewed_sha = ?2,
             updated_at        = ?3
         WHERE id = ?1
           AND deleted_at IS NULL
           AND (last_reviewed_sha IS NULL OR last_reviewed_sha != ?2)",
        params![task_id, sha, now_string()],
    )?;
    Ok(())
}

fn advance_cycle_root_to_in_review_in_tx(
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    now: &str,
) -> Result<()> {
    let status_before: Option<String> = tx
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(status_before) = status_before else {
        return Ok(());
    };
    if matches!(status_before.as_str(), "done" | "archived" | "in_review") {
        return Ok(());
    }
    tx.execute(
        "UPDATE tasks
         SET status            = 'in_review',
             updated_at        = ?2,
             last_status_actor = 'engine',
             blocked_reason    = NULL,
             blocked_attempt_id = NULL
         WHERE id = ?1
           AND deleted_at IS NULL
           AND status NOT IN ('done', 'archived', 'in_review')",
        params![task_id, now],
    )?;
    cascade_dependents_after_prereq_status_change(pending, tx, task_id, "in_review", now)?;
    Ok(())
}
