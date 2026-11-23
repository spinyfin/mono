//! Preserve accepted leaf reports when a merged PR prevents consolidation.

use super::*;
use boss_pr_review::{ReviewFinding, ReviewOrigin, ReviewResult, ReviewerReport};
use boss_protocol::{ReviewBatchMember, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewReportProposalPayload};

const MERGED_FINDINGS_FOLLOWUP: &str = "followup";
const MERGED_FINDINGS_BELOW_GATE: &str = "below_gate";
const MERGED_FINDINGS_UNREADABLE: &str = "unreadable";

/// One reported reviewer's payload, or the reason that payload could not be read.
#[derive(Debug)]
pub struct ReviewBatchMemberReport {
    pub member: ReviewBatchMember,
    pub report: Result<ReviewerReport, String>,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Accepted reports remain readable even when the batch has no verdict.
    /// A corrupt member is returned as an error for that member; other members stay readable.
    pub fn review_batch_reports(&self, batch_id: &str) -> Result<Vec<ReviewBatchMemberReport>> {
        self.review_batch_members(batch_id)?
            .into_iter()
            .filter(|member| {
                member.status == ReviewBatchMemberStatus::Reported
                    && matches!(
                        member.role,
                        ReviewBatchMemberRole::ClaudeReviewer
                            | ReviewBatchMemberRole::CodexReviewer
                            | ReviewBatchMemberRole::GrokReviewer
                    )
            })
            .map(|member| {
                let report = match read_member_report(self, &member) {
                    Ok(report) => Ok(report),
                    Err(error) => Err(format!("{error:#}")),
                };
                Ok(ReviewBatchMemberReport { member, report })
            })
            .collect()
    }

    /// Recover failed pre-merge batches only after an actual merge observation.
    /// The existing constructor owns deduplication, follow-up provenance, and
    /// dispatch. No consolidated verdict or clean-review stamp is fabricated.
    ///
    /// Each qualifying failed batch with at least one accepted report is
    /// considered, including a batch that staged a verdict but never applied
    /// it. A newer batch for the same PR skips an older one only when that
    /// newer batch completed. One follow-up per failed batch is idempotent
    /// via `pr_review:unconsolidated:<batch_id>`. Batches whose readable
    /// findings do not warrant a follow-up, or whose reports could not be
    /// read, record `merged_findings_disposition` so later sweeps skip them.
    pub fn preserve_merged_review_batch_findings(&self) -> Result<usize> {
        let batch_ids: Vec<String> = {
            let conn = self.connect()?;
            let mut stmt = conn.prepare(
                "SELECT b.id FROM pr_review_batches b
                 JOIN tasks t ON t.id = b.cycle_root_id
                 WHERE b.phase = 'pre_merge' AND b.status = 'failed'
                   AND b.merged_findings_disposition IS NULL AND t.deleted_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM worker_proposals p
                       WHERE p.id = b.final_verdict_proposal_id AND p.state = 'applied'
                   )
                   AND EXISTS (SELECT 1 FROM pr_merge_evidence e WHERE e.root_task_id = t.id)
                   AND EXISTS (
                       SELECT 1 FROM pr_review_batch_members m
                       WHERE m.batch_id = b.id AND m.status = 'reported'
                         AND m.role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer')
                   )
                   AND NOT EXISTS (SELECT 1 FROM tasks f
                       WHERE f.created_via = 'pr_review:unconsolidated:' || b.id)
                   AND NOT EXISTS (
                       SELECT 1 FROM pr_review_batches newer
                       WHERE newer.cycle_root_id = b.cycle_root_id AND newer.pr_url = b.pr_url
                         AND newer.phase = 'pre_merge'
                         AND (newer.created_at > b.created_at
                              OR (newer.created_at = b.created_at AND newer.id > b.id))
                         AND newer.status = 'completed'
                   )
                 ORDER BY b.created_at, b.id",
            )?;
            stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut created = 0;
        for batch_id in batch_ids {
            match self.preserve_merged_review_batch(&batch_id) {
                Ok(Some(_)) => created += 1,
                Ok(None) => {}
                Err(error) => tracing::error!(%batch_id, ?error,
                    "failed to preserve unconsolidated review findings; will retry"),
            }
        }
        Ok(created)
    }

