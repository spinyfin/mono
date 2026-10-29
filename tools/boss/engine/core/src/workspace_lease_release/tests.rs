use super::*;
use crate::test_support::*;
use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
use boss_protocol::{CreateExecutionInput, ExecutionKind};
use std::sync::Mutex;

struct SweepCube;
crate::stub_cube_client! { SweepCube {
    async fn release_workspace(&self, _: &str) -> Result<()> { Ok(()) }
    async fn release_workspace_with_reset(&self, _: &str) -> Result<()> { Ok(()) }
} }

struct FixedOccupancy(Occupancy);
impl WorkspaceOccupancy for FixedOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        self.0
    }
}

struct FixedBookmark(BookmarkStatus);
#[async_trait::async_trait]
impl BookmarkGate for FixedBookmark {
    async fn status(&self, _: &WorkExecution) -> BookmarkStatus {
        self.0.clone()
    }
}

fn decide_release(cause: TerminalCause) -> LeaseReleaseDecision {
    decide(cause, &BookmarkStatus::NotRequired, PaneStatus::Gone, Occupancy::Empty)
}

#[test]
fn proven_completed_releases() {
    assert_eq!(
        decide_release(TerminalCause::ProvenCompleted),
        LeaseReleaseDecision::Release
    );
}

#[test]
fn proven_never_started_releases() {
    assert_eq!(
        decide_release(TerminalCause::ProvenNeverStarted),
        LeaseReleaseDecision::Release
    );
}

#[test]
fn proven_stopped_releases_when_pane_gone() {
    assert_eq!(
        decide_release(TerminalCause::ProvenStopped),
        LeaseReleaseDecision::Release
    );
}

#[test]
fn proven_stopped_keeps_when_pane_present() {
    assert_eq!(
        decide(
            TerminalCause::ProvenStopped,
            &BookmarkStatus::NotRequired,
            PaneStatus::Present,
            Occupancy::Empty,
        ),
        LeaseReleaseDecision::Keep(KeepReason::PaneStillPresent)
    );
}

#[test]
fn inferred_releases_when_pane_gone_and_empty() {
    assert_eq!(decide_release(TerminalCause::Inferred), LeaseReleaseDecision::Release);
}

#[test]
fn inferred_keeps_when_a_process_is_in_the_workspace() {
    assert_eq!(
        decide(
            TerminalCause::Inferred,
            &BookmarkStatus::NotRequired,
            PaneStatus::Gone,
            Occupancy::Occupied,
        ),
        LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace)
    );
}

#[test]
fn inferred_keeps_when_occupancy_is_unknown() {
    assert_eq!(
        decide(
            TerminalCause::Inferred,
            &BookmarkStatus::NotRequired,
            PaneStatus::Gone,
            Occupancy::Unknown,
        ),
        LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed)
    );
}

#[test]
fn bookmark_unconfirmed_keeps_even_for_proven_completion() {
    assert_eq!(
        decide(
            TerminalCause::ProvenCompleted,
            &BookmarkStatus::Unconfirmed("jj bookmark missing".into()),
            PaneStatus::Gone,
            Occupancy::Empty,
        ),
        LeaseReleaseDecision::Keep(KeepReason::BookmarkUnconfirmed)
    );
}

fn seed_terminal(db: &WorkDb, status: ExecutionStatus, lease: &str) -> WorkExecution {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id.clone(), "lease-release");
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(status)
            .cube_lease_id(lease)
            .cube_workspace_id("ws-1")
            .workspace_path("/tmp/ws-lease-release")
            .build(),
    )
    .unwrap()
}

async fn apply_with(
    db: &WorkDb,
    execution: &WorkExecution,
    cause: TerminalCause,
    occupancy: Occupancy,
    bookmark: BookmarkStatus,
    release: Result<()>,
) -> LeaseReleaseOutcome {
    let lease = execution.cube_lease_id.clone().unwrap();
    apply_release(
        db,
        execution,
        &lease,
        ReleaseEvidence {
            cause,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(occupancy),
            bookmark: &FixedBookmark(bookmark),
        },
        async { release },
    )
    .await
}

#[tokio::test]
async fn apply_releases_completed_and_clears_columns() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-done");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::ProvenCompleted,
        Occupancy::Empty,
        BookmarkStatus::Confirmed,
        Ok(()),
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    let after = db.get_execution(&execution.id).unwrap();
    assert!(after.cube_lease_id.is_none());
    assert!(after.workspace_path.is_none());
}

#[tokio::test]
async fn apply_releases_never_started_failure() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Failed, "lease-prestart");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::ProvenNeverStarted,
        Occupancy::Occupied,
        BookmarkStatus::NotRequired,
        Ok(()),
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

#[tokio::test]
async fn apply_releases_cancelled_when_pane_gone() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Cancelled, "lease-stop");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::ProvenStopped,
        Occupancy::Empty,
        BookmarkStatus::NotRequired,
        Ok(()),
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
}

