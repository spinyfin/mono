//! Unit tests for `completion.rs`, split out of the former inline
//! `#[cfg(test)] mod tests`. This module holds the shared fixtures,
//! stub implementations, and helper constructors; the individual test
//! functions live in the `t01`..`tNN` submodules.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::{TempDir, tempdir};
use tokio::sync::Mutex;

use super::*;
use crate::coordinator::{CubeRepoSummary, CubeWorkspaceStatus};
use crate::test_support::*;

use crate::merge_poller::{MergeProbe, PrLifecycleProbe, PrLifecycleState};
use crate::work::{
    ANSWER_AGENT_RUN_STATUS_FAILED, CreateChoreInput, CreateExecutionInput, CreateProductInput, FakePrStateChecker,
    PrOpenState, THREAD_ENTRY_KIND_ANSWER, WorkDb, WorkItem,
};

/// Captured arguments from one `detect_pr` call. Tests assert on
/// these to confirm the branch name passed in is execution-unique
/// (the AI #6 regression guard: sibling workers in other cube
/// workspaces must derive different branch names from their own
/// execution IDs).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectCall {
    repo_remote_url: String,
    expected_branch: String,
}

struct StubPrDetector {
    result: Mutex<Result<PrStatus, String>>,
    calls: std::sync::Mutex<Vec<DetectCall>>,
}

