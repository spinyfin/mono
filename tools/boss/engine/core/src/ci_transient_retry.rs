//! Engine-direct retry of a PR-branch Buildkite job that failed on positive
//! infrastructure evidence (`boss_ci_log_reader::transient`).
//!
//! Runs inside `ci_watch::on_ci_failure_detected_with`, after every in-flight
//! guard (active rebase or conflict attempt, active CI remediation, in-flight
//! ci-fix revision) and before the budgeted fix-revision path, whenever the
//! probe reports failing required checks. It is deliberately narrow:
//!
//! - every failing check must be a Buildkite job on the PR's own branch, and
//!   every one must classify `Transient`; a single deterministic or
//!   indeterminate failure sends the whole head to the existing fix path
//!   untouched;
//! - only the failed job is retried (`bk job retry <uuid>`), never the
//!   build, never anything on `main` or in a merge queue;
//! - at most [`TRANSIENT_RETRY_BUDGET_PER_HEAD`] retries per
//!   `(work item, head SHA)`, counted from the durable
//!   `pr_blocked_remediations` ledger; past that the head falls through to
//!   the fix path and the card says the budget is spent;
//! - a job whose Buildkite retry lineage still has a running or passed
//!   descendant is never retried again, and a descendant that failed is
//!   classified like any other failure (and consumes the next budgeted
//!   retry);
//! - the step is idempotent per `(work item, head)`: the poller calls it
//!   once a minute while CI stays red, so an exhausted head, a head already
//!   judged non-transient, and a head owned by a fix path are skipped
//!   without another Buildkite fetch or ledger row.
//!
//! Relationship to the pre-triage `retrigger` attempt in `ci_watch`: that
//! path re-runs a whole build for checks whose GitHub conclusion is
//! `STARTUP_FAILURE` or `CANCELLED`, with no log evidence and no budget of
//! its own. This path retries one job because its log or Buildkite record
//! proves an infrastructure fault. `ci_watch` leaves any failure set that
//! pre-triage would retrigger to that path, so one failure never takes both.
//!
//! Every decision — retried, refused by Buildkite, or declined for budget —
//! is a ledger row the blocked-PR sweep renders on the card. The row is
//! written as `pending` before `bk job retry` runs, so a crash between the
//! two steps still counts against the budget.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use boss_ci_log_reader::transient::{
    JobEvidence, LOG_TAIL_LINES, TRANSIENT_RETRY_BUDGET_PER_HEAD, Verdict, classify_job, clean_buildkite_log,
    retry_allowed, tail_lines_of,
};
use boss_ci_log_reader::{
    BuildkiteBuild, BuildkiteJob, BuildkiteLogReader, CiLogReader, fetch_buildkite_build, parse_buildkite_build_id,
    parse_buildkite_pipeline_slug, retry_buildkite_job,
};

use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::{CiProvider, PrLifecycleProbe, RequiredCheckFailure};
use crate::work::{
    BlockedPrRemediationInput, PendingMergeCheck, REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
    REMEDIATION_OUTCOME_BUDGET_EXHAUSTED, REMEDIATION_OUTCOME_PENDING, REMEDIATION_OUTCOME_RETRIED,
    REMEDIATION_OUTCOME_RETRY_FAILED, REMEDIATION_OUTCOME_RETRY_UNCONFIRMED, WorkDb, transient_retry_signal,
};

/// Upper bound on each `bk` invocation. The retry runs inside the merge
/// poller's serial pass, so a hung CLI must not stall every other PR.
const BK_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Buildkite access the retry needs. Production uses the `bk` CLI through
/// [`BkCli`]; tests substitute canned builds and logs.
#[async_trait]
pub trait BuildkiteRetrier: Send + Sync {
    async fn fetch_build(&self, build_url: &str) -> Result<BuildkiteBuild>;
    async fn read_job_log(&self, build_url: &str, job_id: &str) -> Result<String>;
    async fn retry_job(&self, job_id: &str) -> Result<String>;
}

/// The retrier the merge poller uses. Production is the real `bk` CLI;
/// under unit tests it is [`NoTransientRetry`] so poller fixtures never
/// spawn `bk` or retry a real job (the same seam shape as
/// `ci_watch::ci_log_reader_hooks`). Tests that exercise the retry pass
/// their own [`BuildkiteRetrier`] to `on_ci_failure_detected_with`.
pub(crate) fn poller_retrier() -> Box<dyn BuildkiteRetrier> {
    #[cfg(test)]
    {
        Box::new(NoTransientRetry)
    }
    #[cfg(not(test))]
    {
        Box::new(BkCli::default())
    }
}

/// The real thing: `bk api`, `bk job log`, `bk job retry`.
#[derive(Debug, Clone)]
pub struct BkCli {
    binary: String,
}

impl Default for BkCli {
    fn default() -> Self {
        Self {
            binary: "bk".to_owned(),
        }
    }
}

/// A `bk` call that did not answer in time. For a mutating call the request
/// may already have reached Buildkite, so callers must not read it as a
/// refusal.
#[derive(Debug)]
struct BkTimedOut {
    what: String,
}

impl std::fmt::Display for BkTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bk {} timed out after {}s", self.what, BK_CALL_TIMEOUT.as_secs())
    }
}

impl std::error::Error for BkTimedOut {}

async fn bounded<T>(what: &str, call: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(BK_CALL_TIMEOUT, call).await {
        Ok(result) => result,
        Err(_) => Err(BkTimedOut { what: what.to_owned() }.into()),
    }
}

#[async_trait]
impl BuildkiteRetrier for BkCli {
    async fn fetch_build(&self, build_url: &str) -> Result<BuildkiteBuild> {
        bounded("api", fetch_buildkite_build(&self.binary, build_url)).await
    }

