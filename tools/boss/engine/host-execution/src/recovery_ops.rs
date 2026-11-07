//! Recovery workspace operations whose arguments are the recovery crate's own
//! value types: the commit a workspace was positioned on, a fresh start from
//! trunk, confirming that unpublished work is preserved, restoring it, checking
//! whether it is still unpublished, and pruning a finished execution's
//! bookmarks once it is proven published.
//!
//! The wire carries those arguments as the bounded JSON of
//! [`HostRecoveryRequest`] (`WorkspaceOp.request_json`) and returns the recovery
//! crate's result types as JSON, so the hub and the host decode the same
//! structs. Nothing here accepts a path, an argv or a revset from the peer.
use anyhow::{Context, Result, ensure};
use boss_engine_recovery::execution_bookmark::{
    self as recovery, ExecutionBookmark, HostRecoveryRequest, LocalJj, Voucher, WorkspaceTipSource,
};
use serde_json::Value;
use std::path::Path;

/// Largest request payload a host will decode.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Most vouchers one prune request may carry.
pub const MAX_VOUCHERS: usize = 16;

/// Decode and bound a request payload. An empty payload is the defaults.
pub fn parse_request(bytes: &[u8]) -> Result<HostRecoveryRequest> {
    ensure!(
        bytes.len() <= MAX_REQUEST_BYTES,
        "recovery request exceeds {MAX_REQUEST_BYTES} bytes"
    );
    if bytes.is_empty() {
        return Ok(HostRecoveryRequest::default());
    }
    let request: HostRecoveryRequest =
        serde_json::from_slice(bytes).context("recovery request is not valid JSON for this operation")?;
    if let Some(tip) = &request.recorded {
        crate::validation::commit(&tip.commit_id).context("recorded commit id must be a full commit identity")?;
        ensure!(
            tip.change_id.len() == 32 && tip.change_id.bytes().all(|b| (b'k'..=b'z').contains(&b)),
            "recorded change id must be full-length jj change-id text"
        );
    }
    Ok(request)
}

/// The commit the working copy sits on top of: after cube positions a workspace
/// this is the floor a fresh recovery pointer starts from.
pub async fn positioned_parent(workspace: &Path) -> Result<Value> {
    Ok(Value::String(recovery::positioned_parent(&LocalJj, workspace).await?))
}

