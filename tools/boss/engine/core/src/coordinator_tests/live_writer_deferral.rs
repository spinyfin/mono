//! The engine's own rebases must not race a live worker on the same PR chain.
//!
//! These tests run against a real `jj` store shared by three workspaces: the
//! live worker's, the ladder's leased scratch workspace and the main one. A
//! rebase from one workspace while another has the same branch checked out is
//! how bookmarks end up conflicted or changes end up divergent.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::helpers::*;
use crate::conflict_ladder::{LadderOutcome, try_mechanical_rungs};
use crate::work::{
    ConflictResolution, ConflictResolutionInsertInput, CreateRevisionInput, FakePrStateChecker, PendingMergeCheck,
    PrOpenState,
};
use boss_engine_test_git::jj::JjRepo;
use boss_execution_contracts::RebaseOutcome;
use boss_host_execution::types::{CubeRepoHandle, CubeWorkspaceLease};

const PR_BRANCH: &str = "boss/live-writer-pr";

/// Cube double over the real store: leasing hands out the scratch workspace and
/// `rebase_workspace` performs the same rewrite cube's rebase does (the PR
/// branch moved onto upstream), pushing nothing anywhere.
struct SharedStoreCube {
    store: std::path::PathBuf,
    scratch: std::path::PathBuf,
    calls: Mutex<Vec<&'static str>>,
    rebases: AtomicUsize,
}

impl SharedStoreCube {
    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }
}

crate::stub_cube_client! { SharedStoreCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        self.record("ensure_repo");
        Ok(CubeRepoHandle { repo_id: "repo".into() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        self.record("lease_workspace");
        Ok(CubeWorkspaceLease {
            lease_id: "ladder-lease".into(),
            workspace_id: "scratch".into(),
            workspace_path: self.scratch.clone(),
            dirty_verified: None,
        })
    }
    async fn goto_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        self.record("goto_workspace");
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<RebaseOutcome> {
        self.record("rebase_workspace");
        self.rebases.fetch_add(1, Ordering::SeqCst);
        JjRepo::run(&self.store, &["rebase", "-b", PR_BRANCH, "-d", "upstream"]);
        Ok(RebaseOutcome { clean: true, pushed: true, conflicted_files: vec![], linearized_commits: 0, linearize_decline: None })
    }
    async fn verify_deletion_tripwire(&self, _repo: &str, _head: &str, _base: &str, _pr: u64) -> Vec<String> {
        vec![]
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        self.record("release_workspace");
        Ok(())
    }
} }

#[derive(bon::Builder)]
struct Fixture {
    dir: tempfile::TempDir,
    db: WorkDb,
    candidate: PendingMergeCheck,
    attempt: ConflictResolution,
    repo: JjRepo,
    live_execution_id: String,
    cube: SharedStoreCube,
}

async fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let product = create_test_product_with_repo(&db, "LiveWriter", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "Live writer");
    let pr_url = "https://github.com/foo/bar/pull/42".to_owned();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.mark_chore_blocked_merge_conflict(&chore.id, &pr_url).unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(&chore.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )
    .unwrap();
    // A revision of the PR with a worker running on it right now.
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(chore.id.clone())
                .description("Live revision of the PR")
                .created_via("test")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let live = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    // The live worker is editing on top of the PR branch.
    JjRepo::run(&repo.worker, &["new", "main", "-m", "PR work"]);
    std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", PR_BRANCH, "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Live revision work"]);
    std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    // Main moved on: this is what the engine's rebase would pull the PR onto.
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Upstream"]);
    std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "upstream", "-r", "@"]);

    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: pr_url.clone(),
            pr_number: 42,
            head_branch: PR_BRANCH.into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    let cube = SharedStoreCube {
        store: repo.repo.clone(),
        scratch: repo.replacement.clone(),
        calls: Mutex::new(Vec::new()),
        rebases: AtomicUsize::new(0),
    };
    Fixture::builder()
        .candidate(PendingMergeCheck {
            work_item_id: chore.id,
            product_id: product.id,
            pr_url,
        })
        .dir(dir)
        .db(db)
        .attempt(attempt)
        .repo(repo)
        .live_execution_id(live.id)
        .cube(cube)
        .build()
}