impl StubPrDetector {
    fn ok(value: Option<&str>) -> Arc<Self> {
        let status = match value {
            Some(url) => PrStatus::Fresh { url: url.to_owned() },
            None => PrStatus::None,
        };
        Arc::new(Self {
            result: Mutex::new(Ok(status)),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn ok_status(status: PrStatus) -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(Ok(status)),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn err(message: &str) -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(Err(message.to_owned())),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Swap the status returned by subsequent `detect_pr` calls.
    /// Lets a test model a worker that's idle for a couple of Stops
    /// and then finally opens a PR.
    async fn set_result(&self, status: PrStatus) {
        *self.result.lock().await = Ok(status);
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("StubPrDetector calls mutex poisoned").len()
    }

    fn calls_snapshot(&self) -> Vec<DetectCall> {
        self.calls.lock().expect("StubPrDetector calls mutex poisoned").clone()
    }
}

#[async_trait]
impl PrDetector for StubPrDetector {
    async fn detect_pr(&self, repo_remote_url: &str, expected_branch: &str) -> Result<PrStatus> {
        self.calls
            .lock()
            .expect("StubPrDetector calls mutex poisoned")
            .push(DetectCall {
                repo_remote_url: repo_remote_url.to_owned(),
                expected_branch: expected_branch.to_owned(),
            });
        let guard = self.result.lock().await;
        match &*guard {
            Ok(value) => Ok(value.clone()),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }
}

/// Configurable branch verifier for tests. Returns a fixed
/// `headRefName` (or error), a fixed `headRefOid` (or error), and a
/// fixed diff line count (or error) without shelling out to `gh`.
///
/// Constructed via [`Self::ok`] / [`Self::err`], which delegate to the
/// derived builder — every field but `result` carries a `#[builder(default
/// = ...)]` stand-in, so both constructors only ever set `result` and the
/// builder earns its keep instead of being redundant with two hand-written
/// struct literals.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct StubBranchVerifier {
    result: Result<String, String>,
    /// `headRefOid` returned by `fetch_pr_head_oid`. Defaults to a stable
    /// stand-in so tests that don't touch the SHA-delta path don't need to
    /// wire one explicitly. Tests that exercise the gate call
    /// [`StubBranchVerifier::set_head_oid`] to override.
    #[builder(default = Mutex::new(Ok("oid_unknown".to_owned())))]
    head_oid_result: Mutex<Result<String, String>>,
    /// `headRefOid` returned by the direct, teardown-time REST read. This is
    /// deliberately independent from `head_oid_result`: an ordinary
    /// SHA-delta read can fail without making the later fresh finalization
    /// read fail as well.
    #[builder(default = Mutex::new(Ok("oid_unknown".to_owned())))]
    fresh_head_oid_result: Mutex<Result<String, String>>,
    /// Line count returned by `fetch_diff_line_count`. Defaults to
    /// `999` (non-trivial) so tests that don't exercise the skip gate
    /// never accidentally trigger a skip.
    #[builder(default = Mutex::new(Ok(999)))]
    diff_line_count_result: Mutex<Result<u64, String>>,
    /// Body returned by `fetch_pr_title_and_body`. Defaults to empty —
    /// tests that care override via `set_body`.
    #[builder(default = Mutex::new(Ok(String::new())))]
    body_result: Mutex<Result<String, String>>,
    /// Title returned by `fetch_pr_title_and_body`. Defaults to empty,
    /// same as `body_result` — tests that care override via `set_title`.
    #[builder(default = Mutex::new(Ok(String::new())))]
    title_result: Mutex<Result<String, String>>,
    /// `baseRefName` returned by `fetch_pr_base_ref`. Defaults to a fixed
    /// stand-in; tests that exercise the pure-rebase gate's CI-remediation
    /// arm (no base branch on the attempt row) override via
    /// [`StubBranchVerifier::set_base_ref`].
    #[builder(default = Mutex::new(Ok("base_unknown".to_owned())))]
    base_ref_result: Mutex<Result<String, String>>,
    /// Signatures returned by `fetch_diff_signature`, keyed by the exact
    /// `(base, head)` argument pair. A pair with no configured entry
    /// returns an error identifying the unexpected pair — so a
    /// pure-rebase gate test that gets the base wrong (the AI #1
    /// regression this guards) fails loudly with "don't skip" instead of
    /// silently matching by `head` alone.
    #[builder(default = Mutex::new(HashMap::new()))]
    diff_signature_results: Mutex<HashMap<(String, String), Result<String, String>>>,
    /// Every `(base, head)` pair `fetch_diff_signature` was actually
    /// called with, in call order. Lets a test assert the exact pairs the
    /// gate requested, not just the resulting skip/no-skip outcome.
    #[builder(default = Mutex::new(Vec::new()))]
    diff_signature_calls: Mutex<Vec<(String, String)>>,
}

impl StubBranchVerifier {
    /// Verifier that always reports the given branch name for
    /// `fetch_pr_head_ref`; every other field takes its builder default.
    fn ok(branch: &str) -> Arc<Self> {
        Arc::new(Self::builder().result(Ok(branch.to_owned())).build())
    }

    /// Verifier that always returns a transient API error from
    /// `fetch_pr_head_ref`. Used to test that a branch-verification
    /// failure does NOT discard the staged URL (it is preserved for
    /// the next sweep to retry).
    fn err(message: &str) -> Arc<Self> {
        Arc::new(Self::builder().result(Err(message.to_owned())).build())
    }

    /// Override the `headRefOid` returned by `fetch_pr_head_oid`.
    /// Used by the SHA-delta gate tests to simulate a PR whose
    /// head has (or has not) moved during the worker's run.
    async fn set_head_oid(&self, oid: Result<String, String>) {
        *self.head_oid_result.lock().await = oid;
    }

    /// Override the head returned by the direct, teardown-time REST read.
    async fn set_fresh_head_oid(&self, oid: Result<String, String>) {
        *self.fresh_head_oid_result.lock().await = oid;
    }

    /// Override the diff line count returned by `fetch_diff_line_count`.
    /// Tests that exercise the no-op / trivial-diff skip gate use this to
    /// simulate a pure rebase (0 lines) or trivially small change.
    async fn set_diff_line_count(&self, count: Result<u64, String>) {
        *self.diff_line_count_result.lock().await = count;
    }

    /// Override the body returned by `fetch_pr_body`. Used by the
    /// metadata-only CI-fix gate tests to simulate the live PR body
    /// the worker did (or did not) edit during its run.
    async fn set_body(&self, body: Result<String, String>) {
        *self.body_result.lock().await = body;
    }

    /// Override the `baseRefName` returned by `fetch_pr_base_ref`.
    async fn set_base_ref(&self, base_ref: Result<String, String>) {
        *self.base_ref_result.lock().await = base_ref;
    }

    /// Configure the diff signature `fetch_diff_signature` returns when
    /// called with the exact `(base, head)` pair. Pure-rebase gate tests
    /// set the same signature for `(base_ref, pre_head)` and
    /// `(base_ref, post_head)` to simulate a clean rebase, or different
    /// signatures to simulate one that also changed content.
    async fn set_diff_signature(&self, base: &str, head: &str, sig: Result<String, String>) {
        self.diff_signature_results
            .lock()
            .await
            .insert((base.to_owned(), head.to_owned()), sig);
    }

    /// Every `(base, head)` pair `fetch_diff_signature` was called with,
    /// in call order.
    async fn diff_signature_calls_snapshot(&self) -> Vec<(String, String)> {
        self.diff_signature_calls.lock().await.clone()
    }
}

#[async_trait]
impl BranchVerifier for StubBranchVerifier {
    async fn fetch_pr_head_ref(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        match &self.result {
            Ok(branch) => Ok(branch.clone()),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }

    async fn fetch_pr_head_oid(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        let guard = self.head_oid_result.lock().await;
        match &*guard {
            Ok(oid) => Ok(oid.clone()),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }

    async fn fetch_pr_head_oid_fresh(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        let guard = self.fresh_head_oid_result.lock().await;
        match &*guard {
            Ok(oid) => Ok(oid.clone()),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }

    async fn fetch_diff_line_count(&self, _repo_slug: &str, _base: &str, _head: &str) -> Result<u64> {
        let guard = self.diff_line_count_result.lock().await;
        match &*guard {
            Ok(count) => Ok(*count),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }

    async fn fetch_pr_base_ref(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        let guard = self.base_ref_result.lock().await;
        match &*guard {
            Ok(base) => Ok(base.clone()),
            Err(msg) => Err(anyhow::anyhow!(msg.clone())),
        }
    }

    async fn fetch_diff_signature(&self, _repo_slug: &str, base: &str, head: &str) -> Result<String> {
        self.diff_signature_calls
            .lock()
            .await
            .push((base.to_owned(), head.to_owned()));
        let guard = self.diff_signature_results.lock().await;
        match guard.get(&(base.to_owned(), head.to_owned())) {
            Some(Ok(sig)) => Ok(sig.clone()),
            Some(Err(msg)) => Err(anyhow::anyhow!(msg.clone())),
            None => Err(anyhow::anyhow!("unexpected base {base} for head {head}")),
        }
    }

    async fn fetch_pr_title_and_body(&self, _repo_slug: &str, _pr_number: u64) -> Result<(String, String)> {
        let title_guard = self.title_result.lock().await;
        let title = match &*title_guard {
            Ok(title) => title.clone(),
            Err(msg) => return Err(anyhow::anyhow!(msg.clone())),
        };
        let body_guard = self.body_result.lock().await;
        let body = match &*body_guard {
            Ok(body) => body.clone(),
            Err(msg) => return Err(anyhow::anyhow!(msg.clone())),
        };
        Ok((title, body))
    }
}

#[derive(Default)]
struct RecordingProbeQueuer {
    calls: std::sync::Mutex<Vec<(String, String)>>,
    clear_calls: std::sync::Mutex<Vec<String>>,
}

impl ProbeQueuer for RecordingProbeQueuer {
    fn queue_probe(&self, run_id: &str, text: &str) {
        self.calls
            .lock()
            .expect("RecordingProbeQueuer mutex poisoned")
            .push((run_id.to_owned(), text.to_owned()));
    }

    fn clear_pending_probes(&self, run_id: &str, _reason: &str) {
        self.clear_calls
            .lock()
            .expect("RecordingProbeQueuer mutex poisoned")
            .push(run_id.to_owned());
    }
}

impl RecordingProbeQueuer {
    fn snapshot(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("RecordingProbeQueuer mutex poisoned").clone()
    }

    fn clear_snapshot(&self) -> Vec<String> {
        self.clear_calls
            .lock()
            .expect("RecordingProbeQueuer mutex poisoned")
            .clone()
    }
}

#[derive(Default)]
struct StubCubeClient {
    release_calls: Mutex<Vec<String>>,
}

crate::stub_cube_client! { StubCubeClient {
    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.release_calls.lock().await.push(lease_id.to_owned());
        Ok(())
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(Vec::new())
    }
} }

#[derive(Default)]
struct RecordingPaneReleaser {
    calls: Mutex<Vec<String>>,
    /// When set, `release_pane` reports this instead of the default
    /// `Reaped` — lets a test simulate a worker still mid-spawn
    /// (no slot mapped → `NoLiveWorker`) so the lease-release gate
    /// can be exercised.
    outcome: std::sync::Mutex<Option<PaneReleaseOutcome>>,
}

impl RecordingPaneReleaser {
    fn with_outcome(outcome: PaneReleaseOutcome) -> Self {
        Self {
            calls: Mutex::default(),
            outcome: std::sync::Mutex::new(Some(outcome)),
        }
    }
}

#[async_trait]
impl WorkerPaneReleaser for RecordingPaneReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        self.calls.lock().await.push(run_id.to_owned());
        self.outcome
            .lock()
            .expect("RecordingPaneReleaser outcome mutex poisoned")
            .unwrap_or(PaneReleaseOutcome::Reaped)
    }
}

/// The standard completion-handler test harness: a
/// [`WorkerCompletionHandler`] wired to the four recording stubs, bundled
/// with the stub `Arc`s so post-run assertions can still reach them
/// (`cube.release_calls`, `publisher.publish_calls`, `pane.calls`,
/// `probes.snapshot()`).
///
/// The PR detector varies across tests (`StubPrDetector::ok(None)`,
/// `detector.clone()`, bespoke detectors, …), so it is a parameter rather
/// than baked in. Tests that need extra wiring keep chaining the existing
/// `.with_*()` builders on the returned `handler`.
struct TestHarness {
    handler: WorkerCompletionHandler,
    cube: Arc<StubCubeClient>,
    publisher: Arc<RecordingPublisher>,
    pane: Arc<RecordingPaneReleaser>,
    probes: Arc<RecordingProbeQueuer>,
}

impl TestHarness {
    /// Build the harness with a default (`Reaped`) pane releaser.
    fn new(db: Arc<WorkDb>, detector: Arc<dyn PrDetector>) -> Self {
        Self::with_pane(db, detector, Arc::new(RecordingPaneReleaser::default()))
    }

    /// Same as [`TestHarness::new`] but with a caller-supplied pane
    /// releaser — for the few tests that wire a
    /// `PaneReleaseOutcome::NoLiveWorker` pane to exercise the
    /// lease-release gate.
    fn with_pane(db: Arc<WorkDb>, detector: Arc<dyn PrDetector>, pane: Arc<RecordingPaneReleaser>) -> Self {
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        )
        // PR-completion terminalization records a fresh head. Keep the shared
        // harness hermetic; tests that exercise branch-specific behaviour
        // replace this with their own verifier.
        .with_branch_verifier(StubBranchVerifier::ok("boss/test"))
        // Auto-advancing by default: most tests drive several `on_stop`
        // calls back-to-back to exercise the circuit breaker's *count*,
        // not its timing, and a synchronous test loop would otherwise
        // spuriously hit the debounce guard on the second call (see
        // `auto_advancing_clock` doc). Tests that specifically exercise
        // debounce timing override with their own clock via
        // `.with_now_fn`.
        .with_now_fn(auto_advancing_clock());
        Self {
            handler,
            cube,
            publisher,
            pane,
            probes,
        }
    }
}

/// Auto-advancing fake clock for the auto-nudge debounce guard
/// ([`crate::nudge_breaker::MIN_RENUDGE_INTERVAL`]). Each call to the
/// returned closure yields a timestamp comfortably past the previous
/// one, so a test that calls `on_stop` several times in a tight loop —
/// the standard way this suite exercises the circuit breaker's *count*
/// — never trips the debounce guard by accident: in production,
/// consecutive Stops for one execution are seconds to minutes apart (a
/// real worker turn); a synchronous test loop is not. Tests that
/// specifically exercise debounce timing (nudge fires, then an
/// immediate re-Stop must be suppressed) wire in their own clock via
/// `with_now_fn` instead of this default.
fn auto_advancing_clock() -> Arc<dyn Fn() -> std::time::Instant + Send + Sync> {
    let next = std::sync::Mutex::new(std::time::Instant::now());
    Arc::new(move || {
        let mut guard = next.lock().expect("auto_advancing_clock mutex poisoned");
        let now = *guard;
        *guard = now + crate::nudge_breaker::MIN_RENUDGE_INTERVAL + std::time::Duration::from_secs(1);
        now
    })
}

/// Build a WorkDb plus a chore in `waiting_human` execution state with
/// a cube lease attached — this is the state the engine is in once
/// `PaneSpawnRunner::run_execution` has returned and
/// `record_run_completion` has run.
fn fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Detect worker stop");
    let execution = create_ready_chore_execution(&db, chore.id.clone());

    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    // Mirror PaneSpawnRunner: run is recorded as completed and the
    // execution sits in `waiting_human` with the lease still held.
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned worker pane"));

    (dir, db, product.id, chore.id, execution.id)
}

/// Stand up a `question`-classified comment already `answering`, with its
/// tracking `answer_agent_runs` row (`running`) and a `running`
/// `answer_agent` execution bound to it — the state `finalize_answer_agent`
/// (called from `on_stop`) expects to find. Returns `(db, comment_id,
/// run_id, execution_id)`.
fn answer_agent_fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let comment = db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_id: "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md".into(),
            anchor: boss_protocol::CommentAnchor {
                exact: "the quoted text".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            artifact_kind: "pr_doc".into(),
            author: "human".into(),
            body: "What does this mean?".into(),
            doc_version: "v1".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, crate::work::INTENT_QUESTION, 0.9)
        .unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = db
        .create_answer_agent_execution(&comment.id, &product.repo_remote_url.clone().unwrap())
        .unwrap();
    let (execution, _run_row) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    (dir, db, comment.id, run.id, execution.id)
}

/// Stand up a `ready` `automation_triage` execution bound to a fresh
/// automation, started and pane-parked exactly like `PaneSpawnRunner`
/// leaves a worker awaiting its Stop hook — the state
/// `finalize_automation_triage` (called from `on_stop`) expects to find.
/// Returns `(db, automation_id, execution_id)`.
fn automation_triage_fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String) {
    automation_triage_fixture_dispatched_as(workspace_path, "worker-1")
}

