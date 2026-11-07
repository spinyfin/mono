//! Workspace and recovery operations as typed commands.
//!
//! Every operation is a `WorkspaceOp` whose `kind` selects the fields the host
//! validates; nothing here builds an argv or names a path the host will open.
//! A workspace is addressed by the id and lease the host granted, and the host
//! checks both against its own registry and against the run that holds them.
//!
//! Only two operations use stable command ids: the lease and the spawn. A retry
//! of either must find the command it already stored instead of acquiring or
//! starting something a second time. Every other operation is a fresh command,
//! and an uncertain one is reconciled by the caller rather than blindly resent.
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, HostRecoveryRequest, PreserveOutcome, PreservedTip, RestoreReport, WorkspaceTipSource,
};
use boss_execution_contracts::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
use boss_protocol_spoke::v1::{
    Bookmark, ResultCode, WorkspaceKind as Kind, WorkspaceOp, WorkspaceResult, command, command_result,
};
use boss_work_db::SpokeAssignment;
use prost::Message;
use serde::de::DeserializeOwned;

use crate::{HostCallError, SpokeHostAdapter, expect_ok, request_digest, run_of};

/// Keep a task label within what the host accepts without dropping its start.
fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].replace('\0', "")
}

fn json<T: DeserializeOwned>(result: &WorkspaceResult, what: &str) -> Result<T> {
    serde_json::from_slice(&result.cube_result_json).with_context(|| format!("decoding the host's {what} result"))
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// How a release ends the lease.
pub(crate) enum ReleaseMode {
    Normal,
    Force(String),
}

impl SpokeHostAdapter {
    /// A workspace op for `assignment`'s repository, addressing its lease when
    /// the kind operates on a working copy (store operations name neither).
    fn op(assignment: &SpokeAssignment, kind: Kind, addresses_lease: bool) -> Result<WorkspaceOp> {
        let mut op = WorkspaceOp {
            kind: kind.into(),
            repo_id: assignment
                .repo_id
                .clone()
                .context("the assignment has not leased in a repository")?,
            ..Default::default()
        };
        if addresses_lease {
            op.workspace_id = assignment
                .workspace_id
                .clone()
                .context("the assignment holds no workspace")?;
            op.lease_id = assignment.lease_id.clone().context("the assignment holds no lease")?;
        }
        Ok(op)
    }

    async fn send(
        &self,
        name: &'static str,
        assignment: &SpokeAssignment,
        op: WorkspaceOp,
        id: Option<String>,
    ) -> Result<WorkspaceResult> {
        let id = id.unwrap_or_else(|| self.hub.unique_id(name, &assignment.run_id));
        let result = self
            .hub
            .send(
                self.host_name(),
                name,
                id,
                Some(run_of(assignment)),
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await?;
        Self::workspace_value(self.host_name(), name, expect_ok(self.host_name(), name, result)?)
    }

    fn workspace_value(
        host: &str,
        name: &str,
        result: boss_protocol_spoke::v1::CommandResult,
    ) -> Result<WorkspaceResult> {
        match result.value {
            Some(command_result::Value::Workspace(value)) => Ok(value),
            _ => bail!("host {host} answered {name} without a workspace result"),
        }
    }

    /// A host-scoped operation: no run, so no ownership to check.
    async fn send_host(&self, name: &'static str, op: WorkspaceOp) -> Result<WorkspaceResult> {
        let id = self.hub.unique_id(name, "host");
        let result = self
            .hub
            .send(
                self.host_name(),
                name,
                id,
                None,
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await?;
        Self::workspace_value(self.host_name(), name, expect_ok(self.host_name(), name, result)?)
    }

    pub(crate) async fn ws_ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        let op = WorkspaceOp {
            kind: Kind::EnsureRepo.into(),
            origin: origin.to_owned(),
            ..Default::default()
        };
        let result = self.send_host("ensure_repo", op).await?;
        ensure!(
            !result.repo_id.is_empty(),
            "host answered ensure_repo without a repository"
        );
        Ok(CubeRepoHandle {
            repo_id: result.repo_id,
        })
    }

    pub(crate) async fn ws_list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        let op = WorkspaceOp {
            kind: Kind::ListRepos.into(),
            ..Default::default()
        };
        json(&self.send_host("list_repos", op).await?, "repo listing")
    }

    pub(crate) async fn ws_list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        let op = WorkspaceOp {
            kind: Kind::ListWorkspaces.into(),
            ..Default::default()
        };
        json(&self.send_host("list_workspaces", op).await?, "workspace listing")
    }

    /// Acquire the assignment's lease. The command id is derived from the
    /// request, so a retry of the same request finds the stored command (and its
    /// result, however late) while a changed request is a new command. The host
    /// also recognises its own lease by the run, so even a lost reply cannot
    /// produce a second lease.
    pub(crate) async fn ws_lease(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        let assignment = self.bound_open().await?;
        let (execution, generation, repo) = (
            assignment.execution_id.clone(),
            assignment.assignment_generation,
            repo_id.to_owned(),
        );
        self.db(move |db| db.bind_spoke_repo(&execution, generation, &repo))
            .await?;
        let op = WorkspaceOp {
            kind: Kind::Lease.into(),
            repo_id: repo_id.to_owned(),
            workspace_id: prefer_workspace_id.unwrap_or_default().to_owned(),
            task_label: clip(task, 1000),
            allow_dirty,
            exclude_workspace_ids: exclude_workspace_ids.iter().map(|id| (*id).to_owned()).collect(),
            ..Default::default()
        };
        let id = format!(
            "lease-{}-{:016x}",
            assignment.run_id,
            request_digest(&op.encode_to_vec())
        );
        self.set_doubt(&assignment, true).await?;
        let sent = self
            .hub
            .send(
                self.host_name(),
                "lease",
                id.clone(),
                Some(run_of(&assignment)),
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await;
        let result = match sent {
            Ok(result) => result,
            Err(error) => {
                // A command that was never stored cannot have leased anything.
                if matches!(error, HostCallError::NotSent { .. }) && self.hub_command_unknown(&id).await? {
                    self.set_doubt(&assignment, false).await?;
                }
                return Err(error.into());
            }
        };
        let code = ResultCode::try_from(result.code).unwrap_or(ResultCode::ResultUnspecified);
        // A lease recovered after a lost reply is reported as uncertain because
        // its setup may not have finished, but it is a real lease and the run
        // owns it: bind it so the work continues and can release it.
        let recovered = code == ResultCode::OutcomeUncertain
            && matches!(&result.value, Some(command_result::Value::Workspace(w)) if !w.lease_id.is_empty());
        if matches!(code, ResultCode::Rejected | ResultCode::Absent) {
            self.set_doubt(&assignment, false).await?;
        }
        let result = if recovered {
            tracing::warn!(host = self.host_name(), run = %assignment.run_id, "recovered an interrupted lease; setup completion is unknown");
            result
        } else {
            expect_ok(self.host_name(), "lease", result)?
        };
        let granted = Self::workspace_value(self.host_name(), "lease", result)?;
        let lease: CubeWorkspaceLease = json(&granted, "lease")?;
        ensure!(
            lease.workspace_id == granted.workspace_id && lease.lease_id == granted.lease_id,
            "host {} answered a lease that disagrees with its own result",
            self.host_name()
        );
        let (execution, workspace, lease_id, path) = (
            assignment.execution_id.clone(),
            lease.workspace_id.clone(),
            lease.lease_id.clone(),
            lease.workspace_path.display().to_string(),
        );
        self.db(move |db| db.bind_spoke_lease(&execution, generation, &workspace, &lease_id, &path))
            .await?;
        Ok(lease)
    }

    async fn set_doubt(&self, assignment: &SpokeAssignment, in_doubt: bool) -> Result<()> {
        let (execution, generation) = (assignment.execution_id.clone(), assignment.assignment_generation);
        self.db(move |db| db.set_spoke_in_doubt(&execution, generation, in_doubt))
            .await
    }

    /// Whether no command with this id was ever stored for the host.
    async fn hub_command_unknown(&self, id: &str) -> Result<bool> {
        let (host, id) = (self.host_name().to_owned(), id.to_owned());
        Ok(self.db(move |db| db.host_command_wire(&host, &id)).await?.is_none())
    }

    /// Whether this run has a spawn that may have started a worker: stored and
    /// either unanswered or answered with an uncertain outcome.
    pub(crate) async fn spawn_in_doubt(&self, assignment: &SpokeAssignment) -> Result<bool> {
        let (host, id) = (self.host_name().to_owned(), format!("spawn-{}", assignment.run_id));
        let stored = self
            .db(move |db| Ok((db.host_command_wire(&host, &id)?, db.host_command_result(&host, &id)?)))
            .await?;
        Ok(match stored {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(_), Some(bytes)) => {
                let result = boss_protocol_spoke::v1::CommandResult::decode(bytes.as_slice())?;
                !matches!(
                    ResultCode::try_from(result.code),
                    Ok(ResultCode::Ok | ResultCode::Rejected | ResultCode::Absent)
                )
            }
        })
    }

    /// Release (or force-release) the lease, and free the assignment once the
    /// host confirms. A release is refused while a spawn is in doubt: the
    /// worker may be running, and only a force release says that is intended.
    pub(crate) async fn ws_release(
        &self,
        assignment: &SpokeAssignment,
        lease_id: &str,
        mode: ReleaseMode,
    ) -> Result<()> {
        let force = matches!(mode, ReleaseMode::Force(_));
        if !force && self.spawn_in_doubt(assignment).await? {
            bail!(
                "the spawn for {} on host {} may have started a worker; refusing to release its workspace \
                 (force release if that is intended)",
                assignment.execution_id,
                self.host_name()
            );
        }
        ensure!(
            assignment.lease_id.as_deref() == Some(lease_id),
            "lease {lease_id} is not the lease of assignment {}",
            assignment.run_id
        );
        let mut op = Self::op(assignment, if force { Kind::ForceRelease } else { Kind::Release }, true)?;
        if let ReleaseMode::Force(reason) = &mode {
            op.reason = if reason.is_empty() {
                "force release requested by the hub".to_owned()
            } else {
                clip(reason, 4000)
            };
        }
        self.send(if force { "force_release" } else { "release" }, assignment, op, None)
            .await?;
        let (execution, generation) = (assignment.execution_id.clone(), assignment.assignment_generation);
        let resolution = if force { "force_released" } else { "released" };
        self.db(move |db| db.resolve_spoke_assignment(&execution, generation, resolution))
            .await?;
        Ok(())
    }

    /// The hub's lease sweep asks "is it still held by this run?". The spoke
    /// extends its own leases locally, so this reads and never extends.
    pub(crate) async fn ws_confirm_lease(&self, assignment: &SpokeAssignment, lease_id: &str) -> Result<()> {
        let status = self.ws_status(assignment).await?;
        ensure!(
            status.state == "leased" && status.lease_id.as_deref() == Some(lease_id),
            "lease {lease_id} is no longer held by workspace {} on host {}",
            status.workspace_id,
            self.host_name()
        );
        Ok(())
    }

    pub(crate) async fn ws_status(&self, assignment: &SpokeAssignment) -> Result<CubeWorkspaceStatus> {
        let op = Self::op(assignment, Kind::Status, true)?;
        json(&self.send("status", assignment, op, None).await?, "status")
    }

    pub(crate) async fn ws_create_change(&self, assignment: &SpokeAssignment, title: &str) -> Result<CubeChangeHandle> {
        let mut op = Self::op(assignment, Kind::CreateChange, true)?;
        op.title = clip(title, 4000);
        let result = self.send("create_change", assignment, op, None).await?;
        ensure!(
            !result.change_id.is_empty(),
            "host answered create_change without a change"
        );
        Ok(CubeChangeHandle {
            change_id: result.change_id,
        })
    }

    pub(crate) async fn ws_goto(&self, assignment: &SpokeAssignment, pr: Option<u64>, revision: &str) -> Result<()> {
        let mut op = Self::op(
            assignment,
            if pr.is_some() { Kind::GotoPr } else { Kind::GotoRevision },
            true,
        )?;
        op.pr_number = pr.unwrap_or_default();
        op.revision = revision.to_owned();
        self.send("goto", assignment, op, None).await?;
        Ok(())
    }

    // ── Recovery ────────────────────────────────────────────────────────────

    fn wire_bookmark(assignment: &SpokeAssignment, record: &ExecutionBookmark) -> Result<Bookmark> {
        Ok(Bookmark {
            repo_id: assignment
                .repo_id
                .clone()
                .context("the assignment has not leased in a repository")?,
            execution_id: record.execution_id.clone(),
            head: record.publication(),
            recovery: record.head(),
            base: record.base(),
            anchor_commit: record.base_commit.clone().unwrap_or_default(),
            pr_head_before: record.pr_head_before.clone().unwrap_or_default(),
        })
    }

    /// The recovery record for a pointer set the host created. Its `repo_path`
    /// is the workspace's display path: the shared store lives on the host and
    /// the hub never opens it, and the host rebuilds the real location itself.
    fn record_from(&self, assignment: &SpokeAssignment, bookmark: &Bookmark) -> ExecutionBookmark {
        ExecutionBookmark {
            execution_id: bookmark.execution_id.clone(),
            repo_path: PathBuf::from(assignment.display_path.clone().unwrap_or_default()),
            host_id: self.host_name().to_owned(),
            base_commit: non_empty(&bookmark.anchor_commit),
            pr_head_before: non_empty(&bookmark.pr_head_before),
        }
    }

    fn owned_record(&self, record: &ExecutionBookmark) -> Result<()> {
        ensure!(
            record.host_id == self.host_name(),
            "recovery bookmark belongs to host {}, not {}",
            record.host_id,
            self.host_name()
        );
        Ok(())
    }

    async fn recovery_op(
        &self,
        name: &'static str,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        kind: Kind,
        addresses_lease: bool,
        fill: impl FnOnce(&mut WorkspaceOp),
    ) -> Result<WorkspaceResult> {
        self.owned_record(record)?;
        let mut op = Self::op(assignment, kind, addresses_lease)?;
        op.bookmark = Some(Self::wire_bookmark(assignment, record)?);
        fill(&mut op);
        self.send(name, assignment, op, None).await
    }

    pub(crate) async fn rec_create(
        &self,
        assignment: &SpokeAssignment,
        execution_id: &str,
        predecessor: Option<&ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<ExecutionBookmark> {
        ensure!(
            assignment.execution_id == execution_id,
            "workspace belongs to execution {}, not {execution_id}",
            assignment.execution_id
        );
        let mut op = Self::op(assignment, Kind::CreateBookmark, true)?;
        if let Some(predecessor) = predecessor {
            self.owned_record(predecessor)?;
            op.predecessor = Some(Self::wire_bookmark(assignment, predecessor)?);
        }
        op.inherited_base = inherited_base.unwrap_or_default().to_owned();
        let result = self.send("create_bookmark", assignment, op, None).await?;
        let bookmark = result.bookmark.context("host created no recovery bookmark")?;
        Ok(self.record_from(assignment, &bookmark))
    }

    pub(crate) async fn rec_diff(&self, assignment: &SpokeAssignment, record: &ExecutionBookmark) -> Result<String> {
        let result = self
            .recovery_op("diff_bookmark", assignment, record, Kind::DiffBookmark, false, |_| {})
            .await?;
        json(&result, "bookmark diff")
    }

    pub(crate) async fn rec_restore(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        self_retry: bool,
    ) -> Result<bool> {
        let result = self
            .recovery_op(
                "restore_bookmark",
                assignment,
                record,
                Kind::RestoreBookmark,
                true,
                |op| {
                    op.self_retry = self_retry;
                },
            )
            .await?;
        json(&result, "bookmark restore")
    }

    pub(crate) async fn rec_discard(&self, assignment: &SpokeAssignment, record: &ExecutionBookmark) -> Result<()> {
        self.recovery_op(
            "discard_baseline",
            assignment,
            record,
            Kind::DiscardBaseline,
            false,
            |_| {},
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn rec_restore_rebased(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        pr_bookmark: Option<&str>,
        base_branch: &str,
    ) -> Result<RestoreReport> {
        let result = self
            .recovery_op(
                "restore_rebased",
                assignment,
                record,
                Kind::RestoreRebased,
                true,
                |op| {
                    op.pr_bookmark = pr_bookmark.unwrap_or_default().to_owned();
                    op.trunk = base_branch.to_owned();
                },
            )
            .await?;
        json(&result, "rebased restore")
    }

    pub(crate) async fn rec_prepare_pr(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        pr: u64,
    ) -> Result<()> {
        self.recovery_op(
            "prepare_recovery_pr",
            assignment,
            record,
            Kind::PrepareRecoveryPr,
            true,
            |op| {
                op.pr_number = pr;
            },
        )
        .await?;
        Ok(())
    }

    /// Operations whose whole answer is one commit id.
    pub(crate) async fn rec_commit(
        &self,
        assignment: &SpokeAssignment,
        kind: Kind,
        name: &'static str,
    ) -> Result<String> {
        let op = Self::op(assignment, kind, true)?;
        json(&self.send(name, assignment, op, None).await?, name)
    }

    pub(crate) async fn rec_confirm(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        recorded: Option<&PreservedTip>,
        since_unix: Option<i64>,
        tip_source: WorkspaceTipSource,
    ) -> Result<PreserveOutcome> {
        let request = serde_json::to_vec(&HostRecoveryRequest {
            recorded: recorded.cloned(),
            since_unix,
            tip_source: Some(tip_source),
            release_names: None,
        })?;
        let result = self
            .recovery_op(
                "confirm_recovery",
                assignment,
                record,
                Kind::ConfirmRecovery,
                true,
                |op| {
                    op.request_json = request;
                },
            )
            .await?;
        json(&result, "recovery confirmation")
    }

    pub(crate) async fn rec_restore_preserved(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        recorded: Option<&PreservedTip>,
    ) -> Result<Option<RestoreReport>> {
        let request = serde_json::to_vec(&HostRecoveryRequest {
            recorded: recorded.cloned(),
            ..Default::default()
        })?;
        let result = self
            .recovery_op(
                "restore_preserved",
                assignment,
                record,
                Kind::RestorePreserved,
                true,
                |op| {
                    op.request_json = request;
                },
            )
            .await?;
        json(&result, "preserved-work restore")
    }

    pub(crate) async fn rec_release_heads(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        release_names: bool,
    ) -> Result<Vec<(String, String)>> {
        // The host releases names unless told otherwise, so only the unusual
        // request carries a payload.
        let request = if release_names {
            Vec::new()
        } else {
            serde_json::to_vec(&HostRecoveryRequest {
                release_names: Some(false),
                ..Default::default()
            })?
        };
        let result = self
            .recovery_op("release_heads", assignment, record, Kind::ReleaseHeads, false, |op| {
                op.request_json = request;
            })
            .await?;
        json(&result, "head release")
    }
}
