use super::*;
use crate::test_support::*;
use boss_engine_recovery::execution_bookmark::{self, Jj, LocalJj};
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

/// A scan seam that must never run.
async fn forbidden_scan(why: &str) -> EmptyOccupancy {
    panic!("{why}")
}

async fn fixture(db: &WorkDb, workspace: &Path) -> WorkExecution {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id, "recovery release");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .cube_lease_id("lease")
                .cube_workspace_id("workspace")
                .workspace_path(workspace.display().to_string())
                .build(),
        )
        .unwrap();
    let record = boss_engine_recovery_fixtures::create(&LocalJj, workspace, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    execution
}

#[tokio::test]
async fn teardown_preserves_workspace_even_when_legacy_pointers_are_missing() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    let record = db.execution_bookmark(&execution.id).unwrap();
    LocalJj
        .run(&repo.worker, &["bookmark", "delete", &record.head()])
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work.txt"), "preserve this work").unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    let tip = db.preserved_tip(&execution.id).unwrap().unwrap();
    let actual = LocalJj
        .run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &execution_bookmark::preserved_name(&execution.id),
                "-T",
                "commit_id",
            ],
        )
        .await
        .unwrap();
    assert_eq!(actual, tip.commit_id);
    assert!(tip.count > 0);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

#[tokio::test]
async fn teardown_overwrites_a_stale_preserved_tip_with_later_unpublished_work() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    std::fs::write(repo.worker.join("first.txt"), "first unpublished").unwrap();
    let first = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(first, LeaseReleaseOutcome::Released);
    let frozen = db.preserved_tip(&execution.id).unwrap().unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET cube_lease_id = 'lease', cube_workspace_id = 'workspace', workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, repo.worker.to_str().unwrap()],
        )
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    LocalJj
        .run(&repo.worker, &["new", "-m", "Later unpublished work"])
        .await
        .unwrap();
    std::fs::write(repo.worker.join("later.txt"), "later unpublished").unwrap();
    let second = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(second, LeaseReleaseOutcome::Released);
    let updated = db.preserved_tip(&execution.id).unwrap().unwrap();
    assert_ne!(updated.commit_id, frozen.commit_id);
    assert_eq!(
        LocalJj
            .run(
                &repo.repo,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &execution_bookmark::preserved_name(&execution.id),
                    "-T",
                    "commit_id",
                ],
            )
            .await
            .unwrap(),
        updated.commit_id
    );
}

#[tokio::test]
async fn failed_legacy_backup_does_not_prevent_teardown_preservation() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    crate::execution_bookmark_recovery::report_failure(&db, &execution, "legacy backup failed");
    std::fs::write(repo.worker.join("work.txt"), "preserve independently of backup").unwrap();
    assert_eq!(
        DbBookmarkGate::new(&db, None).status(&execution).await,
        BookmarkStatus::Confirmed
    );
    assert!(db.preserved_tip(&execution.id).unwrap().is_some());
}

struct UnavailableRegistry;
crate::stub_cube_client! { UnavailableRegistry {
    async fn list_workspaces(&self) -> Result<Vec<boss_execution_contracts::CubeWorkspaceStatus>> {
        anyhow::bail!("cube registry unavailable")
    }
} }

#[tokio::test]
async fn inferred_registry_failure_keeps_lease_but_proven_stop_preserves_live_work() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    std::fs::write(repo.worker.join("work"), "latest unsnapshotted edits").unwrap();
    let gate = DbBookmarkGate::new(&db, None).with_cube(&UnavailableRegistry);
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &gate,
        },
        async { panic!("unconfirmed ownership must not release") },
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::BookmarkUnconfirmed
        }
    );
    assert!(db.preserved_tip(&execution.id).unwrap().is_none());
    assert_eq!(
        gate.status_for_cause(&execution, TerminalCause::ProvenStopped).await,
        BookmarkStatus::Confirmed
    );
    assert!(db.preserved_tip(&execution.id).unwrap().is_some());
}