#[tokio::test]
async fn apply_keeps_inferred_when_a_process_is_in_the_workspace() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Orphaned, "lease-orphan");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::Inferred,
        Occupancy::Occupied,
        BookmarkStatus::NotRequired,
        Ok(()),
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::ProcessInWorkspace
        }
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-orphan")
    );
}

#[tokio::test]
async fn apply_keeps_and_files_attention_when_bookmark_is_unconfirmed() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-bm");
    let released = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&released);
    let outcome = apply_release(
        &db,
        &execution,
        "lease-bm",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            bookmark: &FixedBookmark(BookmarkStatus::Unconfirmed("bookmark write failed".into())),
        },
        async move {
            *flag.lock().unwrap() = true;
            Ok(())
        },
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::BookmarkUnconfirmed
        }
    );
    assert!(
        !*released.lock().unwrap(),
        "cube must not be called when the bookmark is unconfirmed"
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-bm")
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND && item.status == "open")
    );
}

#[tokio::test]
async fn apply_keeps_lease_and_files_attention_when_cube_release_fails() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-fail");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::ProvenCompleted,
        Occupancy::Empty,
        BookmarkStatus::NotRequired,
        Err(anyhow::anyhow!("cube unavailable")),
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }));
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-fail")
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(items.iter().any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND));
}

#[tokio::test]
async fn stale_keep_cannot_resurrect_a_released_lease() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-keep-restore");
    db.clear_execution_workspace(&execution.id).unwrap();
    let cleared = db.get_execution(&execution.id).unwrap();
    let outcome = apply_release(
        &db,
        &cleared,
        "lease-keep-restore",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            bookmark: &FixedBookmark(BookmarkStatus::Unconfirmed("bookmark write failed".into())),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::NoLease);
    assert_eq!(db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(), None);
}

#[tokio::test]
async fn stale_failure_cannot_resurrect_a_released_lease() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-restore");
    db.clear_execution_workspace(&execution.id).unwrap();
    let cleared = db.get_execution(&execution.id).unwrap();
    assert!(cleared.cube_lease_id.is_none());
    let outcome = apply_release(
        &db,
        &cleared,
        "lease-restore",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            bookmark: &FixedBookmark(BookmarkStatus::NotRequired),
        },
        async { Err(anyhow::anyhow!("cube timeout")) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::NoLease);
    assert_eq!(db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(), None);
}

#[tokio::test]
async fn sweep_keeps_completed_row_without_positive_death_evidence() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-sweep");
    let live = LiveWorkerStateRegistry::new();
    let outcome = run_one_pass(&db, &SweepCube, &live).await;
    assert_eq!(outcome.released, 0);
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-sweep")
    );
}

#[tokio::test]
async fn sweep_keeps_orphaned_row_when_occupancy_is_occupied() {
    let (_dir, db) = open_db();
    let dir = tempfile::TempDir::new().unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "occupied-orphan");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Orphaned)
                .cube_lease_id("lease-occ")
                .workspace_path(dir.path().display().to_string())
                .build(),
        )
        .unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    // HostOccupancy is what the sweep uses. Poll until the child cwd is
    // visible: a single short sleep misses on a loaded host, and the
    // occupancy scan itself can take tens of milliseconds.
    assert_eq!(wait_until_occupied(dir.path()), Occupancy::Occupied);
    let outcome = run_one_pass(&db, &SweepCube, &LiveWorkerStateRegistry::new()).await;
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(outcome.released, 0);
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-occ")
    );
}

#[tokio::test]
async fn db_bookmark_gate_rejects_a_record_without_a_pointer() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-rec");
    db.record_execution_bookmark(&ExecutionBookmark {
        execution_id: execution.id.clone(),
        repo_path: PathBuf::from("/tmp/repo"),
        host_id: "local".to_owned(),
        base_commit: None,
    })
    .unwrap();
    assert!(matches!(
        DbBookmarkGate { db: &db, adapter: None }.status(&execution).await,
        BookmarkStatus::Unconfirmed(_)
    ));
}

#[tokio::test]
async fn db_bookmark_gate_rejects_a_failed_or_missing_write() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Failed, "lease-none");
    assert!(matches!(
        DbBookmarkGate { db: &db, adapter: None }.status(&execution).await,
        BookmarkStatus::Unconfirmed(_)
    ));
}

#[test]
fn terminal_status_alone_never_proves_pane_death() {
    let (_dir, db) = open_db();
    for status in [
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Cancelled,
        ExecutionStatus::Orphaned,
        ExecutionStatus::Abandoned,
    ] {
        let execution = seed_terminal(&db, status, "held");
        assert_eq!(
            sweep_pane_status(&db, &LiveWorkerStateRegistry::new(), &execution),
            PaneStatus::Present
        );
    }
}

