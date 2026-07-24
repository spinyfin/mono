//! Periodic reconciler for terminated executions that pushed a branch but
//! never got a PR opened for it.
//!
//! ## The incident this closes (2026-07-24)
//!
//! GitHub returned HTTP 500 for every PR-create request — both `gh pr
//! create` (GraphQL) and the REST `POST .../pulls` path — for roughly 90
//! minutes. Five workers finished their work, pushed verified branches, and
//! were unable to open PRs. Every row fell back to `pr_url: null`,
//! indistinguishable from work that never started, while a complete branch
//! sat on GitHub. Recovery was entirely manual: enumerate `boss/*` refs on
//! GitHub, diff against existing PR head refs, create the PRs by hand, and
//! backfill `pr_url` via the `boss task update --pr-url` escape hatch.
//! Nothing in the engine detected the state; it surfaced only because the
//! stalled pane happened to be seen by chance.
//!
//! [`crate::completion`]'s on-Stop path and the merge poller's
//! `list_recently_terminal_executions_pending_pr_detection` late-PR sweep
//! (the "Bug B" double-spawn recovery) both only look at a *live* Stop
//! event, or a terminal execution whose task is still `active` within a
//! 60-minute lookback. Neither catches a worker that died without ever
//! firing Stop (no clean termination — the dead-PID / lost-workspace
//! sweeps mark the execution terminal from *outside* the worker) after
//! which whatever demoted the task's kanban status (`heal_ghost_active_chores`,
//! a churn guard, a plain fallback) moved it off `active`. That combination
//! — terminal execution, real commits sitting on a remote branch, no PR,
//! task reading as indistinguishable from unstarted work — is exactly what
//! happened to all five rows in the incident above.
//!
//! ## Design
//!
//! Detect the state directly off the execution row rather than depending on
//! worker cooperation or a `tasks.status` value that may itself be wrong
//! (see [`crate::work::WorkDb::list_abandoned_pushed_branch_candidates`]):
//! terminal, workspace-backed (a pane actually spawned), no `pr_url`
//! anywhere, finished more than [`TERMINATION_GRACE_SECS`] ago. The
//! discriminator between "genuinely abandoned" and "worker hasn't reached
//! PR creation yet" is keyed on *when the run ended*, not branch age —
//! branch age would be meaningless here since the branch name is unique per
//! execution and is only ever pushed once, by that one worker.
//!
//! For each candidate:
//!
//! 1. Re-run the same branch-keyed PR detector the on-Stop / merge-poller
//!    paths use ([`crate::completion::CommandPrDetector`]). If a PR already
//!    exists (a human or another process created it, or the worker's own
//!    `gh pr create` actually landed but the engine never heard about it),
//!    bind it — this alone recovers the "PR exists but is invisible" half
//!    of the incident, independent of whatever `tasks.status` fell to.
//! 2. If no PR exists, attempt to create one directly via the GitHub REST
//!    API (`POST /repos/{slug}/pulls`). PR creation here is engine-side
//!    rather than worker-side, because during a PR-create outage the
//!    worker cannot open one either. Idempotent by
//!    construction: the branch-keyed detector in step 1 runs immediately
//!    before every create attempt, so a PR that appears between sweep
//!    passes (a human, a recovering worker, or a concurrent pass) is bound
//!    instead of duplicated; GitHub's own 422 "already exists" response on
//!    the create call itself is a second, belt-and-suspenders check of the
//!    same race.
//! 3. A branch that was never pushed, or was pushed with no commits ahead
//!    of the repo's default branch, has nothing to open a PR for — that is
//!    not this bug (the worker genuinely never got that far) and is skipped
//!    quietly. GitHub's own validation on the create call (404 / 422 "No
//!    commits between…") is the source of truth for this, not a separate
//!    compare call.
//!
//! ## Retry, backoff, and "GitHub is down" (design decision)
//!
//! This sweep runs on a deliberately slow, fixed interval
//! ([`DEFAULT_INTERVAL`]) rather than tracking a per-execution exponential
//! backoff window: this is a rare failure path, not a steady-state one, and
//! [`MAX_LOOKBACK_SECS`] plus [`CommandPrAutoCreator`]'s branch-existence
//! precheck and cached default-branch lookup keep the steady-state
//! candidate set small and each candidate's per-pass cost low, so a GitHub
//! outage costs a bounded number of API round-trips per interval, not a
//! retry storm. A failed create attempt (transient — network/5xx/429 — or
//! permanent — auth/permissions/an unexpected validation error) never marks
//! the execution or task row failed or gives up: both are left completely
//! untouched, and the sweep simply tries again next pass. The only durable
//! consequence of a failure surfaces after
//! [`ESCALATE_AFTER_CONSECUTIVE_FAILURES`] consecutive failed attempts for
//! the same execution (tracked in-memory — reset on engine restart,
//! matching [`crate::transient_recovery`]'s nudge-tracking precedent) an
//! attention item is filed naming the branch, the work item, and the last
//! error, so the row is surfaced in the UI instead of silently reading as
//! unstarted. It resolves automatically the moment a later pass succeeds.
//!
//! ## Duplicate-aware by construction
//!
//! During the outage one worker independently pushed a second branch
//! (`…_v2`) at the same commit as a workaround. This sweep never discovers
//! or acts on that branch: detection is scoped to exactly
//! the engine-supplied canonical branch name
//! ([`crate::completion::expected_branch_name`]) derived from the
//! execution id, never a scan of arbitrary remote branches. A duplicate
//! branch a worker invents on its own is simply never examined, so it can
//! never cause a second PR to be opened for the same work item.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use boss_github::gh_runner::{CommandGhRunner, GhRunner, GhRunnerError};
use boss_protocol::WorkItem;

use crate::completion::{CommandPrDetector, PrDetector, PrStatus, expected_branch_name, parse_repo_slug};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::sweep_loop::{SweepOutcome, spawn_sweep_loop};
use crate::work::{LatePrCandidate, WorkDb};