    async fn read_job_log(&self, build_url: &str, job_id: &str) -> Result<String> {
        let slug = parse_buildkite_pipeline_slug(build_url).ok_or_else(|| anyhow::anyhow!("no pipeline slug"))?;
        let number = parse_buildkite_build_id(build_url).ok_or_else(|| anyhow::anyhow!("no build number"))?;
        bounded(
            "job log",
            BuildkiteLogReader::with_binary(&self.binary, slug, number).read_log_full(job_id),
        )
        .await
    }

    async fn retry_job(&self, job_id: &str) -> Result<String> {
        bounded("job retry", retry_buildkite_job(&self.binary, job_id)).await
    }
}

/// Retry disabled — used by the plain `on_ci_failure_detected` entry point
/// and by poller fixtures under test, so every existing caller and test
/// keeps its exact behaviour.
pub struct NoTransientRetry;

#[async_trait]
impl BuildkiteRetrier for NoTransientRetry {
    async fn fetch_build(&self, _: &str) -> Result<BuildkiteBuild> {
        anyhow::bail!("transient retry disabled")
    }
    async fn read_job_log(&self, _: &str, _: &str) -> Result<String> {
        anyhow::bail!("transient retry disabled")
    }
    async fn retry_job(&self, _: &str) -> Result<String> {
        anyhow::bail!("transient retry disabled")
    }
}

/// What the retry step decided for this probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientOutcome {
    /// Every failure was transient and a retry was requested (or is already
    /// running); the caller must not flip the card or mint a fix.
    Handled { retried_jobs: Vec<String> },
    /// Not every failure was positively transient, or the retry budget for
    /// this head is spent: the caller proceeds with the ordinary fix path.
    NotApplicable { reason: String },
    /// The failure is positively transient but the retry ledger could not be
    /// read, so the budget is unknown. The caller stops this pass: no flip,
    /// no fix revision. The next poller pass looks again.
    Deferred { reason: String },
}

/// Heads already judged not-retryable, keyed by work item, head and failing
/// job set. The poller re-runs the step once a minute while CI stays red;
/// a non-transient verdict cannot change for the same head and jobs, so
/// remembering it avoids re-fetching the build and the full job log each
/// pass. In-process only: after an engine restart the verdict is derived
/// once more.
fn judged_non_transient() -> &'static Mutex<HashSet<String>> {
    static JUDGED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    JUDGED.get_or_init(|| Mutex::new(HashSet::new()))
}

const JUDGED_CAP: usize = 4_096;

fn judged_key(work_item_id: &str, head_sha: &str, failures: &[RequiredCheckFailure]) -> String {
    let mut parts: Vec<String> = failures
        .iter()
        .map(|f| format!("{}#{}", f.name, f.provider_job_id.as_deref().unwrap_or(&f.target_url)))
        .collect();
    parts.sort();
    format!("{work_item_id}|{head_sha}|{}", parts.join(","))
}

fn is_judged(key: &str) -> bool {
    judged_non_transient().lock().is_ok_and(|set| set.contains(key))
}

fn remember_judged(key: String) {
    if let Ok(mut set) = judged_non_transient().lock() {
        if set.len() >= JUDGED_CAP {
            set.clear();
        }
        set.insert(key);
    }
}

/// Where a job in a retry lineage stands.
enum JobPhase {
    /// Not finished: a retry is queued or running.
    InFlight,
    Passed,
    /// Ended without passing; eligible for classification.
    Failed,
}

fn job_phase(state: &str) -> JobPhase {
    match state {
        "passed" => JobPhase::Passed,
        // Only states that will still change on their own. `broken`,
        // `skipped`, `expired`, `waiting_failed` (behind a failed
        // dependency) and `blocked` / `blocked_failed` (waiting on a manual
        // unblock) never reach `passed` by themselves, so they are not in
        // flight: the fix path must be able to run.
        "scheduled" | "assigned" | "accepted" | "running" | "limited" | "waiting" | "creating" | "pending"
        | "canceling" | "timing_out" => JobPhase::InFlight,
        _ => JobPhase::Failed,
    }
}

