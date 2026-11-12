//! Workspace and recovery operations as typed commands.
//!
//! Every operation is a `WorkspaceOp` whose `kind` selects the fields the host
//! validates; nothing here builds an argv or names a path the host will open.
//! A workspace is addressed by the id and lease the host granted, and the host
//! checks both against its own registry and against the run that holds them.
//!
//! Lease, spawn and release retries reuse their durable commands. Read-only
//! status probes reuse an unanswered request and refresh after it settles.
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, HostRecoveryRequest, PreserveOutcome, PreservedScope, PreservedTip, RestoreReport,
    WorkspaceTipSource,
};
use boss_execution_contracts::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus, LeaseOutcomeUncertain,
    WorkerLife,
};
use boss_protocol_spoke::v1::{
    Bookmark, Command, ResultCode, WorkspaceKind as Kind, WorkspaceOp, WorkspaceResult, command, command_result,
};
use boss_work_db::{SpokeAssignment, host_command_lease_id, host_command_spawn_id};
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
    Reset,
    Force(String),
}

/// How far a run's stored spawn command has got.
enum StoredSpawn {
    /// No spawn was ever stored, so nothing can have started.
    None,
    /// Stored and unanswered: still deliverable.
    Unanswered,
    /// Answered with an uncertain outcome.
    SettledUncertain,
    /// Answered with a definitive result.
    Settled,
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
        if result.code == ResultCode::Rejected as i32
            && let Some(command_result::Value::Workspace(value)) = &result.value
            && let Ok(error) = json::<boss_host_execution::cube::CubeCliError>(value, "cube refusal")
            && error.positioning_refused()
        {
            return Err(error.into());
        }
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
    /// result, however late) while a changed request is a new command. The retry
    /// resends the stored command itself, with its original expiry: a freshly
    /// built one would carry a new expiry and be refused as a different request
    /// under a known id. The host also recognises its own lease by the run, so
    /// even a lost reply cannot produce a second lease.
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
        // A lease that settled as a refusal, or as uncertain with no lease to
        // show for it, is asked again as a new recorded attempt: the host
        // recognises its own lease by the run, so a new id cannot make a second.
        let id = self
            .hub
            .current_attempt(
                self.host_name(),
                &host_command_lease_id(&assignment.run_id, request_digest(&op.encode_to_vec())),
                lease_attempt_is_spent,
            )
            .await?;
        self.set_doubt(&assignment, true).await?;
        let sent = self
            .hub
            .send_stable(
                self.host_name(),
                "lease",
                id.clone(),
                Some(run_of(&assignment)),
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await;
        let uncertain = |reason: String, settled: bool| {
            anyhow::Error::new(LeaseOutcomeUncertain {
                host_id: self.host_name().to_owned(),
                command_id: id.clone(),
                reason,
                settled,
            })
        };
        let result = match sent {
            Ok(result) => result,
            Err(HostCallError::Uncertain { reason, .. }) => return Err(uncertain(reason, false)),
            Err(error) => {
                // A command that was never stored cannot have leased anything.
                if self.hub_command_unknown(&id).await? {
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
        } else if code == ResultCode::OutcomeUncertain {
            return Err(uncertain(
                if result.reason.is_empty() {
                    "the host reported an uncertain outcome".to_owned()
                } else {
                    result.reason
                },
                true,
            ));
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
        self.set_doubt(&assignment, false).await?;
        Ok(lease)
    }

    /// Replay the stored lease command (the same id) and bind a late grant.
    /// Used after the execution has ended, when no dispatch pass will ask.
    pub(crate) async fn ws_complete_in_doubt_lease(&self) -> Result<Option<CubeWorkspaceLease>> {
        let assignment = match self.bound_open().await {
            Ok(assignment) => assignment,
            Err(_) => return Ok(None),
        };
        if let (Some(workspace_id), Some(lease_id), Some(path)) = (
            assignment.workspace_id.clone(),
            assignment.lease_id.clone(),
            assignment.display_path.clone(),
        ) {
            return Ok(Some(CubeWorkspaceLease {
                lease_id,
                workspace_id,
                workspace_path: PathBuf::from(path),
                dirty_verified: None,
            }));
        }
        let (host, run_id) = (self.host_name().to_owned(), assignment.run_id.clone());
        let stored = self.db(move |db| db.current_lease_command(&host, &run_id)).await?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let result = match stored.result {
            Some(bytes) => boss_protocol_spoke::v1::CommandResult::decode(bytes.as_slice())?,
            None => {
                let command = Command::decode(stored.wire.as_slice())?;
                match self
                    .hub
                    .send_command(self.host_name(), "lease", command, self.hub.config.command_deadline)
                    .await
                {
                    Ok(result) => result,
                    Err(HostCallError::Uncertain { reason, .. }) => {
                        return Err(anyhow::Error::new(LeaseOutcomeUncertain {
                            host_id: self.host_name().to_owned(),
                            command_id: stored.command_id.clone(),
                            reason,
                            settled: false,
                        }));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        self.bind_lease_result(&assignment, &stored.command_id, result).await
    }

    async fn bind_lease_result(
        &self,
        assignment: &SpokeAssignment,
        id: &str,
        result: boss_protocol_spoke::v1::CommandResult,
    ) -> Result<Option<CubeWorkspaceLease>> {
        let generation = assignment.assignment_generation;
        let uncertain = |reason: String, settled: bool| {
            anyhow::Error::new(LeaseOutcomeUncertain {
                host_id: self.host_name().to_owned(),
                command_id: id.to_owned(),
                reason,
                settled,
            })
        };
        let code = ResultCode::try_from(result.code).unwrap_or(ResultCode::ResultUnspecified);
        let recovered = code == ResultCode::OutcomeUncertain
            && matches!(&result.value, Some(command_result::Value::Workspace(w)) if !w.lease_id.is_empty());
        if matches!(code, ResultCode::Rejected | ResultCode::Absent) {
            self.set_doubt(assignment, false).await?;
            return Ok(None);
        }
        let result = if recovered {
            result
        } else if code == ResultCode::OutcomeUncertain {
            return Err(uncertain(
                if result.reason.is_empty() {
                    "the host reported an uncertain outcome".to_owned()
                } else {
                    result.reason
                },
                true,
            ));
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
        self.set_doubt(assignment, false).await?;
        Ok(Some(lease))
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

    /// Whether this run has a spawn that may have started a worker. A spawn the
    /// host has not answered stays deliverable, so no present answer about the
    /// worker's session can end the doubt: the session may simply not exist yet.
    /// A spawn that settled as uncertain keeps that result forever; what ends
    /// the doubt then is the host's present answer. Adopt says whether the
    /// worker's session exists: gone (absent, exited or missing) is definitive,
    /// while alive, indeterminate or no answer at all keeps the doubt.
    pub(crate) async fn spawn_in_doubt(&self, assignment: &SpokeAssignment) -> Result<bool> {
        match self.stored_spawn(assignment).await? {
            StoredSpawn::None | StoredSpawn::Settled => return Ok(false),
            StoredSpawn::Unanswered => return Ok(true),
            StoredSpawn::SettledUncertain => {}
        }
        Ok(match self.ctl_observe(&assignment.execution_id).await {
            Ok(None) => false,
            Ok(Some(pane)) => !matches!(pane.life, WorkerLife::Exited | WorkerLife::Missing),
            Err(_) => true,
        })
    }

    /// What the work database holds for this run's spawn command.
    async fn stored_spawn(&self, assignment: &SpokeAssignment) -> Result<StoredSpawn> {
        let (host, id) = (self.host_name().to_owned(), host_command_spawn_id(&assignment.run_id));
        let stored = self
            .db(move |db| Ok((db.host_command_wire(&host, &id)?, db.host_command_result(&host, &id)?)))
            .await?;
        Ok(match stored {
            (None, _) => StoredSpawn::None,
            (Some(_), None) => StoredSpawn::Unanswered,
            (Some(_), Some(bytes)) => {
                let result = boss_protocol_spoke::v1::CommandResult::decode(bytes.as_slice())?;
                if matches!(
                    ResultCode::try_from(result.code),
                    Ok(ResultCode::Ok | ResultCode::Rejected | ResultCode::Absent)
                ) {
                    StoredSpawn::Settled
                } else {
                    StoredSpawn::SettledUncertain
                }
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
        let (name, kind) = match &mode {
            ReleaseMode::Normal => ("release", Kind::Release),
            ReleaseMode::Reset => ("release_reset", Kind::ReleaseWithReset),
            ReleaseMode::Force(_) => ("force_release", Kind::ForceRelease),
        };
        let mut op = Self::op(assignment, kind, true)?;
        if let ReleaseMode::Force(reason) = &mode {
            op.reason = if reason.is_empty() {
                "force release requested by the hub".to_owned()
            } else {
                clip(reason, 4000)
            };
        }
        let id = self
            .hub
            .current_attempt(
                self.host_name(),
                &format!("{name}-{}-{lease_id}", assignment.run_id),
                |result| result.code != ResultCode::Ok as i32,
            )
            .await?;
        let result = self
            .hub
            .send_stable(
                self.host_name(),
                name,
                id,
                Some(run_of(assignment)),
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await?;
        if let Err(error) = expect_ok(self.host_name(), name, result) {
            // A settled uncertain attempt can have ended the lease before cube
            // returned an error. A fresh attempt then cannot pass ownership.
            // Resolve only after the host's complete registry proves it gone.
            if self
                .ws_list_workspaces()
                .await?
                .iter()
                .any(|status| status.lease_id.as_deref() == Some(lease_id))
            {
                return Err(error.into());
            }
        }
        let (execution, generation) = (assignment.execution_id.clone(), assignment.assignment_generation);
        let resolution = if force { "force_released" } else { "released" };
        self.db(move |db| db.resolve_spoke_assignment(&execution, generation, resolution))
            .await?;
        self.prune_settled_commands().await;
        Ok(())
    }

    /// Drop settled commands past the retention window, keeping the newest few
    /// so a retry of a recent id still finds its result. Housekeeping on a
    /// terminal path: a failure is logged and never fails the release.
    async fn prune_settled_commands(&self) {
        let host = self.host_name().to_owned();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let cutoff = now.saturating_sub(boss_work_db::SETTLED_HOST_COMMAND_RETENTION_MS);
        let pruned = self
            .db(move |db| {
                db.prune_settled_host_commands(&host, cutoff, boss_work_db::MIN_RETAINED_SETTLED_HOST_COMMANDS)
            })
            .await;
        if let Err(error) = pruned {
            tracing::warn!(host = self.host_name(), error = %format!("{error:#}"), "pruning settled host commands failed");
        }
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
        let result = self
            .hub
            .send_probe(
                self.host_name(),
                "status",
                format!("status-{}", assignment.run_id),
                run_of(assignment),
                command::Body::Workspace(op),
                self.hub.config.command_deadline,
            )
            .await?;
        json(
            &Self::workspace_value(
                self.host_name(),
                "status",
                expect_ok(self.host_name(), "status", result)?,
            )?,
            "status",
        )
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
        ensure!(
            record.execution_id == assignment.execution_id,
            "recovery record belongs to execution {}, not {}",
            record.execution_id,
            assignment.execution_id
        );
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
            ..Default::default()
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

    /// Whether the preserved tip still holds changes no remote bookmark
    /// carries. A read-only store operation: no lease, safe to rerun.
    pub(crate) async fn rec_preserved_unpublished(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        recorded: &PreservedTip,
    ) -> Result<bool> {
        let request = serde_json::to_vec(&HostRecoveryRequest {
            recorded: Some(recorded.clone()),
            ..Default::default()
        })?;
        let result = self
            .recovery_op(
                "check_preserved_unpublished",
                assignment,
                record,
                Kind::CheckPreservedUnpublished,
                false,
                |op| {
                    op.request_json = request;
                },
            )
            .await?;
        json(&result, "preserved-work check")
    }

    pub(crate) async fn rec_forget_preserved(
        &self,
        assignment: &SpokeAssignment,
        record: &ExecutionBookmark,
        scope: PreservedScope,
    ) -> Result<Vec<String>> {
        let request = serde_json::to_vec(&HostRecoveryRequest {
            scope,
            ..Default::default()
        })?;
        let result = self
            .recovery_op(
                "forget_preserved",
                assignment,
                record,
                Kind::ForgetPreserved,
                false,
                |op| {
                    op.request_json = request;
                },
            )
            .await?;
        json(&result, "preserved reference cleanup")
    }
}

/// Whether a settled lease attempt can no longer produce a lease: the host
/// refused it, or its outcome is unknown and names no lease.
fn lease_attempt_is_spent(result: &boss_protocol_spoke::v1::CommandResult) -> bool {
    match ResultCode::try_from(result.code) {
        Ok(ResultCode::Ok) => false,
        Ok(ResultCode::OutcomeUncertain) => {
            !matches!(&result.value, Some(command_result::Value::Workspace(w)) if !w.lease_id.is_empty())
        }
        _ => true,
    }
}
