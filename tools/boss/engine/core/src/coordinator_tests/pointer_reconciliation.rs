use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn task_and_chore_recovery_keeps_verified_pr_files_and_persists_baseline() {
    for kind in [ExecutionKind::TaskImplementation, ExecutionKind::ChoreImplementation] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&path);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE work_executions SET kind = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![kind.as_str(), prior.id, next.id],
            )
            .unwrap();
        next.kind = kind;
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        let mut record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
            .await
            .unwrap();
        record.base_commit = None;
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("pr-only.txt"), "published implementation").unwrap();
        JjRepo::run(&repo.worker, &["describe", "-m", "Published implementation"]);
        JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
        JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
        let cube = Arc::new(FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            ..Default::default()
        });
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        );
        let lease = CubeWorkspaceLease {
            dirty_verified: Some(false),
            lease_id: "lease".into(),
            workspace_id: "replacement".into(),
            workspace_path: repo.replacement.clone(),
        };
        let recovered = coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            recovered.reconciliation.unwrap().anchor,
            recovery::Anchor::PrHead { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        let reloaded = db.execution_bookmark(&prior.id).unwrap();
        assert!(reloaded.base_commit.is_some());
        // Retry after pointers were reconciled but before the new run starts.
        // No reconciliation is needed now; the recorded PR still owns positioning.
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
        assert!(recovery::diff(&LocalJj, &reloaded).await.unwrap().is_empty());
        assert!(
            db.record_execution_bookmark(&reloaded).is_err(),
            "live duplicates must fail"
        );
        db.retire_execution_bookmark(&prior.id, "test retirement").unwrap();
        db.record_execution_bookmark(&reloaded).unwrap();
    }
}

#[tokio::test]
async fn checkout_of_predecessor_publication_cannot_hide_unpublished_work() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("published.txt"), "published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", &record.base(), "-m", "Unpublished publication"]);
    std::fs::write(repo.repo.join("unpublished.txt"), "unique work").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &record.publication(), "-r", "@", "--allow-backwards"],
    );
    let before = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        goto_reset_bookmark: Some(record.publication()),
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(db, WorkerPool::new(1), cube, Arc::new(FakeExecutionRunner::default()));
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    let error = coordinator
        .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", Some(99), Some(99))
        .await
        .unwrap_err();
    assert_eq!(
        recovery::pointer_integrity_kind(&error),
        Some(recovery::PointerIntegrityKind::NotContained)
    );
    assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), before);
}

#[tokio::test]
async fn closed_pr_does_not_prevent_retiring_a_missing_baseline() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let mut record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    record.base_commit = None;
    db.record_execution_bookmark(&record).unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        closed_pr: true,
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    assert!(
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, Some(99))
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_none());
}

#[tokio::test]
async fn abandoned_recorded_baseline_is_retired_and_a_fresh_dispatch_proceeds() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let mut record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    // The record names cube's empty positioning commit as its baseline, which
    // is then abandoned with its bookmark: no successor, no PR to vouch.
    JjRepo::run(&repo.repo, &["new", &record.base()]);
    let empty = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["abandon", "@"]);
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    record.base_commit = Some(empty);
    db.record_execution_bookmark(&record).unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    assert!(
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_none());
    // The retired record no longer blocks: a later dispatch starts fresh.
    assert!(
        coordinator
            .recover_execution_bookmark(&next, &lease, &coordinator.host_adapter, "mono", None, None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn rewrite_checkpoint_survives_baseline_loss_and_record_reload() {
    let dir = tempdir().unwrap();
    let (db, prior, _) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "main", "-m", "PR implementation"]);
    std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "New upstream"]);
    std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "new-upstream", "-r", "@"]);
    JjRepo::run(&repo.worker, &["rebase", "-b", &record.head(), "-d", "new-upstream"]);
    db.checkpoint_rewritten_baselines(&prior.work_item_id).await.unwrap();
    let reloaded = db.execution_bookmark(&prior.id).unwrap();
    assert_ne!(reloaded.base_commit, record.base_commit);
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    let patch = recovery::diff(&LocalJj, &reloaded).await.unwrap();
    assert!(patch.contains("revision.txt"));
    assert!(!patch.contains("pr.txt"));
}

