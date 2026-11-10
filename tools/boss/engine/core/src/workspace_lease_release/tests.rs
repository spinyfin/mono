use super::*;
use crate::test_support::*;
use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};
use std::sync::Mutex;

mod preservation_retry;

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

/// A preservation outcome fixed by the test.
struct FixedPreservation(std::result::Result<(), &'static str>);
#[async_trait::async_trait]
impl Preservation for FixedPreservation {
    async fn preserve(&self, _: &WorkExecution, _: TerminalCause) -> Result<()> {
        self.0.map_err(|message| anyhow::anyhow!(message))
    }
}

fn decide_release(cause: TerminalCause) -> LeaseReleaseDecision {
    decide(cause, PaneStatus::Gone, Occupancy::Empty)
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
        decide(TerminalCause::ProvenStopped, PaneStatus::Present, Occupancy::Empty),
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
        decide(TerminalCause::Inferred, PaneStatus::Gone, Occupancy::Occupied),
        LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace)
    );
}

#[test]
fn inferred_keeps_when_occupancy_is_unknown() {
    assert_eq!(
        decide(TerminalCause::Inferred, PaneStatus::Gone, Occupancy::Unknown),
        LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed)
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
    preservation: std::result::Result<(), &'static str>,
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
            preservation: &FixedPreservation(preservation),
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
        Ok(()),
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
        Ok(()),
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
        Ok(()),
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
        Ok(()),
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
async fn failed_preservation_warns_and_still_releases_the_lease() {
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
            preservation: &FixedPreservation(Err("preserved bookmark write failed")),
        },
        async move {
            *flag.lock().unwrap() = true;
            Ok(())
        },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    assert!(
        *released.lock().unwrap(),
        "an unpreserved workspace must not keep the lease held"
    );
    assert_eq!(db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(), None);
    let items = db.list_attention_items(&execution.id).unwrap();
    let warning = items
        .iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
                && item.status == "open"
        })
        .expect("an inspection warning names the unpreserved work");
    assert!(warning.body_markdown.contains("preserved bookmark write failed"));
    assert!(warning.body_markdown.contains("lease-bm"));
    assert!(
        items.iter().all(|item| item.kind != LEASE_UNRELEASED_ATTENTION_KIND),
        "no lease is held, so no unreleased-lease item is filed"
    );
}

/// The forced reset skips cube's unpushed-work guard, so it is chosen only
/// when preservation succeeded.
#[tokio::test]
async fn forced_reset_is_chosen_only_after_successful_preservation() {
    for (preservation, expect_forced) in [(Ok(()), true), (Err("preserved bookmark write failed"), false)] {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-choose");
        let forced = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&forced);
        let outcome = apply_release_choosing(
            &db,
            &execution,
            "lease-choose",
            ReleaseEvidence {
                cause: TerminalCause::ProvenCompleted,
                pane: PaneStatus::Gone,
                occupancy: &FixedOccupancy(Occupancy::Empty),
                preservation: &FixedPreservation(preservation),
            },
            |preserved| async move {
                *seen.lock().unwrap() = Some(preserved);
                Ok(())
            },
        )
        .await;
        assert_eq!(outcome, LeaseReleaseOutcome::Released);
        assert_eq!(*forced.lock().unwrap(), Some(expect_forced));
    }
}

#[tokio::test]
async fn failed_preservation_then_failed_release_says_the_lease_is_still_held() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-held");
    let outcome = apply_release(
        &db,
        &execution,
        "lease-held",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            preservation: &FixedPreservation(Err("preserved bookmark write failed")),
        },
        async { Err(anyhow::anyhow!("cube exploded")) },
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }));
    let items = db.list_attention_items(&execution.id).unwrap();
    let warning = items
        .iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
        })
        .expect("an inspection warning is filed");
    assert!(warning.body_markdown.contains("still held"));
    assert!(!warning.body_markdown.contains("was released"));
}

#[tokio::test]
async fn failed_preservation_folds_into_an_open_inspection_item() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-second");
    let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
    db.upsert_execution_attention(
        &execution.id,
        kind,
        "Preserved work needs inspection",
        "Earlier evidence: existing preserved commit `abc123`.",
    )
    .unwrap();
    let outcome = apply_release(
        &db,
        &execution,
        "lease-second",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            preservation: &FixedPreservation(Err("second bookmark write failed")),
        },
        async { Ok(()) },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::Released);
    let items = db.list_attention_items(&execution.id).unwrap();
    let open: Vec<_> = items.iter().filter(|i| i.kind == kind && i.status == "open").collect();
    assert_eq!(open.len(), 1, "the failure folds into the open item");
    let body = &open[0].body_markdown;
    assert!(body.contains("Earlier evidence: existing preserved commit `abc123`."));
    assert!(body.contains("second bookmark write failed"));
    assert!(body.contains("lease-second"));
}

