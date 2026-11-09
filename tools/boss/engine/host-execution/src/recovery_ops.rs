//! Recovery workspace operations whose arguments are the recovery crate's own
//! value types: the commit a workspace was positioned on, a fresh start from
//! trunk, confirming that unpublished work is preserved, restoring it, checking
//! whether it is still unpublished, and forgetting an execution's preserved
//! references once they are finished with.
//!
//! The wire carries those arguments as the bounded JSON of
//! [`HostRecoveryRequest`] (`WorkspaceOp.request_json`) and returns the recovery
//! crate's result types as JSON, so the hub and the host decode the same
//! structs. Nothing here accepts a path, an argv or a revset from the peer.
use anyhow::{Context, Result, ensure};
use boss_engine_recovery::execution_bookmark::{
    self as recovery, ExecutionBookmark, HostRecoveryRequest, LocalJj, WorkspaceTipSource,
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

/// Forget the preserved references of the record's execution in one
/// operation: the tip, and with the archive scope its inspection archives.
/// The result is the list of names that were forgotten.
pub async fn forget_preserved(record: &ExecutionBookmark, request: &HostRecoveryRequest) -> Result<Value> {
    Ok(serde_json::to_value(
        recovery::forget_preserved_references(&LocalJj, record, request.scope).await?,
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
        // Unknown fields are ignored: new semantics always use a new WorkspaceKind,
        // so an extra JSON field must not change this kind's meaning.
        assert!(parse_request(b"{\"future\": 1}").is_ok());
    }

    #[test]
    fn recovery_json_payloads_round_trip_the_golden_field_names() {
        let commit = "a".repeat(40);
        let change = "k".repeat(32);
        let request = HostRecoveryRequest {
            recorded: Some(recovery::PreservedTip {
                change_id: change.clone(),
                commit_id: commit.clone(),
                count: 2,
            }),
            since_unix: Some(1_700_000_000),
            tip_source: Some(WorkspaceTipSource::Live),
            scope: recovery::PreservedScope::TipAndArchives,
        };
        let request_json = serde_json::json!({
            "recorded": {
                "change_id": change.as_str(),
                "commit_id": commit.as_str(),
                "count": 2
            },
            "since_unix": 1_700_000_000,
            "tip_source": "Live",
            "scope": "TipAndArchives"
        });
        assert_eq!(serde_json::to_value(&request).unwrap(), request_json);
        assert_eq!(
            parse_request(&serde_json::to_vec(&request_json).unwrap()).unwrap(),
            request
        );

        let preserve = recovery::PreserveOutcome {
            tip: Some(recovery::PreservedTip {
                change_id: change.clone(),
                commit_id: commit.clone(),
                count: 1,
            }),
            inspect: Some(recovery::PreserveInspect {
                existing_commit_id: None,
                workspace_change_id: Some(change.clone()),
                workspace_commit_id: Some(commit.clone()),
                workspace_count: Some(1),
                reason: "inspect".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&preserve).unwrap(),
            serde_json::json!({
                "tip": {"change_id": change.as_str(), "commit_id": commit.as_str(), "count": 1},
                "inspect": {
                    "existing_commit_id": null,
                    "workspace_change_id": change.as_str(),
                    "workspace_commit_id": commit.as_str(),
                    "workspace_count": 1,
                    "reason": "inspect"
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<recovery::PreserveOutcome>(serde_json::to_value(&preserve).unwrap()).unwrap(),
            preserve
        );

        let restore = recovery::RestoreReport::builder()
            .pointer("preserved/exec_1")
            .commits("aaa")
            .base_sha(commit.clone())
            .conflicts("")
            .inherited_base("main")
            .pr_bound(true)
            .base_fallback("used origin/main")
            .discarded_tips("bbb")
            .unrestaged_unpublished("ccc")
            .build();
        let restore_json = serde_json::json!({
            "pointer": "preserved/exec_1",
            "commits": "aaa",
            "base_sha": commit.as_str(),
            "conflicts": "",
            "inherited_base": "main",
            "pr_bound": true,
            "base_fallback": "used origin/main",
            "discarded_tips": "bbb",
            "unrestaged_unpublished": "ccc"
        });
        assert_eq!(serde_json::to_value(&restore).unwrap(), restore_json);
        assert_eq!(
            serde_json::from_value::<recovery::RestoreReport>(restore_json).unwrap(),
            restore
        );
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

        // The tip teardown preserved goes in one operation, and a repeat is a no-op.
        let tip_name = recovery::preserved_name(&run().execution_id);
        for expected in [vec![tip_name.clone()], Vec::new()] {
            let mut forget = op(Kind::ForgetPreserved);
            forget.bookmark = created.bookmark.clone();
            (forget.workspace_id, forget.lease_id) = Default::default();
            forget.request_json = payload(&HostRecoveryRequest::default());
            let forgotten = executor.execute(&command(forget), false, 1).await;
            assert_eq!(forgotten.code, v1::ResultCode::Ok as i32, "{}", forgotten.reason);
            let Some(command_result::Value::Workspace(forgotten)) = forgotten.value else {
                panic!("missing forget result")
            };
            assert_eq!(
                serde_json::from_slice::<Vec<String>>(&forgotten.cube_result_json).unwrap(),
                expected
            );
        }
        let listed = boss_engine_test_git::jj::JjRepo::run(
            &repo.repo,
            &[
                "bookmark",
                "list",
                "-T",
                "name ++ \"\\n\"",
                &format!("exact:{tip_name}"),
            ],
        );
        assert!(listed.trim().is_empty(), "the preserved tip survived: {listed}");
    }

    #[tokio::test]
    async fn executor_forgets_archives_only_when_the_archive_scope_asks() {
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
        let execution = run().execution_id;
        let tip = recovery::preserved_name(&execution);
        let archive = recovery::preserved_archive_name(&execution, "aaaa");
        for name in [&tip, &archive] {
            boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["bookmark", "create", name, "-r", "@"]);
        }
        let forget = |scope| {
            let mut forget = op(Kind::ForgetPreserved);
            forget.bookmark = created.bookmark.clone();
            (forget.workspace_id, forget.lease_id) = Default::default();
            forget.request_json = payload(&HostRecoveryRequest {
                scope,
                ..Default::default()
            });
            command(forget)
        };
        let names =
            || boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
        let result = executor.execute(&forget(recovery::PreservedScope::Tip), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
        assert!(!names().lines().any(|name| name == tip), "{}", names());
        assert!(names().lines().any(|name| name == archive), "{}", names());
        let result = executor
            .execute(&forget(recovery::PreservedScope::TipAndArchives), false, 1)
            .await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
        assert!(!names().lines().any(|name| name == archive), "{}", names());
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
        // A check needs the recorded tip; a forget needs its payload, and the scope must be one it knows.
        let mut unrecorded = op(Kind::CheckPreservedUnpublished);
        unrecorded.bookmark = created.bookmark.clone();
        (unrecorded.workspace_id, unrecorded.lease_id) = Default::default();
        unrecorded.request_json = b"{}".to_vec();
        let mut unscoped = op(Kind::ForgetPreserved);
        unscoped.bookmark = created.bookmark.clone();
        (unscoped.workspace_id, unscoped.lease_id) = Default::default();
        let mut unknown_scope = unscoped.clone();
        unknown_scope.request_json = b"{\"scope\": \"Everything\"}".to_vec();
        for request in [missing, garbage, stray, unrecorded, unscoped, unknown_scope] {
            let result = executor.execute(&command(request), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{}", result.reason);
        }
        // No preserved bookmark has been created: malformed recorded tips must
        // still be rejected before fallback revsets can inspect unrelated history.
        for kind in [
            Kind::ConfirmRecovery,
            Kind::RestorePreserved,
            Kind::CheckPreservedUnpublished,
            Kind::ForgetPreserved,
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
            bookmark
        });
        foreign.request_json = b"{}".to_vec();
        let result = executor.execute(&command(foreign), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{}", result.reason);
    }
}
