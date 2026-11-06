//! Token-identified tmux worker sessions: Spawn, Adopt, Deliver, Stop.

use crate::{
    cube::{self, CubeJsonTransport},
    homes, input,
};
use anyhow::{Context, Result, ensure};
use boss_engine_driver::{
    DriverRegistry, SpawnPlan, SpawnRequest,
    initial_prompt::{self, delivered_marker_present, is_brief_not_delivered},
    render_env_directive,
};
use boss_protocol::DriverRuntimeState;
use boss_protocol_spoke::{
    Validate,
    v1::{
        Command, CommandResult, Deliver, DeliveryKind, PaneLife, PaneState, ResultCode, Run, Spawn, Stop, command,
        command_result,
    },
};
use boss_tmux::{
    DisplayField, KillSessionError, KillSessionOutcome, NewSession, TMUX_SPAWN_TOKEN_ENV, Tmux,
    insert_color_environment, quote_for_shell,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
pub const HEARTBEAT_TTL_SECONDS: u64 = 300;
pub const BRIEF_DELIVERY_BUDGET: Duration = Duration::from_secs(35);
/// How often the command loop looks for workers that exited since the last look.
pub const EXIT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_SCHEMA: &str = "1";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const RUN_ID_ENV: &str = "BOSS_RUN_ID";
const SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";
const HOST_ENV_DENY: &[&str] = &[
    "BOSS_CODEX_HOMES_DIR",
    "HOME",
    "TMUX",
    "TMUX_TMPDIR",
    "BOSS_CONTROL_SOCKET",
    "BOSS_EVENTS_SOCKET",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Intent,
    Launching,
    Alive,
    Delivering,
    Stopping,
    Terminal,
}

impl Phase {
    fn assigned(self) -> bool {
        !matches!(self, Self::Terminal)
    }
}

/// Durable progress of the one Deliver command that last touched the pane.
/// Written before any key is sent, so a crash can tell "never started" (no
/// entry) from "may have been injected" (entry, not completed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryProgress {
    pub command_id: String,
    pub completed: bool,
}

/// What the pane showed when the worker was found finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEvidence {
    pub life: i32,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub run_id: String,
    pub spawn_token: String,
    pub assignment_generation: u64,
    pub lease_id: String,
    pub workspace_id: String,
    pub session: String,
    pub phase: Phase,
    pub boot_id: String,
    pub pane_id: Option<String>,
    pub pid: Option<u64>,
    pub runtime_state: Option<DriverRuntimeState>,
    pub home: Option<PathBuf>,
    /// Driver that launched the worker; Deliver needs it for the interrupt gesture.
    #[serde(default)]
    #[builder(default)]
    pub driver_id: String,
    /// Driver settings file kept outside the workspace tree; removed with the home.
    #[serde(default)]
    pub settings_file: Option<PathBuf>,
    #[serde(default)]
    pub delivery: Option<DeliveryProgress>,
    #[serde(default)]
    pub terminal: Option<TerminalEvidence>,
    /// True once the run's input home and settings file are gone. A Terminal
    /// record with this unset is retried by the exit sweep.
    #[serde(default)]
    #[builder(default)]
    pub reclaimed: bool,
    #[serde(default)]
    pub reclaim_error: Option<String>,
}

pub trait ExecutionStore: Send + Sync {
    fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>>;
    fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()>;
    fn assigned(&self) -> Result<Vec<Vec<u8>>>;
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Supervisor<T, S> {
    pub cube: T,
    pub tmux: Tmux,
    pub store: S,
    pub host_id: String,
    pub homes_root: PathBuf,
    pub boot_id: String,
    pub brief_budget: Duration,
}

pub fn open_tmux(program: Option<&Path>, socket: impl Into<PathBuf>) -> Result<Tmux> {
    match program {
        Some(path) => Tmux::from_path_with_socket(path, socket),
        None => Tmux::resolve(socket),
    }
}

pub fn session_name(spawn_token: &str) -> String {
    format!("boss-{spawn_token}")
}

/// Exact-match tmux target: a bare session name falls back to prefix matching,
/// which could reach a sibling or replacement session.
fn exact_target(session: &str) -> String {
    format!("={session}:")
}

fn decode(bytes: &[u8]) -> Result<ExecutionRecord> {
    Ok(serde_json::from_slice(bytes)?)
}

fn assigned_records(store: &impl ExecutionStore) -> Result<Vec<ExecutionRecord>> {
    store.assigned()?.into_iter().map(|bytes| decode(&bytes)).collect()
}

impl<T: CubeJsonTransport, S: ExecutionStore> Supervisor<T, S> {
    fn persist(&self, record: &ExecutionRecord) -> Result<()> {
        self.store
            .save(&record.run_id, &record.spawn_token, &serde_json::to_vec(record)?)
    }

