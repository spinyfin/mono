//! Declaration gates for queue failures. These are local checks, deliberately
//! before terminal writes; neither a green PR head nor prose is verification.

use super::*;
use boss_protocol::{QueueCiEvidence, RunDoneOutcome, RunDoneProposalPayload};

pub(crate) const REQUIRED: &str = "Merge-queue CI verification required: fetch origin, rebase the PR onto current origin/main, and run every failed target with Bazel (the full job invocation for whole-job failures). Supply --queue-ci-evidence-file with main_sha, verified_head_sha, queue_sha, queue_base_sha and successful runs (check_name, source_url, command, exit_code, output, coverage). PR-head checks or isolated un-rebased reruns are not evidence. This gate does not expire; continue verification before declaring failure.";

pub(crate) fn queue_ci_verification_required(attempt: &CiRemediation) -> bool {
    // A Trunk construction rejection with no failing build is a conflict or
    // batch collision investigation, not a CI-fix. Keep its existing exit.
    attempt.failure_kind.as_deref() == Some("merge_queue_rebounce")
        || (attempt.failure_kind.as_deref() == Some("trunk_queue_eviction")
            && serde_json::from_str::<Vec<serde_json::Value>>(&attempt.failed_checks)
                .map_or(true, |checks| !checks.is_empty()))
}

/// Reasons a give-up on a Trunk eviction with no captured failing build may
/// cite without verification evidence: no build exists to verify.
pub(crate) const NO_BUILD_REASONS: [&str; 3] = ["no-failing-build-found", "merge-conflict", "sibling-collision"];

fn is_trunk_empty_capture(attempt: &CiRemediation) -> bool {
    attempt.failure_kind.as_deref() == Some("trunk_queue_eviction") && !queue_ci_verification_required(attempt)
}

fn is_lint_check(name: &str) -> bool {
    matches!(
        name.rsplit('/').next(),
        Some("checks" | "checkleft" | "lint" | "clippy")
    )
}

pub(crate) fn validate(attempt: &CiRemediation, evidence: Option<&QueueCiEvidence>) -> Result<()> {
    if !queue_ci_verification_required(attempt) {
        return Ok(());
    }
    let evidence = evidence.context(REQUIRED)?;
    for sha in [
        &evidence.main_sha,
        &evidence.verified_head_sha,
        &evidence.queue_sha,
        &evidence.queue_base_sha,
    ] {
        anyhow::ensure!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{REQUIRED} Use full commit SHAs."
        );
    }
    if attempt.failure_kind.as_deref() == Some("merge_queue_rebounce")
        && let Some(queue_sha) = &attempt.before_commit_sha
    {
        anyhow::ensure!(
            *queue_sha == evidence.queue_sha,
            "{REQUIRED} queue_sha must identify the failing queue commit."
        );
    }
    anyhow::ensure!(!evidence.runs.is_empty(), "{REQUIRED} No Bazel results supplied.");
    for run in &evidence.runs {
        let words: Vec<_> = run.command.split_whitespace().collect();
        let bazel = words.first() == Some(&"bazel")
            && matches!(words.get(1), Some(&"build" | &"test"))
            && words.iter().any(|w| w.starts_with("//"))
            && !words.iter().any(|w| w.starts_with("--test_filter"));
        anyhow::ensure!(
            (bazel || (is_lint_check(&run.check_name) && words == ["checkleft", "run"]))
                && !run.check_name.trim().is_empty()
                && run.exit_code == 0
                && !run.output.trim().is_empty()
                && !run.coverage.trim().is_empty()
                && run.source_url.starts_with("https://"),
            "{REQUIRED} Every run must report a successful complete Bazel invocation (or checkleft run for lint), its output and coverage."
        );
    }
    if let Some(base) = &attempt.queue_base_sha {
        anyhow::ensure!(
            evidence.queue_base_sha == *base,
            "{REQUIRED} queue_base_sha differs from the captured queue base."
        );
    }
    // Malformed captures require resolving the build, like missing captures.
    let checks: Vec<serde_json::Value> = serde_json::from_str(&attempt.failed_checks).unwrap_or_default();
    if checks.is_empty() {
        anyhow::ensure!(
            evidence
                .runs
                .iter()
                .any(|r| r.command.split_whitespace().take(2).eq(["bazel", "test"])),
            "{REQUIRED} Incomplete captures require Bazel test evidence for the resolved queue build."
        );
    }
    for check in checks {
        let url = check["target_url"].as_str().filter(|s| !s.is_empty());
        let name = check["name"].as_str().filter(|s| !s.is_empty()).context(REQUIRED)?;
        anyhow::ensure!(
            evidence.runs.iter().any(|r| url.is_none_or(|url| r.source_url == url)
                && r.check_name == name
                && (is_lint_check(name) || r.command.split_whitespace().nth(1) == Some("test"))),
            "{REQUIRED} Missing complete verification for check {name} (source: {url:?})."
        );
    }
    Ok(())
}

