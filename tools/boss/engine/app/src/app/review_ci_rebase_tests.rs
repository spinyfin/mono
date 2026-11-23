use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    disarms: Mutex<Vec<String>>,
    head_sha: String,
}

impl FakeDirectMergeExecutor {
    fn returning(head_sha: impl Into<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            disarms: Mutex::new(Vec::new()),
            head_sha: head_sha.into(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn disarms(&self) -> Vec<String> {
        self.disarms.lock().unwrap().clone()
    }
}

#[async_trait]
impl DirectMergeExecutor for FakeDirectMergeExecutor {
    async fn execute(&self, pr_url: &str) -> anyhow::Result<DirectMergeSuccess> {
        self.calls.lock().unwrap().push(pr_url.to_owned());
        Ok(DirectMergeSuccess {
            head_sha: self.head_sha.clone(),
        })
    }

    async fn disarm(&self, pr_url: &str) -> anyhow::Result<()> {
        self.disarms.lock().unwrap().push(pr_url.to_owned());
        Ok(())
    }
}

struct RebaseCube {
    outcome: Mutex<RebaseOutcome>,
    rebase_calls: AtomicUsize,
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
        Ok(self.outcome.lock().unwrap().clone())
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
} }

struct ScriptedMergeProbe {
    probe: Mutex<PrLifecycleProbe>,
    behind: Mutex<bool>,
    probe_calls: AtomicUsize,
}

impl ScriptedMergeProbe {
    fn new(probe: PrLifecycleProbe, behind: bool) -> Arc<Self> {
        Arc::new(Self {
            probe: Mutex::new(probe),
            behind: Mutex::new(behind),
            probe_calls: AtomicUsize::new(0),
        })
    }

    fn set(&self, probe: PrLifecycleProbe, behind: bool) {
        *self.probe.lock().unwrap() = probe;
        *self.behind.lock().unwrap() = behind;
    }

    fn probe_calls(&self) -> usize {
        self.probe_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MergeProbe for ScriptedMergeProbe {
    async fn probe(&self, _pr_url: &str) -> anyhow::Result<PrLifecycleProbe> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.probe.lock().unwrap().clone())
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
        let probe = ScriptedMergeProbe::new(failing_probe(&pr_url, "head-old", "BEHIND"), true);
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

        let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
            .await
            .unwrap()
            .expect("expected reply");
        match envelope.payload {
            FrontendEvent::MergeWhenReadyAccepted { action, .. } => assert_eq!(action, "merge_requested"),
            other => panic!("expected MergeWhenReadyAccepted, got {other:?}"),
        }
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
        let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BLOCKED"), true);
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

        let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
            .await
            .unwrap()
            .expect("expected reply");
        match envelope.payload {
            FrontendEvent::MergeWhenReadyAccepted { action, .. } => {
                assert_eq!(action, if trunk { "trunk_enqueued" } else { "merge_requested" });
            }
            other => panic!("expected MergeWhenReadyAccepted, got {other:?}"),
        }
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
    let probe = ScriptedMergeProbe::new(failing_probe(pr_url, "head-old", "BEHIND"), true);
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
    let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
        .await
        .unwrap()
        .expect("expected reply");
    assert!(matches!(envelope.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));
    assert_eq!(cube.rebase_calls(), 1);

    // A stale sweep still reporting the pre-rebase head must NOT prematurely
    // consume the post-rebase marker or disarm the newly armed merge.
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

        let envelope = tokio::time::timeout(Duration::from_secs(5), sink.next())
            .await
            .unwrap()
            .expect("expected reply");
        assert!(matches!(envelope.payload, FrontendEvent::MergeWhenReadyAccepted { .. }));
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
