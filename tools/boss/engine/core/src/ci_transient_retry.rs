//! Engine-direct retry of a PR-branch Buildkite job that failed on positive
//! infrastructure evidence (`boss_ci_log_reader::transient`).
//!
//! Runs inside `ci_watch::on_ci_failure_detected`, before the budgeted
//! fix-revision path, whenever the probe reports failing required checks.
//! It is deliberately narrow:
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
//! - a job Buildkite already shows as retried is never retried again.
//!
//! Every decision — retried, refused by Buildkite, or declined for budget —
//! is a ledger row the blocked-PR sweep renders on the card.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use boss_ci_log_reader::transient::{
    JobEvidence, LOG_TAIL_LINES, TRANSIENT_RETRY_BUDGET_PER_HEAD, Verdict, classify_job, clean_buildkite_log,
    retry_allowed, tail_lines_of,
};
use boss_ci_log_reader::{
    BuildkiteBuild, BuildkiteLogReader, CiLogReader, fetch_buildkite_build, parse_buildkite_build_id,
    parse_buildkite_pipeline_slug, retry_buildkite_job,
};

use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::{CiProvider, PrLifecycleProbe, RequiredCheckFailure};
use crate::work::{
    BlockedPrRemediationInput, PendingMergeCheck, REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
    REMEDIATION_OUTCOME_BUDGET_EXHAUSTED, REMEDIATION_OUTCOME_RETRIED, REMEDIATION_OUTCOME_RETRY_FAILED, WorkDb,
    transient_retry_signal,
};

/// Buildkite access the retry needs. Production uses the `bk` CLI through
/// [`BkCli`]; tests substitute canned builds and logs.
#[async_trait]
pub trait BuildkiteRetrier: Send + Sync {
    async fn fetch_build(&self, build_url: &str) -> Result<BuildkiteBuild>;
    async fn read_job_log(&self, build_url: &str, job_id: &str) -> Result<String>;
    async fn retry_job(&self, job_id: &str) -> Result<String>;
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

#[async_trait]
impl BuildkiteRetrier for BkCli {
    async fn fetch_build(&self, build_url: &str) -> Result<BuildkiteBuild> {
        fetch_buildkite_build(&self.binary, build_url).await
    }

    async fn read_job_log(&self, build_url: &str, job_id: &str) -> Result<String> {
        let slug = parse_buildkite_pipeline_slug(build_url).ok_or_else(|| anyhow::anyhow!("no pipeline slug"))?;
        let number = parse_buildkite_build_id(build_url).ok_or_else(|| anyhow::anyhow!("no build number"))?;
        BuildkiteLogReader::with_binary(&self.binary, slug, number)
            .read_log_full(job_id)
            .await
    }

    async fn retry_job(&self, job_id: &str) -> Result<String> {
        retry_buildkite_job(&self.binary, job_id).await
    }
}

/// Retry disabled — used by the plain `on_ci_failure_detected` entry point
/// so every existing caller and test keeps its exact behaviour.
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