    fn loaded(&self, run: &Run) -> Result<Option<ExecutionRecord>> {
        self.store
            .load(&run.run_id, &run.spawn_token)?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    pub async fn execute(&self, command: &Command, interrupted: bool, now_ms: i64) -> CommandResult {
        match self.dispatch(command, interrupted, now_ms).await {
            Ok(result) => result,
            Err(error) => outcome(command, ResultCode::OutcomeUncertain, error.to_string(), None),
        }
    }

    async fn dispatch(&self, command: &Command, interrupted: bool, now_ms: i64) -> Result<CommandResult> {
        if let Err(error) = command.validate() {
            return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
        }
        if !interrupted && command.not_after_unix_ms <= now_ms {
            return Ok(outcome(command, ResultCode::Rejected, "command expired", None));
        }
        let Some(run) = command.run.as_ref() else {
            return Ok(outcome(command, ResultCode::Rejected, "run identity required", None));
        };
        Ok(match &command.body {
            Some(command::Body::Spawn(spawn)) => self.spawn(command, run, spawn, interrupted).await?,
            Some(command::Body::Adopt(_)) => self.adopt(command, run).await?,
            Some(command::Body::Deliver(deliver)) => self.deliver(command, run, deliver, interrupted).await?,
            Some(command::Body::Stop(stop)) => self.stop(command, run, stop, interrupted).await?,
            _ => outcome(command, ResultCode::Rejected, "execution command required", None),
        })
    }

    async fn spawn(&self, command: &Command, run: &Run, spawn: &Spawn, interrupted: bool) -> Result<CommandResult> {
        if let Err(error) = std::str::from_utf8(&spawn.prompt_utf8) {
            return Ok(outcome(
                command,
                ResultCode::Rejected,
                format!("initial prompt must be UTF-8: {error}"),
                None,
            ));
        }
        if let Err(error) = homes::preflight(Some(&self.homes_root)) {
            return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
        }
        if let Some(existing) = self.loaded(run)? {
            if interrupted {
                return self.reconcile_launch(command, existing).await;
            }
            return Ok(match existing.phase {
                Phase::Alive | Phase::Delivering | Phase::Launching | Phase::Intent | Phase::Stopping => outcome(
                    command,
                    ResultCode::Rejected,
                    "duplicate spawn for this run token",
                    Some(self.observe(&existing).await),
                ),
                Phase::Terminal => outcome(
                    command,
                    ResultCode::Rejected,
                    "spawn token already used",
                    Some(self.observe(&existing).await),
                ),
            });
        }
        let mut record = ExecutionRecord {
            execution_id: run.execution_id.clone(),
            run_id: run.run_id.clone(),
            spawn_token: run.spawn_token.clone(),
            assignment_generation: run.assignment_generation,
            lease_id: spawn.lease_id.clone(),
            workspace_id: spawn.workspace_id.clone(),
            session: session_name(&run.spawn_token),
            phase: Phase::Intent,
            boot_id: self.boot_id.clone(),
            pane_id: None,
            pid: None,
            runtime_state: None,
            home: None,
            driver_id: spawn.driver_id.clone(),
            settings_file: None,
            delivery: None,
            terminal: None,
            reclaimed: false,
            reclaim_error: None,
        };
        self.persist(&record)?;
        if interrupted {
            return self.reconcile_launch(command, record).await;
        }
        let workspace = match self.workspace_path(&spawn.workspace_id, &spawn.lease_id).await {
            Ok(path) => path,
            Err(error) => {
                record.phase = Phase::Terminal;
                record.reclaimed = true;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        let driver = match DriverRegistry::default().require(&spawn.driver_id) {
            Ok(driver) => driver,
            Err(error) => {
                record.phase = Phase::Terminal;
                record.reclaimed = true;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        let runtime = match input::provision(driver.as_ref(), &workspace, &spawn.prompt_utf8, &run.run_id).await {
            Ok(runtime) => runtime,
            Err(error) => {
                record.phase = Phase::Terminal;
                record.reclaimed = true;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        record.runtime_state = runtime.clone();
        record.home = runtime.as_ref().and_then(initial_prompt::owned_home_from_runtime_state);
        if !spawn.settings_json.is_empty() {
            record.settings_file = Some(self.settings_path(&record.spawn_token));
        }
        record.phase = Phase::Launching;
        self.persist(&record)?;
        if let Err(error) = self.launch(&record, spawn, driver.as_ref(), &workspace).await {
            return self.launch_failed(command, record, error).await;
        }
        let pane_id = self
            .tmux
            .display_message(&record.session, DisplayField::PaneId)
            .await
            .ok();
        let pid = self
            .tmux
            .display_message(&record.session, DisplayField::PanePid)
            .await
            .ok()
            .and_then(|text| text.parse().ok())
            .filter(|pid: &u64| *pid > 0);
        record.pane_id = pane_id;
        record.pid = pid;
        record.phase = Phase::Alive;
        self.persist(&record)?;
        let pane = self.wait_for_brief(&record).await;
        self.settle(&mut record, &pane).await?;
        let code = match pane_life(&pane) {
            PaneLife::Indeterminate => ResultCode::OutcomeUncertain,
            PaneLife::Missing => ResultCode::Absent,
            _ => ResultCode::Ok,
        };
        Ok(outcome(command, code, String::new(), Some(pane)))
    }

    /// Driver settings live beside the run's private homes, never in the
    /// worker's workspace tree where a VCS snapshot would pick them up.
    /// The file name is a hex encoding of the spawn token so distinct
    /// tokens cannot share or delete one file.
    fn settings_path(&self, spawn_token: &str) -> PathBuf {
        self.homes_root
            .join(format!("settings-{}.json", hex_token(spawn_token)))
    }

    async fn launch(
        &self,
        record: &ExecutionRecord,
        spawn: &Spawn,
        driver: &dyn boss_engine_driver::AgentDriver,
        workspace: &Path,
    ) -> Result<()> {
        if let Some(path) = &record.settings_file {
            initial_prompt::write_private(path, &spawn.settings_json)?;
        }
        let plan = driver.spawn_invocation(SpawnRequest {
            model: &spawn.model,
            effort: None,
            settings_path: record.settings_file.as_deref(),
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(&record.run_id),
        });
        self.tmux.start_private_server().await?;
        // Caller environment first; the identity keys are stamped last so a
        // caller can never make the worker look foreign to its own supervisor.
        let mut environment = BTreeMap::new();
        for env in &spawn.env {
            if HOST_ENV_DENY.contains(&env.name.as_str()) {
                continue;
            }
            environment.insert(env.name.clone(), env.value.clone());
        }
        insert_color_environment(&mut environment);
        environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), record.spawn_token.clone());
        environment.insert(SESSION_SCHEMA_ENV.to_owned(), SESSION_SCHEMA.to_owned());
        environment.insert(RUN_ID_ENV.to_owned(), record.execution_id.clone());
        environment.insert(homes::ROOT_ENV.to_owned(), self.homes_root.display().to_string());
        self.tmux
            .new_session(&NewSession {
                name: record.session.clone(),
                environment,
                working_directory: workspace.to_path_buf(),
                command: pane_command(&plan),
            })
            .await?;
        self.tmux.set_option(&record.session, "remain-on-exit", "on").await.ok();
        self.tmux
            .set_option(&record.session, SPAWN_TOKEN_OPTION, &record.spawn_token)
            .await?;
        if let Some(config) = boss_engine_driver::tmux_session_config_for(&spawn.driver_id)
            && let Ok(version) = self.tmux.version().await
            && version.supports_source_file_target()
        {
            let _ = self.tmux.source_file(&record.session, config).await;
        }
        Ok(())
    }

    async fn launch_failed(
        &self,
        command: &Command,
        mut record: ExecutionRecord,
        error: anyhow::Error,
    ) -> Result<CommandResult> {
        let pane = self.observe(&record).await;
        match pane_life(&pane) {
            PaneLife::Alive | PaneLife::Exited | PaneLife::BriefNotDelivered => {
                record.phase = Phase::Alive;
                record.pane_id = (!pane.pane_id.is_empty()).then(|| pane.pane_id.clone());
                record.pid = (pane.pid > 0).then_some(pane.pid);
                self.persist(&record)?;
                Ok(outcome(command, ResultCode::Ok, error.to_string(), Some(pane)))
            }
            PaneLife::Missing => {
                self.record_terminal(&mut record, &pane)?;
                Ok(outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    format!("launch may have happened: {error:#}"),
                    Some(pane),
                ))
            }
            // Never reclaim or retire a record whose pane could not be read:
            // the worker may be alive, and a later reconcile decides.
            _ => Ok(outcome(
                command,
                ResultCode::OutcomeUncertain,
                format!("launch outcome uncertain: {error:#}"),
                Some(pane),
            )),
        }
    }

    async fn reconcile_launch(&self, command: &Command, mut record: ExecutionRecord) -> Result<CommandResult> {
        match record.phase {
            Phase::Intent => {
                record.phase = Phase::Terminal;
                record.reclaimed = true;
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::Rejected,
                    "spawn interrupted before launch",
                    None,
                ))
            }
            Phase::Launching => {
                let pane = self.observe(&record).await;
                match pane_life(&pane) {
                    PaneLife::Alive | PaneLife::Exited | PaneLife::BriefNotDelivered => {
                        record.phase = Phase::Alive;
                        record.pane_id = (!pane.pane_id.is_empty()).then(|| pane.pane_id.clone());
                        record.pid = (pane.pid > 0).then_some(pane.pid);
                        self.persist(&record)?;
                        Ok(outcome(command, ResultCode::Ok, String::new(), Some(pane)))
                    }
                    PaneLife::Missing => {
                        self.record_terminal(&mut record, &pane)?;
                        Ok(outcome(
                            command,
                            ResultCode::OutcomeUncertain,
                            "launch interrupted; session absent",
                            Some(pane),
                        ))
                    }
                    _ => Ok(outcome(
                        command,
                        ResultCode::OutcomeUncertain,
                        "launch interrupted; pane indeterminate",
                        Some(pane),
                    )),
                }
            }
            Phase::Alive | Phase::Delivering | Phase::Stopping | Phase::Terminal => {
                let pane = self.observe(&record).await;
                Ok(outcome(command, ResultCode::Ok, String::new(), Some(pane)))
            }
        }
    }

    async fn adopt(&self, command: &Command, run: &Run) -> Result<CommandResult> {
        let Some(mut record) = self.loaded(run)? else {
            return Ok(outcome(command, ResultCode::Absent, "no execution record", None));
        };
        let pane = self.observe(&record).await;
        // A worker found finished after a restart is settled here, the same
        // way the exit sweep would have.
        self.settle(&mut record, &pane).await?;
        Ok(outcome(
            command,
            ResultCode::Ok,
            String::new(),
            Some(self.observe(&record).await),
        ))
    }

    async fn deliver(
        &self,
        command: &Command,
        run: &Run,
        deliver: &Deliver,
        interrupted: bool,
    ) -> Result<CommandResult> {
        let Some(mut record) = self.loaded(run)? else {
            return Ok(outcome(command, ResultCode::Absent, "no execution record", None));
        };
        // Progress is keyed by command id and written before any key is sent,
        // so recovery never guesses from the phase alone.
        if let Some(progress) = record.delivery.clone().filter(|p| p.command_id == command.command_id) {
            if record.phase == Phase::Delivering {
                record.phase = Phase::Alive;
                self.persist(&record)?;
            }
            let pane = self.observe(&record).await;
            return Ok(if progress.completed {
                outcome(command, ResultCode::Ok, String::new(), Some(pane))
            } else {
                let attempt = if interrupted { "interrupted" } else { "repeated" };
                outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    format!("deliver {attempt} after attempting"),
                    Some(pane),
                )
            });
        }
        if record.phase != Phase::Alive {
            return Ok(outcome(
                command,
                ResultCode::Rejected,
                format!("execution is {:?}, not accepting input", record.phase),
                Some(self.observe(&record).await),
            ));
        }
        let gesture = match self.gesture(&record, deliver) {
            Ok(gesture) => gesture,
            Err(error) => return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None)),
        };
        // Prove the live session is this execution's before any side effect.
        let pane = self.observe(&record).await;
        match pane_life(&pane) {
            PaneLife::Alive => {}
            PaneLife::Indeterminate => {
                return Ok(outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    "pane state indeterminate; input not sent",
                    Some(pane),
                ));
            }
            life => {
                self.settle(&mut record, &pane).await?;
                let code = if life == PaneLife::Missing {
                    ResultCode::Absent
                } else {
                    ResultCode::Rejected
                };
                return Ok(outcome(
                    command,
                    code,
                    "worker is not running; input not sent",
                    Some(pane),
                ));
            }
        }
        record.phase = Phase::Delivering;
        record.delivery = Some(DeliveryProgress {
            command_id: command.command_id.clone(),
            completed: false,
        });
        self.persist(&record)?;
        let target = exact_target(&record.session);
        let sent = match &gesture {
            Gesture::Text(text) => self.tmux.send_keys(&target, text).await,
            Gesture::Keys { key, presses, interval } => {
                let mut sent = Ok(());
                for press in 0..*presses {
                    if press > 0 {
                        tokio::time::sleep(*interval).await;
                    }
                    if let Err(error) = self.tmux.send_key(&target, key).await {
                        sent = Err(error);
                        break;
                    }
                }
                sent
            }
        };
        record.phase = Phase::Alive;
        match sent {
            Ok(()) => {
                if let Some(progress) = record.delivery.as_mut() {
                    progress.completed = true;
                }
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::Ok,
                    String::new(),
                    Some(self.observe(&record).await),
                ))
            }
            Err(error) => {
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    format!("deliver attempted: {error:#}"),
                    Some(self.observe(&record).await),
                ))
            }
        }
    }

    fn gesture(&self, record: &ExecutionRecord, deliver: &Deliver) -> Result<Gesture> {
        if deliver.kind == DeliveryKind::Interrupt as i32 {
            let driver = DriverRegistry::default().require(&record.driver_id)?;
            let plan = driver.interrupt_plan().context("driver does not support interrupt")?;
            return Ok(Gesture::Keys {
                key: plan.gesture.key,
                presses: plan.gesture.presses.max(1),
                interval: plan.gesture.press_interval,
            });
        }
        let text = std::str::from_utf8(&deliver.input_utf8).context("deliver input must be UTF-8")?;
        Ok(Gesture::Text(text.to_owned()))
    }

    async fn stop(&self, command: &Command, run: &Run, stop: &Stop, interrupted: bool) -> Result<CommandResult> {
        let Some(mut record) = self.loaded(run)? else {
            return Ok(outcome(command, ResultCode::Absent, "no execution record", None));
        };
        if interrupted && record.phase == Phase::Stopping {
            let pane = self.observe(&record).await;
            self.settle(&mut record, &pane).await?;
            return Ok(if record.phase == Phase::Terminal {
                outcome(
                    command,
                    ResultCode::Ok,
                    String::new(),
                    Some(self.observe(&record).await),
                )
            } else {
                outcome(command, ResultCode::OutcomeUncertain, "stop interrupted", Some(pane))
            });
        }
        record.phase = Phase::Stopping;
        self.persist(&record)?;
        if stop.grace_ms > 0 {
            let budget = Duration::from_millis(u64::from(stop.grace_ms.min(30_000)));
            let deadline = Instant::now() + budget;
            while Instant::now() < deadline {
                if pane_life(&self.observe(&record).await) != PaneLife::Alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        match self
            .tmux
            .kill_session_verified(&record.session, &record.spawn_token)
            .await
        {
            Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => {
                // Record Missing from the kill outcome. Observe after reclaim
                // answers from this evidence rather than from a gone session.
                let pane = PaneState {
                    session: record.session.clone(),
                    pane_id: record.pane_id.clone().unwrap_or_default(),
                    pid: record.pid.unwrap_or(0),
                    boot_id: record.boot_id.clone(),
                    life: PaneLife::Missing.into(),
                    ..Default::default()
                };
                self.record_terminal(&mut record, &pane)?;
                Ok(outcome(
                    command,
                    ResultCode::Ok,
                    String::new(),
                    Some(self.observe(&record).await),
                ))
            }
            Err(KillSessionError::TokenMismatch { actual, .. }) => Ok(outcome(
                command,
                ResultCode::Rejected,
                format!("refusing stop: live token {actual} is not this execution"),
                Some(self.observe(&record).await),
            )),
            Err(error) => Ok(outcome(
                command,
                ResultCode::OutcomeUncertain,
                error.to_string(),
                Some(self.observe(&record).await),
            )),
        }
    }

    async fn workspace_path(&self, workspace_id: &str, lease_id: &str) -> Result<PathBuf> {
        let rows = cube::registered_workspaces(&self.cube).await?;
        let row = rows
            .into_iter()
            .find(|row| row.status.workspace_id == workspace_id)
            .with_context(|| format!("workspace {workspace_id} is not registered"))?;
        ensure!(
            row.status.lease_id.as_deref() == Some(lease_id),
            "workspace {workspace_id} is not held by this lease"
        );
        Ok(row.status.workspace_path)
    }

    async fn wait_for_brief(&self, record: &ExecutionRecord) -> PaneState {
        let deadline = Instant::now() + self.brief_budget;
        loop {
            let pane = self.observe(record).await;
            let delivered = record.home.as_deref().is_some_and(delivered_marker_present);
            match pane_life(&pane) {
                PaneLife::BriefNotDelivered | PaneLife::Exited | PaneLife::Missing => return pane,
                PaneLife::Alive if delivered => return pane,
                // A failed query may be transient; keep looking until the budget ends.
                _ => {}
            }
            if Instant::now() >= deadline {
                return pane;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Classify the pane. `Missing` only when tmux positively reports the
    /// session absent or owned by another token; any failed or unparseable
    /// query is `Indeterminate`, never a guess.
    async fn observe(&self, record: &ExecutionRecord) -> PaneState {
        let mut pane = PaneState {
            session: record.session.clone(),
            pane_id: record.pane_id.clone().unwrap_or_default(),
            pid: record.pid.unwrap_or(0),
            boot_id: record.boot_id.clone(),
            ..Default::default()
        };
        // A Terminal record already classified the outcome. Re-deriving from
        // the pane after reclaim would lose the delivered marker and treat a
        // later exit 64 as BRIEF_NOT_DELIVERED.
        if record.phase == Phase::Terminal
            && let Some(evidence) = &record.terminal
        {
            pane.life = evidence.life;
            pane.exit_code = evidence.exit_code;
            return pane;
        }
        match self.tmux.show_environment(&record.session, TMUX_SPAWN_TOKEN_ENV).await {
            Ok(Some(token)) if token == record.spawn_token => {}
            Ok(_) => {
                pane.life = PaneLife::Missing.into();
                return pane;
            }
            Err(_) => {
                pane.life = PaneLife::Indeterminate.into();
                return pane;
            }
        }
        match self.tmux.display_message(&record.session, DisplayField::PaneId).await {
            Ok(id) if !id.is_empty() => pane.pane_id = id,
            _ => {
                pane.life = PaneLife::Indeterminate.into();
                return pane;
            }
        }
        if let Ok(pid) = self.tmux.display_message(&record.session, DisplayField::PanePid).await {
            pane.pid = pid.parse().unwrap_or(pane.pid);
        }
        let dead = self.tmux.display_message(&record.session, DisplayField::PaneDead).await;
        match dead.as_deref() {
            Ok("0") => {
                pane.life = PaneLife::Alive.into();
                return pane;
            }
            Ok("1") => {}
            _ => {
                pane.life = PaneLife::Indeterminate.into();
                return pane;
            }
        }
        let status = match self
            .tmux
            .display_message(&record.session, DisplayField::PaneDeadStatus)
            .await
        {
            // tmux leaves the status empty when a signal ended the pane.
            Ok(text) if text.is_empty() => None,
            Ok(text) => match text.parse::<i32>() {
                Ok(status) => Some(status),
                Err(_) => {
                    pane.life = PaneLife::Indeterminate.into();
                    return pane;
                }
            },
            Err(_) => {
                pane.life = PaneLife::Indeterminate.into();
                return pane;
            }
        };
        pane.exit_code = status;
        pane.life = if is_brief_not_delivered(status, record.home.as_deref(), None) {
            PaneLife::BriefNotDelivered.into()
        } else {
            PaneLife::Exited.into()
        };
        pane
    }

    /// Persist the terminal classification before removing any files, then
    /// reclaim. A crash between the two persists leaves a Terminal record
    /// with `reclaimed == false` that the sweep retries.
    fn record_terminal(&self, record: &mut ExecutionRecord, pane: &PaneState) -> Result<()> {
        record.terminal = Some(TerminalEvidence {
            life: pane.life,
            exit_code: pane.exit_code,
        });
        record.phase = Phase::Terminal;
        self.persist(record)?;
        self.reclaim(record);
        self.persist(record)
    }

    /// Record terminal evidence, reclaim the run's files and retire the record
    /// when the pane is positively finished. Idempotent, and a no-op for
    /// `Alive`/`Indeterminate` panes and for records mid-command. `Stopping`
    /// records are settled once the session is Missing, or once an
    /// Exited/BriefNotDelivered pane's session has been killed.
    async fn settle(&self, record: &mut ExecutionRecord, pane: &PaneState) -> Result<()> {
        match record.phase {
            Phase::Alive | Phase::Delivering => {
                if !matches!(
                    pane_life(pane),
                    PaneLife::Exited | PaneLife::BriefNotDelivered | PaneLife::Missing
                ) {
                    return Ok(());
                }
                self.record_terminal(record, pane)
            }
            Phase::Stopping => match pane_life(pane) {
                PaneLife::Missing => self.record_terminal(record, pane),
                PaneLife::Exited | PaneLife::BriefNotDelivered => {
                    match self
                        .tmux
                        .kill_session_verified(&record.session, &record.spawn_token)
                        .await
                    {
                        Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => {
                            self.record_terminal(record, pane)
                        }
                        _ => Ok(()),
                    }
                }
                _ => Ok(()),
            },
            Phase::Terminal if !record.reclaimed => {
                self.reclaim(record);
                self.persist(record)
            }
            _ => Ok(()),
        }
    }

    /// Remove the run's input home and settings file. The outcome is recorded
    /// on the record: `reclaimed` once clean, otherwise the error to retry.
    fn reclaim(&self, record: &mut ExecutionRecord) {
        if record.reclaimed {
            return;
        }
        let mut errors = Vec::new();
        if let Some(path) = &record.settings_file
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("settings file: {error}"));
        }
        let home = if let Some(state) = &record.runtime_state {
            input::reclaim(state)
        } else if let Some(home) = &record.home {
            input::reclaim_home(home)
        } else {
            Ok(())
        };
        if let Err(error) = home {
            errors.push(format!("{error:#}"));
        }
        record.reclaimed = errors.is_empty();
        record.reclaim_error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    /// Find workers that finished since the last look, and retry reclaims that
    /// failed. Runs on the command loop between commands, so it never races a
    /// command on the same record, and works from persisted records so it also
    /// covers workers that exited while the daemon was down.
    pub async fn sweep(&self) -> Vec<(String, String)> {
        let Ok(records) = assigned_records(&self.store) else {
            return vec![("-".into(), "execution store unavailable".into())];
        };
        let mut failures = Vec::new();
        for mut record in records {
            let pane = match record.phase {
                Phase::Alive | Phase::Delivering | Phase::Stopping => self.observe(&record).await,
                Phase::Terminal if !record.reclaimed => PaneState::default(),
                _ => continue,
            };
            if let Err(error) = self.settle(&mut record, &pane).await {
                failures.push((record.run_id.clone(), format!("{error:#}")));
            } else if let Some(error) = record.reclaim_error.clone().filter(|_| !record.reclaimed) {
                failures.push((record.run_id.clone(), error));
            }
        }
        failures
    }
}

enum Gesture {
    Text(String),
    Keys {
        key: &'static str,
        presses: u8,
        interval: Duration,
    },
}

/// Heartbeat every assigned lease. Cube failures are returned, never fatal.
pub async fn heartbeat_leases<T: CubeJsonTransport>(cube: &T, store: &impl ExecutionStore) -> Vec<(String, String)> {
    let Ok(records) = assigned_records(store) else {
        return vec![("-".into(), "execution store unavailable".into())];
    };
    let mut failures = Vec::new();
    for record in records {
        if !record.phase.assigned() || record.lease_id.is_empty() {
            continue;
        }
        if let Err(error) = cube::heartbeat_lease(cube, &record.lease_id, Some(HEARTBEAT_TTL_SECONDS)).await {
            failures.push((record.lease_id, format!("{error:#}")));
        }
    }
    failures
}

fn pane_command(plan: &SpawnPlan) -> String {
    let env_prefix: String = plan.env.iter().map(render_env_directive).collect();
    format!(
        "/bin/sh -lc {}",
        quote_for_shell(&format!("{env_prefix}{}", plan.command))
    )
}

fn pane_life(pane: &PaneState) -> PaneLife {
    PaneLife::try_from(pane.life).unwrap_or_default()
}

fn hex_token(spawn_token: &str) -> String {
    spawn_token
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn outcome(command: &Command, code: ResultCode, reason: impl Into<String>, pane: Option<PaneState>) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: code.into(),
        reason: reason.into(),
        value: pane.map(command_result::Value::Pane),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cube::CubeJsonTransport;
    use async_trait::async_trait;
    use boss_tmux::{CommandOutput, CommandRunner};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    type StoredExecutions = HashMap<(String, String), Vec<u8>>;

    #[derive(Clone, Default)]
    struct MemoryStore(Arc<Mutex<StoredExecutions>>);

    impl ExecutionStore for MemoryStore {
        fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(run_id.to_owned(), spawn_token.to_owned()))
                .cloned())
        }
        fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert((run_id.to_owned(), spawn_token.to_owned()), bytes.to_vec());
            Ok(())
        }
        fn assigned(&self) -> Result<Vec<Vec<u8>>> {
            Ok(self.0.lock().unwrap().values().cloned().collect())
        }
    }

    /// Fails the Nth `save` so a crash between classification and the
    /// post-reclaim persist can be exercised.
    struct FailingStore {
        inner: MemoryStore,
        saves: Mutex<usize>,
        fail_at: usize,
    }

    impl ExecutionStore for FailingStore {
        fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>> {
            self.inner.load(run_id, spawn_token)
        }
        fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()> {
            let n = {
                let mut saves = self.saves.lock().unwrap();
                *saves += 1;
                *saves
            };
            if n == self.fail_at {
                anyhow::bail!("injected persist failure");
            }
            self.inner.save(run_id, spawn_token, bytes)
        }
        fn assigned(&self) -> Result<Vec<Vec<u8>>> {
            self.inner.assigned()
        }
    }

    struct ListingCube {
        path: PathBuf,
        heartbeats: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeJsonTransport for ListingCube {
        async fn run_cube_json(&self, args: &[&str]) -> Result<Value> {
            match args {
                ["--json", "workspace", "list"] => Ok(json!({
                    "workspaces": [{
                        "repo": "repo",
                        "workspace_id": "worker",
                        "workspace_path": self.path,
                        "state": "leased",
                        "lease_id": "lease",
                        "task": "owner"
                    }]
                })),
                ["--json", "workspace", "heartbeat", "--lease", lease, ..] => {
                    self.heartbeats.lock().unwrap().push((*lease).into());
                    Ok(json!({}))
                }
                _ => Ok(json!({})),
            }
        }
    }

    #[derive(Default)]
    struct FakeTmux {
        sessions: Mutex<HashMap<String, String>>,
        pane_dead: Mutex<HashMap<String, (String, Option<i32>)>>,
        mismatch: Mutex<HashMap<String, String>>,
        killed: Mutex<Vec<String>>,
        show_env_error: Mutex<bool>,
        dead_error: Mutex<bool>,
        kill_error: Mutex<bool>,
        sent: Mutex<Vec<Vec<String>>>,
        envs: Mutex<HashMap<String, Vec<String>>>,
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn err(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    fn subcommand(args: &[String]) -> &[String] {
        let skip = args
            .iter()
            .position(|arg| arg == "-S" || arg == "-L")
            .map(|index| index + 2)
            .unwrap_or(0);
        &args[skip.min(args.len())..]
    }

    #[async_trait]
    impl CommandRunner for FakeTmux {
        async fn run(&self, _program: &Path, args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
            let rest = subcommand(&args);
            let Some(verb) = rest.first().map(String::as_str) else {
                return Ok(ok(""));
            };
            match verb {
                "new-session" => {
                    let name = rest
                        .iter()
                        .position(|arg| arg == "-s")
                        .and_then(|index| rest.get(index + 1))
                        .cloned()
                        .unwrap_or_default();
                    let token = rest
                        .iter()
                        .find_map(|arg| arg.strip_prefix("BOSS_SPAWN_TOKEN="))
                        .unwrap_or("")
                        .to_owned();
                    let envs: Vec<String> = rest
                        .windows(2)
                        .filter(|pair| pair[0] == "-e")
                        .map(|pair| pair[1].clone())
                        .collect();
                    self.envs.lock().unwrap().insert(name.clone(), envs);
                    self.sessions.lock().unwrap().insert(name, token);
                    Ok(ok(""))
                }
                "list-sessions" => {
                    let listed = self
                        .sessions
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(name, token)| format!("{name}\t{token}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(ok(&listed))
                }
                "send-keys" | "load-buffer" | "paste-buffer" => {
                    self.sent.lock().unwrap().push(rest.to_vec());
                    Ok(ok(""))
                }
                "show-environment" => {
                    if *self.show_env_error.lock().unwrap() {
                        return Ok(err("server exploded"));
                    }
                    let session = rest.iter().position(|arg| arg == "-t").and_then(|i| rest.get(i + 1));
                    let Some(session) = session else {
                        return Ok(err("no session"));
                    };
                    if let Some(actual) = self.mismatch.lock().unwrap().get(session) {
                        return Ok(ok(&format!("BOSS_SPAWN_TOKEN={actual}")));
                    }
                    match self.sessions.lock().unwrap().get(session) {
                        Some(token) => Ok(ok(&format!("BOSS_SPAWN_TOKEN={token}"))),
                        None => Ok(err("can't find session")),
                    }
                }
                "display-message" => {
                    let session = rest.iter().position(|arg| arg == "-t").and_then(|i| rest.get(i + 1));
                    let format = rest.last().map(String::as_str).unwrap_or("");
                    let Some(session) = session else {
                        return Ok(err("no session"));
                    };
                    if !self.sessions.lock().unwrap().contains_key(session) {
                        return Ok(err("can't find session"));
                    }
                    if format == "#{pane_dead}" && *self.dead_error.lock().unwrap() {
                        return Ok(err("server exploded"));
                    }
                    let stdout = match format {
                        "#{pane_id}" => "%0".to_owned(),
                        "#{pane_pid}" => "4242".to_owned(),
                        "#{pane_dead}" => self
                            .pane_dead
                            .lock()
                            .unwrap()
                            .get(session)
                            .map(|(dead, _)| dead.clone())
                            .unwrap_or_else(|| "0".into()),
                        "#{pane_dead_status}" => self
                            .pane_dead
                            .lock()
                            .unwrap()
                            .get(session)
                            .and_then(|(_, status)| *status)
                            .map(|status| status.to_string())
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    Ok(ok(&stdout))
                }
                "detach-client" | "if-shell" => {
                    if *self.kill_error.lock().unwrap() {
                        return Ok(err("kill failed"));
                    }
                    if let Some(target) = rest.iter().find(|arg| arg.starts_with('=')) {
                        let name = target.trim_start_matches('=').trim_end_matches(':');
                        self.killed.lock().unwrap().push(name.to_owned());
                        self.sessions.lock().unwrap().remove(name);
                    }
                    Ok(ok(""))
                }
                "kill-session" => Ok(ok("")),
                _ => Ok(ok("")),
            }
        }
    }

    fn run() -> Run {
        Run {
            execution_id: "exec_test".into(),
            run_id: "run-test".into(),
            spawn_token: "token".into(),
            assignment_generation: 1,
        }
    }

    fn spawn_command() -> Command {
        Command {
            sequence: 1,
            command_id: "spawn-1".into(),
            run: Some(run()),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Spawn(Spawn {
                lease_id: "lease".into(),
                workspace_id: "worker".into(),
                driver_id: "claude".into(),
                model: "sonnet".into(),
                prompt_utf8: b"do the thing".to_vec(),
                ..Default::default()
            })),
        }
    }

    /// A private-home root under the driver's default homes root, so reclaim's
    /// containment check accepts its children without touching the process
    /// environment (which other tests in this binary share).
    fn shared_homes() -> &'static Path {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            homes::pin(&boss_engine_driver::codex::codex_homes_root().join("worker-unit-tests")).unwrap()
        })
    }

    async fn supervisor(root: &Path, fake: Arc<FakeTmux>) -> Supervisor<ListingCube, MemoryStore> {
        supervisor_with(root, fake, MemoryStore::default()).await
    }

    async fn supervisor_with<S: ExecutionStore>(
        root: &Path,
        fake: Arc<FakeTmux>,
        store: S,
    ) -> Supervisor<ListingCube, S> {
        let homes = homes::pin(shared_homes()).unwrap();
        let workspace = root.join("worker");
        std::fs::create_dir_all(&workspace).unwrap();
        Supervisor {
            cube: ListingCube {
                path: workspace,
                heartbeats: Mutex::new(Vec::new()),
            },
            tmux: Tmux::with_runner_and_socket("/usr/bin/tmux", fake, root.join("tmux.sock")).unwrap(),
            store,
            host_id: "host".into(),
            homes_root: homes,
            boot_id: "boot".into(),
            brief_budget: Duration::ZERO,
        }
    }

    fn seed(store: &MemoryStore, phase: Phase) {
        let record = ExecutionRecord {
            execution_id: "exec_test".into(),
            run_id: "run-test".into(),
            spawn_token: "token".into(),
            assignment_generation: 1,
            lease_id: "lease".into(),
            workspace_id: "worker".into(),
            session: session_name("token"),
            phase,
            boot_id: "boot".into(),
            pane_id: Some("%0".into()),
            pid: Some(4242),
            runtime_state: None,
            home: None,
            driver_id: "claude".into(),
            settings_file: None,
            delivery: None,
            terminal: None,
            reclaimed: false,
            reclaim_error: None,
        };
        store
            .save(
                &record.run_id,
                &record.spawn_token,
                &serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn spawn_rejects_invalid_utf8_before_tmux() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let supervisor = supervisor(root.path(), fake.clone()).await;
        let mut command = spawn_command();
        if let Some(command::Body::Spawn(spawn)) = &mut command.body {
            spawn.prompt_utf8 = vec![0xff, 0xfe];
        }
        let result = supervisor.execute(&command, false, 1).await;
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        assert!(result.reason.contains("UTF-8"), "{}", result.reason);
        assert!(fake.sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn spawn_rejects_an_unusable_private_home() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let mut supervisor = supervisor(root.path(), fake).await;
        supervisor.homes_root = PathBuf::new();
        let result = supervisor.execute(&spawn_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        assert!(result.reason.contains("unset"), "{}", result.reason);
    }

    #[tokio::test]
    async fn duplicate_spawn_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        fake.sessions
            .lock()
            .unwrap()
            .insert(session_name("token"), "token".into());
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor.execute(&spawn_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        assert!(result.reason.contains("duplicate"), "{}", result.reason);
    }

    #[tokio::test]
    async fn interrupted_launch_without_a_session_is_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Launching);
        let result = supervisor.execute(&spawn_command(), true, 1).await;
        assert_eq!(result.code, ResultCode::OutcomeUncertain as i32, "{}", result.reason);
        assert!(result.reason.contains("absent"), "{}", result.reason);
        let stored = decode(&supervisor.store.load("run-test", "token").unwrap().unwrap()).unwrap();
        assert_eq!(stored.phase, Phase::Terminal);
    }

    #[tokio::test]
    async fn stop_refuses_a_sibling_token() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        fake.sessions
            .lock()
            .unwrap()
            .insert(session_name("token"), "token".into());
        fake.mismatch
            .lock()
            .unwrap()
            .insert(session_name("token"), "other-token".into());
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let command = Command {
            sequence: 1,
            command_id: "stop-1".into(),
            run: Some(run()),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Stop(Stop { grace_ms: 0 })),
        };
        let result = supervisor.execute(&command, false, 1).await;
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        assert!(result.reason.contains("live token"), "{}", result.reason);
        assert!(fake.killed.lock().unwrap().is_empty());
        assert!(fake.sessions.lock().unwrap().contains_key(&session_name("token")));
    }

    #[tokio::test]
    async fn heartbeat_covers_assigned_leases() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let failures = heartbeat_leases(&supervisor.cube, &supervisor.store).await;
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(*supervisor.cube.heartbeats.lock().unwrap(), vec!["lease".to_owned()]);
    }

    fn stored(store: &MemoryStore) -> ExecutionRecord {
        decode(&store.load("run-test", "token").unwrap().unwrap()).unwrap()
    }

    fn update(store: &MemoryStore, change: impl FnOnce(&mut ExecutionRecord)) {
        let mut record = stored(store);
        change(&mut record);
        store
            .save("run-test", "token", &serde_json::to_vec(&record).unwrap())
            .unwrap();
    }

    /// A private run home under the shared root with one file in it.
    fn run_home(name: &str) -> PathBuf {
        let home = shared_homes().join(name);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("initial-prompt.txt"), "brief").unwrap();
        home
    }

    fn run_home_delivered(name: &str) -> PathBuf {
        let home = run_home(name);
        std::fs::write(home.join(initial_prompt::DELIVERED_MARKER), "").unwrap();
        home
    }

    fn live(fake: &FakeTmux) {
        fake.sessions
            .lock()
            .unwrap()
            .insert(session_name("token"), "token".into());
    }

    fn deliver_command(id: &str, kind: DeliveryKind, input: &str) -> Command {
        Command {
            sequence: 1,
            command_id: id.into(),
            run: Some(run()),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Deliver(Deliver {
                kind: kind as i32,
                input_utf8: input.as_bytes().to_vec(),
            })),
        }
    }

    fn stop_command() -> Command {
        Command {
            sequence: 2,
            command_id: "stop-1".into(),
            run: Some(run()),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Stop(Stop { grace_ms: 0 })),
        }
    }

    fn adopt_command() -> Command {
        Command {
            sequence: 3,
            command_id: "adopt-1".into(),
            run: Some(run()),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Adopt(Default::default())),
        }
    }

    fn pane_of(result: &CommandResult) -> PaneState {
        match &result.value {
            Some(command_result::Value::Pane(pane)) => pane.clone(),
            other => panic!("expected a pane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unclassified_show_environment_error_is_indeterminate_not_missing() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        *fake.show_env_error.lock().unwrap() = true;
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor.execute(&adopt_command(), false, 1).await;
        assert_eq!(pane_life(&pane_of(&result)), PaneLife::Indeterminate);
        assert_eq!(
            stored(&supervisor.store).phase,
            Phase::Alive,
            "never retire on a failed query"
        );
    }

    #[tokio::test]
    async fn failed_pane_dead_query_is_indeterminate_not_alive() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        *fake.dead_error.lock().unwrap() = true;
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor.execute(&adopt_command(), false, 1).await;
        assert_eq!(pane_life(&pane_of(&result)), PaneLife::Indeterminate);
    }

    #[tokio::test]
    async fn indeterminate_pane_keeps_the_home_and_the_lease_heartbeat() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        *fake.show_env_error.lock().unwrap() = true;
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let home = run_home("indeterminate-home");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        assert!(supervisor.sweep().await.is_empty());
        assert!(home.exists());
        assert_eq!(stored(&supervisor.store).phase, Phase::Alive);
        let failures = heartbeat_leases(&supervisor.cube, &supervisor.store).await;
        assert!(failures.is_empty());
        assert_eq!(supervisor.cube.heartbeats.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn exit_after_spawn_returned_is_reclaimed_by_the_sweep() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let home = run_home("sweep-home");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        assert!(supervisor.sweep().await.is_empty());
        assert!(home.exists(), "a live worker keeps its home");
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(0)));
        assert!(supervisor.sweep().await.is_empty());
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(record.reclaimed);
        assert_eq!(
            record.terminal,
            Some(TerminalEvidence {
                life: PaneLife::Exited.into(),
                exit_code: Some(0)
            })
        );
        assert!(!home.exists());
        // Terminal records stop receiving lease heartbeats.
        heartbeat_leases(&supervisor.cube, &supervisor.store).await;
        assert!(supervisor.cube.heartbeats.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn exit_found_after_a_daemon_restart_is_settled_by_adopt() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let first = supervisor(root.path(), fake.clone()).await;
        seed(&first.store, Phase::Alive);
        let home = run_home("restart-home");
        update(&first.store, |r| r.home = Some(home.clone()));
        // The worker exits while no daemon is running; a new daemon starts on
        // the same durable store and tmux server.
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(3)));
        let second = supervisor(root.path(), fake).await;
        *second.store.0.lock().unwrap() = first.store.0.lock().unwrap().clone();
        let result = second.execute(&adopt_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32);
        assert_eq!(pane_life(&pane_of(&result)), PaneLife::Exited);
        assert_eq!(stored(&second.store).phase, Phase::Terminal);
        assert!(!home.exists());
    }

    #[tokio::test]
    async fn failed_kill_then_session_gone_is_settled_by_sweep() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        *fake.kill_error.lock().unwrap() = true;
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let home = run_home("stopping-sweep-home");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        let result = supervisor.execute(&stop_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::OutcomeUncertain as i32, "{}", result.reason);
        assert_eq!(stored(&supervisor.store).phase, Phase::Stopping);
        assert!(home.exists());
        heartbeat_leases(&supervisor.cube, &supervisor.store).await;
        assert_eq!(supervisor.cube.heartbeats.lock().unwrap().len(), 1);
        fake.sessions.lock().unwrap().remove(&session_name("token"));
        *fake.kill_error.lock().unwrap() = false;
        assert!(supervisor.sweep().await.is_empty());
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(record.reclaimed);
        assert!(!home.exists());
        supervisor.cube.heartbeats.lock().unwrap().clear();
        heartbeat_leases(&supervisor.cube, &supervisor.store).await;
        assert!(supervisor.cube.heartbeats.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupted_stop_on_an_exited_pane_retries_kill_and_settles() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(0)));
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Stopping);
        let home = run_home("interrupted-stop-home");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        let result = supervisor.execute(&stop_command(), true, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(record.reclaimed);
        assert_eq!(
            record.terminal,
            Some(TerminalEvidence {
                life: PaneLife::Exited.into(),
                exit_code: Some(0)
            })
        );
        assert!(!home.exists());
        assert!(!fake.killed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopping_run_is_settled_by_adopt_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let first = supervisor(root.path(), fake.clone()).await;
        seed(&first.store, Phase::Stopping);
        let home = run_home("stopping-adopt-home");
        update(&first.store, |r| r.home = Some(home.clone()));
        fake.sessions.lock().unwrap().remove(&session_name("token"));
        let second = supervisor(root.path(), fake).await;
        *second.store.0.lock().unwrap() = first.store.0.lock().unwrap().clone();
        let result = second.execute(&adopt_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32);
        assert_eq!(pane_life(&pane_of(&result)), PaneLife::Missing);
        assert_eq!(stored(&second.store).phase, Phase::Terminal);
        assert!(stored(&second.store).reclaimed);
        assert!(!home.exists());
    }

    #[tokio::test]
    async fn delivered_exit_64_stays_exited_across_sweep_and_repeated_adopt() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(64)));
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let home = run_home_delivered("delivered-exit-64");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        assert!(supervisor.sweep().await.is_empty());
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(record.reclaimed);
        assert_eq!(
            record.terminal,
            Some(TerminalEvidence {
                life: PaneLife::Exited.into(),
                exit_code: Some(64)
            })
        );
        assert!(!home.exists());
        for _ in 0..2 {
            let result = supervisor.execute(&adopt_command(), false, 1).await;
            assert_eq!(result.code, ResultCode::Ok as i32);
            assert_eq!(
                pane_life(&pane_of(&result)),
                PaneLife::Exited,
                "re-observe must not fall back to brief-not-delivered after reclaim"
            );
        }
    }

    #[tokio::test]
    async fn persist_failure_after_reclaim_keeps_the_durable_classification() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(64)));
        let inner = MemoryStore::default();
        seed(&inner, Phase::Alive);
        let home = run_home_delivered("persist-fail-home");
        update(&inner, |r| r.home = Some(home.clone()));
        // record_terminal persists classification, reclaims, then persists
        // completion. Fail the second save.
        let supervisor = supervisor_with(
            root.path(),
            fake,
            FailingStore {
                inner: inner.clone(),
                saves: Mutex::new(0),
                fail_at: 2,
            },
        )
        .await;
        let failures = supervisor.sweep().await;
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(!home.exists());
        let record = stored(&inner);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(!record.reclaimed);
        assert_eq!(
            record.terminal,
            Some(TerminalEvidence {
                life: PaneLife::Exited.into(),
                exit_code: Some(64)
            })
        );
        let recovered = supervisor_with(root.path(), Arc::new(FakeTmux::default()), inner).await;
        let result = recovered.execute(&adopt_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32);
        assert_eq!(pane_life(&pane_of(&result)), PaneLife::Exited);
        assert!(stored(&recovered.store).reclaimed);
    }

    #[tokio::test]
    async fn distinct_tokens_that_sanitized_alike_keep_distinct_settings_files() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let supervisor = supervisor(root.path(), fake).await;
        let plus = supervisor.settings_path("a+b");
        let under = supervisor.settings_path("a_b");
        assert_ne!(plus, under, "hex encoding must not collapse + and _");
        std::fs::write(&plus, "{}").unwrap();
        std::fs::write(&under, "{}").unwrap();
        seed(&supervisor.store, Phase::Alive);
        let mut record = stored(&supervisor.store);
        record.settings_file = Some(plus.clone());
        supervisor.reclaim(&mut record);
        assert!(!plus.exists());
        assert!(under.exists(), "reclaiming one run must leave the other's settings");
    }

    #[tokio::test]
    async fn failed_reclaim_is_recorded_and_retried() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        // Outside the private root: reclaim refuses, and the refusal is kept.
        let outside = root.path().join("outside-home");
        std::fs::create_dir_all(&outside).unwrap();
        update(&supervisor.store, |r| r.home = Some(outside.clone()));
        fake.pane_dead
            .lock()
            .unwrap()
            .insert(session_name("token"), ("1".into(), Some(0)));
        let failures = supervisor.sweep().await;
        assert_eq!(failures.len(), 1, "{failures:?}");
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(!record.reclaimed);
        assert!(record.reclaim_error.is_some());
        assert!(outside.exists());
        // A later sweep retries instead of forgetting.
        assert_eq!(supervisor.sweep().await.len(), 1);
    }

    #[tokio::test]
    async fn stop_reclaims_the_run_home() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake).await;
        seed(&supervisor.store, Phase::Alive);
        let home = run_home("stop-home");
        update(&supervisor.store, |r| r.home = Some(home.clone()));
        let result = supervisor.execute(&stop_command(), false, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        assert!(!home.exists());
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Terminal);
        assert!(record.reclaimed);
    }

    #[tokio::test]
    async fn deliver_after_stop_is_rejected_and_does_not_revive_the_run() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Terminal);
        let result = supervisor
            .execute(&deliver_command("d1", DeliveryKind::Nudge, "hello"), false, 1)
            .await;
        assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
        assert_eq!(stored(&supervisor.store).phase, Phase::Terminal);
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deliver_with_a_foreign_live_token_sends_nothing() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        fake.mismatch
            .lock()
            .unwrap()
            .insert(session_name("token"), "other-token".into());
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor
            .execute(&deliver_command("d1", DeliveryKind::Nudge, "hello"), false, 1)
            .await;
        assert_eq!(result.code, ResultCode::Absent as i32, "{}", result.reason);
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deliver_types_text_into_an_exact_session_target() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor
            .execute(&deliver_command("d1", DeliveryKind::Answer, "hello"), false, 1)
            .await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        let record = stored(&supervisor.store);
        assert_eq!(record.phase, Phase::Alive);
        assert_eq!(
            record.delivery,
            Some(DeliveryProgress {
                command_id: "d1".into(),
                completed: true
            })
        );
        let sent = fake.sent.lock().unwrap();
        assert!(!sent.is_empty());
        let exact = format!("={}:", session_name("token"));
        assert!(sent.iter().all(|args| args.contains(&exact)), "{sent:?}");
    }

    #[tokio::test]
    async fn interrupt_sends_the_driver_gesture_without_a_return() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        let result = supervisor
            .execute(&deliver_command("i1", DeliveryKind::Interrupt, ""), false, 1)
            .await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        let sent = fake.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].last().map(String::as_str), Some("Escape"));
        assert!(sent.iter().flatten().all(|arg| arg != "C-m"));
    }

    #[tokio::test]
    async fn interrupted_deliver_after_the_send_is_not_replayed() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Alive);
        // Crash window: keys sent and Alive saved, result not yet committed.
        update(&supervisor.store, |r| {
            r.delivery = Some(DeliveryProgress {
                command_id: "d1".into(),
                completed: true,
            })
        });
        let result = supervisor
            .execute(&deliver_command("d1", DeliveryKind::Nudge, "hello"), true, 1)
            .await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        assert!(fake.sent.lock().unwrap().is_empty(), "input was replayed");
    }

    #[tokio::test]
    async fn interrupted_deliver_that_may_have_started_is_uncertain_and_not_replayed() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        live(&fake);
        let supervisor = supervisor(root.path(), fake.clone()).await;
        seed(&supervisor.store, Phase::Delivering);
        update(&supervisor.store, |r| {
            r.delivery = Some(DeliveryProgress {
                command_id: "d1".into(),
                completed: false,
            })
        });
        let result = supervisor
            .execute(&deliver_command("d1", DeliveryKind::Nudge, "hello"), true, 1)
            .await;
        assert_eq!(result.code, ResultCode::OutcomeUncertain as i32, "{}", result.reason);
        assert!(fake.sent.lock().unwrap().is_empty());
        assert_eq!(
            stored(&supervisor.store).phase,
            Phase::Alive,
            "phase must not stay stuck"
        );
    }

    #[tokio::test]
    async fn spawn_stamps_identity_last_and_keeps_settings_out_of_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTmux::default());
        let supervisor = supervisor(root.path(), fake.clone()).await;
        let mut command = spawn_command();
        if let Some(command::Body::Spawn(spawn)) = &mut command.body {
            spawn.settings_json = br#"{"hooks":{}}"#.to_vec();
            for (name, value) in [
                ("BOSS_SPAWN_TOKEN", "forged"),
                ("BOSS_RUN_ID", "forged"),
                ("BOSS_SESSION_SCHEMA", "forged"),
                ("KEEP_ME", "yes"),
            ] {
                spawn.env.push(boss_protocol_spoke::v1::Env {
                    name: name.into(),
                    value: value.into(),
                });
            }
        }
        let workspace = root.path().join("worker");
        let result = supervisor.execute(&command, false, 1).await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
        let envs = fake.envs.lock().unwrap().get(&session_name("token")).cloned().unwrap();
        assert!(envs.contains(&"BOSS_SPAWN_TOKEN=token".to_owned()), "{envs:?}");
        assert!(envs.contains(&"BOSS_RUN_ID=exec_test".to_owned()), "{envs:?}");
        assert!(
            envs.contains(&format!("BOSS_SESSION_SCHEMA={SESSION_SCHEMA}")),
            "{envs:?}"
        );
        assert!(envs.contains(&"KEEP_ME=yes".to_owned()), "{envs:?}");
        assert!(envs.iter().filter(|e| e.contains("forged")).count() == 0, "{envs:?}");
        assert!(!workspace.join(".boss-spoke-settings.json").exists());
        let record = stored(&supervisor.store);
        let settings = record.settings_file.expect("settings recorded");
        assert!(settings.starts_with(shared_homes()));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Stop removes it with the home.
        let stopped = supervisor.execute(&stop_command(), false, 1).await;
        assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
        assert!(!settings.exists());
    }

    #[test]
    fn brief_not_delivered_is_distinct_from_a_generic_exit() {
        assert!(is_brief_not_delivered(Some(64), None, None));
        assert!(!is_brief_not_delivered(Some(1), None, None));
    }
}