    fn preserve_merged_review_batch(&self, batch_id: &str) -> Result<Option<String>> {
        let batch = self.review_batch(batch_id)?.context("review batch disappeared")?;
        let Some(evidence) = self.get_merge_evidence(&batch.cycle_root_id)? else {
            return Ok(None);
        };
        if !merge_evidence::same_binding(&evidence.canonical_pr_url, &batch.pr_url) {
            return Ok(None);
        }
        let reads = self.review_batch_reports(batch_id)?;
        let mut reports = Vec::new();
        let mut unreadable = Vec::new();
        for read in reads {
            match read.report {
                Ok(report) => reports.push((read.member, report)),
                Err(error) => unreadable.push((read.member, error)),
            }
        }
        let unreadable_detail = if unreadable.is_empty() {
            None
        } else {
            Some(
                unreadable
                    .iter()
                    .map(|(member, error)| {
                        format!(
                            "{} ({}): {error}",
                            member.role,
                            member.report_proposal_id.as_deref().unwrap_or("missing proposal")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        };
        let mut findings = Vec::new();
        let mut summaries = Vec::new();
        for (member, report) in reports {
            summaries.push(format!(
                "{} ({}): {}\nCoverage: {}",
                member.role,
                member.resolved_model,
                report.summary,
                serde_json::to_string(&report.coverage)?,
            ));
            findings.extend(report.findings.into_iter().map(|finding| ReviewFinding {
                severity: finding.severity,
                category: finding.category,
                file: finding.file,
                location: finding.location,
                title: finding.title,
                detail: format!(
                    "Reviewer: {}\nProblem: {}\nImpact: {}\nSuggested fix: {}\nStatic evidence: {}\nNeeds runtime verification: {}",
                    member.role, finding.problem, finding.impact, finding.suggested_fix,
                    finding.static_evidence, finding.needs_runtime_verification,
                ),
                confidence: finding.confidence,
                sources: finding.id.iter().cloned().collect(),
                id: finding.id,
            }));
        }
        let result = ReviewResult::builder()
            .pr_url(batch.pr_url.clone())
            .head_sha(batch.target_sha.clone())
            .summary(summaries.join("\n\n"))
            .revision_warranted(false)
            .findings(findings)
            .regression_check(Default::default())
            .build();
        if !boss_pr_review::passes_severity_gate(&result) {
            let disposition = if unreadable.is_empty() {
                MERGED_FINDINGS_BELOW_GATE
            } else {
                self.note_unreadable_reports(&batch.cycle_root_id, batch_id, unreadable_detail.as_deref().unwrap())?;
                MERGED_FINDINGS_UNREADABLE
            };
            self.record_merged_findings_disposition(batch_id, disposition)?;
            return Ok(None);
        }
        if let Some(detail) = unreadable_detail.as_deref() {
            self.note_unreadable_reports(&batch.cycle_root_id, batch_id, detail)?;
        }
        let origin_task = {
            let conn = self.connect()?;
            query_task(&conn, &batch.cycle_root_id)?.context("review cycle root disappeared")?
        };
        let origin = ReviewOrigin {
            pr_number: Some(batch.pr_number),
        };
        let mut instructions = format!(
            "These unconsolidated findings came from review of {} at `{}` before the PR merged. \
             No supervisor verdict was produced; reviewers may overlap or disagree. Verify each finding \
             against the landed code and explain any finding that no longer applies.\n\n{}",
            batch.pr_url,
            batch.target_sha,
            boss_pr_review::render_revision_instructions(&result, origin),
        );
        if !unreadable.is_empty() {
            instructions.push_str("\n\nUnreadable accepted report(s) were omitted:");
            for (member, error) in &unreadable {
                instructions.push_str(&format!("\n- {} : {error}", member.role));
            }
        }
        let task = self.materialize_review_findings(
            &origin_task,
            &batch.cycle_root_id,
            &format!("{CREATED_VIA_PR_REVIEW_PREFIX}unconsolidated:{}", batch.id),
            &boss_pr_review::render_revision_title(origin, result.findings.len()),
            &instructions,
            &StaticPrStateChecker(PrOpenState::Merged),
        )?;
        self.append_open_batch_attention_note(
            &batch.cycle_root_id,
            batch_id,
            &format!(
                "Member findings from this batch were preserved as unconsolidated follow-up `{}`. \
                 The reports remain unconsolidated; this attention is not resolved.",
                task.id
            ),
        )?;
        self.record_merged_findings_disposition(batch_id, MERGED_FINDINGS_FOLLOWUP)?;
        Ok(Some(task.id))
    }

    fn note_unreadable_reports(&self, cycle_root_id: &str, batch_id: &str, detail: &str) -> Result<()> {
        tracing::error!(
            %batch_id,
            %detail,
            "accepted review report(s) could not be read; preserving readable members"
        );
        self.append_open_batch_attention_note(
            cycle_root_id,
            batch_id,
            &format!("Accepted report(s) could not be read: {detail}."),
        )
    }

    fn record_merged_findings_disposition(&self, batch_id: &str, disposition: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE pr_review_batches
             SET merged_findings_disposition = ?2
             WHERE id = ?1 AND merged_findings_disposition IS NULL",
            params![batch_id, disposition],
        )?;
        Ok(())
    }

    fn append_open_batch_attention_note(&self, cycle_root_id: &str, batch_id: &str, note: &str) -> Result<()> {
        let conn = self.connect()?;
        let rows: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, body_markdown FROM work_attention_items
                 WHERE work_item_id = ?1 AND status = 'open'
                   AND kind IN ('pr_review_quorum_failed', ?2)",
            )?;
            stmt.query_map(
                rusqlite::params![cycle_root_id, PR_REVIEW_BATCH_STALE_ATTENTION_KIND],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?
        };
        for (id, body) in rows {
            if !body.contains(batch_id) || body.contains(note) {
                continue;
            }
            let updated = format!("{body}\n\n{note}");
            conn.execute(
                "UPDATE work_attention_items SET body_markdown = ?2 WHERE id = ?1 AND status = 'open'",
                params![id, updated],
            )?;
        }
        Ok(())
    }
}

fn read_member_report(db: &WorkDb, member: &ReviewBatchMember) -> Result<ReviewerReport> {
    let id = member
        .report_proposal_id
        .as_deref()
        .context("reported member has no proposal")?;
    let proposal = db.get_worker_proposal(id)?;
    let payload: ReviewReportProposalPayload =
        serde_json::from_str(&proposal.payload_json).with_context(|| format!("reading accepted report {id}"))?;
    serde_json::from_value(payload.report).with_context(|| format!("reading accepted report body {id}"))
}
