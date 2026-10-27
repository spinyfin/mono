use crate::{
    cube::CubeJsonTransport,
    tests::{Cube, command, op, run},
    workspace::{self, RecoveryStore},
};
use anyhow::Result;
use async_trait::async_trait;
use boss_engine_recovery::execution_bookmark::{self as recovery, Jj, LocalJj};
use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command_result};

struct PositioningCube {
    cube: Cube,
    record: recovery::ExecutionBookmark,
    hang: bool,
}
#[async_trait]
impl CubeJsonTransport for PositioningCube {
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        if args.get(2) == Some(&"goto") {
            LocalJj
                .run(
                    &self.record.repo_path,
                    &[
                        "bookmark",
                        "delete",
                        &self.record.base(),
                        &self.record.head(),
                        &self.record.publication(),
                    ],
                )
                .await?;
            if self.hang {
                std::future::pending::<()>().await;
            }
            anyhow::bail!("positioning failed after deleting pointers");
        }
        self.cube.run_cube_json(args).await
    }
}
struct AttemptFile(std::path::PathBuf);
impl RecoveryStore for AttemptFile {
    fn load(&self, _: u64) -> Result<Option<Vec<u8>>> {
        Ok(Some(std::fs::read(&self.0)?))
    }
    fn save(&self, _: u64, bytes: &[u8]) -> Result<()> {
        std::fs::write(&self.0, bytes)?;
        Ok(())
    }
}

#[tokio::test]
async fn executor_restores_pointers_on_failed_and_interrupted_positioning() {
    for interrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
        let record = recovery::create(&LocalJj, &repo.worker, "exec_test", "host")
            .await
            .unwrap();
        let saved = recovery::snapshot_pointers(&LocalJj, &record).await.unwrap();
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube: PositioningCube {
                cube,
                record: record.clone(),
                hang: interrupt,
            },
            host_id: "host".into(),
        };
        let request = command(v1::WorkspaceOp {
            pr_number: 42,
            bookmark: Some(v1::Bookmark {
                repo_id: "repo".into(),
                execution_id: record.execution_id.clone(),
                head: record.publication(),
                recovery: record.head(),
                base: record.base(),
                anchor_commit: record.base_commit.clone().unwrap(),
            }),
            ..op(Kind::PrepareRecoveryPr)
        });
        let store = AttemptFile(root.path().join("attempt.json"));
        if interrupt {
            let mut effect = Box::pin(executor.execute_with_store(&request, false, 1, Some(&store)));
            tokio::select! {
                _ = &mut effect => panic!("positioning should remain in progress"),
                _ = async {
                    loop {
                        if store.0.exists() && recovery::snapshot_pointers(&LocalJj, &record).await.unwrap().iter().all(|(_, id)| id.is_none()) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                } => {}
            }
            drop(effect);
            // A new store handle reads the pre-effect snapshot after interruption.
            let reopened = AttemptFile(store.0.clone());
            let snapshot = std::fs::read(&store.0).unwrap();
            let offline = root.path().join("offline-store");
            std::fs::rename(&record.repo_path, &offline).unwrap();
            assert!(
                executor
                    .execute_with_store(&request, true, 2000, Some(&reopened))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&store.0).unwrap(), snapshot);
            std::fs::rename(&offline, &record.repo_path).unwrap();
            let result = executor
                .execute_with_store(&request, true, 2000, Some(&reopened))
                .await
                .unwrap();
            assert_eq!(
                result.code,
                v1::ResultCode::OutcomeUncertain as i32,
                "{}",
                result.reason
            );
        } else {
            let result = executor
                .execute_with_store(&request, false, 1, Some(&store))
                .await
                .unwrap();
            assert_eq!(
                result.code,
                v1::ResultCode::OutcomeUncertain as i32,
                "{}",
                result.reason
            );
        }
        assert_eq!(recovery::snapshot_pointers(&LocalJj, &record).await.unwrap(), saved);
    }
}

#[tokio::test]
async fn executor_runs_store_and_workspace_recovery_operations() {
    let root = tempfile::tempdir().unwrap();
    let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
    let cube = Cube::new(root.path());
    *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
    let executor = workspace::Executor {
        cube,
        host_id: "host".into(),
    };
    let created = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
    assert_eq!(created.code, v1::ResultCode::Ok as i32, "{}", created.reason);
    let Some(command_result::Value::Workspace(created)) = created.value else {
        panic!("missing bookmark")
    };
    std::fs::write(repo.worker.join("work.txt"), "preserved").unwrap();
    LocalJj.run(&repo.worker, &["status"]).await.unwrap();
    for kind in [Kind::CheckpointBaseline, Kind::ReconcileBookmark, Kind::RestoreBookmark] {
        let mut request = op(kind);
        request.bookmark = created.bookmark.clone();
        if kind != Kind::RestoreBookmark {
            request.workspace_id.clear();
            request.lease_id.clear();
        }
        if kind == Kind::ReconcileBookmark {
            request.trunk = "main".into();
        }
        let result = executor.execute(&command(request), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{kind:?}: {}", result.reason);
        if kind == Kind::CheckpointBaseline {
            let Some(command_result::Value::Workspace(value)) = result.value else {
                panic!("missing checkpoint")
            };
            let base: String = serde_json::from_slice(&value.cube_result_json).unwrap();
            assert_eq!(base, created.bookmark.as_ref().unwrap().anchor_commit);
        }
    }
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("work.txt")).unwrap(),
        "preserved"
    );
}
