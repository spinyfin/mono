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
    let record = execution_bookmark::create(&LocalJj, workspace, &execution.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    execution
}

#[tokio::test]
async fn real_gate_confirms_snapshot_and_rejects_missing_or_stale_head() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    let gate = DbBookmarkGate { db: &db, adapter: None };
    std::fs::write(repo.worker.join("work.txt"), "preserve this work").unwrap();
    assert_eq!(gate.status(&execution).await, BookmarkStatus::Confirmed);
    // A trailing `jj new` leaves the head behind @; the gate advances it.
    LocalJj
        .run(&repo.worker, &["new", "-m", "Trailing change"])
        .await
        .unwrap();
    std::fs::write(repo.worker.join("more.txt"), "moved past the head").unwrap();
    assert_eq!(gate.status(&execution).await, BookmarkStatus::Confirmed);
    let record = db.execution_bookmark(&execution.id).unwrap();
    let at = |rev: &str| {
        let worker = repo.worker.clone();
        let rev = rev.to_owned();
        async move {
            LocalJj
                .run(&worker, &["log", "--no-graph", "-r", &rev, "-T", "commit_id"])
                .await
                .unwrap()
        }
    };
    assert_eq!(at(&record.head()).await, at("@").await);
    // A workspace commit on a different line than the head is never covered.
    LocalJj
        .run(&repo.worker, &["new", &record.base(), "-m", "Unbookmarked work"])
        .await
        .unwrap();
    std::fs::write(repo.worker.join("other.txt"), "unpreserved").unwrap();
    assert!(matches!(gate.status(&execution).await, BookmarkStatus::Unconfirmed(_)));
    LocalJj.run(&repo.worker, &["new", &record.head()]).await.unwrap();
    assert_eq!(gate.status(&execution).await, BookmarkStatus::Confirmed);
    LocalJj
        .run(&repo.worker, &["bookmark", "delete", &record.head()])
        .await
        .unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "lease",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &gate,
        },
        async { panic!("missing recovery pointer must hold the lease") },
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::BookmarkUnconfirmed
        }
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease")
    );
    assert!(
        db.list_attention_items(&execution.id)
            .unwrap()
            .iter()
            .any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND)
    );
}

#[tokio::test]
async fn failed_backup_prevents_release_even_with_a_valid_pointer() {
    let (dir, db) = open_db();
    let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
    let execution = fixture(&db, &repo.worker).await;
    let record = db.execution_bookmark(&execution.id).unwrap();
    let failure = LocalJj
        .run(
            &repo.worker,
            &["bookmark", "set", &record.head(), "-r", "nonexistent_revision"],
        )
        .await
        .unwrap_err();
    crate::execution_bookmark_recovery::report_failure(&db, &execution, &format!("{failure:#}"));
    assert!(matches!(
        DbBookmarkGate { db: &db, adapter: None }.status(&execution).await,
        BookmarkStatus::Unconfirmed(_)
    ));
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
    let record = execution_bookmark::create(&LocalJj, &repo.worker, &execution.id, "local")
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
    let record = execution_bookmark::create(&LocalJj, &repo.worker, &execution.id, "local")
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
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
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
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
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
            bookmark: &DbBookmarkGate { db: &db, adapter: None },
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert!(open_kinds(&db, &execution.id).is_empty());
}
