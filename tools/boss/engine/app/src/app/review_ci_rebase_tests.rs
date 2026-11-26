use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use boss_engine_merge_poller::{
    CiProvider, MergeProbe, OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleProbe, PrLifecycleState,
    PrReviewState, RequiredCheckFailure,
};
use boss_engine_runtime::merge_when_ready::{DirectMergeExecutor, DirectMergeSuccess};
use boss_engine_runtime::test_support::{create_test_chore_manual, create_test_product_with_repo};
use boss_execution_contracts::{CubeClient, CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
use boss_feature_flags::ConflictDeferralArm;
use boss_trunk_client::{CallConfig, StaticTokenProvider, TrunkClient};
use boss_work_db::WorkItemPatch;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::review::handle_merge_when_ready;
use super::super::*;

struct FakeDirectMergeExecutor {
    calls: Mutex<Vec<String>>,
    pinned_heads: Mutex<Vec<Option<String>>>,
    disarms: Mutex<Vec<String>>,
    head_sha: String,
    ignore_expected_head: bool,
}

impl FakeDirectMergeExecutor {
    fn returning(head_sha: impl Into<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            pinned_heads: Mutex::new(Vec::new()),
            disarms: Mutex::new(Vec::new()),
            head_sha: head_sha.into(),
            ignore_expected_head: false,
        }
    }

    fn ignoring_expected_head(head_sha: impl Into<String>) -> Self {
        Self {
            ignore_expected_head: true,
            ..Self::returning(head_sha)
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn pinned_heads(&self) -> Vec<Option<String>> {
        self.pinned_heads.lock().unwrap().clone()
    }

    fn disarms(&self) -> Vec<String> {
        self.disarms.lock().unwrap().clone()
    }
}

#[async_trait]
impl DirectMergeExecutor for FakeDirectMergeExecutor {
    async fn execute(&self, pr_url: &str) -> anyhow::Result<DirectMergeSuccess> {
        self.execute_matching_head(pr_url, None).await
    }

    async fn execute_matching_head(
        &self,
        pr_url: &str,
        expected_head_sha: Option<&str>,
    ) -> anyhow::Result<DirectMergeSuccess> {
        self.calls.lock().unwrap().push(pr_url.to_owned());
        self.pinned_heads
            .lock()
            .unwrap()
            .push(expected_head_sha.map(str::to_owned));
        let head_sha = if self.ignore_expected_head {
            self.head_sha.clone()
        } else {
            expected_head_sha
                .map(str::to_owned)
                .unwrap_or_else(|| self.head_sha.clone())
        };
        Ok(DirectMergeSuccess { head_sha })
    }

    async fn disarm(&self, pr_url: &str) -> anyhow::Result<()> {
        self.disarms.lock().unwrap().push(pr_url.to_owned());
        Ok(())
    }
}

struct RebaseCube {
    outcome: Mutex<RebaseOutcome>,
    rebase_calls: AtomicUsize,
    deletion_tripwire: Mutex<Vec<String>>,
    rebase_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

impl RebaseCube {
    fn clean_pushed() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(RebaseOutcome {
                clean: true,
                pushed: true,
                conflicted_files: Vec::new(),
                linearized_commits: 0,
                linearize_decline: None,
            }),
            rebase_calls: AtomicUsize::new(0),
            deletion_tripwire: Mutex::new(Vec::new()),
            rebase_gate: None,
        })
    }

    fn clean_pushed_gated() -> (Arc<Self>, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let cube = Arc::new(Self {
            outcome: Mutex::new(RebaseOutcome {
                clean: true,
                pushed: true,
                conflicted_files: Vec::new(),
                linearized_commits: 0,
                linearize_decline: None,
            }),
            rebase_calls: AtomicUsize::new(0),
            deletion_tripwire: Mutex::new(Vec::new()),
            rebase_gate: Some((entered.clone(), release.clone())),
        });
        (cube, entered, release)
    }

    fn clean_pushed_with_deletions(deleted_files: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(RebaseOutcome {
                clean: true,
                pushed: true,
                conflicted_files: Vec::new(),
                linearized_commits: 0,
                linearize_decline: None,
            }),
            rebase_calls: AtomicUsize::new(0),
            deletion_tripwire: Mutex::new(deleted_files.into_iter().map(str::to_owned).collect()),
            rebase_gate: None,
        })
    }

    fn conflicting(files: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(RebaseOutcome {
                clean: false,
                pushed: false,
                conflicted_files: files.into_iter().map(str::to_owned).collect(),
                linearized_commits: 0,
                linearize_decline: Some("conflicted_head".to_owned()),
            }),
            rebase_calls: AtomicUsize::new(0),
            deletion_tripwire: Mutex::new(Vec::new()),
            rebase_gate: None,
        })
    }

    fn rebase_calls(&self) -> usize {
        self.rebase_calls.load(Ordering::SeqCst)
    }
}

boss_execution_contracts::stub_cube_client! { RebaseCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle {
            repo_id: "repo-1".to_owned(),
        })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease {
            lease_id: "L1".to_owned(),
            workspace_id: "W1".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ci-merge-rebase-ws"),
            dirty_verified: None,
        })
    }
    async fn goto_workspace(&self, _workspace_path: &Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &Path, _pr: u64) -> anyhow::Result<RebaseOutcome> {
        self.rebase_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = &self.rebase_gate {
            entered.notify_one();
            release.notified().await;
        }
        Ok(self.outcome.lock().unwrap().clone())
    }
    async fn verify_deletion_tripwire(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _pr_number: u64,
    ) -> Vec<String> {
        self.deletion_tripwire.lock().unwrap().clone()
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
} }

struct ScriptedMergeProbe {
    probe: Mutex<PrLifecycleProbe>,
    behind: Mutex<bool>,
    probe_calls: AtomicUsize,
    /// Head SHA returned after the first `probe()` (the behind-base check).
    /// Subsequent probes used to wait for GitHub to show the rebased head.
    after_rebase_head: Mutex<Option<String>>,
}

impl ScriptedMergeProbe {
    fn new(probe: PrLifecycleProbe, behind: bool) -> Arc<Self> {
        Arc::new(Self {
            probe: Mutex::new(probe),
            behind: Mutex::new(behind),
            probe_calls: AtomicUsize::new(0),
            after_rebase_head: Mutex::new(None),
        })
    }

    fn with_after_rebase_head(self: &Arc<Self>, head: impl Into<String>) -> Arc<Self> {
        *self.after_rebase_head.lock().unwrap() = Some(head.into());
        self.clone()
    }

    fn set(&self, probe: PrLifecycleProbe, behind: bool) {
        *self.probe.lock().unwrap() = probe;
        *self.behind.lock().unwrap() = behind;
        *self.after_rebase_head.lock().unwrap() = None;
    }

