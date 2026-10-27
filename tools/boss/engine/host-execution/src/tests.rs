use super::*;
use anyhow::Result;
use async_trait::async_trait;
use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command, command_result};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Mutex};

pub(super) fn run() -> v1::Run {
    v1::Run {
        execution_id: "exec_test".into(),
        run_id: "run-test".into(),
        spawn_token: "token".into(),
        assignment_generation: 1,
    }
}
pub(super) fn op(kind: Kind) -> v1::WorkspaceOp {
    v1::WorkspaceOp {
        kind: kind.into(),
        repo_id: "repo".into(),
        workspace_id: "worker".into(),
        lease_id: "lease".into(),
        ..Default::default()
    }
}
pub(super) fn command(op: v1::WorkspaceOp) -> v1::Command {
    v1::Command {
        sequence: 1,
        command_id: "command".into(),
        run: Some(run()),
        not_after_unix_ms: 1000,
        body: Some(command::Body::Workspace(op)),
    }
}
pub(super) struct Cube {
    lost_reply: bool,
    root: PathBuf,
    path: PathBuf,
    pub(super) owner: Mutex<Option<String>>,
    calls: Mutex<Vec<Vec<String>>>,
}
impl Cube {
    pub(super) fn new(root: &std::path::Path) -> Self {
        Self {
            lost_reply: true,
            root: root.into(),
            path: root.join("worker"),
            owner: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }
    fn lease_calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args.get(2).is_some_and(|arg| arg == "lease"))
            .count()
    }
}
#[async_trait]
impl cube::CubeJsonTransport for Cube {
    async fn run_cube_json(&self, args: &[&str]) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        match args {
            ["--json", "repo", "list"] => Ok(
                json!({"repos": [{"repo": "repo", "origin": "https://github.com/example/repo", "main_branch": "main", "workspace_root": self.root, "workspace_prefix": "worker", "source": self.root.join("shared")}]}),
            ),
            ["--json", "workspace", "list"] => {
                let owner = self.owner.lock().unwrap();
                Ok(
                    json!({"workspaces": [{"repo": "repo", "workspace_id": "worker", "workspace_path": self.path, "state": if owner.is_some() { "leased" } else { "free" }, "lease_id": owner.as_ref().map(|_| "lease"), "task": *owner}]}),
                )
            }
            ["--json", "workspace", "lease", _, "--task", owner, ..] => {
                *self.owner.lock().unwrap() = Some((*owner).into());
                if self.lost_reply {
                    anyhow::bail!("reply lost after cube committed owner");
                }
                Ok(json!({"workspace": {"workspace_id": "worker", "lease_id": "lease", "workspace_path": self.path}}))
            }
            ["--json", "change", "create", ..] => Ok(json!({"change": {"change_id": "change"}})),
            _ => Ok(json!({})),
        }
    }
}

#[tokio::test]
async fn interrupted_acquisition_reconciles_owner_without_second_lease() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("worker")).unwrap();
    let executor = workspace::Executor {
        cube: Cube::new(root.path()),
        host_id: "host".into(),
    };
    let command = command(v1::WorkspaceOp {
        kind: Kind::Lease.into(),
        repo_id: "repo".into(),
        ..Default::default()
    });
    assert_eq!(
        executor.execute(&command, false, 1).await.code,
        v1::ResultCode::OutcomeUncertain as i32
    );
    let result = executor.execute(&command, true, 2000).await;
    assert_eq!(result.code, v1::ResultCode::OutcomeUncertain as i32);
    let Some(command_result::Value::Workspace(lease)) = result.value else {
        panic!("missing recovered lease");
    };
    let decoded: types::CubeWorkspaceLease = serde_json::from_slice(&lease.cube_result_json).unwrap();
    assert_eq!(decoded.dirty_verified, None);
    assert_eq!(decoded.lease_id, "lease");
    assert_eq!(lease.lease_id, "lease");
    assert_eq!(lease.workspace_id, "worker");
    assert_eq!(executor.cube.lease_calls(), 1);
}

