use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::completion::{
    PaneReleaseOutcome, PrDetector, PrStatus, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::coordinator::{CubeClient, CubeRepoSummary, CubeWorkspaceStatus};
use crate::test_support::*;
use crate::work::{
    AddDependencyInput, CommentAnchor, ConflictResolutionInsertInput, CreateCommentInput, CreateExecutionInput,
    CreateProjectInput, CreateTaskInput, ExecutionStatus, FinishExecutionRunInput, WorkDb, WorkItem, WorkItemPatch,
};

struct StubProbe {
    states: std::sync::Mutex<std::collections::HashMap<String, Result<PrLifecycleProbe, String>>>,
}

impl StubProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            states: std::sync::Mutex::new(Default::default()),
        })
    }

    fn set(&self, url: &str, state: PrLifecycleState) {
        self.set_with_base(url, state, None);
    }

    fn set_with_base(&self, url: &str, state: PrLifecycleState, base_ref_oid: Option<&str>) {
        self.states.lock().unwrap().insert(
            url.to_owned(),
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(state)
                .maybe_base_ref_oid(base_ref_oid.map(str::to_owned))
                .labels(Vec::new())
                .review(PrReviewState::Unknown)
                .build()),
        );
    }

    /// Set a probe with both `base_ref_oid` and `head_ref_oid`
    /// populated. The conflict attempt's unique key is keyed on the
    /// base sha; the CI remediation's unique key needs the head sha
    /// (and `on_ci_failure_detected` defers entirely when it is
    /// missing). Stranded-recovery regression tests vary both across
    /// sweeps so a fresh attempt row (and a fresh revision) can be
    /// inserted for the re-conflict / re-failure.
    fn set_with_base_head(&self, url: &str, state: PrLifecycleState, base_ref_oid: &str, head_ref_oid: &str) {
        self.states.lock().unwrap().insert(
            url.to_owned(),
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(state)
                .base_ref_oid(base_ref_oid.to_owned())
                .head_ref_oid(head_ref_oid.to_owned())
                .head_ref_name("feature-branch")
                .base_ref_name("main")
                .labels(Vec::new())
                .review(PrReviewState::Unknown)
                .build()),
        );
    }

    fn set_with_labels(&self, url: &str, state: PrLifecycleState, labels: &[&str]) {
        self.states.lock().unwrap().insert(
            url.to_owned(),
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(state)
                .labels(labels.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .review(PrReviewState::Unknown)
                .build()),
        );
    }

    fn set_err(&self, url: &str, msg: &str) {
        self.states.lock().unwrap().insert(url.to_owned(), Err(msg.to_owned()));
    }
}

#[async_trait]
impl MergeProbe for StubProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        let map = self.states.lock().unwrap();
        match map.get(pr_url) {
            Some(Ok(state)) => Ok(state.clone()),
            Some(Err(msg)) => Err(anyhow!(msg.clone())),
            None => Ok(PrLifecycleProbe::builder()
                .url(pr_url.to_owned())
                .state(PrLifecycleState::Open(OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(PrReviewState::Unknown)
                .build()),
        }
    }
}

/// Build a `kind = 'project_task'` row in `in_review` with a PR
/// attached — the post-completion shape that the merge poller
/// must also sweep, not just `kind = 'chore'`.
fn make_project_task_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: format!("Project-{name}"),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (product.id, task.id)
}

fn make_chore_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    // Move chore directly to in_review with a pr_url, mirroring
    // the post-completion state.
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

/// Build a `kind = 'chore'` row held `active` (P992 `PendingReview`)
/// with a `pr_review` execution in the given terminal-but-not-completed
/// status — the reviewer-fallback candidate shape
/// `list_tasks_with_stalled_reviewer` targets (T2235 / PR #1766: a
/// `pr_review` pane-spawn failure left the execution `failed` while the
/// task stayed `active`).
fn make_chore_active_with_dead_review(db: &WorkDb, name: &str, pr_url: &str, review_status: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("https://github.com/foo/bar"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(boss_protocol::ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, review_status],
        )
        .unwrap();
    (product.id, chore.id)
}

/// Stub `CubeClient` that records every `release_workspace` call.
#[derive(Default)]
struct RecordingCubeClient {
    releases: Mutex<Vec<String>>,
}

crate::stub_cube_client! { RecordingCubeClient {
    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.releases.lock().await.push(lease_id.to_owned());
        Ok(())
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }
    async fn list_repos(&self) -> Result<Vec<crate::coordinator::CubeRepoSummary>> {
        Ok(Vec::new())
    }
} }