    fn probe_calls(&self) -> usize {
        self.probe_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MergeProbe for ScriptedMergeProbe {
    async fn probe(&self, _pr_url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let n = self.probe_calls.fetch_add(1, Ordering::SeqCst);
        let mut probe = self.probe.lock().unwrap().clone();
        if n > 0
            && let Some(head) = self.after_rebase_head.lock().unwrap().clone()
        {
            probe.head_ref_oid = Some(head);
        }
        Ok(probe)
    }

    async fn is_pr_behind_base(&self, _pr_url: &str, _probe: &PrLifecycleProbe) -> anyhow::Result<bool> {
        Ok(*self.behind.lock().unwrap())
    }
}

fn failing_check() -> RequiredCheckFailure {
    RequiredCheckFailure {
        name: "buildkite/ci".to_owned(),
        conclusion: "FAILURE".to_owned(),
        target_url: "https://buildkite.com/org/repo/builds/1".to_owned(),
        provider: CiProvider::Buildkite,
        provider_job_id: None,
        job_key: None,
    }
}

fn failing_probe(pr_url: &str, head_sha: &str, merge_state_status: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url)
        .state(PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing {
                failures: vec![failing_check()],
            },
        }))
        .base_ref_oid("base-sha-1")
        .head_ref_oid(head_sha)
        .head_ref_name("boss/feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Approved {
            reviewers: vec!["reviewer".to_owned()],
        })
        .raw_mergeable("MERGEABLE")
        .raw_merge_state_status(merge_state_status)
        .build()
}

fn clean_probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url)
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .base_ref_oid("base-sha-2")
        .head_ref_oid(head_sha)
        .head_ref_name("boss/feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Approved {
            reviewers: vec!["reviewer".to_owned()],
        })
        .raw_mergeable("MERGEABLE")
        .raw_merge_state_status("CLEAN")
        .auto_merge_enabled(true)
        .build()
}

fn merged_probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url)
        .state(PrLifecycleState::Merged)
        .base_ref_oid("base-sha-2")
        .head_ref_oid(head_sha)
        .head_ref_name("boss/feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Approved {
            reviewers: vec!["reviewer".to_owned()],
        })
        .build()
}

fn trunk_client_for(base_url: String) -> TrunkClient {
    TrunkClient::new(
        Arc::new(StaticTokenProvider::new("test-token")),
        CallConfig::new(Duration::from_secs(5)).with_base_url(base_url),
    )
}

fn test_server(
    trunk_client: TrunkClient,
    direct_merge_executor: Arc<dyn DirectMergeExecutor>,
    cube_client: Arc<dyn CubeClient>,
    merge_probe: Arc<dyn MergeProbe>,
) -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            trunk_client: Some(trunk_client),
            direct_merge_executor: Some(direct_merge_executor),
            cube_client: Some(cube_client),
            merge_probe: Some(merge_probe),
            ..Default::default()
        },
    )
    .unwrap();
    (state, temp)
}

fn enable_conflict_deferral_ab_split(state: &ServerState) {
    state.feature_flags.set("conflict_resolution_deferral", true).unwrap();
    state
        .feature_flags
        .set("conflict_resolution_deferral_ab_split", true)
        .unwrap();
}

fn dispatch_ctx(server_state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(server_state.clone())
        .work_db(server_state.work_db.clone())
        .sink(sink.clone())
        .session_id("s1")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

fn make_session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(
        shutdown_tx,
        crate::app::frontend_metrics::test_registry(),
    ))
}

fn seed_ci_blocked_chore(db: &WorkDb, name: &str, pr_url: &str, exhausted: bool, trunk: bool) -> (String, String) {
    let product = create_test_product_with_repo(db, name, Some("git@github.com:spinyfin/mono.git"));
    if trunk {
        db.set_product_merge_mechanism(&product.id, Some("trunk_queue"))
            .unwrap();
    }
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    if exhausted {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET ci_attempt_budget = 3, ci_attempts_used = 3 WHERE id = ?1",
                [&chore.id],
            )
            .unwrap();
        db.mark_chore_blocked_ci_failure_exhausted(&chore.id, pr_url).unwrap();
    } else {
        db.mark_chore_blocked_ci_failure(&chore.id, pr_url, None).unwrap();
    }
    (product.id, chore.id)
}

fn chore_of(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(task) => task,
        other => panic!("expected chore, got {other:?}"),
    }
}

async fn wait_until<F>(label: &str, mut cond: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {label}");
}

async fn expect_rebasing_reply(sink: &SessionSink) {
    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected early MergeWhenReadyAccepted reply");
    match envelope.payload {
        FrontendEvent::MergeWhenReadyAccepted { action, .. } => {
            assert_eq!(action, "rebasing_onto_base");
        }
        other => panic!("expected MergeWhenReadyAccepted(rebasing_onto_base), got {other:?}"),
    }
}

/// (a) `blocked: ci_failure` + behind base → mechanical rebase runs, arms merge
/// (`--auto`), green CI clears `ci_failure` so the item proceeds to merge normally.
/// Exercised across both arms of the conflict-deferral A/B experiment (even PR =
/// Control, odd PR = Deferred).
#[tokio::test]
async fn ci_failure_behind_base_rebases_and_merges_when_green_in_both_arms() {
    for (pr_number, expected_arm) in [
        (400u64, ConflictDeferralArm::Control),
        (401u64, ConflictDeferralArm::Deferred),
    ] {
        let server = MockServer::start().await;
        let pr_url = format!("https://github.com/spinyfin/mono/pull/{pr_number}");
        let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
        let cube = RebaseCube::clean_pushed();
        let probe = ScriptedMergeProbe::new(failing_probe(&pr_url, "head-old", "BEHIND"), true)
            .with_after_rebase_head("head-rebased");
        let (state, _temp) = test_server(
            trunk_client_for(server.uri()),
            direct_executor.clone(),
            cube.clone(),
            probe.clone(),
        );
        enable_conflict_deferral_ab_split(&state);
        assert_eq!(state.feature_flags.conflict_deferral_arm(Some(pr_number)), expected_arm);

        let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-fail-behind", &pr_url, false, false);
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore_id.clone(),
                confirmed_revisions: vec![],
            },
        )
        .await;

        expect_rebasing_reply(&sink).await;
        wait_until("direct merge armed after rebase", || {
            !direct_executor.calls().is_empty()
                && state
                    .work_db
                    .get_ci_merge_rebase(&chore_id, &pr_url)
                    .unwrap()
                    .and_then(|r| r.rebased_head_sha)
                    .is_some()
        })
        .await;
        assert_eq!(cube.rebase_calls(), 1, "mechanical rebase rung must run once");
        assert_eq!(direct_executor.calls(), vec![pr_url.clone()]);
        assert!(state.work_db.get_ci_merge_rebase(&chore_id, &pr_url).unwrap().is_some());

        // CI on the rebased head goes green → clears `ci_failure`, card stays
        // armed in Merging.
        probe.set(clean_probe(&pr_url, "head-rebased"), false);
        boss_engine_merge_poller::run_one_pass(
            &state.work_db,
            probe.as_ref(),
            state.publisher.as_ref(),
            None,
            None,
            Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
        )
        .await;
        let task = chore_of(&state.work_db, &chore_id);
        assert_eq!(task.status, TaskStatus::InReview);
        assert_eq!(task.blocked_reason, None);
        assert!(task.merge_queue_state.is_some());
        assert!(state.work_db.get_ci_merge_rebase(&chore_id, &pr_url).unwrap().is_none());

        // GitHub auto-merge lands the PR → poller marks it done.
        probe.set(merged_probe(&pr_url, "head-rebased"), false);
        boss_engine_merge_poller::run_one_pass(
            &state.work_db,
            probe.as_ref(),
            state.publisher.as_ref(),
            None,
            None,
            Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
        )
        .await;
        assert_eq!(chore_of(&state.work_db, &chore_id).status, TaskStatus::Done);
    }
}