/// Cube's shared-store mutations exercised through both production rebase paths.
struct RecoveryRebaseCube {
    record: recovery::ExecutionBookmark,
    deletions: bool,
}

crate::stub_cube_client! { RecoveryRebaseCube {
    async fn goto_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        JjRepo::run(&self.record.repo_path, &["bookmark", "set", &self.record.publication(), "-r", &self.record.head(), "--allow-backwards"]);
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<crate::coordinator::RebaseOutcome> {
        Ok(crate::coordinator::RebaseOutcome::builder().clean(true).pushed(true).conflicted_files(vec![]).build())
    }
    async fn rebase_workspace_no_push(&self, workspace: &std::path::Path, _pr: u64) -> anyhow::Result<crate::coordinator::RebaseOutcome> {
        JjRepo::run(workspace, &["rebase", "-b", &self.record.head(), "-d", "upstream"]);
        Ok(crate::coordinator::RebaseOutcome::builder().clean(true).pushed(false).conflicted_files(vec![]).build())
    }
    async fn verify_deletion_tripwire(&self, _repo: &str, _head: &str, _base: &str, _pr: u64) -> Vec<String> {
        if self.deletions { vec!["merged parent feature removed".into()] } else { vec![] }
    }
} }

fn rebase_candidate(db: &WorkDb) -> (crate::work::PendingMergeCheck, String) {
    let product = create_test_product_with_repo(db, "RecoveryRebase", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), "Recovery rebase");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some("https://github.com/foo/bar/pull/42".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (
        crate::work::PendingMergeCheck {
            work_item_id: chore.id.clone(),
            product_id: product.id,
            pr_url: "https://github.com/foo/bar/pull/42".into(),
        },
        chore.id,
    )
}

#[tokio::test]
async fn damaged_historical_checkpoint_cannot_skip_retirement_or_deletion_signoff() {
    use crate::conflict_ladder::{LadderOutcome, run_rung1_in_lease};
    for deletions in [false, true] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (candidate, chore_id) = rebase_candidate(&db);
        db.mark_chore_blocked_merge_conflict(&chore_id, &candidate.pr_url)
            .unwrap();
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: candidate.product_id.clone(),
                work_item_id: chore_id.clone(),
                pr_url: candidate.pr_url.clone(),
                pr_number: 42,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base".into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap()
            .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&chore_id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let repo = JjRepo::new(dir.path());
        let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("head.txt"), "head work").unwrap();
        JjRepo::run(&repo.worker, &["describe", "-m", "Recovery work"]);
        JjRepo::run(&repo.repo, &["new", &record.base(), "-m", "Publication-only work"]);
        std::fs::write(repo.repo.join("unique.txt"), "unique unpublished content").unwrap();
        JjRepo::run(
            &repo.repo,
            &["bookmark", "set", &record.publication(), "-r", "@", "--allow-backwards"],
        );
        let error = recovery::checkpoint_baseline(&LocalJj, &record).await.unwrap_err();
        assert_eq!(
            recovery::pointer_integrity_kind(&error),
            Some(recovery::PointerIntegrityKind::Diverged)
        );
        let before = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
        let publisher = RecordingPublisher::default();
        let cube = RecoveryRebaseCube {
            record: record.clone(),
            deletions,
        };
        let lease = CubeWorkspaceLease {
            lease_id: "lease".into(),
            workspace_id: "workspace".into(),
            workspace_path: repo.worker.clone(),
            dirty_verified: None,
        };
        let outcome = run_rung1_in_lease(&db, &publisher, &cube, &candidate, &attempt, 42, &lease).await;
        assert_eq!(
            outcome,
            if deletions {
                LadderOutcome::HaltedForSignoff
            } else {
                LadderOutcome::Retired
            }
        );
        assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), before);
        assert_eq!(
            db.execution_bookmark(&execution.id).unwrap().base_commit,
            record.base_commit
        );
        let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
        assert_eq!(row.resolved_by_rung, Some(1));
        if deletions {
            assert!(
                db.list_attention_items_for_work_item(&chore_id)
                    .unwrap()
                    .iter()
                    .any(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND)
            );
            let WorkItem::Chore(chore) = db.get_work_item(&chore_id).unwrap() else {
                panic!("expected chore");
            };
            assert_eq!(chore.blocked_reason.as_deref(), Some("deletion_signoff"));
        }
    }
}