#[tokio::test]
async fn missing_owner_after_interruption_never_acquires_again() {
    let root = tempfile::tempdir().unwrap();
    let executor = workspace::Executor {
        cube: Cube::new(root.path()),
        host_id: "host".into(),
    };
    let result = executor
        .execute(
            &command(v1::WorkspaceOp {
                kind: Kind::Lease.into(),
                repo_id: "repo".into(),
                ..Default::default()
            }),
            true,
            1,
        )
        .await;
    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
    assert_eq!(executor.cube.lease_calls(), 0);
}

#[tokio::test]
async fn lease_administration_does_not_require_a_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let cube = Cube::new(root.path());
    *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
    let executor = workspace::Executor {
        cube,
        host_id: "host".into(),
    };
    assert!(!executor.cube.path.exists());
    for kind in [Kind::Heartbeat, Kind::Release, Kind::ForceRelease] {
        let mut request = op(kind);
        if kind == Kind::ForceRelease {
            request.reason = "missing workspace".into();
        }
        let result = executor.execute(&command(request), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
    }
}

#[tokio::test]
async fn rejects_foreign_lease_and_symlink_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cube = Cube::new(root.path());
    *cube.owner.lock().unwrap() = Some("other assignment".into());
    std::os::unix::fs::symlink(outside.path(), &cube.path).unwrap();
    let executor = workspace::Executor {
        cube,
        host_id: "host".into(),
    };
    let request = command(v1::WorkspaceOp {
        title: "must not write through symlink".into(),
        ..op(Kind::CreateChange)
    });
    assert_eq!(
        executor.execute(&request, false, 1).await.code,
        v1::ResultCode::Rejected as i32
    );
    *executor.cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
    assert_eq!(
        executor.execute(&request, false, 1).await.code,
        v1::ResultCode::Rejected as i32
    );
    assert!(outside.path().exists());
    assert!(executor.cube.calls.lock().unwrap().iter().all(|args| args[2] == "list"));
}

#[test]
fn every_workspace_kind_has_required_and_forbidden_fields() {
    let assignment = run();
    let bookmark = v1::Bookmark {
        repo_id: "repo".into(),
        execution_id: assignment.execution_id.clone(),
        head: "boss/exec_test".into(),
        recovery: "boss-recovery/exec_test".into(),
        base: "boss-base/exec_test".into(),
        anchor_commit: "a".repeat(40),
    };
    for number in 1..=20 {
        let kind = Kind::try_from(number).unwrap();
        let mut request = op(kind);
        match kind {
            Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces => {
                request = v1::WorkspaceOp {
                    kind: kind.into(),
                    ..Default::default()
                };
                if kind == Kind::EnsureRepo {
                    request.origin = "https://github.com/example/repo".into();
                }
            }
            Kind::Lease => {
                request.lease_id.clear();
            }
            Kind::CreateChange => request.title = "Implement workspace execution".into(),
            Kind::GotoPr | Kind::PrepareRecoveryPr => request.pr_number = 1,
            Kind::GotoRevision => request.revision = "a".repeat(40),
            Kind::ForceRelease => request.reason = "hub authorized release".into(),
            Kind::RestoreRebased | Kind::ReconcileBookmark => request.trunk = "main".into(),
            _ => {}
        }
        if crate::validation::store_operation(kind) {
            request.workspace_id.clear();
            request.lease_id.clear();
        }
        if matches!(number, 13..=20) {
            request.bookmark = Some(bookmark.clone());
        }
        assert_eq!(validate_workspace(&request, Some(&assignment)).unwrap(), kind);
        request.title = "unexpected extra field".into();
        if kind == Kind::CreateChange {
            request.origin = "unexpected".into();
        }
        assert!(validate_workspace(&request, Some(&assignment)).is_err(), "{kind:?}");
    }
}