/// Classify every failing check and retry the jobs when all are transient.
pub async fn try_transient_retry(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    retrier: &dyn BuildkiteRetrier,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    failures: &[RequiredCheckFailure],
) -> TransientOutcome {
    let Some(head_sha) = probe.head_ref_oid.as_deref() else {
        return TransientOutcome::NotApplicable {
            reason: "probe has no head sha".to_owned(),
        };
    };
    let Some(head_branch) = probe.head_ref_name.as_deref() else {
        return TransientOutcome::NotApplicable {
            reason: "probe has no head branch".to_owned(),
        };
    };
    if failures.is_empty() || failures.iter().any(|f| f.provider != CiProvider::Buildkite) {
        return TransientOutcome::NotApplicable {
            reason: "not every failing check is a Buildkite job".to_owned(),
        };
    }

    // Idempotency: the poller calls this once a minute while CI is red.
    let judged = judged_key(&candidate.work_item_id, head_sha, failures);
    if is_judged(&judged) {
        return TransientOutcome::NotApplicable {
            reason: "this head and job set was already judged not transient".to_owned(),
        };
    }
    match work_db.count_pr_blocked_remediations(
        &candidate.work_item_id,
        REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
        Some(head_sha),
        None,
        &[REMEDIATION_OUTCOME_BUDGET_EXHAUSTED],
    ) {
        Ok(0) => {}
        Ok(_) => {
            return TransientOutcome::NotApplicable {
                reason: "transient retry budget already exhausted for this head".to_owned(),
            };
        }
        Err(err) => {
            return deferred(candidate, format!("could not read the retry ledger: {err:#}"));
        }
    }

    // One build fetch per distinct build URL.
    let mut builds: HashMap<String, BuildkiteBuild> = HashMap::new();
    let mut to_retry: Vec<(String, &RequiredCheckFailure, Verdict)> = Vec::new();
    let mut in_flight = Vec::new();
    for failure in failures {
        let build_url = failure.target_url.split('#').next().unwrap_or("").to_owned();
        if !builds.contains_key(&build_url) {
            match retrier.fetch_build(&build_url).await {
                Ok(build) => {
                    builds.insert(build_url.clone(), build);
                }
                Err(err) => {
                    return not_applicable(
                        candidate,
                        format!("could not read Buildkite build {build_url}: {err:#}"),
                    );
                }
            }
        }
        let build = &builds[&build_url];
        if build.branch != head_branch {
            remember_judged(judged);
            return not_applicable(
                candidate,
                format!(
                    "build {} is for branch {}, not the PR branch {head_branch}",
                    build.number, build.branch
                ),
            );
        }
        let Some(found) = find_job(build, failure) else {
            return not_applicable(
                candidate,
                format!("no job in build {} matches {}", build.number, failure.name),
            );
        };
        let job = lineage_tip(build, found);
        match job_phase(&job.outcome.state) {
            JobPhase::InFlight | JobPhase::Passed => {
                // The retry is running or already cleared; the next probe
                // reflects it. Nothing to flip, nothing to mint.
                in_flight.push(job.id.clone());
                continue;
            }
            JobPhase::Failed if job.retry.superseded => {
                return not_applicable(
                    candidate,
                    format!("job {} was retried but its retry is not in the build", job.id),
                );
            }
            JobPhase::Failed => {}
        }
        let log_tail = match retrier.read_job_log(&build_url, &job.id).await {
            Ok(raw) => Some(tail_lines_of(&clean_buildkite_log(&raw), LOG_TAIL_LINES)),
            Err(err) => {
                tracing::debug!(job_id = %job.id, error = %format!("{err:#}"), "ci_transient_retry: no job log");
                None
            }
        };
        let verdict = classify_job(&JobEvidence {
            state: job.outcome.state.clone(),
            exit_status: job.outcome.exit_status,
            signal_reason: job.outcome.signal_reason.clone(),
            log_tail,
        });
        match verdict {
            Verdict::Transient { .. } => to_retry.push((job.id.clone(), failure, verdict)),
            other => {
                remember_judged(judged);
                return not_applicable(
                    candidate,
                    format!("{} is {}: {}", failure.name, other.kind(), other.summary()),
                );
            }
        }
    }

    if to_retry.is_empty() {
        return TransientOutcome::Handled {
            retried_jobs: in_flight,
        };
    }

    let used = match work_db.count_pr_blocked_remediations(
        &candidate.work_item_id,
        REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
        Some(head_sha),
        None,
        &[
            REMEDIATION_OUTCOME_PENDING,
            REMEDIATION_OUTCOME_RETRIED,
            REMEDIATION_OUTCOME_RETRY_UNCONFIRMED,
            REMEDIATION_OUTCOME_RETRY_FAILED,
        ],
    ) {
        Ok(used) => used,
        // An unreadable ledger says nothing about the budget. Record nothing
        // (a `budget_exhausted` row would pin the head permanently) and let
        // the next pass count again.
        Err(err) => {
            return deferred(candidate, format!("could not count the retry ledger: {err:#}"));
        }
    };
    let budget = TRANSIENT_RETRY_BUDGET_PER_HEAD;
    if !retry_allowed(used, to_retry.len() as u32, budget) {
        let (_, failure, verdict) = &to_retry[0];
        record(
            work_db,
            candidate,
            head_sha,
            &failure.name,
            verdict,
            RetryRecord {
                job_id: None,
                action: "declined",
                outcome: REMEDIATION_OUTCOME_BUDGET_EXHAUSTED,
                attempt_token: format!("{used}/{budget}"),
                response: Some("declined: budget exhausted"),
            },
        );
        return not_applicable(
            candidate,
            format!("transient retry budget exhausted for this head ({used}/{budget})"),
        );
    }

    let mut retried_jobs = in_flight;
    let mut attempt = used;
    for (job_id, failure, verdict) in &to_retry {
        attempt += 1;
        let attempt_token = format!("{attempt}/{budget}");
        // Reserve the budget slot before acting: if the engine dies after
        // `bk job retry` the attempt is still counted, and if the ledger
        // cannot be written the retry is not made.
        let action = format!("bk job retry {job_id}");
        let Some(row_id) = record(
            work_db,
            candidate,
            head_sha,
            &failure.name,
            verdict,
            RetryRecord {
                job_id: Some(job_id),
                action: &action,
                outcome: REMEDIATION_OUTCOME_PENDING,
                attempt_token: attempt_token.clone(),
                response: None,
            },
        ) else {
            return not_applicable(
                candidate,
                "could not reserve a retry in the ledger; not retrying".to_owned(),
            );
        };
        let (outcome, response) = match retrier.retry_job(job_id).await {
            Ok(out) => (REMEDIATION_OUTCOME_RETRIED, out),
            Err(err) if err.is::<BkTimedOut>() => (REMEDIATION_OUTCOME_RETRY_UNCONFIRMED, format!("{err:#}")),
            Err(err) => (REMEDIATION_OUTCOME_RETRY_FAILED, format!("{err:#}")),
        };
        let detail = detail_for(verdict, &attempt_token, Some(&response));
        if let Err(err) = work_db.update_pr_blocked_remediation(row_id, outcome, &detail) {
            tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "ci_transient_retry: failed to finish ledger row");
        }
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            head_sha,
            check = %failure.name,
            job_id = %job_id,
            signal = verdict.summary(),
            evidence = verdict.evidence(),
            outcome,
            attempt,
            budget,
            "ci_transient_retry: transient CI failure handled",
        );
        if outcome == REMEDIATION_OUTCOME_RETRIED || outcome == REMEDIATION_OUTCOME_RETRY_UNCONFIRMED {
            retried_jobs.push(job_id.clone());
        }
    }
    publisher
        .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "ci_transient_retry")
        .await;
    TransientOutcome::Handled { retried_jobs }
}