/// Like [`automation_triage_fixture`], but lets the caller pick the run's
/// `agent_id` — so a test can dispatch the triage run on the automation pool
/// (`auto-worker-N`) instead of the main pool, to exercise
/// `driver_transcript`'s pool-dispatch override for `finalize_automation_triage`.
fn automation_triage_fixture_dispatched_as(
    workspace_path: &Path,
    worker_id: &str,
) -> (TempDir, Arc<WorkDb>, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let automation = db
        .create_automation(boss_protocol::CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Dead code".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "America/Los_Angeles".to_owned(),
            },
            standing_instruction: "Clean up dead code.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: Some("cli".to_owned()),
        })
        .unwrap();
    let execution = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            worker_id,
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned worker pane"));
    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(1_700_000_000i64)
            .started_at(1_700_000_000i64)
            .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .triage_execution_id(execution.id.clone())
            .next_due_at(1_700_086_400i64)
            .build(),
    )
    .unwrap();
    (dir, db, automation.id, execution.id)
}

fn ci_remediation_fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Fix CI");
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    let attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr_url.into(),
            pr_number: 88,
            head_branch: "feature".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    db.mark_chore_blocked_ci_failure(&chore.id, pr_url, Some(&attempt.id))
        .unwrap();
    db.mark_ci_remediation_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap();

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::CiRemediation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned worker pane"));
    (dir, db, product.id, chore.id, execution.id, attempt.id)
}

