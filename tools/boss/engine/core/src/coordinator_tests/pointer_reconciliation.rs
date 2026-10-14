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
        Ok(crate::coordinator::RebaseOutcome { clean: true, pushed: true, conflicted_files: vec![], linearized_commits: 0, linearize_decline: None })
    }
    async fn rebase_workspace_no_push(&self, workspace: &std::path::Path, _pr: u64) -> anyhow::Result<crate::coordinator::RebaseOutcome> {
        JjRepo::run(workspace, &["rebase", "-b", &self.record.head(), "-d", "upstream"]);
        Ok(crate::coordinator::RebaseOutcome { clean: true, pushed: false, conflicted_files: vec![], linearized_commits: 0, linearize_decline: None })
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