/// (b) `blocked: ci_failure_exhausted` + behind base → same mechanical rebase,
/// green CI clears `ci_failure_exhausted` and merges (tested on both Direct and
/// TrunkQueue products).
#[tokio::test]
async fn ci_failure_exhausted_behind_base_rebases_and_merges_when_green() {
    for trunk in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(if trunk { 1 } else { 0 })
            .mount(&server)
            .await;

        let pr_url = "https://github.com/spinyfin/mono/pull/402";
        let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased-exh"));
        let cube = RebaseCube::clean_pushed();
        // GitHub reports BLOCKED while CI is red even though the branch is
        // behind base — `is_pr_behind_base` returns true.
        let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BLOCKED"), true)
            .with_after_rebase_head("head-rebased-exh");
        let (state, _temp) = test_server(
            trunk_client_for(server.uri()),
            direct_executor.clone(),
            cube.clone(),
            probe.clone(),
        );
        enable_conflict_deferral_ab_split(&state);

        let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-exh-behind", pr_url, true, trunk);
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore_id.clone(),
                confirmed_revisions: vec![],
            },
        )
        .await;

        expect_rebasing_reply(&sink).await;
        wait_until("exhausted CI rebase armed", || {
            state
                .work_db
                .get_ci_merge_rebase(&chore_id, pr_url)
                .unwrap()
                .and_then(|r| r.rebased_head_sha)
                .is_some()
                && chore_of(&state.work_db, &chore_id).merge_queue_state.is_some()
        })
        .await;
        assert_eq!(cube.rebase_calls(), 1);
        if trunk {
            assert!(direct_executor.calls().is_empty(), "Trunk must never call gh pr merge");
            assert!(
                state
                    .work_db
                    .get_active_trunk_merge_intent(&chore_id)
                    .unwrap()
                    .is_some()
            );
        } else {
            assert_eq!(direct_executor.calls(), vec![pr_url.to_owned()]);
        }

        // Green CI on the rebased head clears `ci_failure_exhausted`.
        probe.set(clean_probe(pr_url, "head-rebased-exh"), false);
        boss_engine_merge_poller::run_one_pass(
            &state.work_db,
            probe.as_ref(),
            state.publisher.as_ref(),
            None,
            None,
            Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
        )
        .await;
        let task = chore_of(&state.work_db, &chore_id);
        assert_eq!(task.status, TaskStatus::InReview);
        assert_eq!(task.blocked_reason, None);
        assert!(task.merge_queue_state.is_some());

        // PR merges → done.
        probe.set(merged_probe(pr_url, "head-rebased-exh"), false);
        boss_engine_merge_poller::run_one_pass(
            &state.work_db,
            probe.as_ref(),
            state.publisher.as_ref(),
            None,
            None,
            Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
        )
        .await;
        assert_eq!(chore_of(&state.work_db, &chore_id).status, TaskStatus::Done);
        server.verify().await;
    }
}

/// (c) Red CI + branch already up to date with base → refused with a clear
/// message saying CI is failing on an up-to-date branch; no rebase and no merge.
#[tokio::test]
async fn ci_failure_up_to_date_branch_refuses_without_rebasing() {
    for exhausted in [false, true] {
        let server = MockServer::start().await;
        let pr_url = "https://github.com/spinyfin/mono/pull/403";
        let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
        let cube = RebaseCube::clean_pushed();
        let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BLOCKED"), false);
        let (state, _temp) = test_server(
            trunk_client_for(server.uri()),
            direct_executor.clone(),
            cube.clone(),
            probe.clone(),
        );
        let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-up-to-date", pr_url, exhausted, false);
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore_id.clone(),
                confirmed_revisions: vec![],
            },
        )
        .await;

        let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
            .await
            .unwrap()
            .expect("expected refusal");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                assert!(
                    message.contains("CI is failing on an up-to-date branch"),
                    "unexpected refusal message: {message}"
                );
            }
            other => panic!("expected WorkError refusal, got {other:?}"),
        }
        assert_eq!(cube.rebase_calls(), 0, "must not rebase an up-to-date branch");
        assert!(direct_executor.calls().is_empty(), "must not arm merge");
        let task = chore_of(&state.work_db, &chore_id);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(
            task.blocked_reason.as_deref(),
            Some(if exhausted {
                "ci_failure_exhausted"
            } else {
                "ci_failure"
            })
        );
    }
}

/// (d) Rebased head still red → returns to existing CI-failure handling with
/// visible reason `"rebased onto main; CI still failing"`, disarms auto-merge,
/// does not merge, and does not loop rebases. Also verifies stale probes for the
/// pre-rebase head are ignored until the rebased head is observed.
#[tokio::test]
async fn rebased_head_still_red_returns_to_ci_failure_handling_with_reason_and_no_loop() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/404";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-still-red", pr_url, true, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("rebase and direct arm complete", || {
        state
            .work_db
            .get_ci_merge_rebase(&chore_id, pr_url)
            .unwrap()
            .and_then(|r| r.rebased_head_sha)
            .is_some()
            && !direct_executor.calls().is_empty()
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);

    // A stale sweep still reporting the pre-rebase head must NOT prematurely
    // consume the post-rebase marker or disarm the newly armed merge.
    probe.set(failing_probe(pr_url, "head-old", "BEHIND"), true);
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;
    assert!(
        state.work_db.get_ci_merge_rebase(&chore_id, pr_url).unwrap().is_some(),
        "stale pre-rebase head probe must leave the post-rebase marker intact"
    );
    assert!(direct_executor.disarms().is_empty());

    // Now the rebased head reports failing CI.
    probe.set(failing_probe(pr_url, "head-rebased", "BLOCKED"), false);
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;

    let task = chore_of(&state.work_db, &chore_id);
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("ci_failure_exhausted"));
    assert_eq!(
        task.blocked_detail.as_deref(),
        Some("rebased onto main; CI still failing")
    );
    assert!(task.merge_queue_state.is_none(), "card must leave Merging");
    assert!(
        task.merge_queue_detail
            .as_deref()
            .unwrap_or("")
            .contains("rebased onto main; CI still failing")
    );
    assert!(
        state.work_db.get_ci_merge_rebase(&chore_id, pr_url).unwrap().is_none(),
        "post-rebase marker must be consumed (one rebase per Merge click)"
    );

    // Next sweep disarms GitHub auto-merge and does NOT trigger another rebase.
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;
    assert_eq!(direct_executor.disarms(), vec![pr_url.to_owned()]);
    assert_eq!(cube.rebase_calls(), 1, "must never loop rebases automatically");
}

