use super::*;
use crate::workspace_lease_release::{
    BookmarkGate, BookmarkStatus, DbBookmarkGate, EmptyOccupancy, LeaseReleaseOutcome, PaneStatus, ReleaseEvidence,
    TerminalCause, apply_release,
};
use recovery::{ExecutionBookmark, PreservedTip, RestoreReport};

struct RecordedLookupFailure;

#[async_trait::async_trait]
impl recovery::Jj for RecordedLookupFailure {
    async fn shared_repo(&self, workspace: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        recovery::Jj::shared_repo(&LocalJj, workspace).await
    }
    async fn run(&self, repo: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
        if args.iter().any(|arg| arg.starts_with("bookmarks(exact:")) {
            return Ok(String::new());
        }
        if args.iter().any(|arg| arg.starts_with("present(")) {
            anyhow::bail!("SSH transport unavailable");
        }
        recovery::Jj::run(&LocalJj, repo, args).await
    }
}

struct FailedRestore {
    integrity: bool,
    positions: std::sync::atomic::AtomicUsize,
}
crate::stub_host_adapter! { FailedRestore {
    fn host_id(&self) -> &str { "local" }
    async fn goto_workspace(&self, workspace: &std::path::Path, _: u64) -> anyhow::Result<()> {
        self.positions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        JjRepo::run(workspace, &["new", "main", "-m", "Fresh execution"]);
        Ok(())
    }
    async fn restore_preserved_work(&self, record: &ExecutionBookmark, workspace: &std::path::Path, recorded: Option<&PreservedTip>) -> anyhow::Result<Option<RestoreReport>> {
        if self.integrity { return Err(recovery::pointer_integrity_error("undecidable preserved work")); }
        recovery::restore_preserved(&RecordedLookupFailure, record, workspace, recorded).await
    }
} }

#[tokio::test]
async fn failed_self_retry_then_pre_start_release_retains_recorded_work() {
    for integrity in [false, true] {
        let dir = tempdir().unwrap();
        let repo = JjRepo::new(dir.path());
        let db_path = dir.path().join("boss.db");
        let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
        export_origin(&repo);
        let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
        let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
            .await
            .unwrap()
            .tip
            .unwrap();
        db.record_preserved_tip(&execution.id, &tip).unwrap();
        rusqlite::Connection::open(&db_path).unwrap().execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', cube_workspace_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        ).unwrap();
        let execution = db.get_execution(&execution.id).unwrap();
        let (coordinator, _) = coordinator_for(db.clone());
        let lease = CubeWorkspaceLease {
            workspace_id: "retry".into(),
            workspace_path: repo.replacement.clone(),
            lease_id: "retry".into(),
            dirty_verified: None,
        };
        let failed = Arc::new(FailedRestore {
            integrity,
            positions: Default::default(),
        });
        let adapter: Arc<dyn HostAdapter> = failed.clone();
        let result = coordinator
            .recover_execution_bookmark_for_test(&execution, &lease, &adapter, "mono", Some(99), Some(99))
            .await;
        if integrity {
            assert!(result.unwrap().is_none());
            assert_eq!(failed.positions.load(std::sync::atomic::Ordering::SeqCst), 2);
        } else {
            assert!(result.unwrap_err().to_string().contains("SSH transport unavailable"));
            assert_eq!(failed.positions.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(db.list_attention_items(&execution.id).unwrap().is_empty());
        }
        let outcome = apply_release(
            &db,
            &execution,
            "retry",
            ReleaseEvidence {
                cause: if integrity {
                    TerminalCause::ProvenCompleted
                } else {
                    TerminalCause::ProvenNeverStarted
                },
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &DbBookmarkGate { db: &db, adapter: None },
            },
            async { Ok(()) },
        )
        .await;
        assert_eq!(outcome, LeaseReleaseOutcome::Released);
        assert_eq!(db.preserved_tip(&execution.id).unwrap(), Some(tip.clone()));
        assert_eq!(
            JjRepo::run(
                &repo.repo,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &recovery::preserved_name(&execution.id),
                    "-T",
                    "commit_id"
                ]
            ),
            tip.commit_id
        );
        let recovered = coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(recovered.has_work);
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("fix")).unwrap(),
            "unpublished fix"
        );
    }
}

#[tokio::test]
async fn conflicted_preserved_bookmark_releases_clean_workspace_without_moving_variants() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for title in ["First variant", "Second variant"] {
        JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &tip.commit_id, "-m", title]);
    }
    let name = recovery::preserved_name(&execution.id);
    let variants = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""],
    );
    assert_eq!(variants.lines().count(), 2);
    JjRepo::run(&repo.replacement, &["new", "main"]);
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "retry",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        ),
        variants
    );
    assert!(db.preserved_tip(&execution.id).unwrap().is_some());
}