#[tokio::test]
async fn rejects_wire_paths_revsets_and_unknown_operations_before_cube() {
    let root = tempfile::tempdir().unwrap();
    let executor = workspace::Executor {
        cube: Cube::new(root.path()),
        host_id: "host".into(),
    };
    let mut invalid = vec![
        op(Kind::WorkspaceUnspecified),
        v1::WorkspaceOp {
            kind: 999,
            ..Default::default()
        },
    ];
    for path in ["/tmp/worker", "../worker", "--workspace", "."] {
        invalid.push(v1::WorkspaceOp {
            workspace_id: path.into(),
            ..op(Kind::Release)
        });
    }
    invalid.push(v1::WorkspaceOp {
        revision: "all()".into(),
        ..op(Kind::GotoRevision)
    });
    invalid.push(v1::WorkspaceOp {
        origin: "file:///tmp/repo".into(),
        kind: Kind::EnsureRepo.into(),
        ..Default::default()
    });
    for origin in [".", "./repo", "owner/repo", "owner//repo", "owner/repo/subdir"] {
        invalid.push(v1::WorkspaceOp {
            origin: origin.into(),
            kind: Kind::EnsureRepo.into(),
            ..Default::default()
        });
    }
    for request in invalid {
        assert_eq!(
            executor.execute(&command(request), false, 1).await.code,
            v1::ResultCode::Rejected as i32
        );
    }
    assert!(executor.cube.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn interrupted_mutations_are_not_replayed() {
    let root = tempfile::tempdir().unwrap();
    let executor = workspace::Executor {
        cube: Cube::new(root.path()),
        host_id: "host".into(),
    };
    for request in [
        v1::WorkspaceOp {
            title: "new change".into(),
            ..op(Kind::CreateChange)
        },
        op(Kind::Heartbeat),
        v1::WorkspaceOp {
            kind: Kind::EnsureRepo.into(),
            origin: "https://github.com/example/repo".into(),
            ..Default::default()
        },
    ] {
        let result = executor.execute(&command(request), true, 2000).await;
        assert_eq!(result.code, v1::ResultCode::OutcomeUncertain as i32);
    }
    assert!(executor.cube.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn recovery_pointers_survive_failed_pr_positioning() {
    use boss_engine_recovery::execution_bookmark::{self as recovery, Jj, LocalJj};
    let root = tempfile::tempdir().unwrap();
    let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
    let record = recovery::create(&LocalJj, &repo.worker, "exec_test", "host")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work.txt"), "preserved work").unwrap();
    LocalJj.run(&repo.worker, &["status"]).await.unwrap();
    let saved = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
    let result = workspace::preserve_recovery_pointers(&record, async {
        LocalJj
            .run(
                &repo.worker,
                &[
                    "bookmark",
                    "delete",
                    &record.head(),
                    &record.publication(),
                    &record.base(),
                ],
            )
            .await?;
        anyhow::bail!("PR positioning failed after removing pointers")
    })
    .await;
    assert!(result.is_err());
    assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), saved);
    assert!(
        recovery::unpublished_diff(&LocalJj, &record)
            .await
            .unwrap()
            .contains("preserved work")
    );
}

#[tokio::test]
async fn input_is_validated_before_provisioning_and_only_owned_home_is_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        input::provision(&boss_engine_driver::ClaudeDriver, root.path(), &[255], "invalid")
            .await
            .is_err()
    );
    assert!(!root.path().join(".claude").exists());
    let run_id: String = root
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let state = input::provision(
        &boss_engine_driver::ClaudeDriver,
        root.path(),
        b"private input",
        &run_id,
    )
    .await
    .unwrap()
    .unwrap();
    let home = boss_engine_driver::initial_prompt::owned_home_from_runtime_state(&state).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join(".claude/initial-prompt.txt")).unwrap(),
        "private input"
    );
    input::reclaim(&state).unwrap();
    input::reclaim(&state).unwrap();
    assert!(!home.exists());
    let foreign = boss_protocol::DriverRuntimeState::new(json!({"initial_prompt_home": root.path()}));
    assert!(input::reclaim(&foreign).is_err());
    assert!(root.path().exists());
}