/// How often the sweep runs. Deliberately slower than most reconcilers
/// (dead-pid / orphan sweep run every 60s) — see the module doc's "Retry,
/// backoff" section for why a slow fixed interval is the chosen backoff
/// mechanism rather than per-execution exponential backoff.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Minimum time after an execution's `finished_at` before its branch is
/// treated as abandoned rather than "worker hasn't reached PR creation yet."
/// Generous relative to the on-Stop / merge-poller paths' own windows
/// (which run within seconds to minutes of Stop) — this sweep is the
/// backstop of last resort, not the primary detector.
pub const TERMINATION_GRACE_SECS: i64 = 15 * 60;

/// Upper bound on how far back the candidate query looks. Also bounds how
/// long a branch that was never pushed (`AutoCreateOutcome::NothingToCreate`
/// — a quiet skip that writes no state, so nothing else ages it out) keeps
/// getting re-probed every pass: at [`DEFAULT_INTERVAL`] this is roughly 288
/// passes rather than the ~2000 a 7-day window would cost. A row that is
/// still genuinely unresolved this long after its execution finished
/// already has an open attention item from an earlier pass (the
/// [`ESCALATE_AFTER_CONSECUTIVE_FAILURES`] path), so this is a query-cost
/// and probe-volume bound, not a give-up point — see
/// [`crate::work::WorkDb::list_abandoned_pushed_branch_candidates`]'s doc
/// comment.
pub const MAX_LOOKBACK_SECS: i64 = 24 * 60 * 60;

/// Consecutive failed auto-create attempts (transient or permanent) for the
/// same execution before the sweep files an attention item.
pub const ESCALATE_AFTER_CONSECUTIVE_FAILURES: u32 = 3;

/// `work_attention_items.kind` filed once a candidate's auto-create attempts
/// have failed [`ESCALATE_AFTER_CONSECUTIVE_FAILURES`] times in a row.
pub const ATTENTION_KIND_ABANDONED_BRANCH_NO_PR: &str = "abandoned_branch_no_pr";

/// Outcome of one auto-create attempt for a single candidate.
#[derive(Debug)]
pub(crate) enum AutoCreateOutcome {
    /// A PR already existed (bound via the pre-create recheck or GitHub's
    /// own 422 "already exists" on the create call).
    AlreadyExists(String),
    /// The engine successfully opened a PR.
    Created(String),
    /// The branch has nothing to open a PR for (never pushed, or no commits
    /// ahead of the default branch) — not this sweep's bug.
    NothingToCreate,
    /// The attempt failed. `transient` is `true` for network / 5xx / 429
    /// failures GitHub itself is responsible for; `false` for anything that
    /// likely needs a human (auth, permissions, an unexpected response).
    Failed { transient: bool, message: String },
}

/// Attempts to open a PR for an abandoned branch. A trait so tests can stub
/// out the GitHub calls.
#[async_trait]
pub(crate) trait PrAutoCreator: Send + Sync {
    async fn try_create(&self, repo_slug: &str, branch: &str, title: &str, body: &str) -> AutoCreateOutcome;
}

/// Production [`PrAutoCreator`], backed by the `gh` CLI via ambient `gh
/// auth` — the same convention every other GitHub call in the engine
/// follows (see [`crate::completion::CommandPrDetector`]).
///
/// `default_branch_cache` caches `repos/{slug}` lookups by repo slug across
/// every candidate and every pass this creator serves — the default branch
/// essentially never changes, so there is no reason to re-fetch it once per
/// candidate per 5-minute pass (see the module-level "request volume" note
/// this cache and [`Self::branch_ref_exists`] together address).
struct CommandPrAutoCreator {
    gh: Arc<dyn GhRunner>,
    default_branch_cache: AsyncMutex<HashMap<String, String>>,
}

impl CommandPrAutoCreator {
    fn new() -> Self {
        Self {
            gh: Arc::new(CommandGhRunner),
            default_branch_cache: AsyncMutex::new(HashMap::new()),
        }
    }

    /// `GET /repos/{slug}/pulls?head=<owner>:<branch>&state=all` — the same
    /// existing-PR recheck used both as the immediate pre-create race guard
    /// and to resolve GitHub's 422 "already exists" response into a URL.
    async fn existing_pr_url(&self, repo_slug: &str, branch: &str) -> Option<String> {
        let owner = repo_slug.split('/').next()?;
        let path = format!("repos/{repo_slug}/pulls?head={owner}:{branch}&state=all");
        let resp = self.gh.rest_get(&path, None).await.ok()?;
        resp.body
            .as_array()?
            .first()?
            .get("html_url")?
            .as_str()
            .map(str::to_owned)
    }

    /// `GET /repos/{slug}/git/ref/heads/{branch}` — a single cheap call that
    /// tells us whether the branch was ever pushed at all, before paying for
    /// the full existing-PR-recheck + default-branch + create cascade. A
    /// clean 404 means the branch definitely does not exist on the remote
    /// (never pushed): the common, permanent case for a candidate whose
    /// worker died before pushing anything, and the one this precheck exists
    /// to make cheap. Any other error is inconclusive (could be transient),
    /// so the caller falls through to the full path rather than guessing.
    async fn branch_ref_exists(&self, repo_slug: &str, branch: &str) -> Result<bool, GhRunnerError> {
        match self
            .gh
            .rest_get(&format!("repos/{repo_slug}/git/ref/heads/{branch}"), None)
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if err.http_status == Some(404) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn default_branch(&self, repo_slug: &str) -> Result<String, GhRunnerError> {
        if let Some(cached) = self.default_branch_cache.lock().await.get(repo_slug) {
            return Ok(cached.clone());
        }
        let resp = self.gh.rest_get(&format!("repos/{repo_slug}"), None).await?;
        let branch = resp
            .body
            .get("default_branch")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                GhRunnerError::transient(format!("repos/{repo_slug} response had no default_branch field"))
            })?;
        self.default_branch_cache
            .lock()
            .await
            .insert(repo_slug.to_owned(), branch.clone());
        Ok(branch)
    }
}