#[tokio::test]
async fn missing_bookmark_failed_restore_then_release_keeps_recorded_tip() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &recovery::preserved_name(&execution.id)],
    );
    rusqlite::Connection::open(&db_path).unwrap().execute(
        "UPDATE work_executions SET cube_lease_id = 'retry', cube_workspace_id = 'retry', workspace_path = ?2 WHERE id = ?1",
        rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
    ).unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let (coordinator, _) = coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        workspace_id: "retry".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "retry".into(),
        dirty_verified: None,
    };
    let failed = Arc::new(FailedRestore {
        integrity: false,
        positions: Default::default(),
    });
    let adapter: Arc<dyn HostAdapter> = failed.clone();
    let err = coordinator
        .recover_execution_bookmark_for_test(&execution, &lease, &adapter, "mono", Some(99), Some(99))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("SSH transport unavailable"));
    let outcome = apply_release(
        &db,
        &execution,
        "retry",
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert_eq!(db.preserved_tip(&execution.id).unwrap(), Some(tip.clone()));
    let recovered = coordinator
        .recover_execution_bookmark_for_test(
            &execution,
            &lease,
            &coordinator.host_adapter(),
            "mono",
            Some(99),
            Some(99),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

#[tokio::test]
async fn conflicted_preserved_bookmark_is_resolved_so_later_work_can_be_preserved() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for title in ["First variant", "Second variant"] {
        JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &tip.commit_id, "-m", title]);
    }
    let name = recovery::preserved_name(&execution.id);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        )
        .lines()
        .count(),
        2
    );
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', cube_workspace_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let (coordinator, _) = coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        workspace_id: "retry".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "retry".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99)
            )
            .await
            .unwrap()
            .unwrap()
            .has_work
    );
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        )
        .lines()
        .count(),
        1
    );
    JjRepo::run(&repo.replacement, &["new", "-m", "Additional work"]);
    std::fs::write(repo.replacement.join("extra"), "later unpublished").unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "retry",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    let updated = db.preserved_tip(&execution.id).unwrap().unwrap();
    assert_eq!(
        JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", &name, "-T", "commit_id"]),
        updated.commit_id
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("extra")).unwrap(),
        "later unpublished"
    );
    assert!(
        db.list_attention_items(&execution.id).unwrap().iter().any(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_ARCHIVED_ATTENTION_KIND
                && item.body_markdown.contains("preserved-archive/")
        }),
        "unselected unpublished variants must raise archived-variant attention"
    );
}

#[tokio::test]
async fn inspect_verdict_releases_the_lease_and_raises_inspection_attention() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    JjRepo::run(&repo.replacement, &["new", "main"]);
    std::fs::write(repo.replacement.join("other"), "another occupant").unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "retry",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert_eq!(db.preserved_tip(&execution.id).unwrap(), Some(tip.clone()));
    let items = db.list_attention_items(&execution.id).unwrap();
    let inspect = items
        .iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
        })
        .expect("teardown inspect verdict must raise inspection attention");
    assert!(
        inspect.body_markdown.contains(&tip.commit_id),
        "{}",
        inspect.body_markdown
    );
    assert!(
        inspect.body_markdown.contains("unpublished"),
        "{}",
        inspect.body_markdown
    );
}

#[tokio::test]
async fn second_inspect_teardown_appends_the_new_workspace_tip() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let first = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &first).unwrap();
    JjRepo::run(&repo.replacement, &["new", "main"]);
    std::fs::write(repo.replacement.join("other"), "another occupant").unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        apply_release(
            &db,
            &execution,
            "retry",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &DbBookmarkGate { db: &db, adapter: None },
            },
            async { Ok(()) },
        )
        .await,
        LeaseReleaseOutcome::Released
    );
    let first_commit = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let later = dir.path().join("later");
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            later.to_str().unwrap(),
            "--name",
            "later",
            "-r",
            "main",
        ],
    );
    JjRepo::run(&later, &["new", "-m", "Later unpublished tip"]);
    std::fs::write(later.join("later"), "later unpublished").unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'later', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, later.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        apply_release(
            &db,
            &execution,
            "later",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &DbBookmarkGate { db: &db, adapter: None },
            },
            async { Ok(()) },
        )
        .await,
        LeaseReleaseOutcome::Released
    );
    let second_commit = JjRepo::run(&later, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let inspect = db
        .list_attention_items(&execution.id)
        .unwrap()
        .into_iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
                && item.status == "open"
        })
        .expect("inspect attention stays open across teardowns");
    assert!(
        inspect.body_markdown.contains(&first_commit),
        "{}",
        inspect.body_markdown
    );
    assert!(
        inspect.body_markdown.contains(&second_commit),
        "{}",
        inspect.body_markdown
    );
}