/// (e) Rebase hits a genuine conflict → hands off to the existing conflict
/// ladder (spawns conflict-resolution revision, records standing merge
/// continuation, identical in both conflict-deferral arms).
#[tokio::test]
async fn rebase_conflict_hands_off_to_existing_conflict_ladder_in_both_arms() {
    for (pr_number, expected_arm) in [
        (406u64, ConflictDeferralArm::Control),
        (407u64, ConflictDeferralArm::Deferred),
    ] {
        let server = MockServer::start().await;
        let pr_url = format!("https://github.com/spinyfin/mono/pull/{pr_number}");
        let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-resolved"));
        let cube = RebaseCube::conflicting(vec!["src/lib.rs"]);
        let probe = ScriptedMergeProbe::new(failing_probe(&pr_url, "head-old", "BEHIND"), true);
        let (state, _temp) = test_server(
            trunk_client_for(server.uri()),
            direct_executor.clone(),
            cube.clone(),
            probe.clone(),
        );
        enable_conflict_deferral_ab_split(&state);
        assert_eq!(state.feature_flags.conflict_deferral_arm(Some(pr_number)), expected_arm);

        let (_product_id, chore_id) =
            seed_ci_blocked_chore(&state.work_db, "ci-conflict-handoff", &pr_url, true, false);
        let sink = make_session_sink();

        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore_id.clone(),
                confirmed_revisions: vec![],
            },
        )
        .await;

        expect_rebasing_reply(&sink).await;
        wait_until("conflict ladder handoff recorded", || {
            state
                .work_db
                .merge_conflict_resolution_requested(&chore_id, &pr_url)
                .unwrap()
        })
        .await;
        assert_eq!(cube.rebase_calls(), 1);
        assert!(
            direct_executor.calls().is_empty(),
            "must not arm merge while conflicts remain unresolved"
        );

        let attempt = state
            .work_db
            .active_conflict_resolution_for_work_item(&chore_id)
            .unwrap()
            .expect("conflict ladder attempt must be active");
        assert!(
            attempt.revision_task_id.is_some(),
            "conflict ladder must spawn a conflict-resolution revision"
        );
        assert_eq!(
            attempt.resolved_by_rung,
            Some(2),
            "single-file conflict should route through rung 2 small-agent profile"
        );
        assert!(
            state
                .work_db
                .merge_conflict_resolution_requested(&chore_id, &pr_url)
                .unwrap(),
            "Direct product must retain the standing merge request for continuation after conflict resolution"
        );
    }
}

/// (f) `InReview` merge path is unchanged: no live behind-base probe and no
/// mechanical rebase — directly arms GitHub auto-merge as before.
#[tokio::test]
async fn in_review_merge_path_is_unchanged() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/408";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-in-review"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-in-review", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let product = create_test_product_with_repo(
        &state.work_db,
        "in-review-unchanged",
        Some("git@github.com:spinyfin/mono.git"),
    );
    let chore = create_test_chore_manual(&state.work_db, product.id, "In review task");
    state
        .work_db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..Default::default()
            },
        )
        .unwrap();
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore.id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected reply");
    match envelope.payload {
        FrontendEvent::MergeWhenReadyAccepted { action, .. } => assert_eq!(action, "merge_requested"),
        other => panic!("expected MergeWhenReadyAccepted, got {other:?}"),
    }
    assert_eq!(
        probe.probe_calls(),
        0,
        "InReview merge must not probe PR before merging"
    );
    assert_eq!(cube.rebase_calls(), 0, "InReview merge must not run mechanical rebase");
    assert_eq!(direct_executor.calls(), vec![pr_url.to_owned()]);
}

struct NoopContractFetcher;

#[async_trait]
impl boss_engine_merge_poller::queue_ci_contract::ContractFetcher for NoopContractFetcher {
    async fn config(&self, _pr_url: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn step_keys(&self, _url: &str) -> anyhow::Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }
}

/// (d) Trunk: a still-red rebased head stamps `boss:withdraw` and the queue
/// poller calls `cancelPullRequest`, then retires the intent.
#[tokio::test]
async fn rebased_head_still_red_on_trunk_cancels_the_queue_entry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submitPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/getQueue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "running",
            "branch": "main",
            "enqueuedPullRequests": [{
                "id": "entry-409",
                "state": "pending",
                "prNumber": 409
            }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/cancelPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let pr_url = "https://github.com/spinyfin/mono/pull/409";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-still-red-trunk", pr_url, true, true);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("trunk queue submit after rebase", || {
        state
            .work_db
            .get_ci_merge_rebase(&chore_id, pr_url)
            .unwrap()
            .and_then(|r| r.rebased_head_sha)
            .is_some()
            && chore_of(&state.work_db, &chore_id).merge_queue_state.is_some()
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);
    assert!(direct_executor.calls().is_empty(), "Trunk must never call gh pr merge");

    probe.set(failing_probe(pr_url, "head-rebased", "BLOCKED"), false);
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;

    let intent = state
        .work_db
        .get_active_trunk_merge_intent(&chore_id)
        .unwrap()
        .expect("intent stays active until Trunk confirms cancel");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_WITHDRAW)
    );
    let task = chore_of(&state.work_db, &chore_id);
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.blocked_detail.as_deref(),
        Some("rebased onto main; CI still failing")
    );

    let mut trunk_probe = boss_engine_merge_poller::trunk_queue_poller::TrunkQueueProbe::new();
    let outcome = trunk_probe
        .run_pass(
            &boss_engine_merge_poller::trunk_queue_poller::TrunkSweepContext {
                contract_fetcher: &NoopContractFetcher,
                work_db: &state.work_db,
                publisher: state.publisher.as_ref(),
                api: &state.trunk_client,
            },
            Instant::now(),
        )
        .await;
    assert_eq!(outcome.queue_cancellations, 1);
    assert_eq!(outcome.intents_retired, 1);
    assert!(
        state
            .work_db
            .get_active_trunk_merge_intent(&chore_id)
            .unwrap()
            .is_none(),
        "intent must retire only after Trunk confirms cancel"
    );
    assert_eq!(cube.rebase_calls(), 1, "must never loop rebases automatically");
    server.verify().await;
}

/// (e) Trunk: a conflicting rebase records a superseded-by-conflict intent so
/// the existing cancel/resubmit path submits after the conflict fix lands.
#[tokio::test]
async fn rebase_conflict_on_trunk_records_superseded_intent_for_resubmit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submitPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;

    let pr_url = "https://github.com/spinyfin/mono/pull/410";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-resolved"));
    let cube = RebaseCube::conflicting(vec!["src/lib.rs"]);
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-conflict-trunk", pr_url, true, true);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    expect_rebasing_reply(&sink).await;
    wait_until("trunk conflict intent recorded", || {
        state
            .work_db
            .get_active_trunk_merge_intent(&chore_id)
            .unwrap()
            .is_some()
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);
    assert!(direct_executor.calls().is_empty());

    let intent = state
        .work_db
        .get_active_trunk_merge_intent(&chore_id)
        .unwrap()
        .expect("Trunk conflict handoff must record an active intent");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
    );
    server.verify().await;
}

/// Merge on a behind CI-failed card refuses when mechanical rebase is off.
#[tokio::test]
async fn mechanical_rebase_flag_off_refuses_behind_ci_failed_merge() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/411";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    state
        .feature_flags
        .set("conflict_ladder_mechanical_rebase", false)
        .unwrap();
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-flag-off", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected refusal");
    match envelope.payload {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("mechanical rebase is disabled; rebase manually"),
                "unexpected refusal message: {message}"
            );
        }
        other => panic!("expected WorkError refusal, got {other:?}"),
    }
    assert_eq!(cube.rebase_calls(), 0);
    assert!(direct_executor.calls().is_empty());
}