#[async_trait]
impl PrAutoCreator for CommandPrAutoCreator {
    async fn try_create(&self, repo_slug: &str, branch: &str, title: &str, body: &str) -> AutoCreateOutcome {
        // Cheap precheck: a branch that was never pushed has nothing to
        // open a PR for, and costs one API call to rule out instead of the
        // four below. An inconclusive result (network blip, unexpected
        // status) falls through to the full path rather than being treated
        // as a false "doesn't exist".
        if let Ok(false) = self.branch_ref_exists(repo_slug, branch).await {
            return AutoCreateOutcome::NothingToCreate;
        }

        // Immediate pre-create recheck (race safety): a PR opened by a
        // human, a recovering worker, or a concurrent sweep pass since the
        // caller's own `PrDetector` check a moment earlier.
        if let Some(url) = self.existing_pr_url(repo_slug, branch).await {
            return AutoCreateOutcome::AlreadyExists(url);
        }

        let default_branch = match self.default_branch(repo_slug).await {
            Ok(b) => b,
            Err(err) => return classify_gh_error(err),
        };
        if default_branch == branch {
            // Paranoia guard — should be structurally impossible given
            // engine-supplied branch names, but never open a PR from a
            // branch onto itself.
            return AutoCreateOutcome::NothingToCreate;
        }

        let payload = serde_json::json!({
            "title": title,
            "head": branch,
            "base": default_branch,
            "body": body,
        });
        match self
            .gh
            .rest_post(&format!("repos/{repo_slug}/pulls"), &payload, None)
            .await
        {
            Ok(resp) => match resp.body.get("html_url").and_then(|v| v.as_str()) {
                Some(url) => AutoCreateOutcome::Created(url.to_owned()),
                None => AutoCreateOutcome::Failed {
                    transient: false,
                    message: "gh pulls-create response had no html_url".to_owned(),
                },
            },
            Err(err) => {
                // 422 covers several distinct cases GitHub does not
                // otherwise disambiguate by status code alone: "A pull
                // request already exists for owner:branch" (the exact race
                // the pre-create recheck above is meant to catch, closed
                // one API call later), "No commits between base and head"
                // (nothing to create — quiet skip), and other validation
                // failures (a ruleset blocking the base, an invalid head,
                // PR creation disabled) that need a human, not a silent
                // skip. Tell them apart by message text; anything that
                // isn't one of the first two known-benign cases is a real
                // failure.
                if err.http_status == Some(422) {
                    if err.message.to_lowercase().contains("already exists") {
                        return match self.existing_pr_url(repo_slug, branch).await {
                            Some(url) => AutoCreateOutcome::AlreadyExists(url),
                            None => AutoCreateOutcome::Failed {
                                transient: true,
                                message: err.message,
                            },
                        };
                    }
                    if err.message.to_lowercase().contains("no commits between") {
                        return AutoCreateOutcome::NothingToCreate;
                    }
                    return AutoCreateOutcome::Failed {
                        transient: false,
                        message: err.message,
                    };
                }
                // A 404 on the create call itself (as opposed to the
                // branch-existence precheck above) means something GitHub
                // considers missing at create time — most often a
                // permissions/auth problem the token can't see past (GitHub
                // returns 404, not 403, for a repo the token cannot access).
                // That needs a human, not a quiet skip.
                if err.http_status == Some(404) {
                    return AutoCreateOutcome::Failed {
                        transient: false,
                        message: err.message,
                    };
                }
                classify_gh_error(err)
            }
        }
    }
}

/// Classify a `gh` failure that isn't one of the specific-meaning codes
/// (`404`, `422`) already handled at the call site: no status / `0`
/// (spawn or connection failure), `429`, and `5xx` are GitHub's own
/// responsibility and worth retrying; everything else (`401`, `403`, an
/// unrecognised `4xx`) likely needs a human.
fn classify_gh_error(err: GhRunnerError) -> AutoCreateOutcome {
    let transient = match err.http_status {
        None | Some(0) => true,
        Some(429) => true,
        Some(status) => (500..600).contains(&status),
    };
    AutoCreateOutcome::Failed {
        transient,
        message: err.message,
    }
}

/// Counts from one pass of the sweep; logged at `info` when non-zero.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AbandonedBranchPrSweepOutcome {
    /// A PR already existed and was bound to the work item.
    pub recovered: usize,
    /// The engine opened a new PR and bound it to the work item.
    pub created: usize,
    /// The branch had nothing to open a PR for (never pushed, or no commits
    /// ahead of the default branch).
    pub skipped_no_commits: usize,
    /// An auto-create attempt failed this pass (transient or permanent).
    pub failed: usize,
    /// An attention item was filed this pass (consecutive-failure cap hit).
    pub escalated: usize,
}

impl SweepOutcome for AbandonedBranchPrSweepOutcome {
    fn has_activity(&self) -> bool {
        self.recovered > 0 || self.created > 0 || self.failed > 0 || self.escalated > 0
    }

    fn log(&self) {
        tracing::info!(
            recovered = self.recovered,
            created = self.created,
            skipped_no_commits = self.skipped_no_commits,
            failed = self.failed,
            escalated = self.escalated,
            "abandoned-branch-pr sweep: pass complete",
        );
    }
}

/// Which of the two idempotent-bind outcomes a `try_create`/detector result
/// produced — replaces a stringly-typed `action: &'static str` flag so the
/// two cases can't be confused with an unrelated string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindAction {
    /// A PR already existed and was bound to the work item.
    Bound,
    /// The engine opened a new PR and bound it to the work item.
    Created,
}