#[tokio::test]
async fn abandoning_a_restaged_tip_clears_the_row_and_is_not_restaged() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    db.record_preserved_tip(&execution.id, &tip).unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let (coordinator, _) = coordinator_for(db.clone());
    let lease = CubeWorkspaceLease {
        workspace_id: "retry".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "retry".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99)
            )
            .await
            .unwrap()
            .unwrap()
            .has_work
    );
    JjRepo::run(&repo.replacement, &["abandon", "@"]);
    assert_eq!(
        apply_release(
            &db,
            &execution,
            "retry",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &DbBookmarkGate { db: &db, adapter: None },
            },
            async { Ok(()) },
        )
        .await,
        LeaseReleaseOutcome::Released
    );
    assert!(db.preserved_tip(&execution.id).unwrap().is_none());
    let successor = dir.path().join("successor");
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            successor.to_str().unwrap(),
            "--name",
            "successor",
            "-r",
            "main",
        ],
    );
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'successor', workspace_path = ?2, status = 'ready' WHERE id = ?1",
            rusqlite::params![execution.id, successor.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let successor_lease = CubeWorkspaceLease {
        workspace_id: "successor".into(),
        workspace_path: successor.clone(),
        lease_id: "successor".into(),
        dirty_verified: None,
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(
                &execution,
                &successor_lease,
                &coordinator.host_adapter(),
                "mono",
                Some(99),
                Some(99)
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(!successor.join("fix").exists());
}

#[tokio::test]
async fn absent_recorded_tip_bookmarks_fresh_unpublished_work_and_releases() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let db_path = dir.path().join("boss.db");
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&db_path);
    export_origin(&repo);
    let record = recovery::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    db.record_preserved_tip(
        &execution.id,
        &PreservedTip {
            change_id: "kqkqkqkqkqkqkqkqkqkqkqkqkqkqkqkq".into(),
            commit_id: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into(),
            count: 1,
        },
    )
    .unwrap();
    JjRepo::run(&repo.replacement, &["new", "main"]);
    std::fs::write(repo.replacement.join("fresh"), "new unpublished").unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'retry', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.replacement.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        apply_release(
            &db,
            &execution,
            "retry",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &DbBookmarkGate { db: &db, adapter: None },
            },
            async { Ok(()) },
        )
        .await,
        LeaseReleaseOutcome::Released
    );
    let tip = db.preserved_tip(&execution.id).unwrap().expect("fresh tip recorded");
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &recovery::preserved_name(&execution.id),
                "-T",
                "commit_id",
            ],
        ),
        tip.commit_id
    );
    let inspect = db
        .list_attention_items(&execution.id)
        .unwrap()
        .into_iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
        })
        .expect("absent recorded commit must be named");
    assert!(
        inspect.body_markdown.contains(&tip.commit_id),
        "{}",
        inspect.body_markdown
    );
    assert!(
        inspect
            .body_markdown
            .contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        "{}",
        inspect.body_markdown
    );
}

#[tokio::test]
async fn db_bookmark_gate_holds_without_a_preservation_source() {
    let dir = tempdir().unwrap();
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    db.create_attention_item(boss_protocol::CreateAttentionItemInput {
        execution_id: Some(execution.id.clone()),
        work_item_id: None,
        kind: boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "bookmark create failed".into(),
        body_markdown: "store unavailable".into(),
        resolved_at: None,
    })
    .unwrap();
    let status = DbBookmarkGate { db: &db, adapter: None }.status(&execution).await;
    assert!(
        matches!(status, BookmarkStatus::Unconfirmed(ref msg) if msg.contains("store location is unavailable")),
        "{status:?}"
    );
}

#[tokio::test]
async fn db_bookmark_gate_holds_a_remote_row_without_an_adapter() {
    let dir = tempdir().unwrap();
    let (db, execution, _) = super::super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    db.record_execution_bookmark(&ExecutionBookmark {
        execution_id: execution.id.clone(),
        repo_path: dir.path().join("store"),
        host_id: "anaplian".into(),
        base_commit: None,
    })
    .unwrap();
    let status = DbBookmarkGate { db: &db, adapter: None }.status(&execution).await;
    assert!(
        matches!(status, BookmarkStatus::Unconfirmed(ref msg) if msg.contains("owning host adapter is unavailable")),
        "{status:?}"
    );
}
