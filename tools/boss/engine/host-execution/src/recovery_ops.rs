//! Recovery workspace operations whose arguments are the recovery crate's own
//! value types: the commit a workspace was positioned on, a fresh start from
//! trunk, confirming that unpublished work is preserved, and restoring it.
//!
//! The wire carries those arguments as the bounded JSON of
//! [`HostRecoveryRequest`] (`WorkspaceOp.request_json`) and returns the recovery
//! crate's result types as JSON, so the hub and the host decode the same
//! structs. Nothing here accepts a path, an argv or a revset from the peer.
use anyhow::{Context, Result, ensure};
use boss_engine_recovery::execution_bookmark::{
    self as recovery, ExecutionBookmark, HostRecoveryRequest, Jj, LocalJj, WorkspaceTipSource,
};
use serde_json::Value;
use std::path::Path;

/// Largest request payload a host will decode.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Decode and bound a request payload. An empty payload is the defaults.
pub fn parse_request(bytes: &[u8]) -> Result<HostRecoveryRequest> {
    ensure!(
        bytes.len() <= MAX_REQUEST_BYTES,
        "recovery request exceeds {MAX_REQUEST_BYTES} bytes"
    );
    if bytes.is_empty() {
        return Ok(HostRecoveryRequest::default());
    }
    serde_json::from_slice(bytes).context("recovery request is not valid JSON for this operation")
}

/// The commit the working copy sits on top of: after cube positions a workspace
/// this is the floor a fresh recovery pointer starts from.
pub async fn positioned_parent(workspace: &Path) -> Result<Value> {
    Ok(Value::String(recovery::one_commit(&LocalJj, workspace, "@-").await?))
}

/// Position a fresh child of the repository's trunk, resolved once, and report
/// the trunk commit it landed on.
pub async fn start_fresh(workspace: &Path) -> Result<Value> {
    let trunk = recovery::one_commit(&LocalJj, workspace, "trunk()").await?;
    LocalJj
        .run(workspace, &["new", &trunk, "-m", "Fresh execution"])
        .await?;
    Ok(Value::String(trunk))
}

pub async fn confirm(record: &ExecutionBookmark, workspace: &Path, request: &HostRecoveryRequest) -> Result<Value> {
    let outcome = recovery::preserve_from(
        &LocalJj,
        record,
        workspace,
        request.recorded.as_ref(),
        request.since_unix,
        request.tip_source.unwrap_or(WorkspaceTipSource::Live),
    )
    .await?;
    Ok(serde_json::to_value(outcome)?)
}

pub async fn restore_preserved(
    record: &ExecutionBookmark,
    workspace: &Path,
    request: &HostRecoveryRequest,
) -> Result<Value> {
    let report = recovery::restore_preserved(&LocalJj, record, workspace, request.recorded.as_ref()).await?;
    Ok(serde_json::to_value(report)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tests::{Cube, command, op, run},
        workspace,
    };
    use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command_result};

    fn payload(request: &HostRecoveryRequest) -> Vec<u8> {
        serde_json::to_vec(request).unwrap()
    }

    #[test]
    fn requests_are_bounded_and_default_when_empty() {
        assert_eq!(parse_request(&[]).unwrap(), HostRecoveryRequest::default());
        assert!(parse_request(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        assert!(parse_request(b"{\"tip_source\": \"Nowhere\"}").is_err());
        // Fields a newer hub adds are ignored, not an error.
        assert!(parse_request(b"{\"release_names\": false, \"future\": 1}").is_ok());
    }

    #[tokio::test]
    async fn executor_positions_confirms_and_restores_through_the_wire_kinds() {
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

        let parent = executor.execute(&command(op(Kind::PositionedParent)), false, 1).await;
        assert_eq!(parent.code, v1::ResultCode::Ok as i32, "{}", parent.reason);
        let Some(command_result::Value::Workspace(parent)) = parent.value else {
            panic!("missing parent")
        };
        let commit: String = serde_json::from_slice(&parent.cube_result_json).unwrap();
        assert_eq!(commit.len(), 40, "a full commit id, got {commit:?}");

        std::fs::write(repo.worker.join("work.txt"), "unpublished").unwrap();
        let mut confirm = op(Kind::ConfirmRecovery);
        confirm.bookmark = created.bookmark.clone();
        confirm.request_json = payload(&HostRecoveryRequest {
            tip_source: Some(WorkspaceTipSource::Live),
            ..Default::default()
        });
        let confirmed = executor.execute(&command(confirm), false, 1).await;
        assert_eq!(confirmed.code, v1::ResultCode::Ok as i32, "{}", confirmed.reason);
        let Some(command_result::Value::Workspace(confirmed)) = confirmed.value else {
            panic!("missing outcome")
        };
        let outcome: recovery::PreserveOutcome = serde_json::from_slice(&confirmed.cube_result_json).unwrap();
        let tip = outcome.tip.expect("the unpublished change is preserved");

        let mut restore = op(Kind::RestorePreserved);
        restore.bookmark = created.bookmark.clone();
        restore.request_json = payload(&HostRecoveryRequest {
            recorded: Some(tip),
            ..Default::default()
        });
        let restored = executor.execute(&command(restore), false, 1).await;
        assert_eq!(restored.code, v1::ResultCode::Ok as i32, "{}", restored.reason);

        let fresh = executor.execute(&command(op(Kind::StartFresh)), false, 1).await;
        assert_eq!(fresh.code, v1::ResultCode::Ok as i32, "{}", fresh.reason);
    }

    #[tokio::test]
    async fn executor_refuses_malformed_or_misplaced_recovery_requests() {
        let root = tempfile::tempdir().unwrap();
        boss_engine_test_git::jj::JjRepo::new(root.path());
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        let created = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
        let Some(command_result::Value::Workspace(created)) = created.value else {
            panic!("missing bookmark")
        };
        // The payload is required where its arguments are, and forbidden elsewhere.
        let mut missing = op(Kind::ConfirmRecovery);
        missing.bookmark = created.bookmark.clone();
        let mut garbage = missing.clone();
        garbage.request_json = b"not json".to_vec();
        let mut stray = op(Kind::PositionedParent);
        stray.request_json = b"{}".to_vec();
        for request in [missing, garbage, stray] {
            let result = executor.execute(&command(request), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{}", result.reason);
        }
        // A bookmark naming another execution cannot be confirmed through this run.
        let mut foreign = op(Kind::ConfirmRecovery);
        foreign.bookmark = created.bookmark.clone().map(|mut bookmark| {
            bookmark.execution_id = "exec_other".into();
            bookmark.head = "boss/exec_other".into();
            bookmark.recovery = "boss-recovery/exec_other".into();
            bookmark.base = "boss-base/exec_other".into();
            bookmark
        });
        foreign.request_json = b"{}".to_vec();
        let result = executor.execute(&command(foreign), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{}", result.reason);
    }
}