/// GitHub still reporting the pre-rebase head after a successful rebase is
/// an error: the one-shot marker must not be recorded against the old SHA.
#[tokio::test]
async fn lagged_github_head_after_rebase_refuses_without_arming() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/412";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-lagged-head", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    expect_rebasing_reply(&sink).await;
    wait_until("lagged head refusal surfaced in blocked_detail", || {
        chore_of(&state.work_db, &chore_id)
            .blocked_detail
            .as_deref()
            .is_some_and(|d| d.contains("GitHub still reports the pre-rebase head"))
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);
    assert!(direct_executor.calls().is_empty(), "must not arm merge on a stale head");
    // The push succeeded, so the rebase record is persisted with
    // `rebased_head_sha = None` so a retry can arm once GitHub catches up.
    let rebase = state
        .work_db
        .get_ci_merge_rebase(&chore_id, pr_url)
        .unwrap()
        .expect("post-push rebase record must persist for retry");
    assert_eq!(rebase.head_sha_before.as_deref(), Some("head-old"));
    assert_eq!(rebase.rebased_head_sha, None);
}

/// Even after GitHub shows a new head, a Direct merge executor that ignores
/// the pinned head and still returns the pre-rebase SHA must refuse AND record
/// a durable `disarm_pending` obligation so GitHub auto-merge is not left
/// armed and untracked.
#[tokio::test]
async fn direct_merge_executor_returning_pre_rebase_head_is_refused() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/413";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::ignoring_expected_head("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-stale-executor", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    expect_rebasing_reply(&sink).await;
    wait_until("stale executor refusal surfaced in blocked_detail", || {
        chore_of(&state.work_db, &chore_id)
            .blocked_detail
            .as_deref()
            .is_some_and(|d| d.contains("GitHub still reports the pre-rebase head"))
    })
    .await;
    assert_eq!(direct_executor.calls(), vec![pr_url.to_owned()]);
    assert_eq!(direct_executor.pinned_heads(), vec![Some("head-rebased".to_owned())]);
    assert!(
        state.work_db.merge_disarm_pending(&chore_id, pr_url).unwrap(),
        "rejecting an already-accepted external merge must record a durable disarm obligation"
    );
    assert!(
        state
            .work_db
            .get_active_github_merge_intent(&chore_id, pr_url)
            .unwrap()
            .is_none()
    );
    let rebase = state
        .work_db
        .get_ci_merge_rebase(&chore_id, pr_url)
        .unwrap()
        .expect("post-push rebase record must stay intact for retry");
    assert_eq!(rebase.rebased_head_sha, None);
}

/// Trunk conflict handoff when a prior eviction left the active intent in
/// `'failed'` must stamp it `boss:superseded_by_conflict` so conflict
/// retirement advances it to `boss:awaiting_resubmit` and the poller
/// resubmits it to Trunk.
#[tokio::test]
async fn rebase_conflict_on_trunk_with_failed_intent_stamps_superseded_and_resubmits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/getQueue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "running",
            "branch": "main",
            "enqueuedPullRequests": [],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/submitPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let pr_url = "https://github.com/spinyfin/mono/pull/414";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-resolved"));
    let cube = RebaseCube::conflicting(vec!["src/lib.rs"]);
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-conflict-trunk-failed", pr_url, true, true);
    let existing = state
        .work_db
        .insert_trunk_merge_intent(
            boss_work_db::TrunkMergeIntentInsertInput::builder()
                .work_item_id(&chore_id)
                .pr_url(pr_url)
                .pr_number(414)
                .repo("spinyfin/mono")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .expect("seeded intent");
    state
        .work_db
        .record_trunk_merge_intent_state(&existing.id, "failed")
        .unwrap();
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    expect_rebasing_reply(&sink).await;
    wait_until("failed trunk intent stamped superseded_by_conflict", || {
        state
            .work_db
            .get_active_trunk_merge_intent(&chore_id)
            .unwrap()
            .and_then(|i| i.last_trunk_state)
            .as_deref()
            == Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
    })
    .await;

    let intent = state
        .work_db
        .get_active_trunk_merge_intent(&chore_id)
        .unwrap()
        .expect("active Trunk intent must survive");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
        "failed active intent must be stamped superseded_by_conflict on conflict handoff"
    );

    // Simulate conflict resolution retiring the merge_conflict signal (what
    // `conflict_watch::on_resolved` runs once GitHub reports the PR mergeable):
    // the intent advances to `boss:awaiting_resubmit` and the poller resubmits.
    let retired = state
        .work_db
        .clear_resolved_pr_signal(&chore_id, pr_url, false)
        .unwrap();
    assert!(
        matches!(retired, boss_work_db::ClearResolvedSignal::Applied(_)),
        "expected conflict signal retirement to apply, got {retired:?}"
    );
    let after_retire = state
        .work_db
        .get_active_trunk_merge_intent(&chore_id)
        .unwrap()
        .expect("intent still active");
    assert_eq!(
        after_retire.last_trunk_state.as_deref(),
        Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT),
        "conflict retirement must advance superseded_by_conflict to awaiting_resubmit"
    );

    let mut trunk_probe = boss_engine_merge_poller::trunk_queue_poller::TrunkQueueProbe::new();
    let outcome = trunk_probe
        .run_pass(
            &boss_engine_merge_poller::trunk_queue_poller::TrunkSweepContext {
                contract_fetcher: &NoopContractFetcher,
                work_db: &state.work_db,
                publisher: state.publisher.as_ref(),
                api: &state.trunk_client,
            },
            Instant::now(),
        )
        .await;
    assert_eq!(outcome.resubmits, 1);
    server.verify().await;
}

/// When the head wait sees the verified rebased SHA, the Direct merge
/// executor is pinned to that SHA rather than re-probing (even if an unpinned
/// read would still return the pre-rebase SHA).
#[tokio::test]
async fn direct_merge_pins_verified_rebased_head_without_reprobing_stale_sha() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/415";
    // Fallback unpinned head is still the stale pre-rebase SHA; pinning must
    // supply the verified `"head-rebased"` SHA instead.
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-pinned-head", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    expect_rebasing_reply(&sink).await;
    wait_until("pinned direct merge intent recorded", || {
        state
            .work_db
            .get_active_github_merge_intent(&chore_id, pr_url)
            .unwrap()
            .is_some()
    })
    .await;
    assert_eq!(direct_executor.pinned_heads(), vec![Some("head-rebased".to_owned())]);
    let intent = state
        .work_db
        .get_active_github_merge_intent(&chore_id, pr_url)
        .unwrap()
        .expect("merge intent recorded");
    assert_eq!(intent.head_sha, "head-rebased");
}