/// Helper to build a probe with CI failures + a head sha.
fn probe_ci_failing(pr: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus::ci_failing(vec![
            RequiredCheckFailure {
                name: "ci/test".into(),
                conclusion: "FAILURE".into(),
                target_url: "".into(),
                provider: CiProvider::Other,
                provider_job_id: None,
            },
        ])))
        .base_ref_oid("base-1")
        .head_ref_oid(head_sha.to_owned())
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

fn probe_ci_clean(pr: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .base_ref_oid("base-1")
        .head_ref_oid(head_sha.to_owned())
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

/// Probe whose CI is non-terminal (`InFlight`) — the state a rollup
/// with at least one still-running required check collapses to,
/// including the "one leaf already failed but others are still running"
/// case after the terminal-failure gate (see `classify_ci`).
fn probe_ci_in_flight(pr: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::InFlight,
        }))
        .base_ref_oid("base-1")
        .head_ref_oid(head_sha.to_owned())
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

/// Which blocking signal a stranded-recovery case exercises. The
/// reconciliation pass is parameterised over both so a conflict-only or
/// ci-only regression cannot hide (work-item requirement #6).
#[derive(Clone, Copy)]
enum StrandKind {
    Conflict,
    Ci,
}

impl StrandKind {
    /// The `task_blocked_signals.reason` literal this kind arms.
    fn reason(self) -> &'static str {
        match self {
            StrandKind::Conflict => "merge_conflict",
            StrandKind::Ci => "ci_failure",
        }
    }
}

/// Point a stub probe at `pr` reporting the dirty/red signal for `kind`,
/// keyed on the given base/head SHAs so a fresh attempt row can be
/// inserted for a re-conflict / re-failure.
fn set_dirty_probe(probe: &StubProbe, pr: &str, kind: StrandKind, base_sha: &str, head_sha: &str) {
    match kind {
        StrandKind::Conflict => probe.set_with_base_head(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            base_sha,
            head_sha,
        ),
        StrandKind::Ci => {
            probe
                .states
                .lock()
                .unwrap()
                .insert(pr.to_owned(), Ok(probe_ci_failing(pr, head_sha)));
        }
    }
}

/// Helper to build a `gh pr view --json …` JSON document for the
/// parser-matrix tests. Defaults give an OPEN mergeable PR with no
/// labels and no rollup; per-test overrides re-shape specific fields.
fn json_doc(
    state: &str,
    merged_at: &str,
    mergeable: &str,
    merge_state_status: &str,
    // (base_ref_oid, head_ref_oid) — bundled to keep the parameter
    // count under clippy::too_many_arguments.
    ref_oids: (&str, &str),
    labels: &[&str],
    rollup: serde_json::Value,
) -> String {
    let (base_ref_oid, head_ref_oid) = ref_oids;
    let labels_json: Vec<serde_json::Value> = labels.iter().map(|n| serde_json::json!({ "name": n })).collect();
    serde_json::json!({
        "state": state,
        "mergedAt": merged_at,
        "closedAt": "",
        "mergeable": mergeable,
        "mergeStateStatus": merge_state_status,
        "baseRefOid": base_ref_oid,
        "headRefOid": head_ref_oid,
        "labels": labels_json,
        "statusCheckRollup": rollup,
    })
    .to_string()
}

fn gh_api_include_body(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut out = format!("{status_line}\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    out
}

#[cfg(unix)]
fn test_exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

/// Build a CheckRun rollup leaf with the given name + verdict shape.
fn check_run(name: &str, status: &str, conclusion: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "isRequired": true,
    })
}

struct FixedPrDetector(Option<String>);

#[async_trait]
impl PrDetector for FixedPrDetector {
    async fn detect_pr(&self, _repo_remote_url: &str, _expected_branch: &str) -> Result<PrStatus> {
        Ok(match &self.0 {
            Some(url) => PrStatus::Fresh { url: url.clone() },
            None => PrStatus::None,
        })
    }
}

struct NoopPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

struct NoopProbeQueuer;

impl ProbeQueuer for NoopProbeQueuer {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
    fn clear_pending_probes(&self, _run_id: &str) {}
}

struct NoopCubeClient;