/// Seed a chore + execution left in `waiting_human` (the lease still
/// held), occupying cube workspace `workspace_id`. Returns
/// `(chore_id, execution_id)`. Mirrors the `fixture` lifecycle but
/// lets a test place several occupants in one workspace.
fn seed_workspace_occupant(
    db: &Arc<WorkDb>,
    product_id: &str,
    name: &str,
    lease: &str,
    workspace_id: &str,
    workspace_path: &str,
) -> (String, String) {
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product_id)
                .name(name)
                .force_duplicate(true)
                .build(),
        )
        .unwrap();
    let exec = create_ready_chore_execution(db, chore.id.clone());
    let (_e, run) = db
        .start_execution_run(&exec.id, "worker", "mono", lease, workspace_id, workspace_path)
        .unwrap();
    finish_run_worker_pane_alive(db, &exec.id, &run.id, Some("spawned worker pane"));
    (chore.id, exec.id)
}

/// AI #6 running-status gate: if the Stop hook fires on an
/// execution that's still in `running` status (i.e. the worker is
/// alive and racing through turns) and there's no staged URL, the
/// fallback MUST NOT fire. Pre-incident-001 it did, and the
/// per-turn firing rate against cube's shared `.jj/repo/store/git`
/// is what produced the May 14 fan-out.
/// Build a fixture left in `running` status — i.e. `start_execution_run`
/// has fired but `finish_execution_run` has not yet been called. The
/// in-cube worker pane is alive, and a `Stop` hook fires for the
/// first assistant turn before the upper layer has had a chance to
/// stamp `waiting_human`.
fn fixture_running(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Running execution");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    // `start_execution_run` flips the row to `running`. Do not
    // follow up with `finish_execution_run` — we want the row to
    // stay in `running` to exercise the AI #6 gate.
    let (execution, _run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running);
    (dir, db, product.id, chore.id, execution.id)
}

/// Variant of [`fixture`] that mirrors a resume bounce-back: the
/// chore already carries a `pr_url` (set by an earlier run's
/// on-Stop machinery), and the new execution has its
/// `pr_head_before` snapshot already persisted (the equivalent of
/// `on_execution_started` having run at dispatch time).
fn resume_fixture(
    workspace_path: &Path,
    bound_pr_url: &str,
    head_before: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let (dir, db, product_id, chore_id, execution_id) = fixture(workspace_path);
    db.update_work_item(
        &chore_id,
        crate::work::WorkItemPatch {
            pr_url: Some(bound_pr_url.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    db.set_execution_pr_head_before(&execution_id, head_before).unwrap();
    (dir, db, product_id, chore_id, execution_id)
}

/// Write a single-turn assistant transcript JSONL for `execution_id`
/// and register its path, so the no-op gate's transcript read finds
/// `text`. Mirrors the pr_review fixtures' transcript seeding.
fn write_assistant_transcript(db: &WorkDb, workspace_path: &Path, execution_id: &str, text: &str) {
    let obj = serde_json::json!({
        "type": "assistant",
        "message": { "content": [{"type": "text", "text": text}] }
    });
    let jsonl = format!("{obj}\n");
    let transcript_path = workspace_path.join(format!("transcript-{execution_id}.jsonl"));
    std::fs::write(&transcript_path, jsonl.as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(execution_id, transcript_path.to_str().unwrap())
        .unwrap();
}

/// Point `work_item_id`'s driver at `slug`, so
/// `WorkDb::get_execution_driver_slug` — and therefore every driver-aware
/// read of its executions' transcripts — resolves to that driver.
fn set_work_item_driver(db: &WorkDb, work_item_id: &str, slug: &str) {
    db.update_work_item(
        work_item_id,
        crate::work::WorkItemPatch {
            driver: Some(slug.to_owned()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
}

/// Write a transcript for `execution_id` in the **canonical entry shape** —
/// what every driver's `normalize_transcript_entry` produces, as opposed to
/// [`write_assistant_transcript`]'s Claude-native `message` envelope — and
/// register its path.
///
/// Lets a test assert a Stop-boundary behaviour holds for *every* registered
/// driver without the test itself having to know each backend's native
/// dialect (which would just re-hardcode the per-driver knowledge the driver
/// abstraction exists to hold).
fn write_canonical_assistant_transcript(db: &WorkDb, workspace_path: &Path, execution_id: &str, text: &str) {
    let obj = serde_json::json!({
        "type": "assistant",
        "content": [{"type": "text", "text": text}]
    });
    let jsonl = format!("{obj}\n");
    let transcript_path = workspace_path.join(format!("transcript-{execution_id}.jsonl"));
    std::fs::write(&transcript_path, jsonl.as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(execution_id, transcript_path.to_str().unwrap())
        .unwrap();
}

/// Write a transcript for `execution_id` in Codex's **native rollout
/// dialect** — `session_meta` / `event_msg` / `response_item` envelopes, none
/// of which the Claude transcript parser recognises — and register its path.
///
/// This is the shape the engine actually finds on disk for a Codex run
/// (`work_runs.transcript_path` points at the rollout JSONL under the
/// run-private `CODEX_HOME`), so a test using it exercises the real
/// normalize-then-parse path rather than a pre-canonicalised stand-in. `text`
/// is emitted the way Codex records a final answer: as the `agent_message`
/// event and again as the assistant `response_item`.
fn write_codex_rollout_transcript(db: &WorkDb, workspace_path: &Path, execution_id: &str, text: &str) {
    let lines = [
        serde_json::json!({"type": "session_meta", "payload": {"id": "session-1"}}),
        serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}),
        serde_json::json!({"type": "event_msg", "payload": {"type": "agent_message", "message": text}}),
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            }
        }),
    ];
    let jsonl = lines.iter().map(|line| format!("{line}\n")).collect::<String>();
    let transcript_path = workspace_path.join(format!("rollout-{execution_id}.jsonl"));
    std::fs::write(&transcript_path, jsonl.as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(execution_id, transcript_path.to_str().unwrap())
        .unwrap();
}

/// Build a revision fixture but leave `execution.pr_url` as NULL
/// (simulates an execution created before pr_url was reliably stamped).
/// The parent chore still has `pr_url` set so the chain-root lookup
/// can find it.
fn revision_fixture_no_execution_pr_url(
    workspace_path: &Path,
    parent_pr_url: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String) {
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-revision-chain-root-test");
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, parent_pr_url],
        )
        .unwrap();
    }
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Fix conflict")
                .build(),
            &checker,
        )
        .unwrap();
    // Create execution WITHOUT pr_url (simulates older dispatch path).
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                // Intentionally omitting pr_url to test chain-root fallback.
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned revision worker pane"));
    (dir, db, product.id, revision.id, execution.id)
}