impl BindAction {
    fn as_str(self) -> &'static str {
        match self {
            BindAction::Bound => "bound",
            BindAction::Created => "created",
        }
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// using the shared [`crate::sweep_loop::spawn_sweep_loop`] scaffold. The
/// per-execution failure-count map must survive across passes so
/// consecutive-failure escalation works; it lives in an `Arc<Mutex<_>>` the
/// pass closure borrows each iteration, mirroring how
/// [`crate::terminal_work_sweep::spawn_loop`] threads its own cross-pass
/// `seen_terminal` set through the same helper — a single task ever holds
/// the lock, so there is no real contention.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let pr_detector: Arc<dyn PrDetector> = Arc::new(CommandPrDetector::new());
    let pr_creator: Arc<dyn PrAutoCreator> = Arc::new(CommandPrAutoCreator::new());
    let failure_counts: Arc<AsyncMutex<HashMap<String, u32>>> = Arc::new(AsyncMutex::new(HashMap::new()));
    spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let pr_detector = Arc::clone(&pr_detector);
        let pr_creator = Arc::clone(&pr_creator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let failure_counts = Arc::clone(&failure_counts);
        async move {
            let mut failure_counts = failure_counts.lock().await;
            run_one_pass(
                work_db.as_ref(),
                pr_detector.as_ref(),
                pr_creator.as_ref(),
                dispatch_events.as_ref(),
                &mut failure_counts,
            )
            .await
        }
    })
}

/// Deduplicate candidates by `work_item_id`, keeping the one with the
/// newest `finished_at`. A work item with two terminal workspace-backed
/// executions and no PR — the double-spawn shape
/// [`crate::work::WorkDb::mark_execution_redundant`] produces, or exactly
/// what a PR-create outage leaves behind when several attempts fail — would
/// otherwise appear as two candidates in the same pass, each with a
/// distinct engine-supplied branch, and reach [`PrAutoCreator::try_create`]
/// twice: two PRs opened on GitHub for one task, with only the first bind
/// succeeding and the second orphaned. [`list_abandoned_pushed_branch_candidates`]
/// orders by `finished_at ASC, id ASC`, so a plain overwriting insert keeps
/// the newest row per work item.
///
/// [`list_abandoned_pushed_branch_candidates`]: crate::work::WorkDb::list_abandoned_pushed_branch_candidates
fn dedup_by_work_item(candidates: Vec<LatePrCandidate>) -> Vec<LatePrCandidate> {
    let mut by_work_item: HashMap<String, LatePrCandidate> = HashMap::with_capacity(candidates.len());
    for candidate in candidates {
        by_work_item.insert(candidate.work_item_id.clone(), candidate);
    }
    by_work_item.into_values().collect()
}

/// Whether the work item already carries a non-empty `pr_url` right now.
/// Used as an immediate pre-create recheck: a bind from earlier in the same
/// pass (or a concurrent path) must suppress a second `try_create` call for
/// the same work item, even after [`dedup_by_work_item`] — belt-and-suspenders
/// alongside the dedup, and the only guard for a `pr_url` set by something
/// other than this sweep between the candidate query and this point.
fn work_item_pr_url_already_set(work_db: &WorkDb, work_item_id: &str) -> bool {
    match work_db.get_work_item(work_item_id) {
        Ok(WorkItem::Task(t)) | Ok(WorkItem::Chore(t)) => t.pr_url.as_deref().is_some_and(|url| !url.is_empty()),
        _ => false,
    }
}

/// Run a single sweep pass. `failure_counts` persists across calls (owned
/// by the spawn loop) so consecutive-failure escalation survives from one
/// pass to the next; entries for execution ids not seen this pass are
/// pruned at the end so a row that resolves, ages out, or has its task
/// closed does not leak in the map for the engine's lifetime.
pub(crate) async fn run_one_pass(
    work_db: &WorkDb,
    pr_detector: &dyn PrDetector,
    pr_creator: &dyn PrAutoCreator,
    dispatch_events: &dyn DispatchEventSink,
    failure_counts: &mut HashMap<String, u32>,
) -> AbandonedBranchPrSweepOutcome {
    let mut outcome = AbandonedBranchPrSweepOutcome::default();

    let candidates = match work_db.list_abandoned_pushed_branch_candidates(TERMINATION_GRACE_SECS, MAX_LOOKBACK_SECS) {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "abandoned-branch-pr sweep: failed to list candidates; skipping pass"
            );
            return outcome;
        }
    };
    let candidates = dedup_by_work_item(candidates);
    failure_counts.retain(|execution_id, _| candidates.iter().any(|c| &c.execution_id == execution_id));

    for candidate in &candidates {
        let expected_branch = expected_branch_name(
            &candidate.execution_id,
            &candidate.branch_naming,
            candidate.worker_branch_prefix.as_deref(),
        );
        let repo_slug = match parse_repo_slug(&candidate.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id = %candidate.execution_id,
                    ?err,
                    "abandoned-branch-pr sweep: could not parse repo slug; skipping",
                );
                continue;
            }
        };

        let pr_status = match pr_detector
            .detect_pr(&candidate.repo_remote_url, &expected_branch)
            .await
        {
            Ok(status) => status,
            Err(err) => {
                tracing::debug!(
                    execution_id = %candidate.execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "abandoned-branch-pr sweep: detector failed; will retry next pass",
                );
                continue;
            }
        };

        match pr_status {
            PrStatus::Fresh { url } | PrStatus::Merged { url } => {
                bind_and_record(
                    work_db,
                    dispatch_events,
                    candidate,
                    &url,
                    BindAction::Bound,
                    &mut outcome,
                    failure_counts,
                )
                .await;
            }
            // A PR exists but isn't cleanly actionable here — the same
            // quiet skip the merge poller's late-PR sweep applies to these
            // states. Not this sweep's failure mode: there IS a PR, just
            // not a fresh/merged one.
            PrStatus::Stale { .. } | PrStatus::EmptyDiff { .. } | PrStatus::Closed { .. } => {}
            PrStatus::None => {
                // A bind from an earlier candidate in this same pass (or a
                // concurrent path) may have already set `pr_url` — recheck
                // immediately before paying for a create attempt.
                if work_item_pr_url_already_set(work_db, &candidate.work_item_id) {
                    continue;
                }
                let (title, body) = build_pr_title_body(work_db, candidate);
                match pr_creator.try_create(&repo_slug, &expected_branch, &title, &body).await {
                    AutoCreateOutcome::AlreadyExists(url) => {
                        bind_and_record(
                            work_db,
                            dispatch_events,
                            candidate,
                            &url,
                            BindAction::Bound,
                            &mut outcome,
                            failure_counts,
                        )
                        .await;
                    }
                    AutoCreateOutcome::Created(url) => {
                        bind_and_record(
                            work_db,
                            dispatch_events,
                            candidate,
                            &url,
                            BindAction::Created,
                            &mut outcome,
                            failure_counts,
                        )
                        .await;
                    }
                    AutoCreateOutcome::NothingToCreate => {
                        outcome.skipped_no_commits += 1;
                        dispatch_events
                            .emit(
                                DispatchEvent::new(
                                    Stage::AbandonedBranchPrRecovery,
                                    Outcome::Skipped,
                                    &candidate.execution_id,
                                )
                                .with_work_item(&candidate.work_item_id)
                                .with_details(serde_json::json!({ "action": "nothing_to_create" })),
                            )
                            .await;
                    }
                    AutoCreateOutcome::Failed { transient, message } => {
                        outcome.failed += 1;
                        let count = {
                            let entry = failure_counts.entry(candidate.execution_id.clone()).or_insert(0);
                            *entry += 1;
                            *entry
                        };
                        tracing::warn!(
                            execution_id = %candidate.execution_id,
                            work_item_id = %candidate.work_item_id,
                            transient,
                            consecutive_failures = count,
                            error = %message,
                            "abandoned-branch-pr sweep: auto-create attempt failed",
                        );
                        dispatch_events
                            .emit(
                                DispatchEvent::new(
                                    Stage::AbandonedBranchPrRecovery,
                                    Outcome::Error,
                                    &candidate.execution_id,
                                )
                                .with_work_item(&candidate.work_item_id)
                                .with_details(serde_json::json!({
                                    "action": "create_failed",
                                    "transient": transient,
                                    "error": message,
                                    "consecutive_failures": count,
                                })),
                            )
                            .await;
                        if count >= ESCALATE_AFTER_CONSECUTIVE_FAILURES {
                            file_attention(work_db, candidate, &expected_branch, transient, &message, count);
                            outcome.escalated += 1;
                        }
                    }
                }
            }
        }
    }

    outcome
}

