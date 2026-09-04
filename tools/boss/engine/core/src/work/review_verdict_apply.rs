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
                    // `apply_review_verdict_proposal` also returns `Ok(None)`
                    // for a proposal it deliberately left `proposed` for
                    // retry (e.g. the batch hasn't reached `applying` yet) —
                    // that is not an application and must not count as one.
                    // Re-check the row's own state rather than trusting the
                    // call's `Ok`/`Err` split, so `applied` means what it
                    // says.
                    let actually_applied = !matches!(
                        self.worker_proposal(&proposal.id),
                        Ok(Some(WorkerProposal {
                            state: ProposalState::Proposed,
                            ..
                        }))
                    );
                    if actually_applied {
                        stats.applied += 1;
                        if created.is_some() {
                            stats.created_work += 1;
                        }
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
        // The batch's own state machine is the authority on whether this
        // verdict is ready to apply. `advance_review_batch_quorum_best_effort`
        // is explicitly best-effort and can leave the batch stranded in
        // `supervising` (its own doc comment: "the batch may now be stuck
        // until a human intervenes") while this proposal still reads
        // `proposed`. Applying anyway would fully materialise/complete a
        // batch that never reached `applying`, and — because the completion
        // write below is itself gated on `status = 'applying'` — leave the
        // batch permanently stuck in `supervising` even though its outcome
        // already landed. Bail here and leave the proposal `proposed` so a
        // later pass (after the batch is nudged back to `applying`, or by an
        // operator) can retry.
        if batch.status != boss_protocol::ReviewBatchStatus::Applying {
            tracing::warn!(
                proposal_id = %proposal.id,
                batch_id = %batch.id,
                batch_status = %batch.status,
                "review-verdict apply: batch is not in `applying`; leaving the proposal \
                 proposed for retry instead of materialising against a stranded batch",
            );
            return Ok(None);
        }

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
            // A proposal that moved out of `proposed` to anything other than
            // `applied` between this call's materialisation (above, in its
            // own already-committed transaction) and this bookkeeping commit
            // was decided against — most commonly `superseded` by
            // `proposal_apply::supersede_other_undecided_review_verdicts`
            // when a corrected verdict landed in that window. The revision/
            // follow-up `materialize_review_findings` already created is
            // therefore an orphan of a decision that didn't stand: tombstone
            // it so it never autostarts as the (wrong) chain head, rather
            // than leaving it live alongside whatever the corrected verdict
            // mints under its own `created_via` key.
            if current_state != ProposalState::Applied.as_str()
                && let Some(task_id) = remediating_task_id
            {
                tombstone_orphaned_remediation_in_tx(&tx, task_id, &now_string())?;
            }
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
        let batch_rows_changed = tx.execute(
            "UPDATE pr_review_batches
             SET status = 'completed', completed_at = ?2, updated_at = ?2
             WHERE id = ?1 AND status = 'applying'",
            params![batch.id, now],
        )?;
        if batch_rows_changed == 0 {
            // The batch had already moved off `applying` by the time this
            // apply reached its bookkeeping commit (e.g. a concurrent apply
            // for the same batch, or the batch was otherwise nudged out from
            // under us). The verdict/task/proposal writes below still land —
            // this apply's own outcome is real — but the batch-completion
            // signal this UPDATE was meant to send did not, so surface it
            // rather than silently no-opping: nothing else observes that gap.
            tracing::warn!(
                proposal_id = %proposal.id,
                batch_id = %batch.id,
                "review-verdict apply: batch-completion UPDATE matched no row (batch was \
                 not `applying`); the batch's status may not reflect this applied verdict",
            );
        }

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

    /// `list_worker_proposals` orders newest-first (its `bossctl` callers
    /// want that); this applier processes oldest-first so proposals apply in
    /// the order the batches were decided, so the result is reversed here
    /// rather than by adding a second `ORDER BY` variant of the same query.
    fn list_proposed_review_verdicts(&self) -> Result<Vec<WorkerProposal>> {
        let mut rows = self.list_worker_proposals(
            None,
            None,
            Some(ProposalKind::ReviewVerdict),
            Some(ProposalState::Proposed),
        )?;
        rows.reverse();
        Ok(rows)
    }

    fn worker_proposal(&self, proposal_id: &str) -> Result<Option<WorkerProposal>> {
        let conn = self.connect()?;
        super::proposals::find_worker_proposal_by_id(&conn, proposal_id)
    }
}

fn parent_no_longer_revisable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RevisionGateError>().is_some_and(|gate| {
        matches!(
            gate,
            RevisionGateError::Merged { .. } | RevisionGateError::ClosedUnmerged { .. }
        )
    })
}

/// Soft-delete a revision/follow-up this call materialised whose owning
/// proposal turned out not to be the decision that stands (see the
/// `current_state != Applied` guard in `commit_applied_review_verdict`).
/// Soft-deleted so it drops out of every `deleted_at IS NULL` dispatch/list
/// query (including the `autostart` scheduler query) without touching
/// history.
fn tombstone_orphaned_remediation_in_tx(tx: &rusqlite::Transaction<'_>, task_id: &str, now: &str) -> Result<()> {
    tx.execute(
        "UPDATE tasks SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
        params![task_id, now],
    )?;
    Ok(())
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
    let status_and_block: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT status, blocked_reason FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((status_before, blocked_reason)) = status_and_block else {
        return Ok(());
    };
    if matches!(status_before.as_str(), "done" | "archived" | "in_review") {
        return Ok(());
    }
    // incident-002 postmortem gate: a cycle root held `blocked:
    // deletion_signoff` (see `pr_flow::DELETION_SIGNOFF_BLOCKED_REASON` and
    // `conflict_ladder.rs`) is pending explicit operator sign-off — this
    // async apply path must not silently release that hold by advancing the
    // task to `in_review` and clearing `blocked_reason`.
    if blocked_reason.as_deref() == Some(super::pr_flow::DELETION_SIGNOFF_BLOCKED_REASON) {
        return Ok(());
    }
    let changed = tx.execute(
        "UPDATE tasks
         SET status            = 'in_review',
             updated_at        = ?2,
             last_status_actor = 'engine',
             blocked_reason    = NULL,
             blocked_attempt_id = NULL
         WHERE id = ?1
           AND deleted_at IS NULL
           AND status NOT IN ('done', 'archived', 'in_review')
           AND (blocked_reason IS NULL OR blocked_reason != ?3)",
        params![task_id, now, super::pr_flow::DELETION_SIGNOFF_BLOCKED_REASON],
    )?;
    if changed == 0 {
        return Ok(());
    }
    cascade_dependents_after_prereq_status_change(pending, tx, task_id, "in_review", now)?;
    Ok(())
}