#[tokio::test]
async fn lease_and_workspace_operations_use_registry_path_and_assignment() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("worker")).unwrap();
    let mut cube = Cube::new(root.path());
    cube.lost_reply = false;
    let executor = workspace::Executor {
        cube,
        host_id: "host".into(),
    };
    let lease = executor
        .execute(
            &command(v1::WorkspaceOp {
                kind: Kind::Lease.into(),
                repo_id: "repo".into(),
                task_label: "workspace execution".into(),
                ..Default::default()
            }),
            false,
            1,
        )
        .await;
    assert_eq!(lease.code, v1::ResultCode::Ok as i32, "{}", lease.reason);
    let Some(command_result::Value::Workspace(value)) = &lease.value else {
        panic!("missing lease")
    };
    let decoded: types::CubeWorkspaceLease = serde_json::from_slice(&value.cube_result_json).unwrap();
    assert_eq!(decoded.lease_id, "lease");
    assert_eq!(decoded.dirty_verified, None);
    for kind in [
        Kind::Status,
        Kind::Heartbeat,
        Kind::CreateChange,
        Kind::GotoPr,
        Kind::GotoRevision,
        Kind::Release,
        Kind::ForceRelease,
    ] {
        let mut request = op(kind);
        match kind {
            Kind::CreateChange => request.title = "workspace execution".into(),
            Kind::GotoPr => request.pr_number = 42,
            Kind::GotoRevision => request.revision = "a".repeat(40),
            Kind::ForceRelease => request.reason = "hub authorized".into(),
            _ => {}
        }
        let result = executor.execute(&command(request), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{kind:?}: {}", result.reason);
    }
    assert_eq!(executor.cube.lease_calls(), 1);
    let calls = executor.cube.calls.lock().unwrap();
    let goto = calls
        .iter()
        .find(|args| args.get(2).is_some_and(|s| s == "goto"))
        .unwrap();
    assert_eq!(
        goto[4],
        root.path().join("worker").canonicalize().unwrap().to_str().unwrap()
    );
}

#[tokio::test]
async fn dispatcher_recovers_bookmark_diff_after_workspace_removal() {
    use boss_engine_recovery::execution_bookmark::{Jj, LocalJj};
    let root = tempfile::tempdir().unwrap();
    let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
    let cube = Cube::new(root.path());
    *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
    let executor = workspace::Executor {
        cube,
        host_id: "host".into(),
    };
    let result = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
    assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
    let Some(command_result::Value::Workspace(created)) = result.value else {
        panic!("missing bookmark result")
    };
    std::fs::write(repo.worker.join("preserved.txt"), "survives workspace removal").unwrap();
    LocalJj.run(&repo.worker, &["status"]).await.unwrap();
    LocalJj
        .run(&repo.repo, &["workspace", "forget", "worker"])
        .await
        .unwrap();
    std::fs::remove_dir_all(&repo.worker).unwrap();
    *executor.cube.owner.lock().unwrap() = None;
    let request = v1::WorkspaceOp {
        kind: Kind::DiffBookmark.into(),
        repo_id: "repo".into(),
        bookmark: created.bookmark,
        ..Default::default()
    };
    let result = executor.execute(&command(request), false, 1).await;
    assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
    let Some(command_result::Value::Workspace(diff)) = result.value else {
        panic!("missing diff result")
    };
    let diff: String = serde_json::from_slice(&diff.cube_result_json).unwrap();
    assert!(diff.contains("survives workspace removal"));
}

#[tokio::test]
async fn failed_read_is_rejected() {
    struct FailedRead;
    #[async_trait]
    impl cube::CubeJsonTransport for FailedRead {
        async fn run_cube_json(&self, _: &[&str]) -> Result<Value> {
            anyhow::bail!("registry unavailable")
        }
    }
    let executor = workspace::Executor {
        cube: FailedRead,
        host_id: "host".into(),
    };
    for kind in [Kind::ListRepos, Kind::ListWorkspaces] {
        let request = command(v1::WorkspaceOp {
            kind: kind.into(),
            ..Default::default()
        });
        assert_eq!(
            executor.execute(&request, false, 1).await.code,
            v1::ResultCode::Rejected as i32
        );
    }
}