pub(crate) fn attempt_for_execution(conn: &Connection, execution_id: &str) -> Result<Option<CiRemediation>> {
    // Include terminal attempts: calling mark-failed first must not remove the
    // declaration gate. Legacy remediation executions bind through worker_id.
    let id: Option<String> = conn
        .query_row(
            "SELECT c.id FROM ci_remediations c JOIN work_executions e ON e.id = ?1
         LEFT JOIN tasks t ON t.id = e.work_item_id
         WHERE c.revision_task_id = e.work_item_id OR t.created_via = 'ci-fix:' || c.id
            OR c.worker_id = e.id
         ORDER BY c.created_at DESC LIMIT 1",
            [execution_id],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| super::mappers::query_ci_remediation(conn, &id))
        .transpose()
        .map(Option::flatten)
}

pub(crate) fn validate_declaration(
    conn: &Connection,
    execution_id: &str,
    payload: &RunDoneProposalPayload,
) -> Result<()> {
    let Some(attempt) = attempt_for_execution(conn, execution_id)? else {
        return Ok(());
    };
    if !queue_ci_verification_required(&attempt) {
        return Ok(());
    }
    validate(&attempt, payload.queue_ci_evidence.as_ref())?;
    anyhow::ensure!(
        payload.outcome != RunDoneOutcome::NoChangesNeeded,
        "Verified queue recovery is delivered, not no-changes-needed. Re-enqueue the verified head without an empty commit, then declare delivered with the evidence."
    );
    if payload.outcome == RunDoneOutcome::Delivered
        && attempt.failure_kind.as_deref() == Some("merge_queue_rebounce")
        && payload.queue_ci_evidence.as_ref().is_some_and(|e| {
            e.verified_head_sha == crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger)
        })
    {
        anyhow::ensure!(
            payload
                .queue_ci_evidence
                .as_ref()
                .and_then(|e| e.queue_entry_id.as_deref())
                .is_some_and(|id| !id.trim().is_empty()),
            "Re-enqueue the verified PR head with enqueuePullRequest and include its queue_entry_id receipt. No new SHA or empty commit is required."
        );
    }
    Ok(())
}

/// Retire the failed episode atomically with accepting its requeue delivery.
/// The applied proposal retains the receipt and full verification evidence.
/// Archive the head dedup key by queue SHA so the same failure remains deduped,
/// but a later construction at the same PR head can consume the next budget slot.
pub(crate) fn record_delivery(conn: &Connection, execution_id: &str, payload: &RunDoneProposalPayload) -> Result<()> {
    let Some(attempt) = attempt_for_execution(conn, execution_id)? else {
        return Ok(());
    };
    if payload.outcome == RunDoneOutcome::Delivered
        && attempt.failure_kind.as_deref() == Some("merge_queue_rebounce")
        && let Some(evidence) = &payload.queue_ci_evidence
        && attempt.failure_reason.as_deref() != Some("verified_requeue")
        && evidence.queue_entry_id.as_ref().is_some_and(|id| !id.trim().is_empty())
    {
        let head = super::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger);
        conn.execute(
            "UPDATE ci_remediations SET status = 'abandoned', finished_at = ?2,
             failure_reason = 'verified_requeue', head_sha_at_trigger = ?3, head_sha_after = ?4 WHERE id = ?1",
            params![
                attempt.id,
                now_string(),
                format!("mq:{head}@{}", evidence.queue_sha),
                evidence.verified_head_sha
            ],
        )?;
        conn.execute(
            "UPDATE task_blocked_signals SET cleared_at = ?2 WHERE work_item_id = ?1
             AND reason IN ('ci_failure', 'ci_failure_exhausted', 'ci_flaky_retriggered') AND cleared_at IS NULL
             AND NOT EXISTS (SELECT 1 FROM ci_remediations WHERE work_item_id = ?1 AND status IN ('pending', 'running'))",
            params![attempt.work_item_id, now_string()],
        )?;
    }
    Ok(())
}