#[tokio::test]
async fn preservation_does_not_run_while_a_gate_keeps_the_lease() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Orphaned, "lease-occupied");
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::Inferred,
        Occupancy::Occupied,
        Err("must not be attempted"),
        Ok(()),
    )
    .await;
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Kept {
            reason: KeepReason::ProcessInWorkspace
        }
    );
    assert!(
        db.list_attention_items(&execution.id).unwrap().is_empty(),
        "preservation snapshots the working copy, so it waits for the pane and occupancy verdicts"
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
        Ok(()),
        Err(anyhow::anyhow!("cube unavailable")),
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { .. }));
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-fail")
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.kind == LEASE_RELEASE_FAILED_ATTENTION_KIND)
    );
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
            preservation: &FixedPreservation(Err("preserved bookmark write failed")),
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
            preservation: &FixedPreservation(Ok(())),
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
async fn db_preservation_errors_for_a_record_without_a_pointer() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lease-rec");
    db.record_execution_bookmark(&ExecutionBookmark {
        execution_id: execution.id.clone(),
        repo_path: PathBuf::from("/tmp/repo"),
        host_id: "local".to_owned(),
        base_commit: None,
        pr_head_before: None,
        has_recovery_head: None,
    })
    .unwrap();
    assert!(
        DbPreservation { db: &db, adapter: None }
            .preserve(&execution, TerminalCause::ProvenCompleted)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn db_preservation_skips_a_workspace_this_execution_never_positioned() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Failed, "lease-none");
    assert!(execution.started_at.is_none());
    DbPreservation { db: &db, adapter: None }
        .preserve(&execution, TerminalCause::ProvenNeverStarted)
        .await
        .expect("cube placed everything in a workspace that was never positioned");
}

#[tokio::test]
async fn db_preservation_errors_for_a_started_execution_without_a_record() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Ready, "lease-started");
    db.start_execution_run(
        &execution.id,
        "worker",
        "repo",
        "lease-started",
        "ws-1",
        "/tmp/ws-started",
    )
    .unwrap();
    db.cancel_execution(&execution.id).unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    assert!(execution.started_at.is_some());
    assert!(
        DbPreservation { db: &db, adapter: None }
            .preserve(&execution, TerminalCause::ProvenStopped)
            .await
            .is_err(),
        "a missing record is never evidence of an empty workspace"
    );
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
    snapshot.record(123, Err(std::io::Error::from_raw_os_error(libc::EPERM)), || {
        Some(unsafe { libc::geteuid() })
    });
    // A directory that provably does not exist cannot hold a process.
    assert_eq!(snapshot.occupancy(&dir.path().join("missing")), Occupancy::Empty);
    let occupancy = snapshot.occupancy(dir.path());
    assert_eq!(occupancy, Occupancy::Unknown);
    assert_eq!(
        decide(TerminalCause::Inferred, PaneStatus::Gone, occupancy),
        LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed)
    );
}

#[test]
fn other_users_unreadable_processes_and_deleted_cwds_do_not_block_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut snapshot = ProcessSnapshot {
        cwds: vec![],
        complete: true,
    };
    let other_uid = unsafe { libc::geteuid() }.wrapping_add(1);
    snapshot.record(1, Err(std::io::Error::from_raw_os_error(libc::EPERM)), || {
        Some(other_uid)
    });
    snapshot.record(2, Ok(dir.path().join("deleted-cwd")), || None);
    assert_eq!(snapshot.occupancy(dir.path()), Occupancy::Empty);
    // An unknown owner for a failed lookup stays inconclusive.
    snapshot.record(3, Err(std::io::Error::from_raw_os_error(libc::EACCES)), || None);
    assert_eq!(snapshot.occupancy(dir.path()), Occupancy::Unknown);
}

#[test]
fn real_host_snapshot_of_a_fresh_dir_is_never_occupied() {
    let dir = tempfile::TempDir::new().unwrap();
    // A sandboxed CI host can hold same-uid processes whose cwd is
    // unreadable, which legitimately yields `Unknown`; what must never
    // happen is a fresh directory reading as occupied.
    assert_ne!(ProcessSnapshot::capture().occupancy(dir.path()), Occupancy::Occupied);
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
            Ok(()),
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
    let preservation = FixedPreservation(Ok(()));
    let first = apply_release(
        &db,
        &execution,
        "concurrent",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            preservation: &preservation,
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
                        preservation: &preservation,
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
                preservation: &FixedPreservation(Ok(())),
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