fn conflicted_bookmarks(repo: &JjRepo) -> String {
    JjRepo::run(
        &repo.repo,
        &["--ignore-working-copy", "bookmark", "list", "--conflicted"],
    )
    .trim()
    .to_owned()
}

fn divergent_changes(repo: &JjRepo) -> String {
    JjRepo::run(
        &repo.repo,
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "all()",
            "-T",
            "if(divergent, commit_id ++ \"\\n\")",
        ],
    )
    .trim()
    .to_owned()
}

/// `(commit_id, change_id)` of the PR branch tip.
fn pr_tip(repo: &JjRepo) -> (String, String) {
    let out = JjRepo::run(
        &repo.repo,
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            PR_BRANCH,
            "-T",
            "commit_id ++ \" \" ++ change_id",
        ],
    );
    let (commit, change) = out.trim().split_once(' ').expect("commit and change id");
    (commit.to_owned(), change.to_owned())
}

#[tokio::test]
async fn rung1_defers_while_a_worker_is_live_and_leaves_the_store_untouched() {
    let fx = fixture().await;
    let (commit_before, change_before) = pr_tip(&fx.repo);
    // The live worker rewrites its own history in its workspace (rebasing the
    // PR branch onto upstream) as the ladder is invoked: the overlap that used
    // to leave conflicted bookmarks. Both sides wait on the barrier so the
    // rewrite starts as the ladder call begins.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let worker = fx.repo.worker.clone();
    let worker_barrier = barrier.clone();
    let worker_rebase = std::thread::spawn(move || {
        worker_barrier.wait();
        JjRepo::run(&worker, &["rebase", "-b", PR_BRANCH, "-d", "upstream"]);
        std::fs::write(worker.join("more.txt"), "more live work").unwrap();
        JjRepo::run(&worker, &["status"]);
    });
    barrier.wait();
    let during = try_mechanical_rungs(
        &fx.db,
        &RecordingPublisher::default(),
        &fx.cube,
        &fx.candidate,
        &fx.attempt,
    )
    .await;
    worker_rebase.join().unwrap();
    // And again once the worker's rewrite has landed.
    let after = try_mechanical_rungs(
        &fx.db,
        &RecordingPublisher::default(),
        &fx.cube,
        &fx.candidate,
        &fx.attempt,
    )
    .await;

    // The worker really rewrote the PR commit: new commit id, same change.
    let (commit_after, change_after) = pr_tip(&fx.repo);
    assert_ne!(commit_before, commit_after, "the worker's rebase rewrote nothing");
    assert_eq!(change_before, change_after);

    assert_eq!(during, LadderOutcome::DeferredForLiveWriter);
    assert_eq!(after, LadderOutcome::DeferredForLiveWriter);
    assert!(
        fx.cube.calls.lock().unwrap().is_empty(),
        "the ladder touched cube while a worker was live: {:?}",
        fx.cube.calls.lock().unwrap()
    );
    assert_eq!(fx.cube.rebases.load(Ordering::SeqCst), 0);
    assert_eq!(conflicted_bookmarks(&fx.repo), "", "bookmarks were left conflicted");
    assert_eq!(divergent_changes(&fx.repo), "", "changes were left divergent");
    // The attempt stays pending so a later tick retries; nothing was spawned.
    let attempt = fx.db.get_conflict_resolution(&fx.attempt.id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending");
    assert!(attempt.revision_task_id.is_none());
    // The worker's own edits are intact.
    let files = JjRepo::run(&fx.repo.worker, &["file", "list", "-r", "@"]);
    assert!(files.contains("more.txt") && files.contains("revision.txt"), "{files}");
}

#[tokio::test]
async fn rung1_still_runs_when_no_worker_is_live() {
    let fx = fixture().await;
    fx.db.mark_execution_redundant(&fx.live_execution_id).unwrap();
    let outcome = try_mechanical_rungs(
        &fx.db,
        &RecordingPublisher::default(),
        &fx.cube,
        &fx.candidate,
        &fx.attempt,
    )
    .await;

    assert_eq!(outcome, LadderOutcome::Retired);
    assert_eq!(fx.cube.rebases.load(Ordering::SeqCst), 1);
    let calls = fx.cube.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [
            "ensure_repo",
            "lease_workspace",
            "goto_workspace",
            "rebase_workspace",
            "release_workspace"
        ]
    );
    assert_eq!(conflicted_bookmarks(&fx.repo), "");
}