#[tokio::test]
async fn unresolved_first_teardown_raises_inspection_without_prior_record() {
    for boundary in [None, Some(1)] {
        let (dir, db) = open_db();
        let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
        let execution = fixture(&db, &repo.worker).await;
        if let Some(boundary) = boundary {
            db.confirm_workspace_lease(&execution.id, "lease", boundary).unwrap();
        }
        assert!(db.preserved_tip(&execution.id).unwrap().is_none());
        let gate = DbBookmarkGate::new(&db, None).with_cube(&OkCube);
        assert_eq!(
            gate.status_for_cause(&execution, TerminalCause::Inferred).await,
            BookmarkStatus::Confirmed
        );
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(items.iter().any(|item| item.kind == "preserved_work_needs_inspection"));
    }
}

/// The registry row cube would report for the test workspace.
fn registry_row(
    workspace: &Path,
    state: &str,
    lease_id: Option<&str>,
    generation: Option<i64>,
) -> boss_execution_contracts::CubeWorkspaceStatus {
    boss_execution_contracts::CubeWorkspaceStatus::builder()
        .workspace_id("worker")
        .workspace_path(workspace.to_path_buf())
        .state(state)
        .maybe_lease_id(lease_id.map(str::to_owned))
        .maybe_lease_generation(generation)
        .build()
}

struct HandoffRegistry(Vec<boss_execution_contracts::CubeWorkspaceStatus>);
crate::stub_cube_client! { HandoffRegistry {
    async fn release_workspace(&self, _: &str) -> Result<()> { Ok(()) }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> { Ok(()) }
    async fn list_workspaces(&self) -> Result<Vec<boss_execution_contracts::CubeWorkspaceStatus>> { Ok(self.0.clone()) }
} }

#[tokio::test]
async fn leftover_sweep_uses_original_heartbeat_after_intervening_holders() {
    use boss_engine_test_git::jj::JjRepo;
    for state in ["free", "unlisted", "leased"] {
        let (dir, db) = open_db();
        let repo = JjRepo::new(dir.path());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "intervening holders");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker",
            "mono",
            "lease",
            "worker",
            repo.worker.to_str().unwrap(),
        )
        .unwrap();
        let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("ours"), "original worker").unwrap();
        let ours = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
        db.confirm_workspace_lease(
            &execution.id,
            "lease",
            boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        )
        .unwrap();
        db.record_workspace_lease_generation(&execution.id, "lease", 1).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        JjRepo::run(&repo.worker, &["new", "root()", "-m", "Intervening holder"]);
        std::fs::write(repo.worker.join("theirs"), "released holder").unwrap();
        let intervening = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
        if state == "leased" {
            JjRepo::run(&repo.worker, &["new", "root()", "-m", "Third holder"]);
            std::fs::write(repo.worker.join("third"), "current holder").unwrap();
        }
        let current = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
        let registry = HandoffRegistry(if state == "unlisted" {
            vec![]
        } else {
            // Other leases were claimed after this execution's (generation 1).
            vec![registry_row(
                &repo.worker,
                state,
                (state == "leased").then_some("third-lease"),
                Some(if state == "leased" { 3 } else { 2 }),
            )]
        });
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        db.set_run_shell_pid_for_execution(&execution.id, i64::from(pid))
            .unwrap();
        db.mark_execution_orphaned(&execution.id, "original worker exited")
            .unwrap();
        let outcome = run_one_pass_with_occupancy(&db, &registry, &LiveWorkerStateRegistry::new(), || async {
            EmptyOccupancy
        })
        .await;
        assert_eq!(outcome.released, 1, "{state}: {outcome:?}");
        assert_eq!(db.preserved_tip(&execution.id).unwrap().unwrap().commit_id, ours);
        // Another holder's stack is reported only when cube shows no evidence
        // of a later holder. A workspace leased under another id, or free
        // with an advanced claim counter, is theirs: never snapshotted and
        // never reported. An unlisted workspace has no such evidence.
        assert_eq!(
            open_kinds(&db, &execution.id).contains(&"preserved_work_needs_inspection".to_owned()),
            state == "unlisted",
            "{state}: {:?}",
            open_kinds(&db, &execution.id)
        );
        let name = execution_bookmark::preserved_name(&execution.id);
        assert_eq!(
            JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", &name, "-T", "commit_id"]),
            ours
        );
        assert_eq!(
            JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]),
            current
        );
        assert_eq!(
            JjRepo::run(
                &repo.repo,
                &["log", "--no-graph", "-r", &intervening, "-T", "commit_id"]
            ),
            intervening
        );
    }
}