crate::stub_cube_client! { NoopCubeClient {
    async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
        Ok(())
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
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

fn make_abandoned_chore_with_workspace(db: &WorkDb, name: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, &format!("Prod-{name}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
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
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/ws/1")
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
    // Simulate orphan sweep abandoning exec_A.
    db.mark_execution_redundant(&exec.id).unwrap();
    (product.id, chore.id, exec.id)
}

/// Build a chore that is `blocked: ci_failure` with a PR and a live
/// worker execution still attached (status `running`). Mirrors the
/// issue-#898 scenario: a worker that fixed CI but is left polling.
/// Returns `(product_id, chore_id, execution_id)`.
fn make_blocked_ci_chore_with_live_worker(db: &WorkDb, name: &str, pr: &str) -> (String, String, String) {
    let (product_id, chore) = make_chore_in_review(db, name, pr);
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:foo/bar.git")
                .build(),
        )
        .unwrap();
    let (exec, _run) = db
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/ws/1")
        .unwrap();
    // Precondition: the worker is live for the task.
    assert!(
        db.get_live_execution_for_work_item(&chore, "").unwrap().is_some(),
        "setup: worker should be live before the sweep",
    );
    (product_id, chore, exec.id)
}

/// Stable string tag for a `LeafVerdict` so the `normalize_leaf` cases
/// can assert with `assert_eq!` (the enum itself isn't `PartialEq`).
/// `Fail` carries its conclusion so we can check it is preserved.
fn leaf_tag(v: &super::LeafVerdict) -> String {
    match v {
        super::LeafVerdict::InFlight => "InFlight".to_owned(),
        super::LeafVerdict::Pass => "Pass".to_owned(),
        super::LeafVerdict::Fail { conclusion } => format!("Fail:{conclusion}"),
    }
}

/// Build a minimal `RequiredCheckFailure` for the pure-helper tests
/// below; only `name` and `conclusion` are meaningful to those helpers,
/// the rest are filler.
fn failure(name: &str, conclusion: &str) -> RequiredCheckFailure {
    RequiredCheckFailure {
        name: name.to_owned(),
        conclusion: conclusion.to_owned(),
        target_url: String::new(),
        provider: CiProvider::Other,
        provider_job_id: None,
    }
}

/// Test-only helper for building a minimal `Open(clean)` probe with the
/// merge-queue and auto-merge fields under test; every other field is a
/// harmless default.
fn probe_with_queue_fields(
    in_merge_queue: bool,
    merge_queue_entry_state: Option<&str>,
    merge_queue_position: Option<i64>,
    merge_queue_enqueued_at: Option<&str>,
    auto_merge_enabled: bool,
    auto_merge_enabled_at: Option<&str>,
) -> PrLifecycleProbe {
    PrLifecycleProbe {
        url: "https://github.com/foo/bar/pull/1".to_owned(),
        state: PrLifecycleState::Open(OpenPrStatus::clean()),
        base_ref_oid: None,
        head_ref_oid: None,
        head_ref_name: None,
        base_ref_name: None,
        labels: Vec::new(),
        review: PrReviewState::Unknown,
        in_merge_queue,
        merge_queue_entry_state: merge_queue_entry_state.map(str::to_owned),
        merge_queue_position,
        merge_queue_enqueued_at: merge_queue_enqueued_at.map(str::to_owned),
        raw_mergeable: String::new(),
        raw_merge_state_status: String::new(),
        auto_merge_enabled,
        auto_merge_enabled_at: auto_merge_enabled_at.map(str::to_owned),
    }
}

/// Second (or later) chore in an existing product, moved straight to
/// `in_review` with a bound `pr_url` — mirrors `make_chore_in_review`
/// but reuses a caller-supplied product so multiple chores land in the
/// same merge queue.
fn chore_in_review_for_product(db: &WorkDb, product_id: &str, name: &str, pr_url: &str) -> String {
    let chore = create_test_chore_manual(db, product_id.to_owned(), name);
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    chore.id
}

/// Read back `merge_queue_state` / `merge_queue_detail` for a task,
/// parsing the detail JSON into a `serde_json::Value` (or `Value::Null`
/// when the column is NULL) for easy field assertions.
fn merge_queue_columns(db: &WorkDb, task_id: &str) -> (Option<String>, serde_json::Value) {
    let conn = db.connect().unwrap();
    let (state, detail): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let detail_json = detail
        .as_deref()
        .map(|s| serde_json::from_str(s).unwrap())
        .unwrap_or(serde_json::Value::Null);
    (state, detail_json)
}

fn seed_queued(db: &WorkDb, task_id: &str, position: Option<i64>, enqueued_at: &str, state: &str) {
    // Mirrors `merge_queue_detail_json`'s real shape (section_order always
    // present alongside position) so these fixtures match what a genuine
    // probe write leaves behind, rather than an artifact of the test.
    let detail = serde_json::json!({
        "position": position,
        "state": state,
        "enqueued_at": enqueued_at,
        "section_order": position.unwrap_or(QUEUED_NO_POSITION_SECTION_ORDER),
    })
    .to_string();
    db.update_task_pr_poll_state(
        task_id,
        PrPollStateInput {
            ci_required_state: "success",
            review_required_state: "approved",
            merge_queue_state: Some("queued"),
            merge_queue_detail: Some(&detail),
            ..Default::default()
        },
    )
    .unwrap();
}

mod classify_tests;
mod merge_queue_tests;
mod metrics_tests;
mod probe_tests;
mod schedule_tests;
mod sweep_tests;
