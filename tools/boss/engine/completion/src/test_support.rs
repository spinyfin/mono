//! Fixtures for tests of the completion service, shared with the engine's own
//! test suite through the `test-support` feature.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use boss_pr_review_sources::SourcePacket;
use boss_protocol::WorkComment;

use crate::review_guide_capture::SourcePacketCollector;
use tempfile::TempDir;

use crate::work::WorkDb;

pub use boss_execution_contracts::test_doubles::{NoopCube, RecordingPublisher};
pub use boss_work_db::test_support::{
    TEST_REPO_REMOTE_URL, add_guide_comment, create_active_chore, create_execution_started_now,
    create_execution_started_secs_ago, create_old_execution, create_product, create_ready_chore_execution,
    create_spawned_execution, create_test_chore, create_test_chore_manual, create_test_product,
    create_test_product_named, create_test_product_with_repo, finish_run_worker_pane_alive, insert_host_capability,
    log_capture, review_guide_source_packet, seed_daily_automation, seed_published_guide, seed_review_guide_series,
    seed_review_guide_series_for_pr, start_tmux_run,
};

/// Build identity stamped on fixture databases. Completion never compares it
/// with a running binary's, only with what the same database recorded.
const FIXTURE_BUILD_IDENTITY: &str = "completion-test-build";

/// Open a fresh file-backed [`WorkDb`] under a throwaway [`TempDir`], stamped
/// with [`FIXTURE_BUILD_IDENTITY`]. The returned `TempDir` must outlive the
/// `WorkDb`: dropping it deletes the backing `state.db`.
pub fn open_db() -> (TempDir, WorkDb) {
    let (dir, db) = crate::work::test_support::open_db();
    (dir, db.with_engine_build_identity(FIXTURE_BUILD_IDENTITY))
}

/// Like [`open_db`], wrapped in an [`std::sync::Arc`] for sharing with spawned tasks.
pub fn open_db_arc() -> (TempDir, std::sync::Arc<WorkDb>) {
    let (dir, db) = open_db();
    (dir, std::sync::Arc::new(db))
}

/// Declare `no-changes-needed` the way a worker does: submit a real `run_done`
/// proposal through [`WorkDb::submit_worker_proposal`] (validation, derived
/// idempotency key, auto-apply), which stamps `run_done_outcome` /
/// `run_done_declared_at` with a realistic timestamp. Re-declaring after a
/// refusal consumed the earlier declaration is a fresh submission.
pub fn declare_no_changes_needed(db: &WorkDb, execution_id: &str) {
    use boss_engine_proposal_validation::{derive_idempotency_key, validate_payload};
    let execution = db.get_execution(execution_id).unwrap();
    let kind = boss_protocol::ProposalKind::RunDone;
    let validated = validate_payload(
        kind,
        &serde_json::json!({"outcome": "no_changes_needed", "summary": "verified: nothing left to change"}),
    )
    .expect("run_done payload validates");
    let key = derive_idempotency_key(execution_id, kind, &validated.canonical_json);
    let outcome = db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id,
            work_item_id: &execution.work_item_id,
            kind,
            payload_json: &validated.canonical_json,
            idempotency_key: &key,
        })
        .unwrap()
        .expect("run_done submission accepted");
    assert!(!outcome.already_submitted, "declaration must be a fresh submission");
    assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Applied);
}

/// Direct-DB fixtures must model the bookmark that dispatch creates before spawn.
/// Retain the returned directory for the duration of the test.
pub async fn seed_empty_execution_bookmark(db: &WorkDb, execution_id: &str) -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
    let record = boss_engine_recovery::execution_bookmark::create(
        &boss_engine_recovery::execution_bookmark::LocalJj,
        &repo.worker,
        execution_id,
        "local",
    )
    .await
    .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    root
}

/// [`seed_published_guide`] plus one anchored comment. Returns
/// `(root, series, comment)`.
pub fn seed_published_guide_comment(db: &WorkDb, pr_number: u32, body: &str) -> (String, String, WorkComment) {
    let (root, series) = seed_published_guide(db, pr_number);
    let comment = add_guide_comment(db, &series, body);
    (root, series, comment)
}

