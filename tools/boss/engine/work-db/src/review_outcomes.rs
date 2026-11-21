//! Structured review remediation, stored in the attributed proposal ledger.
//! Verification runs before the write transaction; acceptance rechecks assignment.

use super::*;
use boss_protocol::{
    ExecutionKind, ProposalState, ReviewFindingDisposition, ReviewFindingOutcome, ReviewOutcomesProposalPayload,
};
use rusqlite::Transaction;
use std::collections::BTreeSet;

#[cfg(test)]
#[path = "review_outcomes_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Assignment {
    verdict_id: String,
    finding_ids: BTreeSet<String>,
    pr_url: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredOutcomes {
    submission: ReviewOutcomesProposalPayload,
    pr_url: Option<String>,
}

pub(crate) struct PreparedOutcomes {
    assignment: Assignment,
    payload_json: String,
}

/// A remediation execution finished without a complete outcome set for its
/// assigned finding ids. Distinct from a storage failure so Stop/finalize
/// can surface it as incomplete instead of retrying as `DbError`.
#[derive(Debug)]
pub struct ReviewRemediationIncomplete(pub String);

impl std::fmt::Display for ReviewRemediationIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReviewRemediationIncomplete {}

fn remediates_review_findings(kind: ExecutionKind) -> bool {
    matches!(
        kind,
        ExecutionKind::RevisionImplementation
            | ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::InvestigationImplementation
    )
}

fn finding_ids_in_verdict_payload(raw: &str) -> Result<BTreeSet<String>> {
    let payload: boss_protocol::ReviewVerdictProposalPayload = serde_json::from_str(raw)?;
    let verdict: boss_pr_review::SupervisorVerdict = serde_json::from_value(payload.verdict)?;
    let finding_ids: BTreeSet<_> = verdict
        .findings
        .iter()
        .filter_map(|finding| finding.id.clone())
        .collect();
    anyhow::ensure!(
        finding_ids.is_empty() || finding_ids.len() == verdict.findings.len(),
        "review verdict has mixed or duplicate finding ids"
    );
    Ok(finding_ids)
}