/// Bind a recovered/created PR to the work item, clear any consecutive
/// failure count and open attention item, and record the outcome.
async fn bind_and_record(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    candidate: &LatePrCandidate,
    pr_url: &str,
    action: BindAction,
    outcome: &mut AbandonedBranchPrSweepOutcome,
    failure_counts: &mut HashMap<String, u32>,
) {
    match work_db.bind_pr_to_task_from_terminal_execution(&candidate.work_item_id, pr_url) {
        Ok(true) => {
            failure_counts.remove(&candidate.execution_id);
            if let Err(err) = work_db
                .resolve_external_tracker_attention(&candidate.work_item_id, ATTENTION_KIND_ABANDONED_BRANCH_NO_PR)
            {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "abandoned-branch-pr sweep: failed to resolve attention item on recovery (non-fatal)",
                );
            }
            match action {
                BindAction::Created => outcome.created += 1,
                BindAction::Bound => outcome.recovered += 1,
            }
            tracing::info!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                pr_url,
                action = action.as_str(),
                "abandoned-branch-pr sweep: bound recovered PR to work item",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::AbandonedBranchPrRecovery, Outcome::Ok, &candidate.execution_id)
                        .with_work_item(&candidate.work_item_id)
                        .with_details(serde_json::json!({ "action": action.as_str(), "pr_url": pr_url })),
                )
                .await;
        }
        Ok(false) => {
            // Already resolved by a concurrent path (another pass, a human
            // bind, the task closed) — nothing to do.
        }
        Err(err) => {
            tracing::warn!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                ?err,
                "abandoned-branch-pr sweep: failed to bind recovered PR",
            );
        }
    }
}

/// File (idempotently) the attention item raised once a
/// candidate's auto-create attempts have failed
/// [`ESCALATE_AFTER_CONSECUTIVE_FAILURES`] times in a row.
fn file_attention(
    work_db: &WorkDb,
    candidate: &LatePrCandidate,
    branch: &str,
    transient: bool,
    message: &str,
    attempts: u32,
) {
    let title = "Pushed branch has no PR — engine auto-recovery is retrying".to_owned();
    let cause = if transient {
        "GitHub appears to be unreachable or erroring (transient failures)"
    } else {
        "the create attempt keeps failing for a reason that likely needs a human (permissions, \
         validation, or an unexpected response)"
    };
    let body = format!(
        "Execution `{execution_id}` finished and pushed commits to `{branch}`, but no pull \
         request exists for it. The engine has tried to open one automatically {attempts} \
         time(s) and keeps failing — {cause}.\n\n\
         **Last error:** {message}\n\n\
         The engine keeps retrying on its normal sweep interval; this attention item resolves \
         automatically once a PR is created (by the engine or manually). To recover by hand in \
         the meantime, open a PR from `{branch}` in the product's repo.",
        execution_id = candidate.execution_id,
    );
    if let Err(err) = work_db.upsert_external_tracker_attention(
        &candidate.work_item_id,
        ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
        &title,
        &body,
    ) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "abandoned-branch-pr sweep: failed to file attention item",
        );
    }
}