/// Build a WorkDb with a chore whose execution is `abandoned` and
/// `workspace_path` is still set (mirrors the double-spawn race where
/// exec_A is abandoned by the orphan sweep while its pane is running).
fn abandoned_execution_fixture() -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss-late.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Late PR chore");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/workspaces/mono-agent-001",
        )
        .unwrap();
    // Mirror the waiting_human state.
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned pane"));
    // Simulate orphan sweep abandoning exec_A.
    db.mark_execution_redundant(&execution.id).unwrap();
    (dir, db, product.id, chore.id, execution.id)
}

/// Build a fixture simulating a revision task whose worker has been
/// spawned and is in `waiting_human` state. The parent chore carries
/// `parent_pr_url` and the revision execution's `pr_url` is set to
/// the same URL (as the dispatcher does at create time). `head_before`
/// is stored as `pr_head_before` to simulate the snapshot taken by
/// `on_execution_started`.
fn revision_fixture(
    workspace_path: &Path,
    parent_pr_url: &str,
    head_before: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String) {
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-revision-test");
    // Parent chore: in_review with a bound pr_url.
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, parent_pr_url],
        )
        .unwrap();
    }
    // Revision task: created against the parent.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Add missing builder derive")
                .build(),
            &checker,
        )
        .unwrap();
    // Execution: revision_implementation with pr_url = parent PR URL.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(parent_pr_url)
                .build(),
        )
        .unwrap();
    // Mirror PaneSpawnRunner: start → running (task → active), then
    // finish → waiting_human (pane spawned, engine waiting for Claude).
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned revision worker pane"));
    // Snapshot the parent PR's head SHA as `on_execution_started` does.
    db.set_execution_pr_head_before(&execution.id, head_before).unwrap();
    (dir, db, product.id, revision.id, execution.id)
}

/// Build a revision fixture (mirrors [`revision_fixture`]) whose revision
/// task was created via a conflict-resolution or CI-fix attempt, with a
/// matching `conflict_resolutions` / `ci_remediations` row recording
/// `head_sha_before` (and, for a conflict resolution, `base_sha_at_trigger`)
/// — the state [`WorkerCompletionHandler::check_pure_rebase_skip`] reads.
/// The attempt row's `revision_task_id` is stamped to the created
/// revision's id, mirroring the production producer path — the gate
/// selects its attempt row by this link, not merely "latest for the root".
///
/// `created_via` selects the attempt table: a value starting with
/// `CREATED_VIA_MERGE_CONFLICT_PREFIX` inserts a `conflict_resolutions` row
/// (with `pre_base` recorded, if given, and `base_branch` fixed to
/// `"main"`); `CREATED_VIA_CI_FIX_PREFIX` inserts a `ci_remediations` row
/// (which never records a base — `pre_base` is ignored for this case, and
/// the gate resolves the base branch live via `fetch_pr_base_ref`).
fn pure_rebase_revision_fixture(
    workspace_path: &Path,
    created_via: &str,
    pr_number: i64,
    head_before: &str,
    pre_base: Option<&str>,
) -> (TempDir, Arc<WorkDb>, String, String, String, String) {
    use crate::work::{CiRemediationInsertInput, ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-pure-rebase-test");
    let parent_pr_url = format!("https://github.com/spinyfin/mono/pull/{pr_number}");
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, parent_pr_url],
        )
        .unwrap();
    }

    let is_conflict = created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX);
    let attempt_id = if is_conflict {
        db.insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: parent_pr_url.clone(),
            pr_number,
            head_branch: "boss/exec_parent".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: pre_base.map(str::to_owned),
            head_sha_before: Some(head_before.to_owned()),
        })
        .unwrap()
        .expect("conflict_resolution insert must not be churn-guarded in a fresh fixture")
        .id
    } else if created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
        db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: parent_pr_url.clone(),
            pr_number,
            head_branch: "boss/exec_parent".to_owned(),
            head_sha_at_trigger: head_before.to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "pr_branch_ci".to_owned(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("ci_remediation insert must not be deduped in a fresh fixture")
        .id
    } else {
        panic!("pure_rebase_revision_fixture: created_via must carry a conflict/CI-fix prefix, got {created_via:?}");
    };

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Rebase onto latest main")
                .created_via(created_via)
                .build(),
            &checker,
        )
        .unwrap();
    // Mirrors the production path (`conflict_watch::on_conflict_detected` /
    // the CI-fix equivalent): the producer stamps the soft-FK from the
    // attempt row to the `kind=revision` task it just spawned, immediately
    // after `create_revision` succeeds. `check_pure_rebase_skip` selects
    // its attempt row by this link.
    if is_conflict {
        db.set_conflict_resolution_revision_task_id(&attempt_id, &revision.id)
            .unwrap();
    } else {
        db.set_ci_remediation_revision_task_id(&attempt_id, &revision.id)
            .unwrap();
    }
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(parent_pr_url.clone())
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned revision worker pane"));
    db.set_execution_pr_head_before(&execution.id, head_before).unwrap();
    (dir, db, product.id, parent.id, revision.id, execution.id)
}

