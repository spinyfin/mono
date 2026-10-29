//! Declaration gates for queue failures. These are local checks, deliberately
//! before terminal writes; neither a green PR head nor prose is verification.

use super::*;
use boss_protocol::{QueueCiEvidence, RunDoneOutcome, RunDoneProposalPayload};

pub(crate) const REQUIRED: &str = "Merge-queue CI verification required: fetch origin, rebase the PR onto current origin/main, and run every command in each failed job's captured verification contract (BOSS_CI_VERIFICATION.json). For jobs without a declaration, run every failed target with Bazel (the full job invocation for whole-job failures; `checkleft run` for a lint job; the job's own npm/pnpm/yarn invocation for a non-Bazel job). Supply --queue-ci-evidence-file with main_sha, verified_head_sha, queue_sha, queue_base_sha and successful runs (check_name, source_url, command, exit_code, output, coverage). A requeue of an unchanged head also needs queue_entry_id and queue_entry_enqueued_at from the enqueuePullRequest receipt (mergeQueueEntry { id enqueuedAt }). PR-head checks or isolated un-rebased reruns are not evidence. This gate does not expire; continue verification before declaring failure.";

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

/// Lowercased alphanumeric tokens of a failed check's job label: the last `/`
/// segment, which for a Trunk capture is the Buildkite step key or job name
/// (see `ci_watch::trunk_eviction_failures_from_build`) and for a GitHub queue
/// capture is the check name. Leading `:emoji:` words are ignored, and tokens
/// are whole words so `lint` never matches inside another word.
fn job_tokens(name: &str) -> Vec<String> {
    let segment = name.rsplit('/').next().unwrap_or(name);
    segment
        .split_whitespace()
        .filter(|w| !(w.len() > 1 && w.starts_with(':') && w.ends_with(':')))
        .flat_map(|w| w.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// What a failed job's label says its verification must cover. A combined job
/// (`frontend-lint-and-test`) needs every part; a label that names none of
/// them is treated as a test job.
#[derive(Debug, Default, PartialEq, Eq)]
struct JobNeeds {
    lint: bool,
    test: bool,
    build: bool,
    /// A Bazel job is verified with Bazel only: a project command never
    /// covers it.
    bazel: bool,
}

fn job_needs(name: &str) -> JobNeeds {
    let tokens = job_tokens(name);
    let has = |words: &[&str]| tokens.iter().any(|t| words.contains(&t.as_str()));
    let mut needs = JobNeeds {
        lint: has(&["lint", "linting", "clippy", "checkleft"])
            || matches!(tokens.first().map(String::as_str), Some("check" | "checks")),
        test: has(&["test", "tests", "testing", "e2e"]),
        build: has(&["build", "builds"]),
        bazel: has(&["bazel"]),
    };
    needs.test |= !needs.lint;
    needs
}

/// What one run demonstrates. Only Bazel tests also demonstrate a build.
#[derive(Debug, Default, PartialEq, Eq)]
struct RunCaps {
    lint: bool,
    test: bool,
    build: bool,
}

/// Flags that narrow a run to a subset of the job's tests, for the npm-style
/// runners (Bazel's `--test_filter` is rejected separately).
fn is_filter_flag(word: &str) -> bool {
    matches!(word, "-t" | "-g")
        || [
            "--grep",
            "--testPathPattern",
            "--testNamePattern",
            "--test-name-pattern",
            "--test-path-pattern",
            "--filter",
            "--only",
            "--test_filter",
        ]
        .iter()
        .any(|f| word.starts_with(f))
}

fn is_bazel_command(words: &[&str]) -> bool {
    words.first() == Some(&"bazel")
        && matches!(words.get(1), Some(&"build" | &"test"))
        && words.iter().any(|w| w.starts_with("//"))
        && !words.iter().any(|w| is_filter_flag(w))
}

/// A non-Bazel job's own invocation, e.g. flunge's
/// `npm ci && npm run build && npm run test`: every `&&` segment must be an
/// npm/pnpm/yarn command and none may narrow the run with a filter.
fn is_project_command(command: &str) -> bool {
    let mut segments = command.split("&&").map(str::trim).peekable();
    segments.peek().is_some()
        && segments.all(|segment| {
            let mut words = segment.split_whitespace();
            matches!(words.next(), Some("npm" | "pnpm" | "yarn")) && !words.any(is_filter_flag)
        })
}

/// True when the command runs script `name` or a `name:*` variant
/// (`npm run test:unit`).
fn runs_script(words: &[&str], name: &str) -> bool {
    words.split(|w| *w == "&&").any(|segment| {
        let script = match segment {
            ["npm" | "pnpm" | "yarn", "run", script, ..] => Some(*script),
            ["npm" | "pnpm" | "yarn", script, ..] => Some(*script),
            _ => None,
        };
        script.is_some_and(|w| w == name || w.strip_prefix(name).is_some_and(|rest| rest.starts_with(':')))
    })
}

fn run_caps(command: &str, bazel_job: bool) -> RunCaps {
    let words: Vec<_> = command.split_whitespace().collect();
    if words == ["checkleft", "run"] {
        return RunCaps {
            lint: true,
            ..RunCaps::default()
        };
    }
    if is_bazel_command(&words) {
        return RunCaps {
            test: words.get(1) == Some(&"test"),
            build: true,
            ..RunCaps::default()
        };
    }
    if !bazel_job && is_project_command(command) {
        let test = runs_script(&words, "test");
        return RunCaps {
            test,
            build: runs_script(&words, "build"),
            ..RunCaps::default()
        };
    }
    RunCaps::default()
}

/// A run that can cover a failed non-lint job: a Bazel test, or a project
/// command that runs the job's tests.
fn is_test_run(command: &str) -> bool {
    run_caps(command, false).test
}

pub(crate) fn validate(attempt: &CiRemediation, evidence: Option<&QueueCiEvidence>) -> Result<()> {
    if !queue_ci_verification_required(attempt) {
        return Ok(());
    }
    validate_evidence(attempt, evidence.context(REQUIRED)?)
}

/// True when `text` names one of [`NO_BUILD_REASONS`], bare or as a prefix
/// followed by `:` or whitespace (so a summary may elaborate on the reason).
fn names_no_build_reason(text: &str) -> bool {
    let text = text.trim();
    NO_BUILD_REASONS.iter().any(|reason| {
        text.strip_prefix(reason)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with(char::is_whitespace))
    })
}

/// Gate for any terminal give-up (classify with a non-`tractable` class,
/// mark-failed, run_done blocked) on a queue-side attempt. Verified-evidence
/// attempts always need full evidence. A Trunk eviction with an empty capture
/// may have no build to verify: evidence, when supplied, is fully validated;
/// without it the give-up must name a [`NO_BUILD_REASONS`] reason.
pub(crate) fn validate_give_up(
    attempt: &CiRemediation,
    evidence: Option<&QueueCiEvidence>,
    reason: &str,
) -> Result<()> {
    if is_trunk_empty_capture(attempt) {
        return match evidence {
            Some(evidence) => validate_evidence(attempt, evidence),
            None => {
                anyhow::ensure!(
                    names_no_build_reason(reason),
                    "Giving up on a Trunk eviction requires --queue-ci-evidence-file when a failing trunk-merge/pr-N/* build exists (verify it with Bazel on a rebased tree). Without a failing build, the reason must be one of: {}.",
                    NO_BUILD_REASONS.join(", ")
                );
                Ok(())
            }
        };
    }
    validate(attempt, evidence)
}

fn validate_evidence(attempt: &CiRemediation, evidence: &QueueCiEvidence) -> Result<()> {
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
    anyhow::ensure!(
        !evidence.runs.is_empty(),
        "{REQUIRED} No verification results supplied."
    );
    let checks: Vec<serde_json::Value> = serde_json::from_str(&attempt.failed_checks).unwrap_or_default();
    for run in &evidence.runs {
        let needs = job_needs(&run.check_name);
        let caps = run_caps(&run.command, needs.bazel);
        let declared = checks.iter().any(|check| {
            check["name"] == run.check_name
                && check["target_url"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .is_none_or(|u| u == run.source_url)
                && check.get("verification").is_some()
        });
        anyhow::ensure!(
            (declared || caps.test || caps.build || (caps.lint && needs.lint))
                && !run.command.trim().is_empty()
                && !run.check_name.trim().is_empty()
                && run.exit_code == 0
                && !run.output.trim().is_empty()
                && !run.coverage.trim().is_empty()
                && run.source_url.starts_with("https://"),
            "{REQUIRED} Every run must report a successful complete invocation declared for its job, or for undeclared jobs (a Bazel build/test of the job's targets, `checkleft run` for a lint job, or the job's own npm/pnpm/yarn build/test script for a non-Bazel job; no test filters), its output and coverage."
        );
    }
    if let Some(base) = &attempt.queue_base_sha {
        anyhow::ensure!(
            evidence.queue_base_sha == *base,
            "{REQUIRED} queue_base_sha differs from the captured queue base."
        );
    }
    // Malformed captures require resolving the build, like missing captures.
    if checks.is_empty() {
        anyhow::ensure!(
            evidence.runs.iter().any(|r| is_test_run(&r.command)),
            "{REQUIRED} Incomplete captures require test evidence for the resolved queue build."
        );
    }
    for check in checks {
        let url = check["target_url"].as_str().filter(|s| !s.is_empty());
        let name = check["name"].as_str().filter(|s| !s.is_empty()).context(REQUIRED)?;
        if let Some(contract) = check.get("verification") {
            let job_key = check["job_key"]
                .as_str()
                .filter(|key| !key.is_empty())
                .context("Declared verification is missing its stable job key")?;
            let required: Vec<crate::queue_ci_contract::RequiredRun> = serde_json::from_value(contract.clone())?;
            crate::queue_ci_contract::validate_contract(&required)?;
            let runs: Vec<_> = evidence
                .runs
                .iter()
                .filter(|r| r.check_name == name && url.is_none_or(|u| r.source_url == u))
                .collect();
            anyhow::ensure!(
                required
                    .iter()
                    .all(|required| runs.iter().any(|r| r.command.trim() == required.command.trim())),
                "{REQUIRED} Missing or mismatched declared command for {name} (job {job_key})."
            );
            continue;
        }
        let needs = job_needs(name);
        let covering = || {
            evidence
                .runs
                .iter()
                .filter(|r| url.is_none_or(|url| r.source_url == url) && r.check_name == name)
                .map(|r| run_caps(&r.command, needs.bazel))
        };
        anyhow::ensure!(
            (!needs.lint || covering().any(|c| c.lint))
                && (!needs.test || covering().any(|c| c.test))
                && (!needs.build || covering().any(|c| c.build)),
            "{REQUIRED} Missing complete verification for check {name} (source: {url:?})."
        );
    }
    Ok(())
}

/// What the post-hoc audit needs from an applied queue-CI declaration.
pub(crate) struct QueueCiAuditInput {
    pub attempt: CiRemediation,
    pub evidence: QueueCiEvidence,
    /// `delivered` (the receipt is checked too) rather than `blocked`.
    pub delivered: bool,
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

/// How far a receipt's `enqueuedAt` may sit outside the attempt's lifetime
/// (before its creation, or ahead of now) to absorb clock skew.
const RECEIPT_CLOCK_SKEW_SECS: i64 = 300;

/// Whether `enqueued_at` can be the time of a requeue made for an attempt
/// created at `created_epoch`, judged at `now_epoch`. The timestamp is a dedup
/// suppression boundary, so a future or pre-attempt value must not be accepted.
fn receipt_time_plausible(enqueued_at: &str, created_epoch: Option<i64>, now_epoch: i64) -> bool {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(enqueued_at) else {
        return false;
    };
    let t = t.timestamp();
    t <= now_epoch + RECEIPT_CLOCK_SKEW_SECS && created_epoch.is_none_or(|c| t >= c - RECEIPT_CLOCK_SKEW_SECS)
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A requeue receipt names the queue entry and when it was enqueued, within
/// the attempt's lifetime.
fn requeue_receipt_present(evidence: &QueueCiEvidence, attempt: &CiRemediation) -> bool {
    evidence.queue_entry_id.as_ref().is_some_and(|id| !id.trim().is_empty())
        && evidence
            .queue_entry_enqueued_at
            .as_deref()
            .is_some_and(|t| receipt_time_plausible(t, attempt.created_at.trim().parse().ok(), epoch_now()))
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
        // Trunk empty capture: only a give-up is gated; supplied evidence is
        // always validated in full.
        if is_trunk_empty_capture(&attempt) {
            if payload.outcome == RunDoneOutcome::Blocked {
                validate_give_up(&attempt, payload.queue_ci_evidence.as_ref(), &payload.summary)?;
            } else if let Some(evidence) = &payload.queue_ci_evidence {
                validate_evidence(&attempt, evidence)?;
            }
        }
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
                .is_some_and(|e| requeue_receipt_present(e, &attempt)),
            "Re-enqueue the verified PR head with enqueuePullRequest and include its queue_entry_id and queue_entry_enqueued_at (mergeQueueEntry {{ id enqueuedAt }}) receipt, with enqueuedAt after the attempt was created and not in the future. No new SHA or empty commit is required."
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
        && requeue_receipt_present(evidence, &attempt)
    {
        let head = super::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger);
        conn.execute(
            "UPDATE ci_remediations SET status = 'abandoned', finished_at = ?2,
             failure_reason = 'verified_requeue', head_sha_at_trigger = ?3, head_sha_after = ?4, requeue_enqueued_at = ?5 WHERE id = ?1",
            params![
                attempt.id,
                now_string(),
                format!("mq:{head}@{}", evidence.queue_sha),
                evidence.verified_head_sha,
                evidence.queue_entry_enqueued_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp())
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

#[work_db_boundary::instrument]
impl WorkDb {
    /// Restore a falsely retired queue episode and its parent in one transaction.
    /// Clean head CI must still see an active queue-side attempt after the audit.
    pub(crate) fn rearm_audited_queue_failure(&self, attempt: &CiRemediation) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let newer: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ci_remediations WHERE work_item_id = ?1 AND id != ?2 AND CAST(created_at AS INTEGER) >= CAST(?3 AS INTEGER))",
            params![attempt.work_item_id, attempt.id, attempt.created_at],
            |row| row.get(0),
        )?;
        if newer {
            return Ok(false);
        }
        let head = super::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger);
        let suppressed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ci_failure_suppressions WHERE work_item_id = ?1 AND head_sha = ?2)",
            params![attempt.work_item_id, head],
            |row| row.get(0),
        )?;
        if suppressed {
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE ci_remediations SET status = 'pending', failure_reason = NULL, finished_at = NULL,
             head_sha_at_trigger = ?2, requeue_enqueued_at = NULL
             WHERE id = ?1 AND status = 'abandoned' AND failure_reason = 'verified_requeue'
             AND EXISTS (SELECT 1 FROM tasks WHERE id = ?3 AND deleted_at IS NULL
                 AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'ci_failure')))",
            params![attempt.id, format!("mq:{head}"), attempt.work_item_id],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        let now = now_string();
        crate::work::task_status_audit::execute(
            &tx,
            "rearm_audited_queue_failure",
            "engine",
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure', updated_at = ?2 WHERE id = ?1",
            params![attempt.work_item_id, now],
        )?;
        upsert_task_blocked_signal(&tx, &attempt.work_item_id, "ci_failure", Some(&attempt.id), &now)?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn validate_queue_ci_failure(
        &self,
        attempt_id: &str,
        evidence: Option<&QueueCiEvidence>,
        reason: &str,
    ) -> Result<()> {
        let attempt = self.get_ci_remediation(attempt_id)?.context("unknown CI attempt")?;
        validate_give_up(&attempt, evidence, reason)
    }

    /// The queue-side attempt and evidence behind this execution's applied
    /// `delivered` or `blocked` declaration, for the post-hoc evidence audit.
    pub(crate) fn queue_ci_delivery_for_audit(&self, execution_id: &str) -> Result<Option<QueueCiAuditInput>> {
        let conn = self.connect()?;
        let payload: Option<String> = conn.query_row(
            "SELECT payload_json FROM worker_proposals WHERE execution_id = ?1 AND kind = 'run_done' AND state = 'applied' ORDER BY created_at DESC LIMIT 1",
            [execution_id], |row| row.get(0),
        ).optional()?;
        let Some(payload) = payload else {
            return Ok(None);
        };
        let payload: RunDoneProposalPayload = serde_json::from_str(&payload)?;
        let (outcome @ (RunDoneOutcome::Delivered | RunDoneOutcome::Blocked), Some(evidence)) =
            (payload.outcome, payload.queue_ci_evidence)
        else {
            return Ok(None);
        };
        Ok(attempt_for_execution(&conn, execution_id)?
            .filter(|a| crate::ci_watch::is_queue_side_failure_kind(a.failure_kind.as_deref()))
            .map(|attempt| QueueCiAuditInput {
                attempt,
                evidence,
                delivered: outcome == RunDoneOutcome::Delivered,
            }))
    }

    /// True when the work item has a CI attempt other than `attempt_id`
    /// created at or after `created_at` (epoch seconds): the failure signal
    /// has already been picked up again.
    pub(crate) fn has_newer_ci_remediation(
        &self,
        work_item_id: &str,
        attempt_id: &str,
        created_at: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ci_remediations WHERE work_item_id = ?1 AND id != ?2
              AND CAST(created_at AS INTEGER) >= CAST(?3 AS INTEGER))",
            params![work_item_id, attempt_id, created_at],
            |row| row.get(0),
        )?)
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

    fn is_lint_check(name: &str) -> bool {
        job_needs(name).lint
    }
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
        assert!(db.validate_queue_ci_failure(&attempt.id, None, "x").is_err());
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
                db.validate_queue_ci_failure(&attempt.id, Some(&invalid), "x").is_err(),
                "{bad}"
            );
        }
        assert!(db.validate_queue_ci_failure(&attempt.id, Some(&proof), "x").is_ok());
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
        assert!(
            !requeue_receipt_present(&proof, &attempt),
            "a receipt id alone does not anchor the requeue"
        );
        let at = |offset: i64| {
            chrono::DateTime::from_timestamp(epoch_now() + offset, 0)
                .unwrap()
                .to_rfc3339()
        };
        for (bad, when) in [("future", at(86_400)), ("pre-attempt", at(-86_400))] {
            proof.queue_entry_enqueued_at = Some(when);
            assert!(!requeue_receipt_present(&proof, &attempt), "{bad}");
        }
        proof.queue_entry_enqueued_at = Some(at(0));
        assert!(requeue_receipt_present(&proof, &attempt));
        payload.queue_ci_evidence = Some(proof);
        assert_eq!(submit("same-head-requeued", &payload).state, ProposalState::Applied);
        assert!(db.has_verified_queue_delivery(&execution.id, &"b".repeat(40)).unwrap());
        assert!(!db.has_verified_queue_delivery(&execution.id, &"e".repeat(40)).unwrap());
        let changed = payload.queue_ci_evidence.as_mut().unwrap();
        changed.verified_head_sha = "e".repeat(40);
        changed.queue_entry_id = None;
        changed.queue_entry_enqueued_at = None;
        assert_eq!(
            submit("changed-head-auto-merge", &payload).state,
            ProposalState::Applied
        );
    }

    /// Trunk captures name each failed job `Trunk merge queue: <pipeline>/<label>`
    /// (see `ci_watch::trunk_eviction_failures_from_build`), differing by the
    /// `#job` fragment. A lint job and a non-Bazel job must be evidenceable.
    #[test]
    fn trunk_shaped_lint_and_non_bazel_jobs_are_evidenceable() {
        let build = boss_ci_log_reader::TrunkEvictionBuild {
            pipeline_slug: "flunge-ci".into(),
            build_number: "7".into(),
            web_url: "https://buildkite.com/flunge/flunge-ci/builds/7".into(),
            failed_job_ids: vec!["job-lint".into(), "job-fe".into()],
            failed_job_details: vec![":mag: checks-framework", "frontend-tests"]
                .into_iter()
                .map(|label| boss_ci_log_reader::TrunkJobDetails {
                    label: label.into(),
                    step_key: None,
                })
                .collect(),
        };
        let failures = crate::ci_watch::trunk_eviction_failures_from_build(&build);
        let checks: Vec<_> = failures
            .iter()
            .map(|f| serde_json::json!({"name": f.name, "target_url": f.target_url}))
            .collect();
        assert!(is_lint_check(&failures[0].name));
        assert!(!is_lint_check(&failures[1].name));
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "Trunk CI");
        let mut attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.id,
                work_item_id: revision.id.clone(),
                pr_url: "https://github.com/spinyfin/mono/pull/3150".into(),
                pr_number: 3150,
                head_branch: "feature".into(),
                head_sha_at_trigger: "trunk:entry_2@2026-07-27T23:07:13.000Z".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: serde_json::Value::Array(checks).to_string(),
                failure_kind: "trunk_queue_eviction".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        let run = |i: usize, command: &str| QueueCiRun {
            check_name: failures[i].name.clone(),
            source_url: failures[i].target_url.clone(),
            command: command.into(),
            exit_code: 0,
            output: "ok".into(),
            coverage: "whole job".into(),
        };
        let mut proof = evidence();
        proof.runs = vec![
            run(0, "checkleft run"),
            run(1, "npm ci && npm run build && npm run test"),
        ];
        assert!(validate(&attempt, Some(&proof)).is_ok());
        // Each job still needs its own successful, matching verification.
        proof.runs[1] = run(1, "checkleft run");
        assert!(validate(&attempt, Some(&proof)).is_err());
        proof.runs[1] = run(1, "npm ci && make test");
        assert!(validate(&attempt, Some(&proof)).is_err());
        proof.runs = vec![run(0, "checkleft run")];
        assert!(validate(&attempt, Some(&proof)).is_err());
        // A GitHub-style `checks-framework` lint name is also a lint job.
        assert!(is_lint_check("checks-framework"));
        attempt.failed_checks = "[]".into();
        assert!(!is_lint_check("bazel-build-test"));
    }

    fn job_attempt(db: &WorkDb, checks: serde_json::Value) -> CiRemediation {
        let product = crate::test_support::create_test_product(db);
        let revision = crate::test_support::create_test_chore(db, product.id.clone(), "Job CI");
        db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id,
            work_item_id: revision.id,
            pr_url: "https://github.com/spinyfin/mono/pull/3160".into(),
            pr_number: 3160,
            head_branch: "feature".into(),
            head_sha_at_trigger: "trunk:entry_3@2026-07-27T23:07:13.000Z".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: checks.to_string(),
            failure_kind: "trunk_queue_eviction".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap()
    }

    fn job_proof(name: &str, commands: &[&str]) -> QueueCiEvidence {
        let mut proof = evidence();
        proof.runs = commands
            .iter()
            .map(|command| QueueCiRun {
                check_name: name.into(),
                source_url: URL.into(),
                command: (*command).into(),
                exit_code: 0,
                output: "ok".into(),
                coverage: "whole job".into(),
            })
            .collect();
        proof
    }

    #[test]
    fn superseding_blocked_declarations_cannot_replace_a_persisted_requeue_boundary() {
        let (_dir, db) = crate::test_support::open_db();
        let attempt = job_attempt(&db, serde_json::json!([{"name":"bazel-build-test","target_url":URL}]));
        let revision = &attempt.work_item_id;
        let execution = crate::test_support::create_ready_chore_execution(&db, revision.clone());
        db.set_ci_remediation_revision_task_id(&attempt.id, revision).unwrap();
        db.connect().unwrap().execute(
            "UPDATE ci_remediations SET failure_kind = 'merge_queue_rebounce', head_sha_at_trigger = ?2, before_commit_sha = ?3 WHERE id = ?1",
            params![attempt.id, "b".repeat(40), "c".repeat(40)],
        ).unwrap();
        let now = epoch_now();
        let at = |offset| chrono::DateTime::from_timestamp(now + offset, 0).unwrap().to_rfc3339();
        let mut proof = evidence();
        proof.queue_entry_id = Some("MQE_receipt".into());
        proof.queue_entry_enqueued_at = Some(at(0));
        let mut payload = RunDoneProposalPayload {
            outcome: RunDoneOutcome::Delivered,
            summary: "Verified and requeued".into(),
            question: None,
            queue_ci_evidence: Some(proof),
        };
        let submit = |key: &str, payload: &RunDoneProposalPayload| {
            db.submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: revision,
                kind: ProposalKind::RunDone,
                payload_json: &serde_json::to_string(payload).unwrap(),
                idempotency_key: key,
            })
            .unwrap()
            .unwrap()
            .proposal
            .state
        };
        assert_eq!(submit("delivered", &payload), ProposalState::Applied);
        let boundary = db.verified_requeue_boundary(revision, &"b".repeat(40)).unwrap();
        assert_eq!(boundary, Some(now));
        payload.outcome = RunDoneOutcome::Blocked;
        payload.queue_ci_evidence.as_mut().unwrap().queue_entry_enqueued_at = None;
        assert_eq!(submit("supersede-without-receipt", &payload), ProposalState::Applied);
        assert_eq!(
            db.verified_requeue_boundary(revision, &"b".repeat(40)).unwrap(),
            boundary
        );
        payload.queue_ci_evidence.as_mut().unwrap().queue_entry_enqueued_at = Some(at(86_400));
        assert_eq!(submit("supersede-future-receipt", &payload), ProposalState::Applied);
        assert_eq!(
            db.verified_requeue_boundary(revision, &"b".repeat(40)).unwrap(),
            boundary
        );
        db.correct_requeue_boundary(&attempt.id, now - 10).unwrap();
        assert_eq!(
            db.verified_requeue_boundary(revision, &"b".repeat(40)).unwrap(),
            Some(now - 10)
        );
    }

    #[test]
    fn declared_job_commands_require_every_step_and_the_exact_tool() {
        let (_dir, db) = crate::test_support::open_db();
        for (name, tool, commands, wrong) in [
            ("make-suite", "project", vec![("test", "make test")], "npm test"),
            (
                "mac-app-build",
                "bazel",
                vec![("build", "bazel build //app/...")],
                "npm run build",
            ),
            (
                "frontend",
                "project",
                vec![("build", "npm run build"), ("test", "npm run test")],
                "npx arbitrary test",
            ),
        ] {
            let contract: Vec<_> = commands
                .iter()
                .map(|(kind, command)| serde_json::json!({"kind":kind,"tool":tool,"command":command}))
                .collect();
            let attempt = job_attempt(
                &db,
                serde_json::json!([{"name":name,"target_url":URL,"job_key":name,"provider_job_id":"uuid","verification":contract}]),
            );
            let required: Vec<_> = commands.iter().map(|(_, command)| *command).collect();
            assert!(validate(&attempt, Some(&job_proof(name, &required))).is_ok());
            let mut extra = required.clone();
            extra.extend(["bazel test //pkg:target", "checkleft run"]);
            assert!(validate(&attempt, Some(&job_proof(name, &extra))).is_ok());
            let mut failed_extra = job_proof(name, &extra);
            failed_extra.runs.last_mut().unwrap().exit_code = 1;
            assert!(validate(&attempt, Some(&failed_extra)).is_err());
            assert!(validate(&attempt, Some(&job_proof(name, &[wrong]))).is_err());
            if required.len() > 1 {
                assert!(validate(&attempt, Some(&job_proof(name, &required[1..]))).is_err());
            }
        }
    }

    #[test]
    fn verification_is_decided_per_job_from_whole_label_tokens() {
        let (_dir, db) = crate::test_support::open_db();
        let check = |name: &str| serde_json::json!([{"name": name, "target_url": URL}]);
        let accepts = |name: &str, commands: &[&str]| {
            validate(&job_attempt(&db, check(name)), Some(&job_proof(name, commands))).is_ok()
        };
        // Whole-token lint detection: a substring never counts.
        assert!(!is_lint_check("Trunk merge queue: ci/splinter-suite"));
        assert!(is_lint_check("Trunk merge queue: ci/:mag: lint"));
        assert!(!is_lint_check("ci/:white_check_mark: frontend-tests"));
        // A combined job needs both its lint and its test evidence.
        let combined = "Trunk merge queue: ci/frontend-lint-and-test";
        assert!(!accepts(combined, &["checkleft run"]));
        assert!(!accepts(combined, &["npm ci && npm run test"]));
        assert!(accepts(combined, &["checkleft run", "npm ci && npm run test"]));
        // A Bazel job is covered by Bazel only, never by a project command.
        let bazel = "Trunk merge queue: ci/bazel-build-test";
        assert!(!accepts(bazel, &["npm test"]));
        assert!(accepts(bazel, &["bazel test //tools/boss/..."]));
        // Narrowing flags are rejected for npm-style runners as for Bazel.
        let frontend = "Trunk merge queue: ci/frontend-tests";
        for filtered in [
            "npm test -- --grep login",
            "npm test -- --testPathPattern=auth",
            "npm run test -- -t login",
        ] {
            assert!(!accepts(frontend, &[filtered]), "{filtered}");
        }
        // Undeclared non-lint jobs always need tests, even when named build.
        assert!(accepts(frontend, &["npm ci && npm run test:unit"]));
        let build_only = "Trunk merge queue: ci/frontend-build";
        assert!(!accepts(build_only, &["npm ci && npm run build"]));
        assert!(!accepts(build_only, &["bazel build //tools/boss/..."]));
        let mac = "buildkite/mono/mac-app-build";
        assert!(!accepts(mac, &["bazel build //tools/boss/app-macos/..."]));
        assert!(!accepts(mac, &["npm run build"]));
        assert!(accepts(mac, &["bazel test //tools/boss/app-macos/..."]));
        assert!(!accepts(build_only, &["npm ci"]));
        // Project tests and builds require separate invocations.
        assert!(!accepts(build_only, &["npm run test"]));
        assert!(!accepts(frontend, &["npm run build"]));
    }

    #[test]
    fn trunk_empty_capture_give_ups_need_evidence_or_a_no_build_reason() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "Trunk eviction");
        let execution = crate::test_support::create_ready_chore_execution(&db, revision.id.clone());
        let attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.id,
                work_item_id: revision.id.clone(),
                pr_url: "https://github.com/spinyfin/mono/pull/3146".into(),
                pr_number: 3146,
                head_branch: "feature".into(),
                head_sha_at_trigger: "trunk:entry_1@2026-07-27T23:07:13.000Z".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "trunk_queue_eviction".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        let give_up = |evidence: Option<&QueueCiEvidence>, reason: &str| {
            db.validate_queue_ci_failure(&attempt.id, evidence, reason)
        };
        // mark-failed / classify path.
        assert!(give_up(None, "flaky").is_err());
        assert!(give_up(None, "").is_err());
        assert!(give_up(None, "no-failing-build-found-ish").is_err());
        for reason in NO_BUILD_REASONS {
            assert!(give_up(None, reason).is_ok(), "{reason}");
        }
        assert!(give_up(None, "no-failing-build-found: checked pr-3146 builds").is_ok());
        // Supplied evidence is fully validated, whatever the reason says.
        let good = evidence();
        assert!(give_up(Some(&good), "flaky").is_ok());
        for bad in ["sha", "exit", "command", "empty"] {
            let mut invalid = good.clone();
            match bad {
                "sha" => invalid.queue_sha = "not-a-sha".into(),
                "exit" => invalid.runs[0].exit_code = 1,
                "command" => invalid.runs[0].command = "make test".into(),
                _ => invalid.runs.clear(),
            }
            assert!(give_up(Some(&invalid), "no-failing-build-found").is_err(), "{bad}");
        }
        // run_done: blocked is a give-up; delivered with bad evidence is refused.
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
            .state
        };
        let mut payload = RunDoneProposalPayload {
            outcome: RunDoneOutcome::Blocked,
            summary: String::new(),
            question: None,
            queue_ci_evidence: None,
        };
        assert_eq!(submit("blocked-empty", &payload), ProposalState::Rejected);
        payload.summary = "flaky infra".into();
        assert_eq!(submit("blocked-prose", &payload), ProposalState::Rejected);
        let mut invalid = good.clone();
        invalid.runs[0].exit_code = 1;
        payload.queue_ci_evidence = Some(invalid);
        assert_eq!(submit("blocked-bad-evidence", &payload), ProposalState::Rejected);
        payload.queue_ci_evidence = None;
        payload.summary = "merge-conflict: rebased and resolved by the queue".into();
        assert_eq!(submit("blocked-no-build", &payload), ProposalState::Applied);
    }
}
