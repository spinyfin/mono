//! Spawn, and control of the worker it starts.
//!
//! A spawn is the one command that starts a long-lived process, so it is the
//! one the adapter is most careful never to send twice. Its command id is the
//! run's, its content is frozen the first time it is stored, and a retry after a
//! timeout reuses that command: the host either already ran it (and answers from
//! its record) or has not, and the token-named tmux session means a second
//! attempt could not start a second worker anyway.
//!
//! Control requests (type text, interrupt, stop, observe) are addressed by
//! execution id; the adapter finds the open assignment and names its run on the
//! wire. What comes back says how far the request got, never whether the agent
//! acted on it.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use boss_execution_contracts::{
    ControlOutcome, LocalSpawnFailure, PaneCaptureOutcome, PaneCaptureRequest, RunOutcome, RunWaitState,
    SpawnOutcomeUncertain, WorkerInputKind, WorkerLife, WorkerPane,
};
use boss_protocol::{WorkExecution, WorkItem};
use boss_protocol_spoke::{
    Validate,
    limits::{CANCEL_CAPTURE_TTL_SECONDS, MAX_STOP_GRACE_MS},
    negotiate::feature,
    v1::{
        Capture, Command, CommandResult, Deliver, DeliveryKind, Empty, PaneLife, PaneState, ResultCode, Spawn, Stop,
        command, command_result,
    },
};
use boss_spoke_stream::CommandOutcome;
use boss_work_db::{SpokeAssignment, host_command_spawn_id, host_command_stop_id};
use prost::Message;

use crate::{HostCallError, SpokeHostAdapter, expect_ok, outbox, run_of};

/// What the host needs to start a worker, composed by the engine.
///
/// The adapter owns identity and delivery (the run, the lease, the workspace,
/// the command id); the composer owns what the worker is told and how it is
/// observed (prompt, driver, model, settings, environment). Keeping them apart
/// is what lets prompt policy change without touching the ownership rules.
#[derive(Debug, Clone)]
pub struct ComposedSpawn {
    /// The spawn without its lease and workspace, which come from the
    /// assignment.
    pub spawn: Spawn,
    /// What a successful spawn reports to the dispatcher.
    pub outcome: RunOutcome,
}

#[derive(bon::Builder)]
pub struct SpawnInput<'a> {
    pub worker_id: &'a str,
    pub execution: &'a WorkExecution,
    pub work_item: &'a WorkItem,
    pub assignment: &'a SpokeAssignment,
    /// The workspace's display path on the host, as the lease reported it.
    pub workspace: &'a Path,
    pub cube_change_id: Option<&'a str>,
}