#[tokio::test]
async fn reaped_worker_preserves_exactly_one_bookmark_only_for_unpushed_work() {
    use boss_engine_test_git::jj::JjRepo;
    for published in [false, true] {
        let (dir, db) = open_db();
        let repo = JjRepo::new(dir.path());
        JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
        JjRepo::run(&repo.repo, &["git", "export"]);
        JjRepo::run(
            &repo.repo,
            &[
                "git",
                "remote",
                "add",
                "origin",
                repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
            ],
        );
        JjRepo::run(&repo.repo, &["git", "fetch"]);
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "reaped worker");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker",
            "mono",
            "lease",
            "worker",
            repo.worker.to_str().unwrap(),
        )
        .unwrap();
        let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
            .await
            .unwrap();
        db.record_execution_bookmark(&record).unwrap();
        // Edits are left unsnapshotted: no jj command runs before the worker dies.
        std::fs::write(repo.worker.join("work.txt"), "worker edits").unwrap();
        db.confirm_workspace_lease(
            &execution.id,
            "lease",
            boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        )
        .unwrap();
        db.record_workspace_lease_generation(&execution.id, "lease", 1).unwrap();
        if published {
            JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
            JjRepo::run(&repo.worker, &["git", "export"]);
            JjRepo::run(&repo.repo, &["git", "fetch"]);
            JjRepo::run(&repo.worker, &["new", "-m", "Trailing empty change"]);
        }
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            db.set_run_shell_pid_for_execution(&execution.id, i64::from(pid))
                .unwrap()
        );
        db.mark_execution_orphaned(&execution.id, "reaped worker").unwrap();
        let before = JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
        // Free again with the claim counter unchanged: no one else held it.
        let registry = HandoffRegistry(vec![registry_row(&repo.worker, "free", None, Some(1))]);
        let outcome = run_one_pass_with_occupancy(&db, &registry, &LiveWorkerStateRegistry::new(), || async {
            EmptyOccupancy
        })
        .await;
        assert_eq!(outcome.released, 1);
        let after = JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
        assert_eq!(after.lines().count(), before.lines().count() + usize::from(!published));
        assert_eq!(db.preserved_tip(&execution.id).unwrap().is_some(), !published);
        if !published {
            assert!(after.contains(&execution_bookmark::preserved_name(&execution.id)));
        }
    }
}

#[tokio::test]
async fn remote_rows_never_reach_local_release_or_occupancy() {
    struct NoLocalCube;
    crate::stub_cube_client! { NoLocalCube {} }
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "remote release");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run_on_host(
        &execution.id,
        "worker",
        "repo",
        "remote-lease",
        "workspace",
        "/remote/workspace",
        "remote",
    )
    .unwrap();
    db.mark_execution_orphaned(&execution.id, "remote worker ended")
        .unwrap();
    assert!(db.list_terminal_executions_holding_lease().unwrap().is_empty());
    assert_eq!(
        run_one_pass_with_occupancy(&db, &NoLocalCube, &LiveWorkerStateRegistry::new(), || forbidden_scan(
            "remote row must not scan local processes"
        ))
        .await,
        EndedRunLeaseSweepOutcome::default()
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("remote-lease")
    );
}

