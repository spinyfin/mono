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
            vouchers: vec![
                recovery::HostVoucher::Commit {
                    label: "PR #1 head".into(),
                    commit_id: commit.clone(),
                },
                recovery::HostVoucher::RemoteBookmark {
                    name: "boss/exec_1".into(),
                    remote: "origin".into(),
                },
            ],
        };
        let request_json = serde_json::json!({
            "recorded": {
                "change_id": change.as_str(),
                "commit_id": commit.as_str(),
                "count": 2
            },
            "since_unix": 1_700_000_000,
            "tip_source": "Live",
            "vouchers": [
                {"Commit": {"label": "PR #1 head", "commit_id": commit.as_str()}},
                {"RemoteBookmark": {"name": "boss/exec_1", "remote": "origin"}}
            ]
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

        let published = recovery::PruneOutcome::Published {
            vouched_by: "PR #1 head".into(),
            commit: commit.clone(),
            forgotten: vec!["boss-base/exec_1".into(), "boss/exec_1".into()],
            rewritten: 0,
        };
        let unproven = recovery::PruneOutcome::Unproven {
            kept: vec!["boss-recovery/exec_1".into()],
            missing: vec![recovery::MissingChange {
                change_id: change.clone(),
                commit: commit.clone(),
                description: "work".into(),
            }],
            forgotten: vec!["boss-base/exec_1".into()],
            tried: vec!["remote bookmarks: unpublished changes remain".into()],
        };
        let gone = recovery::PruneOutcome::AlreadyGone {
            forgotten: vec!["boss-base/exec_1".into()],
        };
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            serde_json::json!({
                "Published": {
                    "vouched_by": "PR #1 head",
                    "commit": commit.as_str(),
                    "forgotten": ["boss-base/exec_1", "boss/exec_1"],
                    "rewritten": 0
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&unproven).unwrap(),
            serde_json::json!({
                "Unproven": {
                    "kept": ["boss-recovery/exec_1"],
                    "missing": [{
                        "change_id": change.as_str(),
                        "commit": commit.as_str(),
                        "description": "work"
                    }],
                    "forgotten": ["boss-base/exec_1"],
                    "tried": ["remote bookmarks: unpublished changes remain"]
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&gone).unwrap(),
            serde_json::json!({"AlreadyGone": {"forgotten": ["boss-base/exec_1"]}})
        );
        for outcome in [published, unproven, gone] {
            let value = serde_json::to_value(&outcome).unwrap();
            assert_eq!(
                serde_json::from_value::<recovery::PruneOutcome>(value).unwrap(),
                outcome
            );
        }

        let restore = recovery::RestoreReport::builder()
            .pointer("boss-recovery/exec_1")
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
            "pointer": "boss-recovery/exec_1",
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

        let bookmark = created.bookmark.as_ref().unwrap();
        boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["bookmark", "set", &bookmark.head, "-r", "@"]);
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
    async fn executor_prunes_as_published_when_a_remote_bookmark_voucher_carries_the_work() {
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
        let bookmark = created.bookmark.as_ref().unwrap();
        std::fs::write(repo.worker.join("published.txt"), "on the branch").unwrap();
        let mut confirm = op(Kind::ConfirmRecovery);
        confirm.bookmark = created.bookmark.clone();
        confirm.request_json = payload(&HostRecoveryRequest {
            tip_source: Some(WorkspaceTipSource::Live),
            ..Default::default()
        });
        let confirmed = executor.execute(&command(confirm), false, 1).await;
        assert_eq!(confirmed.code, v1::ResultCode::Ok as i32, "{}", confirmed.reason);
        boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["bookmark", "set", &bookmark.head, "-r", "@"]);
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["git", "export"]);
        let git_store = repo.repo.join(".jj/repo/store/git");
        boss_engine_test_git::jj::JjRepo::run(
            &repo.repo,
            &["git", "remote", "add", "origin", git_store.to_str().unwrap()],
        );
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["git", "fetch"]);
        let present: Vec<String> = [&bookmark.base, &bookmark.recovery, &bookmark.head]
            .into_iter()
            .filter(|name| {
                boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\"", name])
                    .lines()
                    .any(|line| line == *name)
            })
            .cloned()
            .collect();
        assert!(!present.is_empty(), "expected pointers to forget");

        let mut prune = op(Kind::PruneBookmark);
        prune.bookmark = created.bookmark.clone();
        (prune.workspace_id, prune.lease_id) = Default::default();
        prune.trunk = "main".into();
        prune.request_json = payload(&HostRecoveryRequest {
            vouchers: vec![recovery::HostVoucher::RemoteBookmark {
                name: bookmark.head.clone(),
                remote: "origin".into(),
            }],
            ..Default::default()
        });
        let pruned = executor.execute(&command(prune), false, 1).await;
        assert_eq!(pruned.code, v1::ResultCode::Ok as i32, "{}", pruned.reason);
        let Some(command_result::Value::Workspace(pruned)) = pruned.value else {
            panic!("missing prune outcome")
        };
        let outcome: recovery::PruneOutcome = serde_json::from_slice(&pruned.cube_result_json).unwrap();
        match outcome {
            recovery::PruneOutcome::Published { forgotten, .. } => {
                for name in &present {
                    assert!(forgotten.contains(name), "{name} not forgotten: {forgotten:?}");
                    let listed = boss_engine_test_git::jj::JjRepo::run(
                        &repo.repo,
                        &["bookmark", "list", "-T", "name ++ \"\\n\"", name],
                    );
                    assert!(!listed.lines().any(|line| line == name), "{name} survived: {listed}");
                }
            }
            other => panic!("expected published: {other:?}"),
        }
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