/// Configurable [`MergeProbe`] returning a fixed lifecycle state for
/// any PR url. Drives the bound-PR-health check in the metadata-fix
/// finalize path.
struct FixedStateProbe(crate::merge_poller::PrLifecycleState);
#[async_trait]
impl MergeProbe for FixedStateProbe {
    async fn probe(&self, _pr_url: &str) -> anyhow::Result<crate::merge_poller::PrLifecycleProbe> {
        Ok(crate::merge_poller::PrLifecycleProbe::builder()
            .url(String::new())
            .state(self.0.clone())
            .labels(Vec::new())
            .review(crate::merge_poller::PrReviewState::Unknown)
            .build())
    }
}

/// Reports a fixed live-descendant count for every execution, regardless
/// of id — enough to exercise `nudge_or_park`'s background-children
/// suppression branch without a real process tree. Shared by t02 (the
/// suppression tests themselves) and t08 (the satisfied-deliverable gate's
/// "still working" arm).
struct FixedDescendantProbe(usize);
impl crate::background_children::BackgroundActivityProbe for FixedDescendantProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> std::result::Result<usize, String> {
        Ok(self.0)
    }
}

struct FailingDescendantProbe;
impl crate::background_children::BackgroundActivityProbe for FailingDescendantProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> std::result::Result<usize, String> {
        Err("synthetic process-table failure".to_owned())
    }
}

struct WatermarkedDescendantProbe {
    watermark: std::sync::Mutex<String>,
}

impl WatermarkedDescendantProbe {
    fn new(watermark: &str) -> Arc<Self> {
        Arc::new(Self {
            watermark: std::sync::Mutex::new(watermark.to_owned()),
        })
    }

    fn set_watermark(&self, watermark: &str) {
        *self.watermark.lock().expect("watermark mutex poisoned") = watermark.to_owned();
    }
}

impl crate::background_children::BackgroundActivityProbe for WatermarkedDescendantProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> std::result::Result<usize, String> {
        Ok(1)
    }

    fn activity_watermark(&self, _execution_id: &str) -> Option<String> {
        Some(self.watermark.lock().expect("watermark mutex poisoned").clone())
    }
}

/// Build a conflict-resolution revision fixture. The parent chore is
/// `blocked: merge_conflict`. A `conflict_resolutions` row is inserted
/// in `running` state (simulating an active attempt). A revision task is
/// created for the fix; a `revision_implementation` execution is left in
/// `waiting_human` with `pr_head_before = head_before` (the SHA-delta
/// snapshot). `created_via` is set to `"merge-conflict:<attempt_id>"`.
///
/// Returns `(db, product_id, parent_chore_id, revision_id, execution_id, attempt_id, pr_url)`.
#[allow(clippy::too_many_arguments)]
fn conflict_revision_fixture(
    workspace_path: &Path,
    parent_pr_url: &str,
    head_before: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String, String, String) {
    use crate::work::{ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-conflict-rev-test");
    // Parent chore: blocked:merge_conflict with a bound pr_url.
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict', \
             pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, parent_pr_url],
        )
        .unwrap();
    }
    // Insert a conflict_resolutions attempt (Phase 3 style: `pending`,
    // no cube_lease_id — the fix vehicle is a revision_implementation
    // execution, not a bespoke conflict_resolution execution).
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: parent_pr_url.to_owned(),
            pr_number: 966,
            head_branch: "my-feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base_sha_1".into()),
            head_sha_before: Some(head_before.into()),
        })
        .unwrap()
        .unwrap();
    // Revision task with created_via = "merge-conflict:<attempt_id>".
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Resolve merge conflict against main")
                .created_via(format!("merge-conflict:{}", attempt.id))
                .build(),
            &checker,
        )
        .unwrap();
    // Stamp the reverse link (as conflict_watch::on_conflict_detected does).
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
        .unwrap();
    // Execution: revision_implementation with pr_url = parent PR URL.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(parent_pr_url)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-033",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(
        &db,
        &execution.id,
        &run.id,
        Some("spawned conflict-resolution worker pane"),
    );
    db.set_execution_pr_head_before(&execution.id, head_before).unwrap();
    (dir, db, product.id, parent.id, revision.id, execution.id, attempt.id)
}

/// Build a CI-remediation revision fixture. The parent chore is
/// `blocked: ci_failure` with a bound `pr_url`. A `ci_remediations` row
/// is inserted (`running`) carrying `failed_checks` (the JSON list of
/// checks this attempt was opened to fix). A revision task is created
/// for the fix (`created_via = "ci-fix:<attempt_id>"`) and its id stamped
/// back onto the attempt. A `revision_implementation` execution is left
/// in `waiting_human` with `pr_head_before = head` (the SHA-delta
/// snapshot, so the gate returns NoContribution on an unmoved head).
///
/// Returns `(db, product_id, parent_chore_id, revision_id, execution_id, attempt_id)`.
fn ci_revision_fixture(
    workspace_path: &Path,
    parent_pr_url: &str,
    head: &str,
    failed_checks: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String, String, String) {
    ci_revision_fixture_with_kind(workspace_path, parent_pr_url, head, failed_checks, "pr_branch_ci")
}