/// When the first click times out waiting for GitHub to surface the
/// rebased head, a second click once GitHub shows the new head and not-behind
/// skips the rebase and arms the merge directly.
#[tokio::test]
async fn retry_after_head_wait_timeout_arms_rebased_pr_without_rebasing_again() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/416";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-retry-after-lag", pr_url, false, false);
    let sink = make_session_sink();

    // First click: rebase pushes, head wait times out while GitHub lags.
    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("head-wait timeout surfaced in blocked_detail", || {
        chore_of(&state.work_db, &chore_id)
            .blocked_detail
            .as_deref()
            .is_some_and(|d| d.contains("GitHub still reports the pre-rebase head"))
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);
    assert!(direct_executor.calls().is_empty());

    // GitHub catches up: branch is no longer behind and reports the rebased head.
    probe.set(failing_probe(pr_url, "head-rebased", "CLEAN"), false);

    // Second click: must skip rebase and arm Direct merge pinned to the new head.
    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    let second = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected accepted reply");
    assert!(
        matches!(second.payload, FrontendEvent::MergeWhenReadyAccepted { .. }),
        "retry after head-wait timeout must arm the merge, got {:?}",
        second.payload
    );
    assert_eq!(cube.rebase_calls(), 1, "retry must not rebase a second time");
    assert_eq!(direct_executor.pinned_heads(), vec![Some("head-rebased".to_owned())]);
    let intent = state
        .work_db
        .get_active_github_merge_intent(&chore_id, pr_url)
        .unwrap()
        .expect("merge intent recorded on retry");
    assert_eq!(intent.head_sha, "head-rebased");
    let rebase = state
        .work_db
        .get_ci_merge_rebase(&chore_id, pr_url)
        .unwrap()
        .expect("rebase marker updated");
    assert_eq!(rebase.rebased_head_sha.as_deref(), Some("head-rebased"));
}

/// A Merge click must not retire an active `boss:withdraw` intent while its
/// `cancelPullRequest` is still owed by the poller, and must refuse before
/// force-pushing a rebase that could never be submitted.
#[tokio::test]
async fn trunk_merge_refuses_before_rebasing_while_withdraw_cancel_is_pending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submitPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;

    let pr_url = "https://github.com/spinyfin/mono/pull/417";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) =
        seed_ci_blocked_chore(&state.work_db, "ci-trunk-withdraw-pending", pr_url, false, true);
    let withdraw_intent = state
        .work_db
        .insert_trunk_merge_intent(
            boss_work_db::TrunkMergeIntentInsertInput::builder()
                .work_item_id(&chore_id)
                .pr_url(pr_url)
                .pr_number(417)
                .repo("spinyfin/mono")
                .target_branch("main")
                .initial_trunk_state(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_WITHDRAW)
                .build(),
        )
        .unwrap()
        .expect("seeded withdraw intent");
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected refusal");
    match envelope.payload {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("a previous queue withdrawal is still pending"),
                "unexpected refusal message: {message}"
            );
        }
        other => panic!("expected WorkError refusal, got {other:?}"),
    }
    assert_eq!(
        cube.rebase_calls(),
        0,
        "refusal must happen before the rebase is pushed"
    );
    let still_active = state
        .work_db
        .get_active_trunk_merge_intent(&chore_id)
        .unwrap()
        .expect("boss:withdraw intent must remain active until poller cancels it");
    assert_eq!(still_active.id, withdraw_intent.id);
    assert_eq!(
        still_active.last_trunk_state.as_deref(),
        Some(boss_engine_merge_poller::trunk_merge::TRUNK_INTENT_WITHDRAW)
    );
    server.verify().await;
}

fn move_card_to_in_review(db: &WorkDb, work_item_id: &str) {
    db.update_work_item(
        work_item_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
    )
    .unwrap();
}

/// After a first click pushes the rebase but times out waiting for the new
/// head, an in-flight CI poll can return the card to review. The retry must
/// still arm the rebased head: on Trunk, with the old intent left `failed`,
/// that means retiring it and submitting exactly once, without rebasing again.
#[tokio::test]
async fn trunk_retry_after_head_wait_timeout_submits_once_after_card_returns_to_review() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submitPullRequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let pr_url = "https://github.com/spinyfin/mono/pull/418";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) =
        seed_ci_blocked_chore(&state.work_db, "ci-trunk-retry-in-review", pr_url, false, true);
    let failed_intent = state
        .work_db
        .insert_trunk_merge_intent(
            boss_work_db::TrunkMergeIntentInsertInput::builder()
                .work_item_id(&chore_id)
                .pr_url(pr_url)
                .pr_number(418)
                .repo("spinyfin/mono")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .expect("seeded intent");
    state
        .work_db
        .record_trunk_merge_intent_state(&failed_intent.id, "failed")
        .unwrap();
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("trunk head-wait timeout surfaced in blocked_detail", || {
        chore_of(&state.work_db, &chore_id)
            .blocked_detail
            .as_deref()
            .is_some_and(|d| d.contains("GitHub still reports the pre-rebase head"))
    })
    .await;
    assert_eq!(cube.rebase_calls(), 1);

    // CI goes in flight on the rebased head: the card returns to review.
    move_card_to_in_review(&state.work_db, &chore_id);
    probe.set(failing_probe(pr_url, "head-rebased", "CLEAN"), false);

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    let second = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected accepted reply");
    assert!(
        matches!(second.payload, FrontendEvent::MergeWhenReadyAccepted { .. }),
        "retry from review must submit the rebased PR, got {:?}",
        second.payload
    );
    assert_eq!(cube.rebase_calls(), 1, "retry must not rebase a second time");
    let rebase = state
        .work_db
        .get_ci_merge_rebase(&chore_id, pr_url)
        .unwrap()
        .expect("rebase marker updated");
    assert_eq!(rebase.rebased_head_sha.as_deref(), Some("head-rebased"));
    server.verify().await;
}

/// Same retry when CI on the rebased head went green before the click: the
/// card is back in review and the Direct merge is still armed on the rebased
/// head without a second rebase.
#[tokio::test]
async fn direct_retry_after_head_wait_timeout_arms_when_card_returned_to_review() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/419";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-old"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) =
        seed_ci_blocked_chore(&state.work_db, "ci-direct-retry-in-review", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("direct head-wait timeout surfaced in blocked_detail", || {
        chore_of(&state.work_db, &chore_id)
            .blocked_detail
            .as_deref()
            .is_some_and(|d| d.contains("GitHub still reports the pre-rebase head"))
    })
    .await;

    // CI on the rebased head is green: the card is back in review.
    move_card_to_in_review(&state.work_db, &chore_id);
    probe.set(failing_probe(pr_url, "head-rebased", "CLEAN"), false);

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    let second = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected accepted reply");
    assert!(matches!(second.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));
    assert_eq!(cube.rebase_calls(), 1);
    assert_eq!(direct_executor.pinned_heads(), vec![Some("head-rebased".to_owned())]);
}

struct CapturingPublisher {
    events: Mutex<Vec<FrontendEvent>>,
}

impl CapturingPublisher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn events(&self) -> Vec<FrontendEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl boss_execution_contracts::ExecutionPublisher for CapturingPublisher {
    async fn publish(&self, _product_id: &str, _work_item_id: &str, _execution_id: &str, _event_type: &str) {}

    async fn publish_frontend_event_on_product(&self, _product_id: &str, event: FrontendEvent) {
        self.events.lock().unwrap().push(event);
    }

    async fn publish_work_item_changed(&self, _product_id: &str, _work_item_id: &str, _reason: &str) {}
}

