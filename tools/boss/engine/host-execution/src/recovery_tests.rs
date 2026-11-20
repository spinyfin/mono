use crate::{
    tests::{Cube, command, op, run},
    workspace,
};
use boss_engine_recovery::execution_bookmark::{Jj, LocalJj};
use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command_result};

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
    std::fs::write(repo.worker.join("work.txt"), "preserved").unwrap();
    LocalJj.run(&repo.worker, &["status"]).await.unwrap();
    let created = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
    assert_eq!(created.code, v1::ResultCode::Ok as i32, "{}", created.reason);
    let Some(command_result::Value::Workspace(created)) = created.value else {
        panic!("missing bookmark")
    };
    let mut request = op(Kind::CheckpointBaseline);
    request.bookmark = created.bookmark.clone();
    request.workspace_id.clear();
    request.lease_id.clear();
    let result = executor.execute(&command(request), false, 1).await;
    assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
    let Some(command_result::Value::Workspace(value)) = result.value else {
        panic!("missing checkpoint")
    };
    let base: String = serde_json::from_slice(&value.cube_result_json).unwrap();
    assert_eq!(base, created.bookmark.as_ref().unwrap().anchor_commit);
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("work.txt")).unwrap(),
        "preserved"
    );
}