#[tokio::test]
async fn sweep_requires_both_durable_pane_death_and_empty_cwds() {
    struct RecordingCube(std::sync::atomic::AtomicUsize);
    crate::stub_cube_client! { RecordingCube {
        async fn release_workspace(&self, _: &str) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn release_workspace_with_reset(&self, _: &str) -> Result<()> { panic!("sweep must never force-reset") }
    } }
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "sweep safety");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker",
        "repo",
        "lease",
        "workspace",
        repo.worker.to_str().unwrap(),
    )
    .unwrap();
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    db.set_run_shell_pid_for_execution(&execution.id, i64::from(std::process::id()))
        .unwrap();
    db.mark_execution_orphaned(&execution.id, "unregistered worker")
        .unwrap();
    let cube = RecordingCube(std::sync::atomic::AtomicUsize::new(0));
    let live = LiveWorkerStateRegistry::new();
    let outcome = run_one_pass_with_occupancy(&db, &cube, &live, || {
        forbidden_scan("live durable pid must hold before scanning")
    })
    .await;
    assert_eq!((outcome.held, outcome.kept), (1, 0));

    live.register_spawn(0, &execution.id, "test", std::process::id() as i32, None);
    live.set_activity_for_test(0, boss_protocol::WorkerActivity::Terminated);
    assert!(!live.is_run_live(&execution.id));
    assert_eq!(
        run_one_pass_with_occupancy(&db, &cube, &live, || forbidden_scan(
            "terminal registry entry still owns its pane"
        ))
        .await
        .held,
        1
    );
    live.release_slot(0);

    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    db.set_run_shell_pid_for_execution(&execution.id, i64::from(pid))
        .unwrap();
    let occupied = || async {
        ProcessSnapshot {
            cwds: vec![(std::process::id() + 1, repo.worker.canonicalize().unwrap())],
            complete: true,
        }
    };
    assert_eq!(run_one_pass_with_occupancy(&db, &cube, &live, occupied).await.kept, 1);
    assert_eq!(cube.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        run_one_pass_with_occupancy(&db, &cube, &live, || async { EmptyOccupancy })
            .await
            .released,
        1
    );
    assert_eq!(cube.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

#[tokio::test]
async fn sweep_retries_a_cancelled_row_after_a_transient_cube_failure() {
    struct FlakyCube(std::sync::atomic::AtomicUsize);
    crate::stub_cube_client! { FlakyCube {
        async fn release_workspace(&self, _: &str) -> Result<()> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                anyhow::bail!("cube temporarily unavailable");
            }
            Ok(())
        }
        async fn release_workspace_with_reset(&self, _: &str) -> Result<()> { panic!("sweep must never force-reset") }
    } }
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "cancelled retry");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker",
        "repo",
        "lease",
        "workspace",
        repo.worker.to_str().unwrap(),
    )
    .unwrap();
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    db.set_run_shell_pid_for_execution(&execution.id, i64::from(pid))
        .unwrap();
    db.cancel_execution(&execution.id).unwrap();
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Cancelled
    );

    let cube = FlakyCube(std::sync::atomic::AtomicUsize::new(0));
    let live = LiveWorkerStateRegistry::new();
    let first = run_one_pass_with_occupancy(&db, &cube, &live, || async { EmptyOccupancy }).await;
    assert_eq!((first.released, first.kept), (0, 1));
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease")
    );
    let second = run_one_pass_with_occupancy(&db, &cube, &live, || async { EmptyOccupancy }).await;
    assert_eq!(second.released, 1);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

struct OkCube;
crate::stub_cube_client! { OkCube {
    async fn release_workspace(&self, _: &str) -> Result<()> { Ok(()) }
    async fn release_workspace_with_reset(&self, _: &str) -> Result<()> { panic!("sweep must never force-reset") }
} }

fn open_kinds(db: &WorkDb, execution_id: &str) -> Vec<String> {
    db.list_attention_items(execution_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.status == "open")
        .map(|item| item.kind)
        .collect()
}

/// A spawn that failed before it recorded a pid has no durable death proof,
/// so the sweep holds it until a release that passed every gate fails in
/// cube; that failure is the proof, and the next pass retries and lowers it.
#[tokio::test]
async fn sweep_retries_a_pidless_row_only_after_a_gated_cube_failure() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    let live = LiveWorkerStateRegistry::new();
    let held = run_one_pass_with_occupancy(&db, &OkCube, &live, || forbidden_scan("no proof yet")).await;
    assert_eq!((held.held, held.released), (1, 0));

    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenStopped,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { anyhow::bail!("cube temporarily unavailable") },
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }), "{outcome:?}");
    assert!(open_kinds(&db, &execution.id).contains(&LEASE_RELEASE_FAILED_ATTENTION_KIND.to_owned()));

    let swept = run_one_pass_with_occupancy(&db, &OkCube, &live, || async { EmptyOccupancy }).await;
    assert_eq!(swept.released, 1);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
    assert!(!open_kinds(&db, &execution.id).contains(&LEASE_RELEASE_FAILED_ATTENTION_KIND.to_owned()));
}

/// A run that never started holds only what cube placed there, so the sweep
/// can retry its release without a bookmark once a gated release failed.
#[tokio::test]
async fn sweep_retries_a_never_started_row_without_a_bookmark() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "never started");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Failed)
                .cube_lease_id("lease")
                .cube_workspace_id("workspace")
                .workspace_path("/tmp/never-started-workspace")
                .build(),
        )
        .unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &NeverPositioned,
        },
        async { anyhow::bail!("cube temporarily unavailable") },
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }), "{outcome:?}");
    let swept = run_one_pass_with_occupancy(&db, &OkCube, &LiveWorkerStateRegistry::new(), || async {
        EmptyOccupancy
    })
    .await;
    assert_eq!(swept.released, 1);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