fn ci_revision_fixture_with_kind(
    workspace_path: &Path,
    parent_pr_url: &str,
    head: &str,
    failed_checks: &str,
    failure_kind: &str,
) -> (TempDir, Arc<WorkDb>, String, String, String, String, String) {
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-ci-rev-test");
    let parent = create_test_chore_manual(&db, product.id.clone(), "Fix failing CI: Pull Request Description");
    db.update_work_item(
        &parent.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(parent_pr_url.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    let attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: parent_pr_url.into(),
            pr_number: 440,
            head_branch: "my-feature".into(),
            head_sha_at_trigger: head.into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: failed_checks.into(),
            failure_kind: failure_kind.into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    db.mark_chore_blocked_ci_failure(&parent.id, parent_pr_url, Some(&attempt.id))
        .unwrap();
    db.mark_ci_remediation_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Add the required PR description sections")
                .created_via(format!("ci-fix:{}", attempt.id))
                .build(),
            &checker,
        )
        .unwrap();
    db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id)
        .unwrap();

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(parent_pr_url)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned CI-fix revision worker pane"));
    db.set_execution_pr_head_before(&execution.id, head).unwrap();
    (dir, db, product.id, parent.id, revision.id, execution.id, attempt.id)
}

/// Build a `PrLifecycleProbe` for an open PR with the given CI status.
fn ci_probe(ci: crate::merge_poller::OpenPrCiStatus) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(String::new())
        .state(PrLifecycleState::Open(crate::merge_poller::OpenPrStatus {
            mergeability: crate::merge_poller::OpenPrMergeability::Clean,
            ci,
        }))
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

fn failing_check(name: &str) -> crate::merge_poller::RequiredCheckFailure {
    crate::merge_poller::RequiredCheckFailure {
        name: name.to_owned(),
        conclusion: "FAILURE".to_owned(),
        target_url: String::new(),
        provider: crate::merge_poller::CiProvider::Other,
        provider_job_id: None,
    }
}

// ── No-op / trivial-diff skip gate ──────────────────────────
//
// Helper that creates a fixture with `last_reviewed_sha` already set on the
// chore (simulating a prior review cycle) and stages a PR URL, then returns
// everything needed to drive `on_stop` in a test.
fn noop_skip_fixture(
    workspace_path: &Path,
    last_reviewed_sha: Option<&str>,
) -> (
    TempDir,
    Arc<WorkDb>,
    String, // chore_id
    String, // execution_id
    Arc<crate::pr_url_capture::StagedPrUrlCache>,
    String, // expected_branch
) {
    const PR_URL: &str = "https://github.com/spinyfin/mono/pull/88";
    let (dir, db, _product_id, chore_id, execution_id) = fixture(workspace_path);
    if let Some(sha) = last_reviewed_sha {
        db.increment_task_review_cycle(&chore_id, Some(sha))
            .expect("failed to set last_reviewed_sha");
    }
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_if_unset(&execution_id, PR_URL);
    let branch = expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None);
    (dir, db, chore_id, execution_id, staged, branch)
}

/// Build a JSONL transcript line containing `review_result_json` in an
/// assistant message, matching the format `read_final_triage_message`
/// expects (one JSON object per line, `type=assistant`, `message.content`
/// array with a `text` block).
///
/// Uses `serde_json` for the outer object so the `text` field is properly
/// escaped regardless of what characters appear in the ReviewResult JSON.
fn make_review_transcript_jsonl(review_result_json: &str) -> String {
    let text = format!("Here is my automated PR review.\n\n```json\n{review_result_json}\n```");
    let obj = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": text}]
        }
    });
    format!("{}\n", obj)
}

/// Build a JSONL transcript line containing `review_result_json` as **bare
/// JSON** (no ` ```json ` fence). Simulates the failure mode where the
/// model emits the ReviewResult inline after prose without any code fence.
fn make_bare_review_transcript_jsonl(review_result_json: &str) -> String {
    let text = format!(
        "I have reviewed the PR carefully.\n\n\
         Key findings are summarised below.\n\n\
         {review_result_json}"
    );
    let obj = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": text}]
        }
    });
    format!("{}\n", obj)
}

/// Build a producing chore with `pr_url` already set (simulating the
/// PendingReview state that `finalize_pr_transition` writes) together with
/// a `pr_review` execution in `waiting_human` status. Optionally write a
/// JSONL transcript file and register its path so
/// `finalize_pr_review_pass` can read the `ReviewResult`.
///
/// The `review_result_json` is the raw ReviewResult JSON; it is wrapped in
/// a ` ```json ` fence by `make_review_transcript_jsonl`. Use
/// `pr_review_exec_fixture_with_jsonl` to supply a pre-built transcript
/// JSONL directly (e.g., for bare-JSON regression tests).
///
/// Returns `(db, product_id, chore_id, pr_review_exec_id, pr_url)`.
fn pr_review_exec_fixture(
    workspace_path: &Path,
    review_result_json: Option<&str>,
) -> (TempDir, Arc<WorkDb>, String, String, String, String) {
    let transcript_jsonl = review_result_json.map(make_review_transcript_jsonl);
    pr_review_exec_fixture_with_jsonl(workspace_path, transcript_jsonl.as_deref())
}