#[tokio::test]
async fn speculative_rewrite_restores_original_baseline_before_next_goto() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, chore_id) = rebase_candidate(&db);
    let prior = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(chore_id)
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status(boss_protocol::ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Published implementation"]);
    std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let saved = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
    let original = saved
        .iter()
        .find(|(name, _)| name == &record.publication())
        .unwrap()
        .1
        .clone()
        .unwrap();
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Upstream"]);
    std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "upstream", "-r", "@"]);
    let cube = RecoveryRebaseCube {
        record: record.clone(),
        deletions: false,
    };
    let metrics = crate::metrics::Registry::new();
    crate::speculative_conflict::init(&metrics);
    crate::speculative_conflict::run_in_lease(&db, &cube, &metrics, &candidate, 42, &repo.worker).await;
    assert_eq!(metrics.counter_value("speculative_conflict.clean"), Some(1));
    assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), saved);
    JjRepo::run(
        &repo.repo,
        &[
            "bookmark",
            "set",
            &record.publication(),
            "-r",
            &original,
            "--allow-backwards",
        ],
    );
    let reloaded = db.execution_bookmark(&prior.id).unwrap();
    assert_eq!(reloaded.base_commit, record.base_commit);
    JjRepo::run(&repo.repo, &["bookmark", "delete", &record.base()]);
    let patch = recovery::diff(&LocalJj, &reloaded).await.unwrap();
    assert!(patch.contains("revision.txt"));
    assert!(!patch.contains("upstream.txt"));
}

/// Recovery pointers across the engine's own mechanical rebases: the saved
/// identities are kept across the whole operation (including rung 0's push),
/// a pushed PR head is adopted by every record that already carries its work,
/// and revision executions on the same PR branch are protected too.
mod rebase_adoption {
    use super::*;
    use crate::conflict_ladder::{LadderOutcome, run_rung1_in_lease};
    use crate::work::{
        ConflictResolution, ConflictResolutionInsertInput, CreateRevisionInput, FakePrStateChecker, PrOpenState,
    };
    use recovery::ExecutionBookmark;

    #[derive(bon::Builder)]
    struct Fixture {
        dir: tempfile::TempDir,
        db: WorkDb,
        candidate: crate::work::PendingMergeCheck,
        chore_id: String,
        attempt: ConflictResolution,
        repo: JjRepo,
        root: ExecutionBookmark,
    }

    async fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (candidate, chore_id) = rebase_candidate(&db);
        db.mark_chore_blocked_merge_conflict(&chore_id, &candidate.pr_url)
            .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&chore_id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let repo = JjRepo::new(dir.path());
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
        JjRepo::run(&repo.worker, &["new", "main", "-m", "PR work"]);
        std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
        JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
        let root = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
            .await
            .unwrap();
        db.record_execution_bookmark(&root).unwrap();
        std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        JjRepo::run(&repo.repo, &["new", "main", "-m", "Upstream"]);
        std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
        JjRepo::run(&repo.repo, &["bookmark", "set", "upstream", "-r", "@"]);
        // The PR branch is the root execution's publication bookmark, as it is
        // when cube publishes a worker's branch.
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: candidate.product_id.clone(),
                work_item_id: chore_id.clone(),
                pr_url: candidate.pr_url.clone(),
                pr_number: 42,
                head_branch: root.publication(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base".into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap()
            .unwrap();
        Fixture::builder()
            .dir(dir)
            .db(db)
            .candidate(candidate)
            .chore_id(chore_id)
            .attempt(attempt)
            .repo(repo)
            .root(root)
            .build()
    }

    /// A revision execution on the same PR branch: a separate work item with its own record.
    async fn add_revision_record(fx: &Fixture) -> ExecutionBookmark {
        let revision = fx
            .db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(fx.chore_id.clone())
                    .description("Revision of the PR")
                    .created_via("test")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let execution = fx
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let record = recovery::create(&LocalJj, &fx.repo.worker, &execution.id, "local")
            .await
            .unwrap();
        fx.db.record_execution_bookmark(&record).unwrap();
        record
    }

    /// Cube's shared-store effects: the rebase rewrites the PR branch, and the
    /// publication step moves the PR branch bookmark before it can fail.
    struct RewritingCube {
        root: ExecutionBookmark,
        lease_dir: std::path::PathBuf,
        conflicted: bool,
        push_fails: bool,
    }