fn deferred(candidate: &PendingMergeCheck, reason: String) -> TransientOutcome {
    tracing::warn!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        %reason,
        "ci_transient_retry: ledger unreadable; skipping this pass without a fix revision",
    );
    TransientOutcome::Deferred { reason }
}

fn not_applicable(candidate: &PendingMergeCheck, reason: String) -> TransientOutcome {
    tracing::debug!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        %reason,
        "ci_transient_retry: not applicable; continuing to the fix path",
    );
    TransientOutcome::NotApplicable { reason }
}

/// The failing check's job: by the `#<job-uuid>` fragment GitHub carries,
/// else by step key / display name against the check name's last segment,
/// considering only jobs no retry has superseded.
fn find_job<'a>(build: &'a BuildkiteBuild, failure: &RequiredCheckFailure) -> Option<&'a BuildkiteJob> {
    if let Some(id) = failure
        .provider_job_id
        .as_deref()
        .or_else(|| failure.target_url.split_once('#').map(|(_, frag)| frag))
        .filter(|id| !id.is_empty())
        && let Some(job) = build.job(id)
    {
        return Some(job);
    }
    let step = failure.name.rsplit('/').next().unwrap_or(&failure.name);
    let matches_step = |j: &&BuildkiteJob| {
        !j.retry.superseded
            && (j.step_key.as_deref() == Some(step) || j.name.as_deref().is_some_and(|n| n.ends_with(step)))
    };
    build
        .jobs
        .iter()
        .filter(matches_step)
        .find(|j| j.outcome.state == "failed")
        .or_else(|| {
            build
                .jobs
                .iter()
                .filter(matches_step)
                .find(|j| matches!(job_phase(&j.outcome.state), JobPhase::Failed))
        })
        .or_else(|| build.jobs.iter().find(matches_step))
}