impl WorkDb {
    /// A Trunk eviction with an empty capture is not gated on evidence because
    /// it may have no build. A give-up there must either carry evidence (the
    /// worker found a failing build and verified it) or name a no-build reason.
    pub(crate) fn validate_trunk_no_build_give_up(
        &self,
        attempt_id: &str,
        evidence: Option<&QueueCiEvidence>,
        reason: &str,
    ) -> Result<()> {
        let attempt = self.get_ci_remediation(attempt_id)?.context("unknown CI attempt")?;
        if !is_trunk_empty_capture(&attempt) || evidence.is_some() {
            return Ok(());
        }
        anyhow::ensure!(
            NO_BUILD_REASONS.contains(&reason.trim()),
            "Giving up on a Trunk eviction requires --queue-ci-evidence-file when a failing trunk-merge/pr-N/* build exists (verify it with Bazel on a rebased tree). Without a failing build, --reason must be one of: {}.",
            NO_BUILD_REASONS.join(", ")
        );
        Ok(())
    }

    pub(crate) fn validate_queue_ci_failure(&self, attempt_id: &str, evidence: Option<&QueueCiEvidence>) -> Result<()> {
        let attempt = self.get_ci_remediation(attempt_id)?.context("unknown CI attempt")?;
        validate(&attempt, evidence)
    }