/// C1: A clean CI-merge rebase must NOT emit `ConflictResolutionSucceeded`,
/// must NOT appear in conflict remediation lists or `open_remediation_state`,
/// must NOT increment conflict-ladder telemetry counters, and must NOT count
/// toward the per-work-item conflict churn guard.
#[tokio::test]
async fn clean_ci_merge_rebase_does_not_emit_conflict_cleared_or_consume_churn_guard() {
    let temp = tempfile::tempdir().unwrap();
    let db = WorkDb::open(temp.path().join("state.db")).unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/420";
    let (product_id, chore_id) = seed_ci_blocked_chore(&db, "ci-rebase-no-conflict-signal", pr_url, false, false);
    let publisher = CapturingPublisher::new();
    let cube = RebaseCube::clean_pushed();
    let pr_checker = boss_work_db::StaticPrStateChecker(boss_work_db::PrOpenState::Open);
    let candidate = boss_work_db::PendingMergeCheck {
        work_item_id: chore_id.clone(),
        product_id: product_id.clone(),
        pr_url: pr_url.to_owned(),
    };

    // Run `CHURN_GUARD_THRESHOLD` (3) clean CI-merge rebases on distinct base
    // SHAs on the same work item.
    for i in 0..boss_work_db::CHURN_GUARD_THRESHOLD {
        let mut probe = failing_probe(pr_url, &format!("head-old-{i}"), "BEHIND");
        probe.base_ref_oid = Some(format!("base-sha-{i}"));
        let outcome = boss_engine_runtime::conflict_watch::rebase_for_ci_failure_merge(
            &db,
            publisher.as_ref(),
            cube.as_ref(),
            &pr_checker,
            &candidate,
            &probe,
            "ci_failure",
        )
        .await;
        assert_eq!(
            outcome,
            boss_engine_runtime::conflict_watch::CiMergeRebaseOutcome::RebasedAndPushed
        );
    }

    // 1. No ConflictResolutionSucceeded event emitted.
    let conflict_succeeded = publisher
        .events()
        .into_iter()
        .filter(|e| matches!(e, FrontendEvent::ConflictResolutionSucceeded { .. }))
        .count();
    assert_eq!(
        conflict_succeeded, 0,
        "clean CI-merge rebase must not publish ConflictResolutionSucceeded"
    );

    // 2. Excluded from remediation-list projections and conflict ledger list.
    assert!(
        db.open_remediation_state()
            .unwrap()
            .into_iter()
            .all(|row| row.kind != "conflict" || row.pr_url != pr_url),
        "open_remediation_state must not project ci_merge_rebase as a conflict"
    );
    assert!(
        db.list_conflict_resolutions(None, &[], Some(&chore_id), None)
            .unwrap()
            .is_empty(),
        "list_conflict_resolutions must exclude ci_merge_rebase rows"
    );
    assert!(
        db.list_engine_attempts(&["conflict".into()], None, &[], Some(&chore_id), None)
            .unwrap()
            .is_empty(),
        "list_engine_attempts(conflict) must exclude ci_merge_rebase rows"
    );

    // 3. Conflict-ladder outcome telemetry counter was not incremented.
    let ladder_counter = db
        .metrics_registry()
        .counter_snapshots()
        .into_iter()
        .find(|m| m.name == "conflict_ladder.engine_rebase.retired");
    assert!(
        ladder_counter.is_none(),
        "clean CI-merge rebase must not increment conflict_ladder.engine_rebase.retired, got {ladder_counter:?}"
    );

    // 4. Genuine conflict resolution on the same work item is NOT tripped by
    // the churn guard despite CHURN_GUARD_THRESHOLD prior CI-merge rebases.
    let genuine = db
        .insert_conflict_resolution(boss_work_db::ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 420,
            head_branch: "boss/feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("base-sha-genuine".to_owned()),
            head_sha_before: Some("head-genuine".to_owned()),
        })
        .unwrap()
        .expect("genuine conflict resolution row inserted");
    assert_eq!(
        genuine.status, "pending",
        "CI-merge rebases must not consume the conflict churn-guard budget"
    );
    assert_ne!(
        genuine.failure_reason.as_deref(),
        Some(boss_work_db::CHURN_GUARD_REASON)
    );
}

/// C2: The MergeWhenReady reply (`rebasing_onto_base`) arrives immediately
/// once the branch is confirmed behind base — BEFORE the slow rebase finishes —
/// and publishes card-visible progress state (`blocked_detail`). A duplicate
/// click while the rebase is still running is refused immediately.
#[tokio::test]
async fn merge_click_replies_rebasing_onto_base_before_slow_rebase_completes() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/421";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let (cube, entered, release) = RebaseCube::clean_pushed_gated();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-slow-rebase", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;

    // Reply arrives while `rebase_workspace` is still blocked on `release`.
    expect_rebasing_reply(&sink).await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("rebase_workspace should have started");
    assert!(
        direct_executor.calls().is_empty(),
        "merge must not have armed before rebase finished"
    );
    assert_eq!(
        chore_of(&state.work_db, &chore_id).blocked_detail.as_deref(),
        Some("Rebasing onto main before merging..."),
        "card must show progress state while rebase is in flight"
    );

    // A duplicate Merge click while the rebase is still running is refused
    // immediately rather than sending a second acceptance.
    let dup_sink = make_session_sink();
    handle_merge_when_ready(
        dispatch_ctx(&state, &dup_sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    let dup = tokio::time::timeout(Duration::from_secs(5), dup_sink.next())
        .await
        .unwrap()
        .expect("expected duplicate click refusal");
    assert!(
        matches!(&dup.payload, FrontendEvent::WorkError { message } if message.contains("already active")),
        "expected immediate refusal for in-flight rebase, got {:?}",
        dup.payload
    );

    // Release the rebase and verify arming completes via card state without
    // sending a second reply on the original request.
    release.notify_one();
    wait_until("direct merge armed after slow rebase completes", || {
        state
            .work_db
            .get_active_github_merge_intent(&chore_id, pr_url)
            .unwrap()
            .is_some()
    })
    .await;
    assert_eq!(direct_executor.calls(), vec![pr_url.to_owned()]);
    assert_eq!(
        chore_of(&state.work_db, &chore_id).blocked_detail.as_deref(),
        Some("Rebased onto main; merge armed, waiting for CI")
    );
}

/// C5(1): Under-budget `ci_failure` variant of test (d): when CI on the
/// rebased head is still failing and budget remains, a CI-fix revision is
/// spawned (returning the parent to `in_review`), `merge_queue_detail` still
/// carries `"rebased onto main; CI still failing"`, and auto-merge is disarmed.
#[tokio::test]
async fn rebased_head_still_red_with_budget_remaining_spawns_revision_and_keeps_reason() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/422";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-red-under-budget", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("under-budget rebase armed", || {
        state
            .work_db
            .get_ci_merge_rebase(&chore_id, pr_url)
            .unwrap()
            .and_then(|r| r.rebased_head_sha)
            .is_some()
            && !direct_executor.calls().is_empty()
    })
    .await;

    // Rebased head reports failing CI with budget remaining.
    probe.set(failing_probe(pr_url, "head-rebased", "BLOCKED"), false);
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;

    // A CI-fix revision was spawned and the parent returned to `in_review`.
    let ci_attempts = state
        .work_db
        .list_ci_remediations(None, &[], Some(&chore_id), None)
        .unwrap();
    assert!(
        ci_attempts.iter().any(|a| a.revision_task_id.is_some()),
        "under-budget red rebased head must spawn a CI-fix revision, got {ci_attempts:?}"
    );
    let task = chore_of(&state.work_db, &chore_id);
    assert_eq!(task.status, TaskStatus::InReview);
    assert!(task.merge_queue_state.is_none(), "card must leave Merging");
    assert!(
        task.merge_queue_detail
            .as_deref()
            .unwrap_or("")
            .contains("rebased onto main; CI still failing"),
        "paused merge_queue_detail must retain the post-rebase reason even after unblock_for_revision clears blocked_detail, got {:?}",
        task.merge_queue_detail
    );
    assert!(
        state.work_db.get_ci_merge_rebase(&chore_id, pr_url).unwrap().is_none(),
        "post-rebase marker must be consumed"
    );

    // Next sweep disarms GitHub auto-merge.
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;
    assert_eq!(direct_executor.disarms(), vec![pr_url.to_owned()]);
}