#[test]
fn host_occupancy_sees_a_child_with_cwd_in_the_workspace() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    let seen = wait_until_occupied(dir.path());
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(seen, Occupancy::Occupied);
}

fn wait_until_occupied(path: &Path) -> Occupancy {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let seen = HostOccupancy.occupancy(path);
        if seen == Occupancy::Occupied || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn complete_snapshot_of_an_empty_unique_dir_is_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_eq!(
        ProcessSnapshot {
            cwds: vec![],
            complete: true
        }
        .occupancy(dir.path()),
        Occupancy::Empty
    );
}
#[test]
fn inconclusive_cwd_and_path_probes_keep_the_lease() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut snapshot = ProcessSnapshot {
        cwds: vec![],
        complete: true,
    };
    snapshot.record(123, Err(std::io::Error::from_raw_os_error(libc::EPERM)));
    for occupancy in [
        snapshot.occupancy(dir.path()),
        snapshot.occupancy(&dir.path().join("missing")),
    ] {
        assert_eq!(occupancy, Occupancy::Unknown);
        assert_eq!(
            decide(
                TerminalCause::Inferred,
                &BookmarkStatus::Confirmed,
                PaneStatus::Gone,
                occupancy
            ),
            LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed)
        );
    }
}

#[tokio::test]
async fn lease_not_found_is_success_without_attention() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "missing");
    let error = crate::cube_commands::CubeCliError {
        host: "local".into(),
        exit_code: Some(5),
        stderr: "lease not found".into(),
        stdout: String::new(),
    };
    assert_eq!(
        apply_with(
            &db,
            &execution,
            TerminalCause::ProvenCompleted,
            Occupancy::Empty,
            BookmarkStatus::Confirmed,
            Err(error.into())
        )
        .await,
        LeaseReleaseOutcome::Released
    );
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
    assert!(db.list_attention_items(&execution.id).unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_release_calls_cube_once() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "concurrent");
    let bookmark = FixedBookmark(BookmarkStatus::Confirmed);
    let first = apply_release(
        &db,
        &execution,
        "concurrent",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &bookmark,
        },
        async {
            assert_eq!(
                apply_release(
                    &db,
                    &execution,
                    "concurrent",
                    ReleaseEvidence {
                        cause: TerminalCause::ProvenCompleted,
                        pane: PaneStatus::Gone,
                        occupancy: &EmptyOccupancy,
                        bookmark: &bookmark,
                    },
                    async { panic!("duplicate cube release") }
                )
                .await,
                LeaseReleaseOutcome::NoLease
            );
            Ok(())
        },
    )
    .await;
    assert_eq!(first, LeaseReleaseOutcome::Released);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
}

#[tokio::test]
async fn old_release_cannot_clear_or_restore_over_a_new_lease() {
    for fail in [false, true] {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "old");
        let outcome = apply_release(
            &db,
            &execution,
            "old",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                bookmark: &FixedBookmark(BookmarkStatus::Confirmed),
            },
            async {
                db.restore_execution_workspace(&execution.id, "new", Some("new-workspace"), Some("/tmp/new-workspace"))
                    .unwrap();
                if fail {
                    Err(anyhow::anyhow!("old cube release failed"))
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }), fail);
        let current = db.get_execution(&execution.id).unwrap();
        assert_eq!(current.cube_lease_id.as_deref(), Some("new"));
        assert_eq!(current.cube_workspace_id.as_deref(), Some("new-workspace"));
        assert_eq!(current.workspace_path.as_deref(), Some("/tmp/new-workspace"));
    }
}

#[tokio::test]
async fn sweep_holds_cancelled_spawn_and_failed_abort() {
    let (_dir, db) = open_db();
    for status in [ExecutionStatus::Cancelled, ExecutionStatus::Failed] {
        let execution = seed_terminal(&db, ExecutionStatus::Ready, "spawn-held");
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker",
                "repo",
                "spawn-held",
                "workspace",
                "/tmp/spawn-held",
            )
            .unwrap();
        if status == ExecutionStatus::Cancelled {
            db.cancel_execution(&execution.id).unwrap();
        } else {
            db.finish_execution_run(
                boss_protocol::FinishExecutionRunInput::builder()
                    .execution_id(&execution.id)
                    .run_id(run.id)
                    .execution_status(status)
                    .run_status("failed")
                    .error_text("viewer abort failed")
                    .record_last_error(true)
                    .build(),
            )
            .unwrap();
        }
        assert!(db.get_execution(&execution.id).unwrap().started_at.is_some());
        let outcome = run_one_pass(&db, &SweepCube, &LiveWorkerStateRegistry::new()).await;
        assert_eq!(outcome.released, 0);
        assert_eq!(
            db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
            Some("spawn-held")
        );
    }
}