    pub(crate) fn has_verified_queue_delivery(&self, execution_id: &str, head_sha: &str) -> Result<bool> {
        let conn = self.connect()?;
        let payload: Option<String> = conn.query_row(
            "SELECT payload_json FROM worker_proposals WHERE execution_id = ?1 AND kind = 'run_done' AND state = 'applied' ORDER BY created_at DESC LIMIT 1",
            [execution_id], |row| row.get(0),
        ).optional()?;
        let Some(payload) = payload else {
            return Ok(false);
        };
        let payload: RunDoneProposalPayload = serde_json::from_str(&payload)?;
        Ok(payload.outcome == RunDoneOutcome::Delivered
            && payload
                .queue_ci_evidence
                .as_ref()
                .is_some_and(|e| e.verified_head_sha == head_sha)
            && attempt_for_execution(&conn, execution_id)?
                .is_some_and(|a| crate::ci_watch::is_queue_side_failure_kind(a.failure_kind.as_deref()))
            && validate_declaration(&conn, execution_id, &payload).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{ProposalKind, ProposalState, QueueCiRun};

    const URL: &str = "https://buildkite.com/flunge/mono/builds/20619#failed-job";

    fn evidence() -> QueueCiEvidence {
        QueueCiEvidence::builder()
            .main_sha("a".repeat(40))
            .verified_head_sha("b".repeat(40))
            .queue_sha("c".repeat(40))
            .queue_base_sha("d".repeat(40))
            .runs(vec![QueueCiRun {
                check_name: "bazel-build-test".into(),
                source_url: URL.into(),
                command: "bazel test //tools/boss/engine/core:engine_lib_test".into(),
                exit_code: 0,
                output: "11 shards passed".into(),
                coverage: "All failed targets from bazel-build-test, all shards".into(),
            }])
            .build()
    }

    #[test]
    fn queue_failure_declarations_require_evidence_on_every_attempt() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "Fix queue CI");
        let execution = crate::test_support::create_ready_chore_execution(&db, revision.id.clone());
        let attempt = db
            .insert_ci_remediation_with_queue_base(
                CiRemediationInsertInput {
                    product_id: product.id,
                    work_item_id: revision.id.clone(),
                    pr_url: "https://github.com/spinyfin/mono/pull/3145".into(),
                    pr_number: 3145,
                    head_branch: "feature".into(),
                    head_sha_at_trigger: "b".repeat(40),
                    attempt_kind: "fix".into(),
                    consumes_budget: 1,
                    failed_checks: serde_json::json!([{"name": "bazel-build-test", "target_url": URL}]).to_string(),
                    failure_kind: "merge_queue_rebounce".into(),
                    before_commit_sha: Some("c".repeat(40)),
                },
                Some(&"d".repeat(40)),
            )
            .unwrap()
            .unwrap();
        db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        let submit = |key: &str, payload: &RunDoneProposalPayload| {
            db.submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &revision.id,
                kind: ProposalKind::RunDone,
                payload_json: &serde_json::to_string(payload).unwrap(),
                idempotency_key: key,
            })
            .unwrap()
            .unwrap()
            .proposal
        };
        let mut payload = RunDoneProposalPayload {
            outcome: RunDoneOutcome::Blocked,
            summary: "PR branch passes; infra".into(),
            question: None,
            queue_ci_evidence: None,
        };
        for key in ["first", "second", "third"] {
            let rejected = submit(key, &payload);
            assert_eq!(rejected.state, ProposalState::Rejected);
            assert!(rejected.decision_reason.unwrap().contains("origin/main"));
            assert_eq!(db.execution_run_done_outcome(&execution.id).unwrap(), None);
        }
        assert!(db.validate_queue_ci_failure(&attempt.id, None).is_err());
        let mut proof = evidence();
        for bad in [
            "exit",
            "source",
            "queue",
            "base",
            "filter",
            "lint-only",
            "build-only",
            "empty",
        ] {
            let mut invalid = proof.clone();
            match bad {
                "lint-only" => invalid.runs[0].command = "checkleft run".into(),
                "build-only" => invalid.runs[0].command = "bazel build //...".into(),
                "exit" => invalid.runs[0].exit_code = 1,
                "source" => invalid.runs[0].source_url = "https://example.org/pr-head".into(),
                "queue" => invalid.queue_sha = "e".repeat(40),
                "base" => invalid.queue_base_sha = "e".repeat(40),
                "filter" => invalid.runs[0].command.push_str(" --test_filter=isolated"),
                _ => invalid.runs.clear(),
            }
            assert!(
                db.validate_queue_ci_failure(&attempt.id, Some(&invalid)).is_err(),
                "{bad}"
            );
        }
        assert!(db.validate_queue_ci_failure(&attempt.id, Some(&proof)).is_ok());
        let mut incomplete = attempt.clone();
        incomplete.failed_checks = serde_json::json!([{"name": "bazel-build-test", "target_url": ""}]).to_string();
        assert!(validate(&incomplete, Some(&proof)).is_ok());
        incomplete.failed_checks = "{malformed".into();
        incomplete.failure_kind = Some("trunk_queue_eviction".into());
        assert!(queue_ci_verification_required(&incomplete));
        assert!(validate(&incomplete, Some(&proof)).is_ok());
        incomplete.failed_checks = "[]".into();
        incomplete.failure_kind = Some("merge_queue_rebounce".into());
        incomplete.queue_base_sha = Some("e".repeat(40));
        assert!(validate(&incomplete, Some(&proof)).is_err());
        incomplete.queue_base_sha = Some(proof.queue_base_sha.clone());
        assert!(validate(&incomplete, Some(&proof)).is_ok());
        let mut shared_url = attempt.clone();
        shared_url.failed_checks = serde_json::json!([
            {"name": "bazel-build-test", "target_url": URL},
            {"name": "checks", "target_url": URL}
        ])
        .to_string();
        assert!(validate(&shared_url, Some(&proof)).is_err());
        let mut complete = proof.clone();
        let mut lint_run = complete.runs[0].clone();
        lint_run.check_name = "checks".into();
        lint_run.command = "checkleft run".into();
        complete.runs.push(lint_run);
        assert!(validate(&shared_url, Some(&complete)).is_ok());
        payload.queue_ci_evidence = Some(proof.clone());
        assert_eq!(submit("verified-failure", &payload).state, ProposalState::Applied);
        // A prior mark-failed cannot erase the declaration guard.
        db.mark_ci_remediation_failed(&attempt.id, "verified external blocker")
            .unwrap();
        payload.queue_ci_evidence = None;
        assert_eq!(submit("after-failure", &payload).state, ProposalState::Rejected);
        payload.outcome = RunDoneOutcome::Delivered;
        payload.queue_ci_evidence = Some(proof.clone());
        assert_eq!(submit("missing-requeue", &payload).state, ProposalState::Rejected);
        proof.queue_entry_id = Some("MQE_receipt".into());
        payload.queue_ci_evidence = Some(proof);
        assert_eq!(submit("same-head-requeued", &payload).state, ProposalState::Applied);
        assert!(db.has_verified_queue_delivery(&execution.id, &"b".repeat(40)).unwrap());
        assert!(!db.has_verified_queue_delivery(&execution.id, &"e".repeat(40)).unwrap());
        let changed = payload.queue_ci_evidence.as_mut().unwrap();
        changed.verified_head_sha = "e".repeat(40);
        changed.queue_entry_id = None;
        assert_eq!(
            submit("changed-head-auto-merge", &payload).state,
            ProposalState::Applied
        );
    }
}