/// C5(2): When the PR carries the `boss/no-auto-rebase` opt-out label, a red
/// rebased head still consumes the `ci_merge_rebases` row, disarms auto-merge,
/// stamps the reason, and keeps the card blocked without spawning a revision.
#[tokio::test]
async fn rebased_head_still_red_with_opt_out_label_consumes_marker_disarms_and_stays_blocked() {
    let server = MockServer::start().await;
    let pr_url = "https://github.com/spinyfin/mono/pull/423";
    let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
    let cube = RebaseCube::clean_pushed();
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true)
        .with_after_rebase_head("head-rebased");
    let (state, _temp) = test_server(
        trunk_client_for(server.uri()),
        direct_executor.clone(),
        cube.clone(),
        probe.clone(),
    );
    let (_product_id, chore_id) = seed_ci_blocked_chore(&state.work_db, "ci-red-opt-out", pr_url, false, false);
    let sink = make_session_sink();

    handle_merge_when_ready(
        dispatch_ctx(&state, &sink),
        FrontendRequest::MergeWhenReady {
            work_item_id: chore_id.clone(),
            confirmed_revisions: vec![],
        },
    )
    .await;
    expect_rebasing_reply(&sink).await;
    wait_until("opt-out rebase armed", || {
        state
            .work_db
            .get_ci_merge_rebase(&chore_id, pr_url)
            .unwrap()
            .and_then(|r| r.rebased_head_sha)
            .is_some()
            && !direct_executor.calls().is_empty()
    })
    .await;

    // Rebased head reports failing CI with the opt-out label present.
    let mut opt_out_probe = failing_probe(pr_url, "head-rebased", "BLOCKED");
    opt_out_probe.labels = vec![boss_engine_merge_poller::OPT_OUT_LABEL.to_owned()];
    probe.set(opt_out_probe, false);
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;

    assert!(
        state.work_db.get_ci_merge_rebase(&chore_id, pr_url).unwrap().is_none(),
        "maintenance_disabled branch must still consume the ci_merge_rebases marker"
    );
    let task = chore_of(&state.work_db, &chore_id);
    assert_eq!(task.status, TaskStatus::Blocked, "card must stay blocked");
    assert_eq!(task.blocked_reason.as_deref(), Some("ci_failure"));
    assert_eq!(
        task.blocked_detail.as_deref(),
        Some("rebased onto main; CI still failing")
    );
    assert!(
        state
            .work_db
            .list_ci_remediations(None, &[], Some(&chore_id), None)
            .unwrap()
            .is_empty(),
        "maintenance_disabled must not spawn a CI remediation"
    );

    // Next sweep disarms GitHub auto-merge.
    boss_engine_merge_poller::run_one_pass(
        &state.work_db,
        probe.as_ref(),
        state.publisher.as_ref(),
        None,
        None,
        Some(state.sweep_host.as_ref() as &dyn boss_engine_merge_poller::SweepHost),
    )
    .await;
    assert_eq!(direct_executor.disarms(), vec![pr_url.to_owned()]);
}

/// C5(3): Table-driven coverage for `DeferredForLiveWriter`,
/// `HaltedForSignoff`, and `LockfileRegenerationFailed` outcomes on a
/// CI-blocked Merge click, verifying the resulting card status,
/// `blocked_reason`, and `blocked_detail`.
#[tokio::test]
async fn ci_merge_rebase_refusal_outcomes_leave_expected_card_state() {
    #[derive(Clone, Copy)]
    enum RefusalCase {
        DeferredForLiveWriter,
        HaltedForSignoff,
        LockfileRegenerationFailed,
    }

    let cases = [
        (
            RefusalCase::DeferredForLiveWriter,
            424u64,
            "ci_failure",
            Some("a worker is currently active on this PR's branch"),
        ),
        (RefusalCase::HaltedForSignoff, 425u64, "deletion_signoff", None),
        (RefusalCase::LockfileRegenerationFailed, 426u64, "merge_conflict", None),
    ];

    for (case, pr_num, expected_blocked_reason, expected_detail_substr) in cases {
        let server = MockServer::start().await;
        let pr_url = format!("https://github.com/spinyfin/mono/pull/{pr_num}");
        let direct_executor = Arc::new(FakeDirectMergeExecutor::returning("head-rebased"));
        let cube: Arc<RebaseCube> = match case {
            RefusalCase::DeferredForLiveWriter => RebaseCube::clean_pushed(),
            RefusalCase::HaltedForSignoff => RebaseCube::clean_pushed_with_deletions(vec!["src/deleted.rs"]),
            RefusalCase::LockfileRegenerationFailed => RebaseCube::conflicting(vec!["Cargo.lock"]),
        };
        let probe = ScriptedMergeProbe::new(failing_probe(&pr_url, "head-old", "BEHIND"), true)
            .with_after_rebase_head("head-rebased");
        let (state, _temp) = test_server(
            trunk_client_for(server.uri()),
            direct_executor.clone(),
            cube.clone(),
            probe.clone(),
        );
        let (product_id, chore_id) =
            seed_ci_blocked_chore(&state.work_db, &format!("ci-refusal-{pr_num}"), &pr_url, false, false);

        if matches!(case, RefusalCase::DeferredForLiveWriter) {
            let _ = &product_id;
            state
                .work_db
                .create_execution(
                    boss_protocol::CreateExecutionInput::builder()
                        .work_item_id(&chore_id)
                        .kind(boss_protocol::ExecutionKind::TaskImplementation)
                        .status(boss_protocol::ExecutionStatus::Running)
                        .build(),
                )
                .unwrap();
        }

        let sink = make_session_sink();
        handle_merge_when_ready(
            dispatch_ctx(&state, &sink),
            FrontendRequest::MergeWhenReady {
                work_item_id: chore_id.clone(),
                confirmed_revisions: vec![],
            },
        )
        .await;
        expect_rebasing_reply(&sink).await;

        wait_until("refusal outcome settled on card", || {
            let task = chore_of(&state.work_db, &chore_id);
            match expected_detail_substr {
                Some(substr) => task.blocked_detail.as_deref().is_some_and(|d| d.contains(substr)),
                None => task.blocked_reason.as_deref() == Some(expected_blocked_reason),
            }
        })
        .await;

        let task = chore_of(&state.work_db, &chore_id);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.blocked_reason.as_deref(), Some(expected_blocked_reason));
        if let Some(substr) = expected_detail_substr {
            assert!(
                task.blocked_detail.as_deref().unwrap_or("").contains(substr),
                "expected blocked_detail containing {substr:?}, got {:?}",
                task.blocked_detail
            );
        }
        assert!(direct_executor.calls().is_empty(), "refusal outcome must not arm merge");
    }
}