/// Like `pr_review_exec_fixture`, but accepts a pre-built JSONL transcript
/// string. Use this when the transcript format matters (e.g., bare JSON,
/// multi-turn) and `make_review_transcript_jsonl` does not produce the
/// desired shape.
fn pr_review_exec_fixture_with_jsonl(
    workspace_path: &Path,
    transcript_jsonl: Option<&str>,
) -> (TempDir, Arc<WorkDb>, String, String, String, String) {
    const PR_URL: &str = "https://github.com/spinyfin/mono/pull/88";
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());

    let product = create_test_product(&db);

    // Producing task — starts active, gets pr_url stamped so the reviewer
    // can find the PR (mirrors what finalize_pr_transition writes on
    // PendingReview).
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Implement feature X")
                .description("Feature X adds Y functionality to the pipeline.")
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            pr_url: Some(PR_URL.into()),
            ..Default::default()
        },
    )
    .unwrap();

    // PrReview execution in running (reviewer pane spawned and alive, about to stop).
    // After the fix, `PaneSpawnRunner` returns `WorkerPaneAlive` for `pr_review`
    // executions so the execution stays in `running` (not `waiting_human`) while the
    // reviewer agent is working. The Stop hook transitions it to `completed` via
    // `record_worker_pr_completion`. See runner.rs `RunWaitState::WorkerPaneAlive`.
    let pr_review_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let (pr_review_exec, run) = db
        .start_execution_run(
            &pr_review_exec.id,
            "review-worker-1",
            "mono",
            "lease-review-1",
            "mono-agent-review-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    let _ = db
        .finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&pr_review_exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::Running)
                .run_status("completed")
                .result_summary("reviewer spawned")
                .build(),
        )
        .unwrap();

    // Optionally write a pre-built JSONL transcript and register its path.
    if let Some(jsonl) = transcript_jsonl {
        let transcript_path = workspace_path.join(format!("transcript-{}.jsonl", pr_review_exec.id));
        std::fs::write(&transcript_path, jsonl.as_bytes()).unwrap();
        db.set_run_transcript_path_if_unset(&pr_review_exec.id, transcript_path.to_str().unwrap())
            .unwrap();
    }

    (dir, db, product.id, chore.id, pr_review_exec.id, PR_URL.to_owned())
}

/// Produce a minimal valid `ReviewResult` JSON with no qualifying findings
/// (medium severity only, no regressions) — the engine severity gate must
/// NOT fire for this result.
fn clean_review_result_json(pr_url: &str) -> String {
    serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_abc123",
        "summary": "The PR looks good overall. Minor style note only.",
        "revision_warranted": false,
        "findings": [
            {
                "severity": "medium",
                "category": "readability",
                "file": "src/lib.rs",
                "title": "Minor naming nit",
                "detail": "Consider renaming `x` to `input` for clarity.",
                "confidence": "low"
            }
        ],
        "regression_check": {
            "performed": true,
            "suspected_deletions": []
        }
    })
    .to_string()
}

/// Produce a `ReviewResult` JSON with a HIGH severity correctness finding —
/// the engine severity gate fires for this result.
fn high_finding_review_result_json(pr_url: &str) -> String {
    serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_abc123",
        "summary": "Critical correctness issue found in the PR.",
        "revision_warranted": true,
        "findings": [
            {
                "severity": "high",
                "category": "correctness",
                "file": "src/pr.rs",
                "location": "fn ensure_pr, ~L120",
                "title": "Duplicate PR case not handled",
                "detail": "The `?` on the gh call swallows the 422 — handle the duplicate-PR case explicitly.",
                "confidence": "high"
            }
        ],
        "regression_check": {
            "performed": true,
            "suspected_deletions": []
        }
    })
    .to_string()
}

/// Produce a `ReviewResult` JSON with a LOW severity REGRESSION finding.
/// Even though the severity is low, the engine's
/// gate must fire because `category = "regression"` overrides severity.
fn regression_class_finding_review_result_json(pr_url: &str) -> String {
    serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_abc123",
        "summary": "Forward-port silently dropped the autostart feature.",
        "revision_warranted": true,
        "findings": [
            {
                "severity": "low",
                "category": "regression",
                "file": "tools/boss/engine/core/src/lib.rs",
                "location": "fn init, ~L10",
                "title": "Forward-port dropped the autostart feature",
                "detail": "The autostart flag was removed during conflict resolution; restore it.",
                "confidence": "high"
            }
        ],
        "regression_check": {
            "performed": true,
            "suspected_deletions": []
        }
    })
    .to_string()
}

// ── Tests: finalize_pr_review_pass paths ─────────────────────────────────

/// Build a `FakePrStateChecker` that always reports the PR as open — used
/// by all pr_review tests so `create_revision` doesn't shell out to `gh`.
fn open_pr_checker() -> Arc<dyn crate::work::PrStateChecker> {
    Arc::new(FakePrStateChecker::always(PrOpenState::Open))
}

/// Produce a `ReviewResult` JSON where `suspected_deletions` is a string
/// array — the PR#1497 shape that previously caused the serde
/// type-mismatch error "invalid type: string, expected struct ReviewFinding"
/// and silently rejected the entire review.
fn string_shaped_deletions_review_result_json(pr_url: &str) -> String {
    serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_abc123",
        "summary": "Found a regression: config exclude rule removed without replacement.",
        "revision_warranted": true,
        "findings": [
            {
                "severity": "high",
                "category": "regression",
                "file": "CHECKS.yaml",
                "title": "Config exclude rule dropped without replacement",
                "detail": "The config_dir-scoped exclude_files rule was removed.",
                "confidence": "high"
            }
        ],
        "regression_check": {
            "performed": true,
            // Reviewer filled this with strings instead of structured objects.
            "suspected_deletions": [
                "config_dir-scoped exclude_files matching removed without replacement"
            ]
        }
    })
    .to_string()
}

/// A `ReviewResult` JSON with a single `duplication`-category finding —
/// the rec_engine motivating incident (a crate extraction left two
/// complete copies of the same module in the PR). Severity is
/// deliberately `medium`: duplication forces a revision regardless of
/// severity (see `passes_severity_gate`), so this also proves the
/// forcing rule fires on a revision-triggered pass, not just a
/// first-push pass.
fn duplication_finding_review_result_json(pr_url: &str) -> String {
    serde_json::json!({
        "pr_url": pr_url,
        "head_sha": "sha_reviewed_1941pdt",
        "summary": "The crate extraction left the original module in place alongside the new crate.",
        "revision_warranted": true,
        "findings": [
            {
                "severity": "medium",
                "category": "duplication",
                "file": "crates/rec_engine/src/lib.rs",
                "location": "whole file",
                "title": "rec_engine duplicated across blob/ and the new crate",
                "detail": "The extraction copied rec_engine into its own crate but never removed \
                           the original copy under blob/ — the PR now ships two complete copies \
                           of the same code. Delete the blob/ copy and repoint its callers at the \
                           new crate.",
                "confidence": "high"
            }
        ],
        "regression_check": {
            "performed": true,
            "suspected_deletions": []
        }
    })
    .to_string()
}

mod t01;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
mod t08;
mod t09;