/// Position a fresh child of the repository's trunk, resolved once, and report
/// the trunk commit it landed on.
pub async fn start_fresh(workspace: &Path) -> Result<Value> {
    Ok(Value::String(recovery::start_fresh(&LocalJj, workspace).await?))
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

/// Whether the preserved work the hub recorded still holds changes that no
/// remote bookmark carries. A store operation: no workspace is involved.
pub async fn preserved_unpublished(record: &ExecutionBookmark, request: &HostRecoveryRequest) -> Result<Value> {
    let recorded = request
        .recorded
        .as_ref()
        .context("preserved-work check requires the recorded tip")?;
    Ok(Value::Bool(
        recovery::preserved_work_unpublished(&LocalJj, record, recorded).await?,
    ))
}

/// The request's vouchers as the recovery crate's own, bounded and validated.
pub fn vouchers(request: &HostRecoveryRequest) -> Result<Vec<Voucher>> {
    ensure!(
        request.vouchers.len() <= MAX_VOUCHERS,
        "prune request carries more than {MAX_VOUCHERS} vouchers"
    );
    request
        .vouchers
        .iter()
        .cloned()
        .map(recovery::HostVoucher::into_voucher)
        .collect()
}

/// Forget a finished execution's pointers once a voucher proves the preserved
/// work is published. `trunk` is the repository's main branch.
pub async fn prune(record: &ExecutionBookmark, request: &HostRecoveryRequest, trunk: &str) -> Result<Value> {
    let vouchers = vouchers(request)?;
    Ok(serde_json::to_value(
        recovery::prune(&LocalJj, record, &vouchers, trunk).await?,
    )?)
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
        let tip = outcome.tip.clone().expect("the unpublished change is preserved");

        let mut restore = op(Kind::RestorePreserved);
        restore.bookmark = created.bookmark.clone();
        restore.request_json = payload(&HostRecoveryRequest {
            recorded: Some(tip),
            ..Default::default()
        });
        let restored = executor.execute(&command(restore), false, 1).await;
        assert_eq!(restored.code, v1::ResultCode::Ok as i32, "{}", restored.reason);

        let mut check = op(Kind::CheckPreservedUnpublished);
        check.bookmark = created.bookmark.clone();
        (check.workspace_id, check.lease_id) = Default::default();
        check.request_json = payload(&HostRecoveryRequest {
            recorded: Some(outcome.tip.clone().expect("tip")),
            ..Default::default()
        });
        let checked = executor.execute(&command(check), false, 1).await;
        assert_eq!(checked.code, v1::ResultCode::Ok as i32, "{}", checked.reason);
        let Some(command_result::Value::Workspace(checked)) = checked.value else {
            panic!("missing check result")
        };
        assert!(
            serde_json::from_slice::<bool>(&checked.cube_result_json).unwrap(),
            "the unpublished change is reported as unpublished"
        );

        let mut interrupted_check = op(Kind::CheckPreservedUnpublished);
        interrupted_check.bookmark = created.bookmark.clone();
        (interrupted_check.workspace_id, interrupted_check.lease_id) = Default::default();
        interrupted_check.request_json = payload(&HostRecoveryRequest {
            recorded: outcome.tip.clone(),
            ..Default::default()
        });
        let checked = executor.execute(&command(interrupted_check), true, 1).await;
        assert_eq!(checked.code, v1::ResultCode::Ok as i32, "{}", checked.reason);

        let bookmark = created.bookmark.as_ref().unwrap();
        boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["bookmark", "set", &bookmark.head, "-r", "@"]);
        // The old kind must reject the new payload rather than delete names.
        let mut old_release = op(Kind::ReleaseHeads);
        old_release.bookmark = created.bookmark.clone();
        (old_release.workspace_id, old_release.lease_id) = Default::default();
        old_release.request_json = payload(&HostRecoveryRequest {
            release_names: Some(false),
            ..Default::default()
        });
        let rejected = executor.execute(&command(old_release), false, 1).await;
        assert_eq!(rejected.code, v1::ResultCode::Rejected as i32, "{}", rejected.reason);

        let mut release = op(Kind::ReleaseHeadsConfigured);
        release.request_json = payload(&HostRecoveryRequest {
            release_names: Some(false),
            ..Default::default()
        });
        release.bookmark = created.bookmark.clone();
        (release.workspace_id, release.lease_id) = Default::default();
        let released = executor.execute(&command(release), false, 1).await;
        assert_eq!(released.code, v1::ResultCode::Ok as i32, "{}", released.reason);
        let bookmark = created.bookmark.as_ref().unwrap();
        let names = boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["bookmark", "list", &bookmark.head]);
        assert!(names.contains(&bookmark.head), "publication bookmark retained: {names}");

        let fresh = executor.execute(&command(op(Kind::StartFresh)), false, 1).await;
        assert_eq!(fresh.code, v1::ResultCode::Ok as i32, "{}", fresh.reason);
        let Some(command_result::Value::Workspace(fresh)) = fresh.value else {
            panic!("missing fresh result")
        };
        let trunk: String = serde_json::from_slice(&fresh.cube_result_json).unwrap();
        assert_eq!(trunk.len(), 40, "the trunk commit it started from, got {trunk:?}");
        let parent = executor.execute(&command(op(Kind::PositionedParent)), false, 1).await;
        let Some(command_result::Value::Workspace(parent)) = parent.value else {
            panic!("missing parent")
        };
        assert_eq!(
            serde_json::from_slice::<String>(&parent.cube_result_json).unwrap(),
            trunk,
            "the fresh workspace sits directly on trunk"
        );

        // No voucher is supplied, so pruning must not retire the work as published.
        let mut prune = op(Kind::PruneBookmark);
        prune.bookmark = created.bookmark.clone();
        (prune.workspace_id, prune.lease_id) = Default::default();
        prune.trunk = "main".into();
        prune.request_json = payload(&HostRecoveryRequest::default());
        let pruned = executor.execute(&command(prune), false, 1).await;
        assert_eq!(pruned.code, v1::ResultCode::Ok as i32, "{}", pruned.reason);
        let Some(command_result::Value::Workspace(pruned)) = pruned.value else {
            panic!("missing prune outcome")
        };
        let outcome: recovery::PruneOutcome = serde_json::from_slice(&pruned.cube_result_json).unwrap();
        // Nothing vouched for the work, so it is never reported as published.
        assert!(
            !matches!(outcome, recovery::PruneOutcome::Published { .. }),
            "{outcome:?}"
        );
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
        // A check needs the recorded tip; a prune needs a bounded voucher list and a trunk.
        let mut unrecorded = op(Kind::CheckPreservedUnpublished);
        unrecorded.bookmark = created.bookmark.clone();
        (unrecorded.workspace_id, unrecorded.lease_id) = Default::default();
        unrecorded.request_json = b"{}".to_vec();
        let mut flooded = op(Kind::PruneBookmark);
        flooded.bookmark = created.bookmark.clone();
        (flooded.workspace_id, flooded.lease_id) = Default::default();
        flooded.trunk = "main".into();
        flooded.request_json = payload(&HostRecoveryRequest {
            vouchers: vec![
                recovery::HostVoucher::RemoteBookmark {
                    name: "b".into(),
                    remote: "origin".into()
                };
                MAX_VOUCHERS + 1
            ],
            ..Default::default()
        });
        let mut unsafe_voucher = flooded.clone();
        unsafe_voucher.request_json = payload(&HostRecoveryRequest {
            vouchers: vec![recovery::HostVoucher::Commit {
                label: "head".into(),
                commit_id: "not-a-commit".into(),
            }],
            ..Default::default()
        });
        let mut no_trunk = flooded.clone();
        no_trunk.trunk = "--all".into();
        no_trunk.request_json = b"{}".to_vec();
        for request in [missing, garbage, stray, unrecorded, flooded, unsafe_voucher, no_trunk] {
            let result = executor.execute(&command(request), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{}", result.reason);
        }
        // No preserved bookmark has been created: malformed recorded tips must
        // still be rejected before fallback revsets can inspect unrelated history.
        for kind in [
            Kind::ConfirmRecovery,
            Kind::RestorePreserved,
            Kind::CheckPreservedUnpublished,
            Kind::PruneBookmark,
        ] {
            for (commit, change) in [
                ("a".into(), "k".repeat(32)),
                ("g".repeat(40), "k".repeat(32)),
                ("a".repeat(40), "b".repeat(32)),
                ("all()".into(), "k".repeat(32)),
                ("a".repeat(40), "latest(all())".into()),
            ] {
                let mut request = op(kind);
                request.bookmark = created.bookmark.clone();
                if crate::validation::store_operation(kind) {
                    (request.workspace_id, request.lease_id) = Default::default();
                }
                if kind == Kind::PruneBookmark {
                    request.trunk = "main".into();
                }
                request.request_json = payload(&HostRecoveryRequest {
                    recorded: Some(recovery::PreservedTip {
                        commit_id: commit.clone(),
                        change_id: change.clone(),
                        count: 1,
                    }),
                    ..Default::default()
                });
                let result = executor.execute(&command(request), false, 1).await;
                assert_eq!(
                    result.code,
                    v1::ResultCode::Rejected as i32,
                    "{kind:?}: {}",
                    result.reason
                );
                assert!(result.reason.contains("recorded"), "{}", result.reason);
            }
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