/// A retained lease with an open bookmark creation or backup failure keeps its
/// bookmark gate even though no bookmark row was ever recorded.
#[tokio::test]
async fn open_bookmark_failure_keeps_the_gate_for_a_retained_lease() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "retained");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .cube_lease_id("lease")
                .build(),
        )
        .unwrap();
    assert!(never_positioned(
        &db,
        &execution.id,
        PreStartStage::BeforeRecoveryBookmark
    ));
    assert!(!never_positioned(
        &db,
        &execution.id,
        PreStartStage::AtOrAfterRecoveryBookmark
    ));
    crate::execution_bookmark_recovery::report_failure(&db, &execution, "bookmark write failed");
    assert!(!never_positioned(
        &db,
        &execution.id,
        PreStartStage::BeforeRecoveryBookmark
    ));
    // A later attempt's own success resolves the failure, so it no longer gates.
    db.resolve_attention_kind_for_execution(&execution.id, crate::execution_bookmark_recovery::RECOVERY_FAILED)
        .unwrap();
    assert!(never_positioned(
        &db,
        &execution.id,
        PreStartStage::BeforeRecoveryBookmark
    ));
}

/// Pane and occupancy verdicts come before bookmark confirmation, which
/// would otherwise advance the recovery head under a possibly-live worker.
#[tokio::test]
async fn occupied_workspace_does_not_advance_the_recovery_head() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    let head_at = || {
        let worker = repo.worker.clone();
        let head = db.execution_bookmark(&execution.id).unwrap().head();
        async move {
            LocalJj
                .run(&worker, &["log", "--no-graph", "-r", &head, "-T", "commit_id"])
                .await
                .unwrap()
        }
    };
    let before = head_at().await;
    LocalJj
        .run(&repo.worker, &["new", "-m", "Past the head"])
        .await
        .unwrap();
    std::fs::write(repo.worker.join("later.txt"), "newer than the head").unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &ProcessSnapshot {
                cwds: vec![(std::process::id() + 1, repo.worker.canonicalize().unwrap())],
                complete: true,
            },
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { panic!("an occupied workspace is not released") },
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::ProcessInWorkspace
        }
    );
    assert_eq!(head_at().await, before, "the recovery head must not move");
    assert!(open_kinds(&db, &execution.id).is_empty());
}

#[test]
fn zombie_and_dead_processes_are_not_occupants() {
    assert!(proc_state_is_gone("Name:\tsh\nState:\tZ (zombie)\nPid:\t1\n"));
    assert!(proc_state_is_gone("State:\tX (dead)\n"));
    assert!(!proc_state_is_gone("Name:\tsh\nState:\tS (sleeping)\n"));
    assert!(!proc_state_is_gone("Name:\tsh\n"));
}

#[test]
fn unreadable_same_uid_cwd_is_inconclusive_but_other_users_are_not() {
    let denied = || std::io::Error::from_raw_os_error(libc::EACCES);
    let mut snapshot = ProcessSnapshot {
        cwds: Vec::new(),
        complete: true,
    };
    // SAFETY: geteuid has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    snapshot.record(1, Err(denied()), || Some(own_uid.wrapping_add(1)));
    assert!(snapshot.complete, "another user's process cannot be an occupant");
    snapshot.record(2, Err(denied()), || Some(own_uid));
    assert!(
        !snapshot.complete,
        "a same-uid process with an unreadable cwd is never assumed away"
    );
}

#[tokio::test]
async fn successful_release_resolves_both_lease_attention_kinds() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    raise_unreleased_attention(&db, &execution, "held", "held");
    raise_attention(&db, &execution, LEASE_RELEASE_FAILED_ATTENTION_KIND, "failed", "failed");
    assert_eq!(open_kinds(&db, &execution.id).len(), 2);
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &DbBookmarkGate::new(&db, None),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert!(open_kinds(&db, &execution.id).is_empty());
}

/// Start an execution on a fresh worker workspace whose process has died.
async fn orphaned_worker(db: &WorkDb, repo: &boss_engine_test_git::jj::JjRepo) -> WorkExecution {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id, "orphaned worker");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker",
        "mono",
        "lease",
        "worker",
        repo.worker.to_str().unwrap(),
    )
    .unwrap();
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    execution
}