#[async_trait]
pub trait SpawnComposer: Send + Sync {
    async fn compose(&self, input: SpawnInput<'_>) -> Result<ComposedSpawn>;
}

pub(crate) fn pane_of(state: &PaneState) -> WorkerPane {
    WorkerPane {
        life: match PaneLife::try_from(state.life) {
            Ok(PaneLife::Alive) => WorkerLife::Alive,
            Ok(PaneLife::Exited) => WorkerLife::Exited,
            Ok(PaneLife::Missing) => WorkerLife::Missing,
            Ok(PaneLife::BriefNotDelivered) => WorkerLife::BriefNotDelivered,
            _ => WorkerLife::Indeterminate,
        },
        session: state.session.clone(),
        pane_id: state.pane_id.clone(),
        pid: (state.pid > 0).then_some(state.pid),
        exit_code: state.exit_code,
    }
}

fn pane_in(result: &CommandResult) -> Option<WorkerPane> {
    match &result.value {
        Some(command_result::Value::Pane(state)) => Some(pane_of(state)),
        _ => None,
    }
}

enum ControlId {
    Fresh,
    /// A stop: its id is the work database's `stop-{grace_ms}-{run_id}`.
    StableStop {
        grace_ms: u32,
    },
    Refreshing(String),
}

impl SpokeHostAdapter {
    pub(crate) async fn spawn(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        // Everything before the command is sent is local: a failure there is no
        // evidence about an earlier attempt of the same spawn.
        let (assignment, id, had_stored, command, template) = self
            .prepare_spawn(worker_id, execution, work_item, workspace_path, cube_change_id)
            .await
            .map_err(|error| anyhow::Error::new(LocalSpawnFailure(error)))?;
        let sent = self
            .hub
            .send_command(self.host_name(), "spawn", command, self.hub.config.spawn_deadline)
            .await;
        let result = match sent {
            Ok(result) => result,
            Err(HostCallError::NotSent { reason, .. }) if !had_stored => {
                bail!(
                    "host {} could not take the spawn; nothing was sent: {reason}",
                    self.host_name()
                )
            }
            // An earlier attempt may have been delivered: do not call this a failure.
            Err(HostCallError::NotSent { reason, .. } | HostCallError::Uncertain { reason, .. }) => {
                return Err(self.uncertain_spawn(&id, reason, false));
            }
            Err(other) => return Err(other.into()),
        };
        if boss_protocol_spoke::spawn_result_is_uncertain(&result) {
            return self
                .finish_settled_uncertain_spawn(&assignment, execution, &id, result.reason, template)
                .await;
        }
        match ResultCode::try_from(result.code) {
            Ok(ResultCode::Ok) => {}
            Ok(ResultCode::Rejected) => bail!("host {} rejected the spawn: {}", self.host_name(), result.reason),
            Ok(ResultCode::Absent) => bail!(
                "the worker's session on host {} was already gone when the spawn finished: {}",
                self.host_name(),
                result.reason
            ),
            _ => {
                return self
                    .finish_settled_uncertain_spawn(&assignment, execution, &id, result.reason, template)
                    .await;
            }
        }
        let pane = pane_in(&result).context("the host reported a spawn without the worker's pane")?;
        match pane.life {
            WorkerLife::Alive => {}
            WorkerLife::BriefNotDelivered => bail!(
                "the worker launched on host {} but its initial brief was never delivered",
                self.host_name()
            ),
            WorkerLife::Exited => bail!(
                "the worker on host {} exited during launch (exit code {:?})",
                self.host_name(),
                pane.exit_code
            ),
            WorkerLife::Missing | WorkerLife::Indeterminate => {
                return self
                    .finish_settled_uncertain_spawn(
                        &assignment,
                        execution,
                        &id,
                        "the host could not confirm the worker's session".to_owned(),
                        template,
                    )
                    .await;
            }
        }
        self.finish_launch(&assignment, execution, &id, &pane, template).await
    }