    // One build fetch per distinct build URL.
    let mut builds: HashMap<String, BuildkiteBuild> = HashMap::new();
    let mut to_retry: Vec<(String, &RequiredCheckFailure, Verdict)> = Vec::new();
    let mut already_retried = Vec::new();
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
            return not_applicable(
                candidate,
                format!(
                    "build {} is for branch {}, not the PR branch {head_branch}",
                    build.number, build.branch
                ),
            );
        }
        let Some(job) = find_job(build, failure) else {
            return not_applicable(
                candidate,
                format!("no job in build {} matches {}", build.number, failure.name),
            );
        };
        if job.retry.superseded
            || build
                .jobs
                .iter()
                .any(|j| j.retry.source_job_id.as_deref() == Some(&job.id))
        {
            already_retried.push(job.id.clone());
            continue;
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
                return not_applicable(
                    candidate,
                    format!("{} is {}: {}", failure.name, other.kind(), other.summary()),
                );
            }
        }
    }

    if to_retry.is_empty() {
        // Every failing job already has a retry in flight; the next probe
        // will see it as in-progress. Nothing to flip, nothing to mint.
        return TransientOutcome::Handled {
            retried_jobs: already_retried,
        };
    }

    let used = work_db
        .count_pr_blocked_remediations(
            &candidate.work_item_id,
            REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
            Some(head_sha),
            None,
            &[REMEDIATION_OUTCOME_RETRIED, REMEDIATION_OUTCOME_RETRY_FAILED],
        )
        .unwrap_or(TRANSIENT_RETRY_BUDGET_PER_HEAD);
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

    let mut retried_jobs = already_retried;
    let mut attempt = used;
    for (job_id, failure, verdict) in &to_retry {
        attempt += 1;
        let (outcome, response) = match retrier.retry_job(job_id).await {
            Ok(out) => (REMEDIATION_OUTCOME_RETRIED, out),
            Err(err) => (REMEDIATION_OUTCOME_RETRY_FAILED, format!("{err:#}")),
        };
        record(
            work_db,
            candidate,
            head_sha,
            &failure.name,
            verdict,
            RetryRecord {
                job_id: Some(job_id),
                action: &format!("bk job retry {job_id}"),
                outcome,
                attempt_token: format!("{attempt}/{budget}"),
                response: Some(&response),
            },
        );
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
        if outcome == REMEDIATION_OUTCOME_RETRIED {
            retried_jobs.push(job_id.clone());
        }
    }
    publisher
        .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "ci_transient_retry")
        .await;
    TransientOutcome::Handled { retried_jobs }
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
/// else by step key / display name against the check name's last segment.
fn find_job<'a>(
    build: &'a BuildkiteBuild,
    failure: &RequiredCheckFailure,
) -> Option<&'a boss_ci_log_reader::BuildkiteJob> {
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
    build.jobs.iter().find(|j| {
        j.outcome.state == "failed"
            && (j.step_key.as_deref() == Some(step) || j.name.as_deref().is_some_and(|n| n.ends_with(step)))
    })
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

fn record(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    head_sha: &str,
    check_name: &str,
    verdict: &Verdict,
    rec: RetryRecord<'_>,
) {
    let RetryRecord {
        job_id,
        action,
        outcome,
        attempt_token,
        response,
    } = rec;
    // `signal` is the card-facing "<summary> in <check>"; `detail` is
    // "<attempt>/<budget> <machine signal>: <proving log line>", optionally
    // followed by Buildkite's response in brackets. The sweep parses the
    // leading "<attempt>/<budget>" token; operators read the rest.
    let signal = transient_retry_signal(verdict.summary(), check_name);
    let mut detail = match verdict {
        Verdict::Transient { signal, evidence, .. } => format!("{attempt_token} {}: {evidence}", signal.as_str()),
        other => format!("{attempt_token} {}: {}", other.kind(), other.evidence()),
    };
    if let Some(response) = response.map(str::trim).filter(|r| !r.is_empty()) {
        detail.push_str(&format!(" [{response}]"));
    }
    let input = BlockedPrRemediationInput::builder()
        .work_item_id(candidate.work_item_id.clone())
        .head_sha(head_sha)
        .category(REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY)
        .signal(signal)
        .maybe_target(job_id)
        .action(action)
        .outcome(outcome)
        .detail(detail)
        .build();
    if let Err(err) = work_db.record_pr_blocked_remediation(input) {
        tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "ci_transient_retry: failed to record ledger row");
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
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState, PrReviewState};
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
            }
        }
    }

    #[async_trait]
    impl BuildkiteRetrier for FakeBuildkite {
        async fn fetch_build(&self, _: &str) -> Result<BuildkiteBuild> {
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
            .state(PrLifecycleState::Open(OpenPrStatus::clean()))
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
        let mut job = failed_job(1);
        job.retry.superseded = true;
        let bk = FakeBuildkite::new(BRANCH, job, PR_DESCRIPTION_504_LOG);

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
                retried_jobs: vec![JOB.to_owned()]
            }
        );
        assert!(bk.retried.lock().unwrap().is_empty(), "Buildkite already shows a retry");
        assert!(ledger(&db, &chore, "head-508").is_empty());
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
            &pr_probe(pr, "head-509"),
            &checks_failure(),
        )
        .await;
        assert_eq!(
            outcome,
            TransientOutcome::Handled {
                retried_jobs: Vec::new()
            }
        );
        let rows = ledger(&db, &chore, "head-509");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, REMEDIATION_OUTCOME_RETRY_FAILED);
        assert!(rows[0].detail.as_deref().unwrap().contains("not retryable"));
    }
}