/// Spawn a long-lived `sleep` in its OWN process group and return the
/// child handle. A reap signals the process *group*, so a test exercising
/// a REAL kill (durable-state teardown, `release_worker_pane`,
/// `retire_pane`'s durable-evidence auto-teardown) must target a process
/// in its own group — never `std::process::id()` — or the signal lands on
/// the test runner's own group and kills the test process itself.
pub fn spawn_group_leader_sleeper() -> std::process::Child {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    unsafe {
        Command::new("sleep")
            .arg("300")
            .pre_exec(|| {
                // setpgid(0, 0): become our own process group leader.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .expect("spawn sleep child")
    }
}

/// Give a local completion fixture the same durable recovery references as a
/// dispatched worker. Uses the fixture's existing directory and real jj.
pub fn preserve_test_workspace(db: &WorkDb, execution_id: &str) {
    use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
    use boss_engine_test_git::jj::JjRepo;
    let execution = db.get_execution(execution_id).unwrap();
    let workspace = std::path::Path::new(execution.workspace_path.as_deref().unwrap());
    assert!(workspace.is_dir(), "fixture needs an existing workspace");
    assert_ne!(
        workspace.canonicalize().unwrap(),
        std::path::Path::new("/tmp").canonicalize().unwrap()
    );
    if !workspace.join(".jj").exists() {
        let output = std::process::Command::new(JjRepo::binary())
            .args(["git", "init", "--colocate"])
            .arg(workspace)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let record = ExecutionBookmark {
        execution_id: execution_id.to_owned(),
        repo_path: if workspace.join(".jj/repo").is_dir() {
            workspace.canonicalize().unwrap()
        } else {
            boss_engine_recovery::execution_bookmark::shared_repo_from_pointer(
                &std::fs::read_to_string(workspace.join(".jj/repo")).unwrap(),
            )
            .unwrap()
        },
        host_id: "local".to_owned(),
        base_commit: None,
        pr_head_before: None,
    };
    JjRepo::run(workspace, &["bookmark", "set", &record.base(), "-r", "@-"]);
    JjRepo::run(
        workspace,
        &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
    );
    db.record_execution_bookmark(&record).unwrap();
}

/// Recovery metadata for a simulated remote store; the test adapter must
/// separately implement confirmation on that host.
pub fn record_remote_test_recovery(db: &WorkDb, execution_id: &str, host: &str) {
    let execution = db.get_execution(execution_id).unwrap();
    db.record_execution_bookmark(&boss_engine_recovery::execution_bookmark::ExecutionBookmark {
        execution_id: execution_id.to_owned(),
        repo_path: std::path::PathBuf::from(execution.workspace_path.unwrap()),
        host_id: host.to_owned(),
        base_commit: None,
        pr_head_before: None,
    })
    .unwrap();
}

pub fn confirm_remote_test_recovery(
    record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
    workspace: &std::path::Path,
    host: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        record.host_id == host && record.repo_path == workspace,
        "wrong remote recovery store"
    );
    Ok(())
}

/// Shared source-capture packet fixture. Callers pass the PR URL and the
/// base/head SHAs so creation, completion, and poller tests assert against
/// the same packet shape.
pub fn source_capture_packet(pr_url: &str, base_sha: &str, head_sha: &str) -> SourcePacket {
    let (owner, repo, pr_number) = boss_github::pr_url::parse_pr_url_parts(pr_url).unwrap_or(("spinyfin", "mono", 25));
    let repository = format!("{owner}/{repo}");
    SourcePacket {
        schema_version: 3,
        canonical_pr_url: pr_url.to_owned(),
        pr_number,
        title: "Captured".to_owned(),
        body: None,
        base_repository: repository.clone(),
        head_repository: repository,
        observed_base_sha: base_sha.to_owned(),
        probe_base_sha: None,
        merge_base_sha: "merge-base".to_owned(),
        head_sha: head_sha.to_owned(),
        files: Vec::new(),
        omissions: Vec::new(),
    }
}

/// Collector that returns `packet` and counts how many times it was invoked.
pub fn counting_source_collector(calls: Arc<AtomicUsize>, packet: SourcePacket) -> SourcePacketCollector {
    let fixture_packet = packet.clone();
    let collect: crate::review_guide_capture::PacketCollectFn = Arc::new(move |url, _observed, _branch, _metadata| {
        let packet = packet.clone();
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(url, packet.canonical_pr_url);
            Ok(packet)
        })
    });
    SourcePacketCollector::fixture(collect, fixture_packet)
}

/// Poll until a source-capture row exists for `root`, or panic after 2s.
pub async fn wait_for_source_capture(db: &WorkDb, root: &str) {
    let started = std::time::Instant::now();
    loop {
        if db.get_latest_pr_review_guide_source_capture(root).unwrap().is_some() {
            return;
        }
        if started.elapsed() > std::time::Duration::from_secs(2) {
            panic!("timed out waiting for review-guide source capture");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// Test-only entry points keep completion internals out of the production API.
pub use crate::pr_transition::fixtures as pr_transition;
pub use crate::queue_ci_audit::fixtures as queue_ci_audit;

impl crate::WorkerCompletionHandler {
    pub async fn finalize_pr_transition_for_test(
        &self,
        execution_id: &str,
        pr_url: String,
        target: crate::work::WorkerPrCompletionTarget,
        source: &'static str,
    ) -> crate::StopOutcome {
        self.finalize_pr_transition(execution_id, pr_url, target, source).await
    }
}