    /// The assignment, command id, whether a spawn was already stored, and the
    /// (possibly stored) spawn command with the outcome template, all read and
    /// composed locally.
    async fn prepare_spawn(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<(SpokeAssignment, String, bool, Command, Option<RunOutcome>)> {
        let assignment = self.bound_open().await?;
        ensure!(
            assignment.execution_id == execution.id,
            "adapter is bound to execution {}, not {}",
            assignment.execution_id,
            execution.id
        );
        let lease_id = assignment
            .lease_id
            .clone()
            .context("spawn before the lease was bound")?;
        let workspace_id = assignment
            .workspace_id
            .clone()
            .context("spawn before the workspace was bound")?;
        if let Some(recorded) = execution.cube_lease_id.as_deref() {
            ensure!(
                recorded == lease_id,
                "execution records lease {recorded} but its assignment holds {lease_id}"
            );
        }
        let id = host_command_spawn_id(&assignment.run_id);

        // The first stored spawn is the spawn: a retry sends exactly that command.
        let (host, stored_id) = (self.host_name().to_owned(), id.clone());
        let stored = self
            .db(move |db| db.host_command_wire(&host, &stored_id))
            .await?
            .map(|wire| Command::decode(wire.as_slice()))
            .transpose()
            .context("decoding the stored spawn")?;

        let had_stored = stored.is_some();
        let composed = self
            .hub
            .composer
            .compose(
                SpawnInput::builder()
                    .worker_id(worker_id)
                    .execution(execution)
                    .work_item(work_item)
                    .assignment(&assignment)
                    .workspace(workspace_path)
                    .maybe_cube_change_id(cube_change_id)
                    .build(),
            )
            .await;
        let (mut command, template) = match (stored, composed) {
            (Some(stored), Ok(composed)) => (stored, Some(composed.outcome)),
            (Some(stored), Err(error)) => {
                tracing::warn!(error = %format!("{error:#}"), "recomposing a retried spawn failed; reusing the stored command");
                (stored, None)
            }
            (None, Ok(composed)) => {
                let mut spawn = composed.spawn;
                spawn.lease_id = lease_id;
                spawn.workspace_id = workspace_id;
                spawn
                    .validate()
                    .map_err(|error| anyhow::anyhow!("invalid spawn: {error}"))?;
                (
                    self.hub
                        .new_command(&id, Some(run_of(&assignment)), command::Body::Spawn(spawn)),
                    Some(composed.outcome),
                )
            }
            (None, Err(error)) => return Err(error.context("composing the worker spawn")),
        };
        command.sequence = 0;

        Ok((assignment, id, had_stored, command, template))
    }

    fn uncertain_spawn(&self, command_id: &str, reason: String, settled: bool) -> anyhow::Error {
        anyhow::Error::new(SpawnOutcomeUncertain {
            host_id: self.host_name().to_owned(),
            command_id: command_id.to_owned(),
            reason,
            settled,
        })
    }

    /// What follows a pane the host reports alive, whichever way the report
    /// arrived (the spawn's own result, or an Adopt after it settled as
    /// uncertain): record the pid the liveness sweeps key on, then insist the
    /// worker accepted its brief before calling it launched. An interrupted
    /// launch is exactly when the brief may never have arrived.
    async fn finish_launch(
        &self,
        assignment: &SpokeAssignment,
        execution: &WorkExecution,
        command_id: &str,
        pane: &WorkerPane,
        template: Option<RunOutcome>,
    ) -> Result<RunOutcome> {
        if let Some(pid) = pane.pid {
            let (run_execution, generation, run_id) = (
                assignment.execution_id.clone(),
                assignment.assignment_generation,
                execution.id.clone(),
            );
            // The pid is only the key the existing liveness sweeps look up; the
            // token, not the pid, identifies the worker.
            let recorded = self
                .db(move |db| {
                    db.record_spoke_pane_pid(&run_execution, generation, pid as i64)?;
                    db.set_run_remote_pid_for_execution(&run_id, pid as i64)?;
                    Ok(())
                })
                .await;
            if let Err(error) = recorded {
                tracing::warn!(error = %format!("{error:#}"), "could not record the worker's pane pid");
            }
        }
        if self.stop_unconfirmed.load(std::sync::atomic::Ordering::SeqCst) {
            // The flag lives only on this adapter instance. A new adapter (the
            // in-doubt sweep's, or one created after a restart) never sees it
            // and waits for prompt evidence the usual way.
            if self.hub.signals.has_prompt_accepted_for_run(&execution.id) {
                self.stop_unconfirmed.store(false, std::sync::atomic::Ordering::SeqCst);
            } else {
                return self.retry_unconfirmed_stop(execution, command_id).await;
            }
        }
        let mut stopped = None;
        let confirmed = boss_execution_contracts::confirm_prompt_accepted_or_reap(
            "spoke",
            &execution.id,
            self.hub.config.prompt_timeout,
            Duration::from_millis(100),
            || async { self.hub.signals.has_prompt_accepted_for_run(&execution.id) },
            || async {
                stopped = Some(self.ctl_stop(&execution.id, Duration::ZERO).await);
            },
        )
        .await;
        if let Err(error) = confirmed {
            match stopped {
                Some(Ok(ControlOutcome::Delivered(_))) => return Err(error),
                // The spawn itself is settled: what is unknown is whether the
                // worker was stopped, which a replay of the spawn cannot learn.
                other => {
                    self.stop_unconfirmed.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Err(self.uncertain_spawn(
                        command_id,
                        format!("{error:#}; worker stop was not confirmed: {other:?}"),
                        true,
                    ));
                }
            }
        }
        let mut outcome = template.unwrap_or(RunOutcome {
            wait_state: RunWaitState::WorkerPaneAlive,
            result_summary: None,
            attention: None,
            slot_id: None,
            spawn_config: None,
        });
        outcome.result_summary = Some(format!(
            "Launched worker for execution {} on spoke host {} (session {}).",
            execution.id,
            self.host_name(),
            pane.session
        ));
        Ok(outcome)
    }

    /// A launched worker whose prompt was never confirmed and whose stop is
    /// still unconfirmed: ask for the stable stop again, and only that. Prompt
    /// acceptance is checked by [`Self::finish_launch`] before this runs; a
    /// delivered stop is the definitive failure the first attempt could not
    /// establish. The `stop_unconfirmed` flag is in-memory on this adapter
    /// instance only, so a new adapter falls back to the full prompt wait.
    async fn retry_unconfirmed_stop(&self, execution: &WorkExecution, command_id: &str) -> Result<RunOutcome> {
        match self.ctl_stop(&execution.id, Duration::ZERO).await {
            Ok(ControlOutcome::Delivered(_)) => {
                self.stop_unconfirmed.store(false, std::sync::atomic::Ordering::SeqCst);
                bail!(
                    "the worker on host {} never confirmed its prompt and was stopped",
                    self.host_name()
                )
            }
            other => Err(self.uncertain_spawn(command_id, format!("worker stop was not confirmed: {other:?}"), true)),
        }
    }

    /// A spawn command that already settled as uncertain is not unanswered:
    /// re-sending it only replays the stored result. Adopt the pane instead.
    /// Alive means the worker launched; Missing/Absent is a definite failure;
    /// anything else stays uncertain so the coordinator can park.
    async fn finish_settled_uncertain_spawn(
        &self,
        assignment: &SpokeAssignment,
        execution: &WorkExecution,
        command_id: &str,
        reason: String,
        template: Option<RunOutcome>,
    ) -> Result<RunOutcome> {
        match self.ctl_observe(&assignment.execution_id).await {
            Ok(Some(pane)) if pane.life == WorkerLife::Alive => {
                self.finish_launch(assignment, execution, command_id, &pane, template)
                    .await
            }
            Ok(None)
            | Ok(Some(WorkerPane {
                life: WorkerLife::Missing | WorkerLife::Exited,
                ..
            })) => {
                bail!(
                    "the worker's session on host {} was gone when spawn settled as uncertain: {reason}",
                    self.host_name()
                )
            }
            _ => Err(self.uncertain_spawn(command_id, reason, true)),
        }
    }

    // ── Control ─────────────────────────────────────────────────────────────

    async fn control(
        &self,
        execution_id: &str,
        name: &'static str,
        deadline: Duration,
        body: command::Body,
    ) -> Result<std::result::Result<CommandResult, ControlOutcome>> {
        self.control_with(execution_id, name, deadline, body, ControlId::Fresh)
            .await
    }

    /// The caller chooses whether completion is reusable or needs a fresh observation.
    async fn control_with(
        &self,
        execution_id: &str,
        name: &'static str,
        deadline: Duration,
        body: command::Body,
        policy: ControlId,
    ) -> Result<std::result::Result<CommandResult, ControlOutcome>> {
        let Some(assignment) = self.control_target(execution_id).await? else {
            return Ok(Err(ControlOutcome::NotSubmitted(format!(
                "execution {execution_id} has no open spoke assignment on host {}",
                self.host_name()
            ))));
        };
        let sent = match policy {
            ControlId::Refreshing(prefix) => {
                return Ok(Ok(self
                    .hub
                    .send_probe(
                        self.host_name(),
                        name,
                        format!("{prefix}-{}", assignment.run_id),
                        run_of(&assignment),
                        body,
                        deadline,
                    )
                    .await?));
            }
            ControlId::StableStop { grace_ms } => {
                let id = self
                    .stable_control_id(host_command_stop_id(grace_ms, &assignment.run_id))
                    .await?;
                self.hub
                    .send_stable(self.host_name(), name, id, Some(run_of(&assignment)), body, deadline)
                    .await
            }
            ControlId::Fresh => {
                let id = self.hub.unique_id(name, &assignment.run_id);
                self.hub
                    .send(self.host_name(), name, id, Some(run_of(&assignment)), body, deadline)
                    .await
            }
        };
        Ok(match sent {
            Ok(result) => Ok(result),
            Err(HostCallError::NotSent { reason, .. }) => Err(ControlOutcome::NotSubmitted(reason)),
            Err(HostCallError::Uncertain { reason, .. }) => Err(ControlOutcome::Uncertain(reason)),
            Err(HostCallError::Rejected { reason, .. }) => Err(ControlOutcome::Rejected { reason, pane: None }),
        })
    }

    /// The id a repeatable control request uses for this run. A retry of a stop
    /// whose outcome is unknown, or a repeat of one that already succeeded, lands
    /// on the same stored command rather than queueing another. A stop that
    /// settled as anything but success (refused, nothing to stop, or an
    /// uncertain outcome) is asked again as a new, recorded attempt, which is
    /// itself reused until it settles.
    async fn stable_control_id(&self, base_id: String) -> Result<String> {
        self.hub
            .current_attempt(self.host_name(), &base_id, |result| {
                ResultCode::try_from(result.code) != Ok(ResultCode::Ok)
            })
            .await
    }

    fn control_outcome(result: CommandResult) -> ControlOutcome {
        let pane = pane_in(&result);
        match ResultCode::try_from(result.code) {
            Ok(ResultCode::Ok) => match pane {
                Some(pane) => ControlOutcome::Delivered(pane),
                None => ControlOutcome::Uncertain("the host reported success without the worker's pane".to_owned()),
            },
            Ok(ResultCode::Rejected | ResultCode::Absent) => ControlOutcome::Rejected {
                reason: result.reason,
                pane,
            },
            _ => ControlOutcome::Uncertain(result.reason),
        }
    }

    async fn deliver(
        &self,
        execution_id: &str,
        name: &'static str,
        kind: DeliveryKind,
        text: &str,
    ) -> Result<ControlOutcome> {
        let body = command::Body::Deliver(Deliver {
            kind: kind.into(),
            input_utf8: text.as_bytes().to_vec(),
        });
        Ok(
            match self
                .control(execution_id, name, self.hub.config.command_deadline, body)
                .await?
            {
                Ok(result) => Self::control_outcome(result),
                Err(outcome) => outcome,
            },
        )
    }

    pub(crate) async fn ctl_deliver(
        &self,
        execution_id: &str,
        kind: WorkerInputKind,
        text: &str,
    ) -> Result<ControlOutcome> {
        let (name, kind) = match kind {
            WorkerInputKind::Probe => ("probe", DeliveryKind::Probe),
            WorkerInputKind::Nudge => ("nudge", DeliveryKind::Nudge),
            WorkerInputKind::Answer => ("answer", DeliveryKind::Answer),
        };
        self.deliver(execution_id, name, kind, text).await
    }

    pub(crate) async fn ctl_interrupt(&self, execution_id: &str) -> Result<ControlOutcome> {
        self.deliver(execution_id, "interrupt", DeliveryKind::Interrupt, "")
            .await
    }

    pub(crate) async fn ctl_stop(&self, execution_id: &str, grace: Duration) -> Result<ControlOutcome> {
        let grace_ms = grace.as_millis().min(u128::from(MAX_STOP_GRACE_MS)) as u32;
        let deadline = self.hub.config.command_deadline + Duration::from_millis(u64::from(grace_ms));
        Ok(
            match self
                .control_with(
                    execution_id,
                    "stop",
                    deadline,
                    command::Body::Stop(Stop { grace_ms }),
                    ControlId::StableStop { grace_ms },
                )
                .await?
            {
                Ok(result) => Self::control_outcome(result),
                Err(outcome) => outcome,
            },
        )
    }

    /// What the host sees of the worker now. `None` when this adapter does not
    /// supervise it or the host has no record of it; an error when the question
    /// could not be answered, which is never evidence either way.
    pub(crate) async fn ctl_observe(&self, execution_id: &str) -> Result<Option<WorkerPane>> {
        if self.control_target(execution_id).await?.is_none() {
            return Ok(None);
        }
        let sent = self
            .control_with(
                execution_id,
                "adopt",
                self.hub.config.command_deadline,
                command::Body::Adopt(Empty {}),
                ControlId::Refreshing("adopt".to_owned()),
            )
            .await?;
        match sent {
            Err(other) => bail!("could not observe the worker: {other:?}"),
            Ok(result) => match ResultCode::try_from(result.code) {
                Ok(ResultCode::Absent) => Ok(None),
                Ok(ResultCode::Ok) => Ok(pane_in(&result)),
                _ => {
                    let result = expect_ok(self.host_name(), "adopt", result)?;
                    Ok(pane_in(&result))
                }
            },
        }
    }

    /// Open, renew or cancel a viewer's capture of the worker's pane. The
    /// snapshots themselves arrive on the hub's capture feed, not here.
    ///
    /// A renewal is a fresh command with a short expiry (`not_after`), never a
    /// retry of an old one: a viewer that left or a host that was away must not
    /// find the subscription reopened by a stale command. Nothing is sent when
    /// the host is not connected or did not negotiate capture.
    ///
    /// Every command that is sent, including each renewal and cancel, is a row
    /// in the durable host command outbox. It counts against the host's budget
    /// of unsettled commands until the spoke answers, and once settled it is
    /// kept for the outbox's ordinary retention period like any other command.
    pub(crate) async fn ctl_capture(
        &self,
        execution_id: &str,
        request: &PaneCaptureRequest,
    ) -> Result<PaneCaptureOutcome> {
        /// A capture command is for a viewer who is looking now.
        const COMMAND_TTL: Duration = Duration::from_secs(20);
        const DEADLINE: Duration = Duration::from_secs(10);
        let Some(assignment) = self.control_target(execution_id).await? else {
            return Ok(PaneCaptureOutcome::Ended);
        };
        let host = self.host_name();
        match self.hub.streams.host_supports(host, feature::PANE_CAPTURE).await {
            None => return Ok(PaneCaptureOutcome::Unreachable(format!("host {host} is not connected"))),
            Some(false) => {
                return Ok(PaneCaptureOutcome::Unsupported(format!(
                    "host {host} did not negotiate pane capture"
                )));
            }
            Some(true) => {}
        }
        // One second or less would cancel, so a renewal never asks for it.
        let ttl_seconds = if request.cancel {
            CANCEL_CAPTURE_TTL_SECONDS
        } else if request.ttl_seconds == 0 {
            0
        } else {
            request.ttl_seconds.max(CANCEL_CAPTURE_TTL_SECONDS + 1)
        };
        let id = self.hub.unique_id("capture", &assignment.run_id);
        let mut command = self.hub.new_command(
            &id,
            Some(run_of(&assignment)),
            command::Body::Capture(Capture {
                subscription_id: request.subscription_id.clone(),
                interval_ms: request.interval_ms,
                max_bytes: request.max_bytes,
                ttl_seconds,
            }),
        );
        command.not_after_unix_ms = (std::time::SystemTime::now() + COMMAND_TTL)
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let sent = self
            .hub
            .streams
            .command(host, outbox::command_admission(&command), command, DEADLINE)
            .await;
        Ok(match sent {
            Ok(result) => match ResultCode::try_from(result.code) {
                Ok(ResultCode::Ok) => PaneCaptureOutcome::Accepted,
                Ok(ResultCode::Absent) => PaneCaptureOutcome::Ended,
                Ok(ResultCode::Rejected) => PaneCaptureOutcome::Refused(result.reason),
                _ => PaneCaptureOutcome::Unreachable(format!("host {host} could not answer: {}", result.reason)),
            },
            Err(CommandOutcome::Invalid) => PaneCaptureOutcome::Refused("the capture request was invalid".to_owned()),
            Err(other) => PaneCaptureOutcome::Unreachable(other.to_string()),
        })
    }

    /// Liveness for the existing remote-worker sweeps, which know a worker only
    /// by the pid recorded at launch. A pid that belongs to an open spoke
    /// assignment is answered from the token-named session, so a recycled pid
    /// cannot confuse it; any other pid is not the spoke's to judge and is
    /// answered by the legacy transport the run started on.
    pub(crate) async fn probe_pid_alive(&self, pid: i64) -> Result<Option<bool>> {
        let host = self.host_name().to_owned();
        let Some(assignment) = self
            .db(move |db| db.open_spoke_assignment_for_pane_pid(&host, pid))
            .await?
        else {
            return self.legacy_probe_pid(pid).await;
        };
        let Some(pane) = self.ctl_observe(&assignment.execution_id).await? else {
            return Ok(None);
        };
        match pane.life {
            WorkerLife::Alive => Ok(Some(true)),
            WorkerLife::Exited | WorkerLife::Missing | WorkerLife::BriefNotDelivered => Ok(Some(false)),
            // Not evidence of death: a host outage must never read as one.
            WorkerLife::Indeterminate => bail!("host {} could not read the worker's session", self.host_name()),
        }
    }
}