#[tokio::test]
async fn a_live_review_does_not_defer_the_ladder() {
    let fx = fixture().await;
    fx.db.mark_execution_redundant(&fx.live_execution_id).unwrap();
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&fx.candidate.work_item_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    assert!(
        fx.db
            .live_writers_in_chain(&fx.candidate.work_item_id)
            .unwrap()
            .is_empty()
    );
    let outcome = try_mechanical_rungs(
        &fx.db,
        &RecordingPublisher::default(),
        &fx.cube,
        &fx.candidate,
        &fx.attempt,
    )
    .await;
    assert_eq!(outcome, LadderOutcome::Retired);
}

#[tokio::test]
async fn claimed_and_waiting_human_writers_defer_too() {
    let fx = fixture().await;
    for status in ["claimed", "waiting_human"] {
        rusqlite::Connection::open(fx.dir.path().join("boss.db"))
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?1 WHERE id = ?2",
                rusqlite::params![status, fx.live_execution_id],
            )
            .unwrap();
        let writers = fx.db.live_writers_in_chain(&fx.candidate.work_item_id).unwrap();
        assert_eq!(writers.len(), 1, "{status}");
        assert_eq!(writers[0].id, fx.live_execution_id);
    }
}

/// The conflict-watch caller treats the deferral like an unavailable rung: it
/// spawns no conflict revision alongside the live writer and leaves the
/// attempt for a later tick.
#[tokio::test]
async fn conflict_watch_spawns_no_revision_while_a_worker_is_live() {
    use crate::conflict_watch::on_conflict_detected;
    use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState};

    let fx = fixture().await;
    let probe = PrLifecycleProbe::builder()
        .url(fx.candidate.pr_url.clone())
        .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
        .base_ref_oid("abc123")
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build();

    on_conflict_detected(
        &fx.db,
        &RecordingPublisher::default(),
        Some(&fx.cube),
        &FakePrStateChecker::always(PrOpenState::Open),
        &fx.candidate,
        &probe,
    )
    .await;

    assert!(
        fx.cube.calls.lock().unwrap().is_empty(),
        "the ladder touched cube while a worker was live: {:?}",
        fx.cube.calls.lock().unwrap()
    );
    let latest = fx
        .db
        .latest_conflict_resolution_for_work_item(&fx.candidate.work_item_id)
        .unwrap()
        .expect("detection records an attempt");
    assert_eq!(latest.status, "pending", "{latest:?}");
    assert!(latest.resolved_by_rung.is_none());
    assert!(
        latest.revision_task_id.is_none(),
        "no conflict revision may start alongside the live writer"
    );
    assert_eq!(conflicted_bookmarks(&fx.repo), "");
}

/// The queued remediation path reports the deferral as a retry owed after the
/// cooldown, not as a settled attempt.
#[tokio::test]
async fn queued_remediation_retries_after_cooldown_while_a_worker_is_live() {
    use crate::conflict_remediation::{ConflictRemediationJob, LadderRemediationJob, RemediationDisposition};
    use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState, PrReviewState};

    let fx = fixture().await;
    let cube_calls = Arc::new(SharedStoreCube {
        store: fx.repo.repo.clone(),
        scratch: fx.repo.replacement.clone(),
        calls: Mutex::new(Vec::new()),
        rebases: AtomicUsize::new(0),
    });
    let job = LadderRemediationJob::new(
        Arc::new(fx.db.clone()),
        Arc::new(RecordingPublisher::default()),
        cube_calls.clone(),
    )
    .with_pr_checker(Arc::new(FakePrStateChecker::always(PrOpenState::Open)));
    let probe = PrLifecycleProbe::builder()
        .url(fx.candidate.pr_url.clone())
        .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();

    let disposition = job.run(fx.candidate.clone(), fx.attempt.clone(), probe).await;

    assert_eq!(disposition, RemediationDisposition::RetryAfterCooldown);
    assert!(cube_calls.calls.lock().unwrap().is_empty());
    let current = fx.db.get_conflict_resolution(&fx.attempt.id).unwrap().unwrap();
    assert_eq!(current.status, "pending");
    assert!(current.revision_task_id.is_none());
}