fn assignment(conn: &Connection, execution_id: &str) -> Result<Option<Assignment>> {
    let execution = query_execution(conn, execution_id).require("execution", execution_id)?;
    if !remediates_review_findings(execution.kind) {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT v.id, p.payload_json FROM pr_review_verdicts v
         JOIN worker_proposals p ON p.id = v.proposal_id
         WHERE v.revision_task_id = ?1",
    )?;
    let rows = stmt.query_map(params![execution.work_item_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut found = None;
    for row in rows {
        let (verdict_id, raw) = row?;
        let finding_ids = finding_ids_in_verdict_payload(&raw)?;
        if finding_ids.is_empty() {
            continue;
        }
        anyhow::ensure!(found.is_none(), "multiple review verdicts assigned to this execution");
        let root = chain_root(conn, &execution.work_item_id)?;
        let pr_url = query_task(conn, &root)?.and_then(|task| task.pr_url);
        found = Some(Assignment {
            verdict_id,
            finding_ids,
            pr_url,
        });
    }
    Ok(found)
}

fn validate_set(assignment: &Assignment, payload: &ReviewOutcomesProposalPayload) -> Result<()> {
    payload.validate().map_err(anyhow::Error::msg)?;
    let supplied: BTreeSet<_> = payload
        .outcomes
        .iter()
        .map(|outcome| outcome.finding_id.clone())
        .collect();
    let unknown: Vec<_> = supplied.difference(&assignment.finding_ids).collect();
    let missing: Vec<_> = assignment.finding_ids.difference(&supplied).collect();
    anyhow::ensure!(unknown.is_empty(), "unknown finding ids: {unknown:?}");
    anyhow::ensure!(missing.is_empty(), "missing finding outcomes: {missing:?}");
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The verifier must check membership against the live remediation PR.
    /// No DB connection is held while it performs network or process work.
    pub fn submit_review_outcomes(
        &self,
        input: SubmitWorkerProposalInput<'_>,
        verify: &dyn Fn(&str, &str) -> Result<bool>,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, boss_protocol::ProposalSubmissionError>> {
        anyhow::ensure!(
            input.kind == boss_protocol::ProposalKind::ReviewOutcomes,
            "expected review outcomes"
        );
        // Replays recover the original acknowledgement without another network read.
        // A key hit is only a replay when the stored row is still the live set
        // for this assignment's PR; otherwise the worker must submit again.
        if let Some(existing) =
            self.find_worker_proposal_by_idempotency_key(input.execution_id, input.idempotency_key)?
        {
            let assigned = assignment(&*self.connect()?, input.execution_id)?;
            let replayable = existing.state == ProposalState::Applied
                && assigned.as_ref().is_some_and(|assigned| {
                    serde_json::from_str::<StoredOutcomes>(&existing.payload_json)
                        .ok()
                        .is_some_and(|stored| stored.pr_url == assigned.pr_url)
                });
            if replayable {
                return self.submit_worker_proposal_prepared(input, None, &|_| false, None);
            }
            return Ok(Err(boss_protocol::ProposalSubmissionError::new(
                boss_protocol::ProposalErrorCode::NotAccepted,
                "idempotency key already used for a stale outcome set; resubmit with a new key",
            )));
        }
        let prepared = (|| -> Result<PreparedOutcomes> {
            let assigned = assignment(&*self.connect()?, input.execution_id)?
                .context("this execution has no assigned review finding ids")?;
            let payload: ReviewOutcomesProposalPayload = serde_json::from_str(input.payload_json)?;
            validate_set(&assigned, &payload)?;
            let mut commits = BTreeSet::new();
            for outcome in &payload.outcomes {
                if outcome.outcome == ReviewFindingDisposition::Fixed {
                    commits.insert(outcome.commit_sha.as_deref().context("fixed requires commit_sha")?);
                }
            }
            for sha in commits {
                let pr_url = assigned
                    .pr_url
                    .as_deref()
                    .context("publish the remediation PR before submitting fixed outcomes")?;
                anyhow::ensure!(verify(pr_url, sha)?, "commit {sha} is not on the remediation PR branch");
            }
            Ok(PreparedOutcomes {
                assignment: assigned,
                payload_json: input.payload_json.to_owned(),
            })
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                return Ok(Err(boss_protocol::ProposalSubmissionError::new(
                    boss_protocol::ProposalErrorCode::NotAccepted,
                    err.to_string(),
                )));
            }
        };
        self.submit_worker_proposal_prepared(input, None, &|_| false, Some(&prepared))
    }

    /// Full history, including the execution responsible for each accepted set.
    pub fn review_finding_outcomes(&self, verdict_id: &str) -> Result<Vec<boss_protocol::WorkerProposal>> {
        outcome_records(&*self.connect()?, verdict_id)
    }

    /// Finding ids this execution must account for, if it remediates a verdict.
    pub fn assigned_review_finding_ids(&self, execution_id: &str) -> Result<Vec<String>> {
        Ok(assignment(&*self.connect()?, execution_id)?
            .map(|assigned| assigned.finding_ids.into_iter().collect())
            .unwrap_or_default())
    }

    /// Identified finding ids recorded on a verdict, if any.
    pub fn review_verdict_finding_ids(&self, verdict_id: &str) -> Result<Vec<String>> {
        let raw: Option<String> = self
            .connect()?
            .query_row(
                "SELECT p.payload_json FROM pr_review_verdicts v
             JOIN worker_proposals p ON p.id = v.proposal_id
             WHERE v.id = ?1",
                params![verdict_id],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            Some(raw) => Ok(finding_ids_in_verdict_payload(&raw)?.into_iter().collect()),
            None => Ok(Vec::new()),
        }
    }
}

fn outcome_records(conn: &Connection, verdict_id: &str) -> Result<Vec<boss_protocol::WorkerProposal>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM worker_proposals
         WHERE kind = 'review_outcomes' AND state IN ('applied', 'superseded') AND applied_ref = ?1
         ORDER BY created_at, rowid",
    )?;
    let ids = stmt
        .query_map(params![verdict_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.into_iter()
        .map(|id| super::proposals::find_worker_proposal_by_id(conn, &id)?.context("outcome proposal disappeared"))
        .collect()
}

pub(crate) fn accept(
    tx: &Transaction<'_>,
    input: &SubmitWorkerProposalInput<'_>,
    prepared: Option<&PreparedOutcomes>,
    payload_json: &mut String,
) -> Result<proposal_apply::ApplyDecision> {
    let Some(prepared) = prepared else {
        return Ok(proposal_apply::ApplyDecision::Rejected(
            "review outcomes require commit verification".into(),
        ));
    };
    if prepared.payload_json != input.payload_json
        || assignment(tx, input.execution_id)?.as_ref() != Some(&prepared.assignment)
    {
        return Ok(proposal_apply::ApplyDecision::Rejected(
            "review assignment changed; resubmit outcomes".into(),
        ));
    }
    tx.execute(
        "UPDATE worker_proposals SET state = 'superseded', decided_at = ?2,
             decision_reason = 'replaced by a newer complete outcome set'
         WHERE execution_id = ?1 AND kind = 'review_outcomes' AND state = 'applied'",
        params![input.execution_id, now_string()],
    )?;
    *payload_json = serde_json::to_string(&StoredOutcomes {
        submission: serde_json::from_str(&prepared.payload_json)?,
        pr_url: prepared.assignment.pr_url.clone(),
    })?;
    Ok(proposal_apply::ApplyDecision::Applied(proposal_apply::ApplyOutcome {
        applied_ref: Some(prepared.assignment.verdict_id.clone()),
        post_commit_audit_line: None,
        review_batch_quorum_outcome: None,
    }))
}

/// Both declaration and terminal writes enforce this, including legacy completion paths.
pub(crate) fn require_complete(conn: &Connection, execution_id: &str) -> Result<()> {
    let Some(assigned) = assignment(conn, execution_id)? else {
        return Ok(());
    };
    let stored = latest_outcomes(conn, &assigned.verdict_id, Some(execution_id))?.ok_or_else(|| {
        ReviewRemediationIncomplete(
            "review remediation incomplete: submit boss propose review-outcomes --body-file before declaring done"
                .into(),
        )
    })?;
    if stored.pr_url != assigned.pr_url {
        return Err(ReviewRemediationIncomplete("remediation PR changed; resubmit review outcomes".into()).into());
    }
    validate_set(&assigned, &stored.submission).map_err(|err| ReviewRemediationIncomplete(err.to_string()).into())
}

fn latest_outcomes(conn: &Connection, verdict_id: &str, execution_id: Option<&str>) -> Result<Option<StoredOutcomes>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM worker_proposals
         WHERE kind = 'review_outcomes' AND state = 'applied' AND applied_ref = ?1
           AND (?2 IS NULL OR execution_id = ?2)
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![verdict_id, execution_id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

pub(crate) fn outcomes_on_pr(conn: &Connection, verdict_id: &str, pr_url: &str) -> Result<Vec<ReviewFindingOutcome>> {
    let Some(stored) = latest_outcomes(conn, verdict_id, None)? else {
        return Ok(vec![]);
    };
    if stored.pr_url.as_deref() != Some(pr_url) {
        return Ok(vec![]);
    }
    Ok(stored.submission.outcomes)
}