/// Build the title/body for an auto-created PR from the work item's own
/// name/description, falling back to the bare work-item id if the lookup
/// fails (deleted mid-sweep, DB error).
fn build_pr_title_body(work_db: &WorkDb, candidate: &LatePrCandidate) -> (String, String) {
    let (name, description) = match work_db.get_work_item(&candidate.work_item_id) {
        Ok(WorkItem::Task(t)) | Ok(WorkItem::Chore(t)) => (t.name, t.description),
        _ => (candidate.work_item_id.clone(), String::new()),
    };
    let body = format!(
        "Automated recovery: the worker implementing this finished and pushed commits to this \
         branch, but the run ended before a pull request was opened for it (execution \
         `{execution_id}`). This PR was opened automatically by the engine's abandoned-branch-PR \
         sweep.\n\n---\n\n{description}",
        execution_id = candidate.execution_id,
    );
    (name, body)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use boss_github::gh_runner::GhResponse;
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, FinishExecutionRunInput};

    use super::*;
    use crate::dispatch_events::{NoopDispatchEventSink, RecordingDispatchEventSink};
    use crate::test_support::{create_test_chore_manual, create_test_product_with_repo, open_db};
    use crate::work::WorkItemPatch;

    const GRACE_SECS: i64 = TERMINATION_GRACE_SECS;

    /// Create a chore whose single execution is terminal, workspace-backed,
    /// and finished well outside the grace window — the baseline candidate
    /// shape `run_one_pass` is meant to act on.
    fn make_candidate_chore(db: &WorkDb, label: &str) -> (String, String) {
        let product = create_test_product_with_repo(db, label, Some("git@github.com:foo/bar.git"));
        let chore = create_test_chore_manual(db, product.id.clone(), format!("Chore-{label}"));
        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:foo/bar.git")
                    .build(),
            )
            .unwrap();
        let (exec, run) = db
            .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        db.mark_execution_redundant(&exec.id).unwrap();
        {
            let conn = db.connect().unwrap();
            let backdated = (boss_engine_utils::epoch_time::now_epoch_secs() - GRACE_SECS - 60).to_string();
            conn.execute(
                "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
                rusqlite::params![exec.id, backdated],
            )
            .unwrap();
        }
        // Simulate the fallback the sweep exists to catch: the task
        // demoted off `active` back to `todo`.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("todo".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (chore.id, exec.id)
    }

    /// Add a second terminal, workspace-backed candidate execution onto an
    /// existing chore (as created by [`make_candidate_chore`]), finished
    /// `younger_by_secs` seconds more recently than the first — the
    /// double-spawn shape [`dedup_by_work_item`] must collapse into a
    /// single candidate.
    fn add_second_candidate_execution(db: &WorkDb, chore_id: &str, younger_by_secs: i64) -> String {
        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore_id.to_owned())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:foo/bar.git")
                    .build(),
            )
            .unwrap();
        let (exec, run) = db
            .start_execution_run(&exec.id, "agent-2", "repo-1", "lease-2", "ws-2", "/workspaces/ws-2")
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        db.mark_execution_redundant(&exec.id).unwrap();
        {
            let conn = db.connect().unwrap();
            let backdated =
                (boss_engine_utils::epoch_time::now_epoch_secs() - GRACE_SECS - 60 + younger_by_secs).to_string();
            conn.execute(
                "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
                rusqlite::params![exec.id, backdated],
            )
            .unwrap();
        }
        exec.id
    }

    fn task_status_and_pr_url(db: &WorkDb, work_item_id: &str) -> (String, Option<String>) {
        match db.get_work_item(work_item_id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => (t.status.as_str().to_owned(), t.pr_url),
            other => panic!("expected a Task/Chore work item, got {other:?}"),
        }
    }

    /// [`PrDetector`] stub that always returns the same canned status.
    struct FixedPrDetector(PrStatus);

    #[async_trait]
    impl PrDetector for FixedPrDetector {
        async fn detect_pr(&self, _repo_remote_url: &str, _expected_branch: &str) -> anyhow::Result<PrStatus> {
            Ok(self.0.clone())
        }
    }

    /// [`PrAutoCreator`] stub that returns a pre-queued sequence of
    /// outcomes, one per `try_create` call — lets a test drive several
    /// sweep passes (e.g. the escalation tests) with a different outcome
    /// each time.
    struct QueuePrAutoCreator {
        queue: StdMutex<Vec<AutoCreateOutcome>>,
        calls: AtomicUsize,
    }

    impl QueuePrAutoCreator {
        fn new(mut outcomes: Vec<AutoCreateOutcome>) -> Self {
            outcomes.reverse();
            Self {
                queue: StdMutex::new(outcomes),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PrAutoCreator for QueuePrAutoCreator {
        async fn try_create(&self, _repo_slug: &str, _branch: &str, _title: &str, _body: &str) -> AutoCreateOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.queue.lock().unwrap().pop().expect("no more queued outcomes")
        }
    }

    /// Fake [`GhRunner`] driven by a fixed queue of GET/POST responses, one
    /// per call in the order [`CommandPrAutoCreator::try_create`] issues
    /// them. Exercises the production creator's own response-classification
    /// logic (branch-existence precheck, 422/404 handling, default-branch
    /// caching) without shelling out to `gh`.
    struct FakeGhRunner {
        gets: StdMutex<VecDeque<std::result::Result<GhResponse, GhRunnerError>>>,
        posts: StdMutex<VecDeque<std::result::Result<GhResponse, GhRunnerError>>>,
        get_calls: AtomicUsize,
    }

    impl FakeGhRunner {
        fn new(
            gets: Vec<std::result::Result<GhResponse, GhRunnerError>>,
            posts: Vec<std::result::Result<GhResponse, GhRunnerError>>,
        ) -> Self {
            Self {
                gets: StdMutex::new(gets.into_iter().collect()),
                posts: StdMutex::new(posts.into_iter().collect()),
                get_calls: AtomicUsize::new(0),
            }
        }

        fn get_call_count(&self) -> usize {
            self.get_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl GhRunner for FakeGhRunner {
        async fn graphql(
            &self,
            _query: &str,
            _vars: &[(&str, &str)],
            _token: Option<&str>,
        ) -> std::result::Result<serde_json::Value, GhRunnerError> {
            unimplemented!("try_create never calls graphql")
        }

        async fn rest_get(&self, _path: &str, _token: Option<&str>) -> std::result::Result<GhResponse, GhRunnerError> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            self.gets
                .lock()
                .unwrap()
                .pop_front()
                .expect("no more queued GET responses")
        }

        async fn rest_patch(
            &self,
            _path: &str,
            _fields: &[(&str, &str)],
            _token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            unimplemented!("try_create never calls rest_patch")
        }

        async fn rest_post(
            &self,
            _path: &str,
            _body: &serde_json::Value,
            _token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            self.posts
                .lock()
                .unwrap()
                .pop_front()
                .expect("no more queued POST responses")
        }
    }

    fn creator_with_fake_gh(fake: Arc<FakeGhRunner>) -> CommandPrAutoCreator {
        CommandPrAutoCreator {
            gh: fake,
            default_branch_cache: AsyncMutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn no_candidates_is_a_quiet_no_op() {
        let (_dir, db) = open_db();
        let detector = FixedPrDetector(PrStatus::None);
        let creator = QueuePrAutoCreator::new(vec![]);
        let sink = NoopDispatchEventSink;
        let mut failures = HashMap::new();

        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome, AbandonedBranchPrSweepOutcome::default());
        assert!(!outcome.has_activity());
    }

    #[tokio::test]
    async fn existing_pr_is_bound_and_task_moves_to_in_review() {
        let (_dir, db) = open_db();
        let (chore_id, _exec_id) = make_candidate_chore(&db, "existing-pr");
        let detector = FixedPrDetector(PrStatus::Fresh {
            url: "https://github.com/foo/bar/pull/7".to_owned(),
        });
        let creator = QueuePrAutoCreator::new(vec![]);
        let sink = RecordingDispatchEventSink::new();
        let mut failures = HashMap::new();

        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.recovered, 1);
        assert_eq!(outcome.created, 0);

        let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
        assert_eq!(status, "in_review");
        assert_eq!(pr_url.as_deref(), Some("https://github.com/foo/bar/pull/7"));

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::AbandonedBranchPrRecovery.as_str());
        assert_eq!(events[0].outcome, Outcome::Ok.as_str());
    }

    #[tokio::test]
    async fn no_pr_triggers_auto_create_and_binds_result() {
        let (_dir, db) = open_db();
        let (chore_id, _exec_id) = make_candidate_chore(&db, "auto-create");
        let detector = FixedPrDetector(PrStatus::None);
        let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::Created(
            "https://github.com/foo/bar/pull/8".to_owned(),
        )]);
        let sink = NoopDispatchEventSink;
        let mut failures = HashMap::new();

        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.created, 1);
        assert_eq!(outcome.recovered, 0);

        let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
        assert_eq!(status, "in_review");
        assert_eq!(pr_url.as_deref(), Some("https://github.com/foo/bar/pull/8"));
    }

    #[tokio::test]
    async fn nothing_to_create_is_a_quiet_skip() {
        let (_dir, db) = open_db();
        let (chore_id, _exec_id) = make_candidate_chore(&db, "nothing-to-create");
        let detector = FixedPrDetector(PrStatus::None);
        let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::NothingToCreate]);
        let sink = NoopDispatchEventSink;
        let mut failures = HashMap::new();

        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.skipped_no_commits, 1);
        assert!(!outcome.has_activity(), "a quiet skip must not count as sweep activity");

        let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
        assert_eq!(
            status, "todo",
            "task must be left untouched when there's nothing to create a PR from"
        );
        assert_eq!(pr_url, None);
    }

    #[tokio::test]
    async fn repeated_failures_escalate_to_an_attention_item() {
        let (_dir, db) = open_db();
        let (chore_id, _exec_id) = make_candidate_chore(&db, "escalate");
        let detector = FixedPrDetector(PrStatus::None);
        let sink = NoopDispatchEventSink;
        let mut failures = HashMap::new();

        // First two failures: below the escalation threshold, no attention
        // item yet.
        for _ in 0..(ESCALATE_AFTER_CONSECUTIVE_FAILURES - 1) {
            let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::Failed {
                transient: true,
                message: "network blip".to_owned(),
            }]);
            let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
            assert_eq!(outcome.failed, 1);
            assert_eq!(outcome.escalated, 0);
        }
        let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
        assert!(
            items.is_empty(),
            "must not escalate before the consecutive-failure cap is reached"
        );

        // The Nth failure trips the cap.
        let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::Failed {
            transient: true,
            message: "network blip".to_owned(),
        }]);
        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.escalated, 1);

        let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ATTENTION_KIND_ABANDONED_BRANCH_NO_PR);
        assert_eq!(items[0].status, "open");

        // A later successful pass must clear the failure count and resolve
        // the attention item — the sweep's whole point is that recovery is
        // silent once GitHub (or a human) fixes the underlying problem.
        let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::Created(
            "https://github.com/foo/bar/pull/99".to_owned(),
        )]);
        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.created, 1);

        let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
        assert!(
            items.iter().all(|i| i.status != "open"),
            "the attention item must resolve once the branch is recovered"
        );
    }

    #[tokio::test]
    async fn duplicate_terminal_executions_on_one_work_item_open_only_one_pr() {
        let (_dir, db) = open_db();
        let (chore_id, exec_a) = make_candidate_chore(&db, "dup-candidate");
        let exec_b = add_second_candidate_execution(&db, &chore_id, 30);
        assert_ne!(exec_a, exec_b);

        let detector = FixedPrDetector(PrStatus::None);
        let creator = QueuePrAutoCreator::new(vec![AutoCreateOutcome::Created(
            "https://github.com/foo/bar/pull/55".to_owned(),
        )]);
        let sink = NoopDispatchEventSink;
        let mut failures = HashMap::new();

        let outcome = run_one_pass(&db, &detector, &creator, &sink, &mut failures).await;
        assert_eq!(outcome.created, 1);
        assert_eq!(
            creator.calls(),
            1,
            "two terminal executions on one work item must reach try_create exactly once, not once per execution"
        );

        let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
        assert_eq!(status, "in_review");
        assert_eq!(pr_url.as_deref(), Some("https://github.com/foo/bar/pull/55"));
    }

    #[test]
    fn dedup_by_work_item_keeps_the_newest_finished_candidate() {
        let make = |work_item_id: &str, execution_id: &str| LatePrCandidate {
            execution_id: execution_id.to_owned(),
            work_item_id: work_item_id.to_owned(),
            repo_remote_url: "git@github.com:foo/bar.git".to_owned(),
            branch_naming: Default::default(),
            worker_branch_prefix: None,
        };
        // Query order is `finished_at ASC, id ASC`, so the later entry in
        // the input Vec is the newer row — the dedup must keep it.
        let candidates = vec![make("wi-1", "exec-a"), make("wi-1", "exec-b"), make("wi-2", "exec-c")];

        let deduped = dedup_by_work_item(candidates);
        assert_eq!(deduped.len(), 2);
        let wi1 = deduped.iter().find(|c| c.work_item_id == "wi-1").unwrap();
        assert_eq!(
            wi1.execution_id, "exec-b",
            "must keep the newest (last-ordered) candidate per work item"
        );
        assert!(deduped.iter().any(|c| c.execution_id == "exec-c"));
    }

    #[test]
    fn classify_gh_error_transient_cases() {
        for status in [None, Some(0), Some(429), Some(503), Some(599)] {
            let err = match status {
                None => GhRunnerError::transient("boom"),
                Some(s) => GhRunnerError::with_status(s, "boom"),
            };
            match classify_gh_error(err) {
                AutoCreateOutcome::Failed { transient, .. } => {
                    assert!(transient, "status {status:?} should classify as transient")
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn classify_gh_error_permanent_cases() {
        for status in [401, 403, 418] {
            let err = GhRunnerError::with_status(status, "boom");
            match classify_gh_error(err) {
                AutoCreateOutcome::Failed { transient, .. } => {
                    assert!(!transient, "status {status} should classify as permanent")
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn try_create_skips_expensively_when_branch_was_never_pushed() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![Err(GhRunnerError::with_status(404, "Not Found"))],
            vec![],
        ));
        let creator = creator_with_fake_gh(fake.clone());

        let outcome = creator.try_create("foo/bar", "boss/exec_missing", "t", "b").await;
        assert!(matches!(outcome, AutoCreateOutcome::NothingToCreate));
        assert_eq!(
            fake.get_call_count(),
            1,
            "a branch that was never pushed must be ruled out with a single call"
        );
    }

    #[tokio::test]
    async fn try_create_opens_a_pr_on_the_happy_path() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
            ],
            vec![Ok(GhResponse {
                body: serde_json::json!({"html_url": "https://github.com/foo/bar/pull/1"}),
            })],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_1", "t", "b").await;
        match outcome {
            AutoCreateOutcome::Created(url) => assert_eq!(url, "https://github.com/foo/bar/pull/1"),
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_create_binds_an_already_existing_pr_found_by_the_precheck() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([{"html_url": "https://github.com/foo/bar/pull/2"}]),
                }),
            ],
            vec![],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_2", "t", "b").await;
        match outcome {
            AutoCreateOutcome::AlreadyExists(url) => assert_eq!(url, "https://github.com/foo/bar/pull/2"),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_create_422_already_exists_resolves_via_relookup() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([{"html_url": "https://github.com/foo/bar/pull/3"}]),
                }),
            ],
            vec![Err(GhRunnerError::with_status(
                422,
                "A pull request already exists for foo:boss/exec_3",
            ))],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_3", "t", "b").await;
        match outcome {
            AutoCreateOutcome::AlreadyExists(url) => assert_eq!(url, "https://github.com/foo/bar/pull/3"),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_create_422_no_commits_is_a_quiet_skip() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
            ],
            vec![Err(GhRunnerError::with_status(
                422,
                "No commits between main and boss/exec_4",
            ))],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_4", "t", "b").await;
        assert!(matches!(outcome, AutoCreateOutcome::NothingToCreate));
    }

    #[tokio::test]
    async fn try_create_422_other_validation_error_is_a_real_failure() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
            ],
            vec![Err(GhRunnerError::with_status(
                422,
                "Validation Failed: 3 rulesets block this base",
            ))],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_5", "t", "b").await;
        match outcome {
            AutoCreateOutcome::Failed { transient, .. } => {
                assert!(
                    !transient,
                    "an unrecognised 422 must not be treated as nothing-to-create"
                )
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_create_404_on_create_is_a_real_failure_not_a_quiet_skip() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
            ],
            vec![Err(GhRunnerError::with_status(404, "Not Found"))],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_6", "t", "b").await;
        match outcome {
            AutoCreateOutcome::Failed { transient, .. } => {
                assert!(
                    !transient,
                    "a 404 at create time (branch already confirmed to exist) needs a human"
                )
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_create_missing_html_url_is_a_real_failure() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
            ],
            vec![Ok(GhResponse {
                body: serde_json::json!({}),
            })],
        ));
        let creator = creator_with_fake_gh(fake);

        let outcome = creator.try_create("foo/bar", "boss/exec_7", "t", "b").await;
        match outcome {
            AutoCreateOutcome::Failed { transient, .. } => assert!(!transient),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_branch_is_cached_across_try_create_calls_to_the_same_repo() {
        let fake = Arc::new(FakeGhRunner::new(
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"default_branch": "main"}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!([]),
                }),
                // No third `default_branch` GET queued — the second
                // try_create call must be served from the cache instead.
            ],
            vec![
                Ok(GhResponse {
                    body: serde_json::json!({"html_url": "https://github.com/foo/bar/pull/8"}),
                }),
                Ok(GhResponse {
                    body: serde_json::json!({"html_url": "https://github.com/foo/bar/pull/9"}),
                }),
            ],
        ));
        let creator = creator_with_fake_gh(fake.clone());

        let out1 = creator.try_create("foo/bar", "boss/exec_a", "t", "b").await;
        assert!(matches!(out1, AutoCreateOutcome::Created(_)));
        let out2 = creator.try_create("foo/bar", "boss/exec_b", "t", "b").await;
        assert!(matches!(out2, AutoCreateOutcome::Created(_)));

        assert_eq!(
            fake.get_call_count(),
            5,
            "default_branch must be served from the in-memory cache on the second call to the same repo"
        );
    }
}