    impl RewritingCube {
        fn rebase(&self) {
            JjRepo::run(
                &self.root.repo_path,
                &["rebase", "-b", &self.root.head(), "-d", "upstream"],
            );
        }
    }

    crate::stub_cube_client! { RewritingCube {
        async fn goto_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
            Ok(())
        }
        async fn rebase_workspace(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<crate::coordinator::RebaseOutcome> {
            self.rebase();
            if self.conflicted {
                std::fs::write(
                    self.lease_dir.join("lib.rs"),
                    "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+mod a;\n+++++++ Contents of side #2\nmod b;\n>>>>>>> Conflict 1 of 1 ends\n",
                ).unwrap();
                return Ok(crate::coordinator::RebaseOutcome::builder().clean(false).pushed(false).conflicted_files(vec!["lib.rs".into()]).build());
            }
            Ok(crate::coordinator::RebaseOutcome::builder().clean(true).pushed(true).conflicted_files(vec![]).build())
        }
        async fn rebase_workspace_no_push(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<crate::coordinator::RebaseOutcome> {
            self.rebase();
            Ok(crate::coordinator::RebaseOutcome::builder().clean(true).pushed(false).conflicted_files(vec![]).build())
        }
        async fn push_resolution(&self, _workspace: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
            JjRepo::run(&self.root.repo_path, &["bookmark", "set", &self.root.publication(), "-r", &self.root.head(), "--allow-backwards"]);
            if self.push_fails { anyhow::bail!("push rejected") }
            Ok(())
        }
        async fn verify_deletion_tripwire(&self, _repo: &str, _head: &str, _base: &str, _pr: u64) -> Vec<String> {
            vec![]
        }
    } }

    fn lease(fx: &Fixture) -> CubeWorkspaceLease {
        CubeWorkspaceLease {
            lease_id: "lease".into(),
            workspace_id: "workspace".into(),
            workspace_path: fx.repo.replacement.clone(),
            dirty_verified: None,
        }
    }

    fn cube(fx: &Fixture, conflicted: bool, push_fails: bool) -> RewritingCube {
        RewritingCube {
            root: fx.root.clone(),
            lease_dir: fx.repo.replacement.clone(),
            conflicted,
            push_fails,
        }
    }

    async fn run_ladder(fx: &Fixture, cube: &RewritingCube) -> LadderOutcome {
        run_rung1_in_lease(
            &fx.db,
            &RecordingPublisher::default(),
            cube,
            &fx.candidate,
            &fx.attempt,
            42,
            &lease(fx),
        )
        .await
    }

    async fn commits(record: &ExecutionBookmark) -> Vec<Option<String>> {
        recovery::snapshot_pointers(&LocalJj, record)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, commit)| commit)
            .collect()
    }

    async fn pushed_head(fx: &Fixture) -> String {
        // The PR branch bookmark was restored; the pushed head is what the
        // adopted records now sit on.
        let record = fx.db.execution_bookmark(&fx.root.execution_id).unwrap();
        record.base_commit.unwrap()
    }

    async fn assert_adopted(fx: &Fixture, record: &ExecutionBookmark, before: &[Option<String>]) {
        let reloaded = fx.db.execution_bookmark(&record.execution_id).unwrap();
        let adopted = reloaded.base_commit.clone().unwrap();
        assert_eq!(commits(record).await, vec![Some(adopted.clone()); 3], "one lineage");
        assert_ne!(commits(record).await, before, "moved off the pre-rebase lineage");
        assert!(
            recovery::unpublished_diff(&LocalJj, &reloaded)
                .await
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(
            recovery::reconcile(&LocalJj, &reloaded, None, "main")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn clean_pushed_rebase_moves_root_and_revision_records_onto_pushed_head() {
        let fx = fixture().await;
        let revision = add_revision_record(&fx).await;
        let before = commits(&fx.root).await;
        let outcome = run_ladder(&fx, &cube(&fx, false, false)).await;
        assert_eq!(outcome, LadderOutcome::Retired);
        assert_adopted(&fx, &fx.root, &before).await;
        assert_adopted(&fx, &revision, &before).await;
        assert_eq!(
            pushed_head(&fx).await,
            fx.db
                .execution_bookmark(&revision.execution_id)
                .unwrap()
                .base_commit
                .unwrap()
        );
    }

    #[tokio::test]
    async fn rung0_push_after_residual_conflicts_keeps_one_lineage() {
        let fx = fixture().await;
        let revision = add_revision_record(&fx).await;
        let before = commits(&fx.root).await;
        let outcome = run_ladder(&fx, &cube(&fx, true, false)).await;
        assert_eq!(outcome, LadderOutcome::Retired);
        assert_eq!(
            fx.db
                .get_conflict_resolution(&fx.attempt.id)
                .unwrap()
                .unwrap()
                .resolved_by_rung,
            Some(0)
        );
        assert_adopted(&fx, &fx.root, &before).await;
        assert_adopted(&fx, &revision, &before).await;
    }

    #[tokio::test]
    async fn publication_only_work_survives_rung0_success_and_failure() {
        for push_fails in [false, true] {
            let fx = fixture().await;
            JjRepo::run(&fx.repo.repo, &["new", &fx.root.base(), "-m", "Publication-only work"]);
            std::fs::write(fx.repo.repo.join("unique.txt"), "unique unpublished content").unwrap();
            JjRepo::run(
                &fx.repo.repo,
                &[
                    "bookmark",
                    "set",
                    &fx.root.publication(),
                    "-r",
                    "@",
                    "--allow-backwards",
                ],
            );
            let before = commits(&fx.root).await;
            let outcome = run_ladder(&fx, &cube(&fx, true, push_fails)).await;
            assert_eq!(
                matches!(outcome, LadderOutcome::Retired),
                !push_fails,
                "rung 0 outcome with push_fails={push_fails}: {outcome:?}"
            );
            assert_eq!(
                commits(&fx.root).await,
                before,
                "saved identities kept (push_fails={push_fails})"
            );
            assert_eq!(
                fx.db.execution_bookmark(&fx.root.execution_id).unwrap().base_commit,
                fx.root.base_commit
            );
        }
    }

    /// An older record whose store cannot be read must not stop the others.
    fn add_damaged_record(fx: &Fixture) {
        let execution = fx
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&fx.chore_id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Failed)
                    .build(),
            )
            .unwrap();
        fx.db
            .record_execution_bookmark(&ExecutionBookmark {
                execution_id: execution.id,
                repo_path: fx.dir.path().join("missing-store"),
                host_id: "local".into(),
                base_commit: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn one_damaged_historical_record_does_not_disable_rung1_or_speculation() {
        let fx = fixture().await;
        add_damaged_record(&fx);
        let before = commits(&fx.root).await;
        let outcome = run_ladder(&fx, &cube(&fx, false, false)).await;
        assert_eq!(outcome, LadderOutcome::Retired);
        assert_adopted(&fx, &fx.root, &before).await;

        let fx = fixture().await;
        add_damaged_record(&fx);
        let saved = commits(&fx.root).await;
        let metrics = crate::metrics::Registry::new();
        crate::speculative_conflict::init(&metrics);
        crate::speculative_conflict::run_in_lease(
            &fx.db,
            &cube(&fx, false, false),
            &metrics,
            &fx.candidate,
            42,
            &fx.repo.replacement,
        )
        .await;
        assert_eq!(metrics.counter_value("speculative_conflict.clean"), Some(1));
        assert_eq!(commits(&fx.root).await, saved);
    }

    #[tokio::test]
    async fn speculative_rebase_protects_revision_records_through_baseline_loss() {
        let fx = fixture().await;
        let revision = add_revision_record(&fx).await;
        let saved_root = commits(&fx.root).await;
        let saved_revision = commits(&revision).await;
        let metrics = crate::metrics::Registry::new();
        crate::speculative_conflict::init(&metrics);
        crate::speculative_conflict::run_in_lease(
            &fx.db,
            &cube(&fx, false, false),
            &metrics,
            &fx.candidate,
            42,
            &fx.repo.replacement,
        )
        .await;
        assert_eq!(metrics.counter_value("speculative_conflict.clean"), Some(1));
        assert_eq!(commits(&fx.root).await, saved_root);
        assert_eq!(commits(&revision).await, saved_revision);
        let reloaded = fx.db.execution_bookmark(&revision.execution_id).unwrap();
        assert_eq!(reloaded.base_commit, revision.base_commit);
        JjRepo::run(&fx.repo.repo, &["bookmark", "delete", &revision.base()]);
        let patch = recovery::diff(&LocalJj, &reloaded).await.unwrap();
        assert!(patch.contains("revision.txt"));
        assert!(!patch.contains("upstream.txt"));
    }

    /// A Running revision execution on the PR chain whose working copy is
    /// rewritten by the engine's own rebase. The completed root record is
    /// retired so the only record left is the live worker's.
    async fn add_live_revision_record(fx: &Fixture) -> ExecutionBookmark {
        fx.db
            .retire_execution_bookmark(&fx.root.execution_id, "test: leave only the live worker")
            .unwrap();
        let revision = fx
            .db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(fx.chore_id.clone())
                    .description("Live revision of the PR")
                    .created_via("test")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let execution = fx
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        let record = recovery::create(&LocalJj, &fx.repo.worker, &execution.id, "local")
            .await
            .unwrap();
        fx.db.record_execution_bookmark(&record).unwrap();
        record
    }

    fn jj_ids(fx: &Fixture, revset: &str) -> Vec<String> {
        // Divergent commits are listed by template since jj has no revset for them.
        let template = if revset == "divergent()" {
            "if(divergent, commit_id ++ \"\\n\")"
        } else {
            "commit_id ++ \"\\n\""
        };
        let revset = if revset == "divergent()" { "all()" } else { revset };
        JjRepo::run(&fx.repo.worker, &["log", "--no-graph", "-r", revset, "-T", template])
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The live worker's three pointers must sit on the rebased lineage of its
    /// working copy (ancestors-or-equal of `@`), with no divergent twin, and
    /// work done after the rebase must still be recoverable.
    async fn assert_live_pointers_follow_working_copy(fx: &Fixture, live: &ExecutionBookmark) {
        // The rebase ran from another workspace, so the worker's copy is stale
        // until jj rewrites it onto the rebased commit.
        JjRepo::run(&fx.repo.worker, &["workspace", "update-stale"]);
        for name in [live.base(), live.head(), live.publication()] {
            let on_lineage = jj_ids(fx, &format!("bookmarks(exact:\"{name}\") & ::@"));
            assert_eq!(on_lineage.len(), 1, "{name} is not on the working copy's lineage");
        }
        assert!(jj_ids(fx, "divergent()").is_empty(), "rebase left divergent twins");
        assert!(
            jj_ids(fx, "@ & descendants(bookmarks(exact:\"upstream\"))").len() == 1,
            "working copy was rebased onto upstream"
        );
        std::fs::write(fx.repo.worker.join("after.txt"), "after the rebase").unwrap();
        JjRepo::run(&fx.repo.worker, &["status"]);
        recovery::checkpoint_baseline(&LocalJj, live).await.unwrap();
        let patch = recovery::diff(&LocalJj, live).await.unwrap();
        assert!(patch.contains("revision.txt") && patch.contains("after.txt"), "{patch}");
        assert!(!patch.contains("upstream.txt"), "{patch}");
    }

    #[tokio::test]
    async fn rung1_rebase_leaves_a_live_workers_pointers_following_its_working_copy() {
        let fx = fixture().await;
        let live = add_live_revision_record(&fx).await;
        let outcome = run_ladder(&fx, &cube(&fx, false, false)).await;
        assert_eq!(outcome, LadderOutcome::Retired);
        assert_live_pointers_follow_working_copy(&fx, &live).await;
    }

    #[tokio::test]
    async fn speculative_rewrite_leaves_a_live_workers_pointers_following_its_working_copy() {
        let fx = fixture().await;
        let live = add_live_revision_record(&fx).await;
        let metrics = crate::metrics::Registry::new();
        crate::speculative_conflict::init(&metrics);
        crate::speculative_conflict::run_in_lease(
            &fx.db,
            &cube(&fx, false, false),
            &metrics,
            &fx.candidate,
            42,
            &fx.repo.replacement,
        )
        .await;
        assert_eq!(metrics.counter_value("speculative_conflict.clean"), Some(1));
        assert_live_pointers_follow_working_copy(&fx, &live).await;
    }
}