/// Follow `retry.source_job_id` links from `job` to the newest job in its
/// retry chain.
fn lineage_tip<'a>(build: &'a BuildkiteBuild, job: &'a BuildkiteJob) -> &'a BuildkiteJob {
    let mut current = job;
    for _ in 0..build.jobs.len() {
        match build
            .jobs
            .iter()
            .find(|j| j.retry.source_job_id.as_deref() == Some(current.id.as_str()))
        {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// What one retry decision wrote to the ledger.
struct RetryRecord<'a> {
    job_id: Option<&'a str>,
    action: &'a str,
    outcome: &'a str,
    /// `<attempt>/<budget>`.
    attempt_token: String,
    /// Buildkite's response, or why the retry was declined.
    response: Option<&'a str>,
}

/// `detail` is "<attempt>/<budget> <machine signal>: <proving log line>",
/// optionally followed by Buildkite's response in brackets. The sweep parses
/// the leading "<attempt>/<budget>" token; the rest is human-readable context.
fn detail_for(verdict: &Verdict, attempt_token: &str, response: Option<&str>) -> String {
    let mut detail = match verdict {
        Verdict::Transient { signal, evidence, .. } => format!("{attempt_token} {}: {evidence}", signal.as_str()),
        other => format!("{attempt_token} {}: {}", other.kind(), other.evidence()),
    };
    if let Some(response) = response.map(str::trim).filter(|r| !r.is_empty()) {
        detail.push_str(&format!(" [{response}]"));
    }
    detail
}

/// Append a ledger row; returns its id, or `None` (logged) if the write failed.
fn record(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    head_sha: &str,
    check_name: &str,
    verdict: &Verdict,
    rec: RetryRecord<'_>,
) -> Option<i64> {
    let RetryRecord {
        job_id,
        action,
        outcome,
        attempt_token,
        response,
    } = rec;
    // `signal` is the card-facing "<summary> in <check>".
    let signal = transient_retry_signal(verdict.summary(), check_name);
    let input = BlockedPrRemediationInput::builder()
        .work_item_id(candidate.work_item_id.clone())
        .head_sha(head_sha)
        .category(REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY)
        .signal(signal)
        .maybe_target(job_id)
        .action(action)
        .outcome(outcome)
        .detail(detail_for(verdict, &attempt_token, response))
        .build();
    match work_db.record_pr_blocked_remediation(input) {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "ci_transient_retry: failed to record ledger row");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! The transient-CI retry step inside `on_ci_failure_detected_with`
    //! (`crate::ci_transient_retry`): a positively transient Buildkite failure
    //! is retried in place and never becomes a fix revision; a deterministic
    //! one is never retried; the per-head budget is durable.

    use std::sync::{Arc, Mutex};

    use boss_ci_log_reader::{BuildkiteJob, JobOutcome};

    use super::*;
    use crate::ci_watch::on_ci_failure_detected_with;
    use crate::merge_poller::{OpenPrCiStatus, OpenPrStatus, PrLifecycleState, PrReviewState};
    use crate::test_support::*;
    use crate::work::{
        BlockedPrRemediation, FakePrStateChecker, PrOpenState, REMEDIATION_OUTCOME_BUDGET_EXHAUSTED, TaskStatus,
        WorkItem, WorkItemPatch,
    };

    fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
        let chore = create_test_chore_manual(db, product.id.clone(), name);
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, chore.id)
    }

    fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: work_item_id.to_owned(),
            product_id: product_id.to_owned(),
            pr_url: pr_url.to_owned(),
        }
    }

    fn chore_state(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => (t.status, t.blocked_reason),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    fn fix_checker() -> FakePrStateChecker {
        FakePrStateChecker::always(PrOpenState::Open)
    }

    const BUILD_URL: &str = "https://buildkite.com/flunge/mono/builds/21619";
    const JOB: &str = "01a15a8c-97ac-4218-a4c3-e2d4edcfa502";
    const BRANCH: &str = "boss/exec_feature";

    /// The incident log (mono build 21619): checkleft refusing to report clean
    /// because GitHub answered 504 for the PR description.
    const PR_DESCRIPTION_504_LOG: &str = "\
--- [checks] running checks
checkleft: PR description surface: UNAVAILABLE — GitHub API PR description lookup returned HTTP 504 Gateway Timeout
error[boss-ism/pr-text-leakage]: PR description could not be resolved; refusing to report clean. GitHub API PR description lookup returned HTTP 504 Gateway Timeout
  --> <pr description>
summary: 1 error(s), 0 warning(s), 0 info finding(s)
🚨 Error: The command exited with status 1
";

    /// A real test failure, followed by the artifact-upload tail every
    /// `bazel-build-test` job prints after it.
    const BAZEL_TEST_FAILURE_LOG: &str = "\
FAIL: //tools/boss/engine/core:engine_lib_test (shard 2 of 11) (Exit 101) (see test.log)
INFO: Build completed, 1 test FAILED, 2012 total actions
2026-10-11 15:28:09 INFO   Uploading bazel-testlogs/x/test.log (374 B)
2026-10-11 15:28:10 INFO   Artifact uploads completed successfully
🚨 Error: The command exited with status 1
";

    struct FakeBuildkite {
        build: BuildkiteBuild,
        logs: HashMap<String, String>,
        retried: Mutex<Vec<String>>,
        refuse_retry: bool,
        /// `retry_job` times out, as `bounded` reports it.
        timeout_retry: bool,
        fetches: Mutex<u32>,
    }

    impl FakeBuildkite {
        fn new(branch: &str, job: BuildkiteJob, log: &str) -> Self {
            let mut logs = HashMap::new();
            logs.insert(job.id.clone(), log.to_owned());
            Self {
                build: BuildkiteBuild {
                    number: 21619,
                    branch: branch.to_owned(),
                    state: "failed".to_owned(),
                    web_url: BUILD_URL.to_owned(),
                    jobs: vec![job],
                },
                logs,
                retried: Mutex::new(Vec::new()),
                refuse_retry: false,
                timeout_retry: false,
                fetches: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl BuildkiteRetrier for FakeBuildkite {
        async fn fetch_build(&self, _: &str) -> Result<BuildkiteBuild> {
            *self.fetches.lock().unwrap() += 1;
            Ok(self.build.clone())
        }
        async fn read_job_log(&self, _: &str, job_id: &str) -> Result<String> {
            self.logs
                .get(job_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no log for {job_id}"))
        }
        async fn retry_job(&self, job_id: &str) -> Result<String> {
            if self.refuse_retry {
                anyhow::bail!("bk: 422 job is not retryable");
            }
            if self.timeout_retry {
                return Err(BkTimedOut {
                    what: "job retry".to_owned(),
                }
                .into());
            }
            self.retried.lock().unwrap().push(job_id.to_owned());
            Ok(format!("Retried job {job_id} -> {BUILD_URL}#new"))
        }
    }

    fn failed_job(exit: i64) -> BuildkiteJob {
        BuildkiteJob {
            id: JOB.to_owned(),
            name: Some(":white_check_mark: checks".to_owned()),
            step_key: Some("checks".to_owned()),
            outcome: JobOutcome {
                state: "failed".to_owned(),
                exit_status: Some(exit),
                signal_reason: None,
            },
            ..Default::default()
        }
    }

    fn checks_failure() -> Vec<RequiredCheckFailure> {
        vec![RequiredCheckFailure {
            job_key: None,
            name: "buildkite/mono/checks".into(),
            conclusion: "FAILURE".into(),
            target_url: format!("{BUILD_URL}#{JOB}"),
            provider: CiProvider::Buildkite,
            provider_job_id: Some(JOB.into()),
        }]
    }

    fn pr_probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
        PrLifecycleProbe::builder()
            .url(pr_url.to_owned())
            .state(PrLifecycleState::Open(OpenPrStatus {
                ci: OpenPrCiStatus::Failing {
                    failures: checks_failure(),
                },
                ..OpenPrStatus::clean()
            }))
            .base_ref_oid("base-1")
            .head_ref_oid(head_sha.to_owned())
            .head_ref_name(BRANCH)
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build()
    }

    fn ledger(db: &WorkDb, chore: &str, head: &str) -> Vec<BlockedPrRemediation> {
        db.list_pr_blocked_remediations(chore, REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY, Some(head), None)
            .unwrap()
    }

    #[tokio::test]
    async fn pr_description_504_is_retried_in_place_and_mints_no_fix() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/504";
        let (product, chore) = make_in_review(&db, "C-504", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG);

        let handled = on_ci_failure_detected_with(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-504"),
            &checks_failure(),
            &bk,
        )
        .await;
        assert!(handled);
        assert_eq!(
            *bk.retried.lock().unwrap(),
            vec![JOB.to_owned()],
            "exactly the failed job is retried"
        );
        assert_eq!(
            chore_state(&db, &chore),
            (TaskStatus::InReview, None),
            "card is not flipped to blocked"
        );
        assert!(
            db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
            "no fix attempt, no budget spent"
        );
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

        let rows = ledger(&db, &chore, "head-504");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, REMEDIATION_OUTCOME_RETRIED);
        assert_eq!(rows[0].signal, "HTTP 504 in buildkite/mono/checks");
        assert_eq!(rows[0].target.as_deref(), Some(JOB));
        let detail = rows[0].detail.as_deref().unwrap();
        assert!(detail.starts_with("1/2 upstream_unavailable: "), "{detail}");
        assert!(
            pub_.lifecycle_reasons()
                .await
                .contains(&"ci_transient_retry".to_owned())
        );
    }

    #[tokio::test]
    async fn deterministic_test_failure_is_never_retried_and_takes_the_fix_path() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/505";
        let (product, chore) = make_in_review(&db, "C-det", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), BAZEL_TEST_FAILURE_LOG);

        on_ci_failure_detected_with(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-505"),
            &checks_failure(),
            &bk,
        )
        .await;
        assert!(
            bk.retried.lock().unwrap().is_empty(),
            "a real failure must not be retried"
        );
        assert!(
            ledger(&db, &chore, "head-505").is_empty(),
            "nothing transient to record"
        );
        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("the ordinary fix path ran");
        assert_eq!(attempt.attempt_kind, "fix");
    }

    #[tokio::test]
    async fn retry_budget_is_per_head_and_exhaustion_is_recorded_once_then_falls_through() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/506";
        let (product, chore) = make_in_review(&db, "C-budget", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG);
        for _ in 0..TRANSIENT_RETRY_BUDGET_PER_HEAD {
            db.record_pr_blocked_remediation(
                BlockedPrRemediationInput::builder()
                    .work_item_id(chore.clone())
                    .head_sha("head-506")
                    .category(REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY)
                    .signal("HTTP 504 in buildkite/mono/checks")
                    .target(JOB)
                    .action("bk job retry")
                    .outcome(REMEDIATION_OUTCOME_RETRIED)
                    .detail("1/2")
                    .build(),
            )
            .unwrap();
        }

        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-506"),
            &checks_failure(),
        )
        .await;
        assert!(matches!(outcome, TransientOutcome::NotApplicable { .. }), "{outcome:?}");
        assert!(bk.retried.lock().unwrap().is_empty());
        let rows = ledger(&db, &chore, "head-506");
        assert_eq!(rows.len() as u32, TRANSIENT_RETRY_BUDGET_PER_HEAD + 1);
        assert_eq!(rows[0].outcome, REMEDIATION_OUTCOME_BUDGET_EXHAUSTED);
        assert!(rows[0].detail.as_deref().unwrap().starts_with("2/2"));

        // A new head starts a fresh budget.
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-507"),
            &checks_failure(),
        )
        .await;
        assert!(matches!(outcome, TransientOutcome::Handled { .. }), "{outcome:?}");
        assert_eq!(bk.retried.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn already_retried_job_is_handled_without_another_retry() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/508";
        let (product, chore) = make_in_review(&db, "C-dup", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = retried_lineage("running", PR_DESCRIPTION_504_LOG);

        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-508"),
            &checks_failure(),
        )
        .await;
        assert_eq!(
            outcome,
            TransientOutcome::Handled {
                retried_jobs: vec![RETRY_JOB.to_owned()]
            }
        );
        assert!(bk.retried.lock().unwrap().is_empty(), "Buildkite already shows a retry");
        assert!(ledger(&db, &chore, "head-508").is_empty());
    }

    #[tokio::test]
    async fn a_superseded_job_whose_retry_is_missing_falls_to_the_fix_path() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/513";
        let (product, chore) = make_in_review(&db, "C-orphan-retry", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let mut job = failed_job(1);
        job.retry.superseded = true;
        let bk = FakeBuildkite::new(BRANCH, job, PR_DESCRIPTION_504_LOG);
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-513"),
            &checks_failure(),
        )
        .await;
        assert!(matches!(outcome, TransientOutcome::NotApplicable { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_build_for_another_branch_or_a_refused_retry_is_not_silent() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/509";
        let (product, chore) = make_in_review(&db, "C-branch", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Main-branch build behind the check: never acted on.
        let bk = FakeBuildkite::new("main", failed_job(1), PR_DESCRIPTION_504_LOG);
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-509"),
            &checks_failure(),
        )
        .await;
        assert!(matches!(outcome, TransientOutcome::NotApplicable { ref reason } if reason.contains("main")));
        assert!(bk.retried.lock().unwrap().is_empty());

        // Buildkite refuses the retry: recorded as retry_failed, still handled
        // (the card will say the retry could not be requested).
        let mut bk = FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG);
        bk.refuse_retry = true;
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-509b"),
            &checks_failure(),
        )
        .await;
        assert_eq!(
            outcome,
            TransientOutcome::Handled {
                retried_jobs: Vec::new()
            }
        );
        let rows = ledger(&db, &chore, "head-509b");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, REMEDIATION_OUTCOME_RETRY_FAILED);
        assert!(rows[0].detail.as_deref().unwrap().contains("not retryable"));
    }

    const RETRY_JOB: &str = "02b26b9d-a8bd-4329-b5d4-f3e5fedb0613";

    /// `FakeBuildkite` for a build whose original job (`JOB`) was retried
    /// into `RETRY_JOB` in `state`.
    fn retried_lineage(state: &str, log: &str) -> FakeBuildkite {
        let mut original = failed_job(1);
        original.retry.superseded = true;
        let mut retry = failed_job(1);
        retry.id = RETRY_JOB.to_owned();
        retry.outcome.state = state.to_owned();
        retry.retry.source_job_id = Some(JOB.to_owned());
        let mut bk = FakeBuildkite::new(BRANCH, original, log);
        bk.logs.insert(RETRY_JOB.to_owned(), log.to_owned());
        bk.build.jobs.push(retry);
        bk
    }

    fn failures_without_fragment() -> Vec<RequiredCheckFailure> {
        vec![RequiredCheckFailure {
            job_key: None,
            name: "buildkite/mono/checks".into(),
            conclusion: "FAILURE".into(),
            target_url: BUILD_URL.to_owned(),
            provider: CiProvider::Buildkite,
            provider_job_id: None,
        }]
    }

    #[tokio::test]
    async fn retry_lineage_is_followed_to_the_newest_job_with_and_without_a_fragment() {
        let pr = "https://github.com/foo/bar/pull/510";
        for (label, failures) in [
            ("fragment", checks_failure()),
            ("step-key", failures_without_fragment()),
        ] {
            // A running or passed retry: handled, nothing re-retried.
            for state in ["running", "scheduled", "passed"] {
                let (_dir, db) = open_db();
                let (product, chore) = make_in_review(&db, &format!("C-{label}-{state}"), pr);
                let pub_ = Arc::new(RecordingPublisher::default());
                let bk = retried_lineage(state, PR_DESCRIPTION_504_LOG);
                let outcome = try_transient_retry(
                    &db,
                    pub_.as_ref(),
                    &bk,
                    &candidate(&product, &chore, pr),
                    &pr_probe(pr, "head-510"),
                    &failures,
                )
                .await;
                assert_eq!(
                    outcome,
                    TransientOutcome::Handled {
                        retried_jobs: vec![RETRY_JOB.to_owned()]
                    },
                    "{label} {state}"
                );
                assert!(bk.retried.lock().unwrap().is_empty(), "{label} {state}");
            }

            // A retry that failed again is classified: transient -> the
            // second budgeted retry of the retry, deterministic -> fix path.
            let (_dir, db) = open_db();
            let (product, chore) = make_in_review(&db, &format!("C-{label}-failed"), pr);
            let pub_ = Arc::new(RecordingPublisher::default());
            let bk = retried_lineage("failed", PR_DESCRIPTION_504_LOG);
            let outcome = try_transient_retry(
                &db,
                pub_.as_ref(),
                &bk,
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-510"),
                &failures,
            )
            .await;
            assert!(
                matches!(outcome, TransientOutcome::Handled { .. }),
                "{label}: {outcome:?}"
            );
            assert_eq!(*bk.retried.lock().unwrap(), vec![RETRY_JOB.to_owned()], "{label}");

            let (_dir, db) = open_db();
            let (product, chore) = make_in_review(&db, &format!("C-{label}-det"), pr);
            let bk = retried_lineage("failed", BAZEL_TEST_FAILURE_LOG);
            let outcome = try_transient_retry(
                &db,
                pub_.as_ref(),
                &bk,
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-510"),
                &failures,
            )
            .await;
            assert!(
                matches!(outcome, TransientOutcome::NotApplicable { .. }),
                "{label}: {outcome:?}"
            );
            assert!(bk.retried.lock().unwrap().is_empty(), "{label}");
        }
    }

    #[tokio::test]
    async fn repeated_passes_over_one_head_fetch_and_record_once() {
        let pr = "https://github.com/foo/bar/pull/511";
        let pub_ = Arc::new(RecordingPublisher::default());

        // Deterministic head: judged once, then skipped without a fetch.
        let (_dir, db) = open_db();
        let (product, chore) = make_in_review(&db, "C-passes-det", pr);
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), BAZEL_TEST_FAILURE_LOG);
        for _ in 0..4 {
            let outcome = try_transient_retry(
                &db,
                pub_.as_ref(),
                &bk,
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-511-det"),
                &checks_failure(),
            )
            .await;
            assert!(matches!(outcome, TransientOutcome::NotApplicable { .. }));
        }
        assert_eq!(*bk.fetches.lock().unwrap(), 1, "one Buildkite fetch for the head");

        // Exhausted head: one budget_exhausted row, one fetch, over many passes.
        let (_dir, db) = open_db();
        let (product, chore) = make_in_review(&db, "C-passes-exh", pr);
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG);
        for _ in 0..TRANSIENT_RETRY_BUDGET_PER_HEAD {
            db.record_pr_blocked_remediation(
                BlockedPrRemediationInput::builder()
                    .work_item_id(chore.clone())
                    .head_sha("head-511-exh")
                    .category(REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY)
                    .signal("HTTP 504 in buildkite/mono/checks")
                    .target(JOB)
                    .action("bk job retry")
                    .outcome(REMEDIATION_OUTCOME_RETRIED)
                    .detail("1/2")
                    .build(),
            )
            .unwrap();
        }
        for _ in 0..4 {
            try_transient_retry(
                &db,
                pub_.as_ref(),
                &bk,
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-511-exh"),
                &checks_failure(),
            )
            .await;
        }
        assert_eq!(*bk.fetches.lock().unwrap(), 1);
        let exhausted = ledger(&db, &chore, "head-511-exh")
            .into_iter()
            .filter(|r| r.outcome == REMEDIATION_OUTCOME_BUDGET_EXHAUSTED)
            .count();
        assert_eq!(exhausted, 1, "the exhaustion is recorded once, not once a minute");
    }

    #[tokio::test]
    async fn an_active_fix_path_means_no_buildkite_fetch_at_all() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/512";
        let (product, chore) = make_in_review(&db, "C-guard", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = FakeBuildkite::new(BRANCH, failed_job(1), BAZEL_TEST_FAILURE_LOG);
        // First pass mints the ci-fix revision (deterministic failure).
        on_ci_failure_detected_with(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-512"),
            &checks_failure(),
            &bk,
        )
        .await;
        let fetched = *bk.fetches.lock().unwrap();
        assert_eq!(fetched, 1, "the first pass classifies the failed job");
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_some());
        // While that revision runs, later passes (even with a different,
        // never-judged failure set) must not touch Buildkite.
        let other = vec![RequiredCheckFailure {
            name: "buildkite/mono/other".into(),
            ..checks_failure().remove(0)
        }];
        for _ in 0..3 {
            on_ci_failure_detected_with(
                &db,
                pub_.as_ref(),
                &fix_checker(),
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-512"),
                &other,
                &bk,
            )
            .await;
        }
        assert_eq!(
            *bk.fetches.lock().unwrap(),
            fetched,
            "no fetch while a fix owns the head"
        );
    }
    #[tokio::test]
    async fn a_timed_out_retry_is_unconfirmed_not_failed_and_still_spends_budget() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/520";
        let (product, chore) = make_in_review(&db, "C-timeout", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let mut bk = FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG);
        bk.timeout_retry = true;
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-520"),
            &checks_failure(),
        )
        .await;
        assert!(matches!(outcome, TransientOutcome::Handled { .. }), "{outcome:?}");
        let rows = ledger(&db, &chore, "head-520");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, REMEDIATION_OUTCOME_RETRY_UNCONFIRMED);
        assert!(rows[0].detail.as_deref().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_broken_retry_tip_is_not_in_flight_and_the_fix_path_runs() {
        for state in ["broken", "skipped", "waiting_failed", "blocked", "expired"] {
            let (_dir, db) = open_db();
            let pr = "https://github.com/foo/bar/pull/521";
            let (product, chore) = make_in_review(&db, &format!("C-{state}"), pr);
            let pub_ = Arc::new(RecordingPublisher::default());
            let mut bk = retried_lineage(state, PR_DESCRIPTION_504_LOG);
            // The dead tip never ran, so it has no log and no exit status.
            bk.logs.remove(RETRY_JOB);
            bk.build.jobs[1].outcome.exit_status = None;
            let outcome = try_transient_retry(
                &db,
                pub_.as_ref(),
                &bk,
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-521"),
                &checks_failure(),
            )
            .await;
            assert!(
                matches!(outcome, TransientOutcome::NotApplicable { .. }),
                "{state}: {outcome:?}"
            );
            assert!(bk.retried.lock().unwrap().is_empty(), "{state}");

            let handled = on_ci_failure_detected_with(
                &db,
                pub_.as_ref(),
                &fix_checker(),
                &candidate(&product, &chore, pr),
                &pr_probe(pr, "head-521"),
                &checks_failure(),
                &bk,
            )
            .await;
            assert!(handled);
            assert!(
                db.active_ci_remediation_for_work_item(&chore).unwrap().is_some(),
                "{state}: the ordinary fix path ran"
            );
        }
    }

    /// Hides the remediation ledger the moment a job log is read, so the
    /// budget count that follows fails as it would on `SQLITE_BUSY`.
    struct HidesLedger<'a> {
        inner: FakeBuildkite,
        db: &'a WorkDb,
    }

    #[async_trait]
    impl BuildkiteRetrier for HidesLedger<'_> {
        async fn fetch_build(&self, url: &str) -> Result<BuildkiteBuild> {
            self.inner.fetch_build(url).await
        }
        async fn read_job_log(&self, url: &str, job_id: &str) -> Result<String> {
            let log = self.inner.read_job_log(url, job_id).await?;
            self.db
                .connect()?
                .execute_batch("ALTER TABLE pr_blocked_remediations RENAME TO pr_blocked_remediations_hidden")?;
            Ok(log)
        }
        async fn retry_job(&self, job_id: &str) -> Result<String> {
            self.inner.retry_job(job_id).await
        }
    }

    #[tokio::test]
    async fn an_unreadable_ledger_skips_the_pass_without_recording_exhaustion() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/522";
        let (product, chore) = make_in_review(&db, "C-ledger", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = HidesLedger {
            inner: FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG),
            db: &db,
        };
        let outcome = try_transient_retry(
            &db,
            pub_.as_ref(),
            &bk,
            &candidate(&product, &chore, pr),
            &pr_probe(pr, "head-522"),
            &checks_failure(),
        )
        .await;
        assert!(
            matches!(outcome, TransientOutcome::Deferred { ref reason } if reason.contains("could not count")),
            "{outcome:?}"
        );
        assert!(bk.inner.retried.lock().unwrap().is_empty());
        db.connect()
            .unwrap()
            .execute_batch("ALTER TABLE pr_blocked_remediations_hidden RENAME TO pr_blocked_remediations")
            .unwrap();
        assert!(
            ledger(&db, &chore, "head-522").is_empty(),
            "no budget_exhausted row may be written from a failed count"
        );
    }

    #[tokio::test]
    async fn a_ledger_read_error_stops_the_pass_with_no_fix_and_a_later_pass_retries() {
        let (_dir, db) = open_db();
        let pr = "https://github.com/foo/bar/pull/523";
        let (product, chore) = make_in_review(&db, "C-defer", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let bk = HidesLedger {
            inner: FakeBuildkite::new(BRANCH, failed_job(1), PR_DESCRIPTION_504_LOG),
            db: &db,
        };
        let checker = fix_checker();
        let cand = candidate(&product, &chore, pr);
        let probe = pr_probe(pr, "head-523");
        let failures = checks_failure();
        macro_rules! detect {
            ($retrier:expr) => {
                on_ci_failure_detected_with(&db, pub_.as_ref(), &checker, &cand, &probe, &failures, $retrier)
            };
        }

        assert!(!detect!(&bk).await, "a deferred pass transitions nothing");
        assert!(bk.inner.retried.lock().unwrap().is_empty());
        assert_eq!(chore_state(&db, &chore), (TaskStatus::InReview, None));
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

        db.connect()
            .unwrap()
            .execute_batch("ALTER TABLE pr_blocked_remediations_hidden RENAME TO pr_blocked_remediations")
            .unwrap();
        assert!(detect!(&bk.inner).await, "the next pass retries normally");
        assert_eq!(*bk.inner.retried.lock().unwrap(), vec![JOB.to_owned()]);
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    }
}