fn kill_worker(db: &WorkDb, execution: &WorkExecution) {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    db.set_run_shell_pid_for_execution(&execution.id, i64::from(pid))
        .unwrap();
    db.mark_execution_orphaned(&execution.id, "worker exited").unwrap();
}

/// Files in the tree `preserved/<exec>` points at, or empty when it is unset.
fn preserved_files(repo: &boss_engine_test_git::jj::JjRepo, execution: &WorkExecution) -> String {
    let name = execution_bookmark::preserved_name(&execution.id);
    let listed = boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
    if listed.lines().any(|line| line == name) {
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["file", "list", "-r", &name])
    } else {
        String::new()
    }
}

/// A successor leased the workspace dirty (claim counter bumped), edited the
/// inherited change, and force-released it. Whether or not it ran a jj command,
/// its edits must never become `preserved/<exec>`.
#[tokio::test]
async fn leftover_sweep_never_adopts_an_allow_dirty_successors_edits() {
    use boss_engine_test_git::jj::JjRepo;
    for successor_ran_jj in [true, false] {
        let (dir, db) = open_db();
        let repo = JjRepo::new(dir.path());
        let execution = orphaned_worker(&db, &repo).await;
        std::fs::write(repo.worker.join("ours"), "original worker").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        db.confirm_workspace_lease(
            &execution.id,
            "lease",
            boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        )
        .unwrap();
        db.record_workspace_lease_generation(&execution.id, "lease", 1).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        std::fs::write(repo.worker.join("theirs"), "allow-dirty successor").unwrap();
        if successor_ran_jj {
            JjRepo::run(&repo.worker, &["status"]);
        }
        kill_worker(&db, &execution);
        let registry = HandoffRegistry(vec![registry_row(&repo.worker, "free", None, Some(2))]);
        let outcome = run_one_pass_with_occupancy(&db, &registry, &LiveWorkerStateRegistry::new(), || async {
            EmptyOccupancy
        })
        .await;
        assert_eq!(outcome.released, 1, "{outcome:?}");
        let preserved = preserved_files(&repo, &execution);
        assert!(
            !preserved.contains("theirs"),
            "ran_jj={successor_ran_jj}: the successor's edits were bookmarked: {preserved}"
        );
        if let Some(tip) = db.preserved_tip(&execution.id).unwrap() {
            let files = JjRepo::run(&repo.repo, &["file", "list", "-r", &tip.commit_id]);
            assert!(!files.contains("theirs"), "ran_jj={successor_ran_jj}: {files}");
        }
        assert!(open_kinds(&db, &execution.id).contains(&"preserved_work_needs_inspection".to_owned()));
    }
}

/// A free workspace with no ownership evidence: the dead worker's unsnapshotted
/// edits after its last confirmed boundary are reported, not dropped.
#[tokio::test]
async fn leftover_sweep_reports_unsnapshotted_work_in_a_free_workspace_without_ownership_evidence() {
    use boss_engine_test_git::jj::JjRepo;
    for action in ["edit", "new"] {
        let (dir, db) = open_db();
        let repo = JjRepo::new(dir.path());
        let execution = orphaned_worker(&db, &repo).await;
        std::fs::write(repo.worker.join("ours"), "original worker").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        db.confirm_workspace_lease(
            &execution.id,
            "lease",
            boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if action == "new" {
            JjRepo::run(&repo.worker, &["new", "-m", "Next change"]);
            std::fs::write(repo.worker.join("next"), "unsnapshotted").unwrap();
        } else {
            std::fs::write(repo.worker.join("ours"), "edited after the boundary").unwrap();
        }
        kill_worker(&db, &execution);
        let registry = HandoffRegistry(vec![registry_row(&repo.worker, "free", None, None)]);
        let outcome = run_one_pass_with_occupancy(&db, &registry, &LiveWorkerStateRegistry::new(), || async {
            EmptyOccupancy
        })
        .await;
        assert_eq!(outcome.released, 1, "{action}: {outcome:?}");
        assert!(
            open_kinds(&db, &execution.id).contains(&"preserved_work_needs_inspection".to_owned()),
            "{action}: post-boundary work was dropped silently"
        );
    }
}
