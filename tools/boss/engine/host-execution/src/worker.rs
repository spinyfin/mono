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
    DisplayField, KillSessionError, KillSessionOutcome, NewSession, PaneProbe, TMUX_SPAWN_TOKEN_ENV, Tmux,
    insert_color_environment, quote_for_shell,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::cube::{DEFAULT_HEARTBEAT_INTERVAL as HEARTBEAT_INTERVAL, LEASE_TTL_SECS as HEARTBEAT_TTL_SECONDS};
pub const BRIEF_DELIVERY_BUDGET: Duration = Duration::from_secs(initial_prompt::DELIVERY_BUDGET.as_secs() + 30);
/// How often the command loop looks for workers that exited since the last look.
pub const EXIT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_SCHEMA: &str = "1";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const RUN_ID_ENV: &str = "BOSS_RUN_ID";
const SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";
pub const EVENT_SPOOL_ENV: &str = "BOSS_EVENT_SPOOL";
pub const HOST_ENV_DENY: &[&str] = &[
    "BOSS_CODEX_HOMES_DIR",
    "BOSS_GROK_HOMES_DIR",
    "HOME",
    "TMUX",
    "TMUX_TMPDIR",
    "BOSS_CONTROL_SOCKET",
    "BOSS_SOCKET_PATH",
    "BOSS_BIN",
    "BOSS_EVENTS_SOCKET",
    EVENT_SPOOL_ENV,
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
    #[serde(default)]
    pub observation: Option<ObservationConfig>,
    #[serde(default)]
    #[builder(default)]
    pub worker_outputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationConfig {
    pub workspace: PathBuf,
    pub pending: PathBuf,
    pub file: Option<(boss_engine_driver::AgentJsonlFileIngress, Vec<PathBuf>)>,
    pub pid_start: Option<u64>,
}

pub trait ExecutionStore: Send + Sync {
    fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>>;
    fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()>;
    fn assigned(&self) -> Result<Vec<Vec<u8>>>;
    fn observation_paths(&self) -> Option<(PathBuf, PathBuf)> {
        None
    }
    fn worker_api_paths(&self) -> Option<(PathBuf, PathBuf)> {
        None
    }
    fn observations_drained(&self, _record: &ExecutionRecord) -> Result<bool> {
        Ok(true)
    }
    fn prune_observation_sources(&self, _record: &ExecutionRecord) -> Result<()> {
        Ok(())
    }
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
        let mut output_environment = BTreeMap::new();
        if let Err(error) =
            insert_output_environment(&mut output_environment, spawn, &self.homes_root, &run.spawn_token)
        {
            return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
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
            observation: None,
            worker_outputs: output_environment.clone(),
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
        record.home = runtime.as_ref().and_then(input::owned_home);
        // Provisioning reads the process environment for its root, so confirm
        // it landed under the root this supervisor preflighted and can reclaim.
        if let Some(state) = &runtime
            && let Err(reason) = self.check_owned_home(state)
        {
            return self.reject_unowned_home(command, record, reason);
        }
        if !spawn.settings_json.is_empty() || self.store.observation_paths().is_some() {
            record.settings_file = Some(self.settings_path(&record.spawn_token));
        }
        let mut provisioned = spawn.clone();
        if let Some((socket, forwarder)) = self.store.observation_paths() {
            let config = boss_engine_driver::ProgressObservationConfig {
                events_socket_path: socket,
                forwarder_binary: forwarder,
                lease_id: record.lease_id.clone(),
                run_id: record.run_id.clone(),
                workspace_path: workspace.clone(),
            };
            if let Err(error) = self.prepare_observation(&mut record, &mut provisioned, driver.as_ref(), config) {
                record.phase = Phase::Terminal;
                self.reclaim(&mut record);
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        }
        record.phase = Phase::Launching;
        self.persist(&record)?;
        if let Err(error) = self
            .launch(&record, &provisioned, driver.as_ref(), &workspace, output_environment)
            .await
        {
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
        adopt_pid(&mut record, pid);
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

    /// Retire a spawn whose provisioned home the supervisor cannot own. The
    /// record keeps its runtime state so the reclaim outcome is real: a failed
    /// removal leaves `reclaimed` false with the error, and the sweep retries.
    fn reject_unowned_home(
        &self,
        command: &Command,
        mut record: ExecutionRecord,
        reason: String,
    ) -> Result<CommandResult> {
        record.phase = Phase::Terminal;
        self.reclaim(&mut record);
        self.persist(&record)?;
        Ok(outcome(command, ResultCode::Rejected, reason, None))
    }

    /// A run's home must be one the supervisor owns: recognised from the runtime
    /// state, and a strict child of the preflighted private root.
    fn check_owned_home(&self, state: &DriverRuntimeState) -> std::result::Result<(), String> {
        match input::owned_home(state) {
            None => Err("driver home is not one the spoke can own".to_owned()),
            Some(home) if !homes::is_owned_child(&self.homes_root, &home) => Err(format!(
                "driver home {} is outside the private-home root",
                home.display()
            )),
            Some(_) => Ok(()),
        }
    }

    fn prepare_observation(
        &self,
        record: &mut ExecutionRecord,
        spawn: &mut Spawn,
        driver: &dyn boss_engine_driver::AgentDriver,
        config: boss_engine_driver::ProgressObservationConfig,
    ) -> Result<()> {
        use boss_engine_driver::{HookWiringDestination, ProgressIngress, discovery};
        use std::os::unix::fs::DirBuilderExt;
        let pending = self
            .homes_root
            .join(format!("observations-{}", hex_token(&record.spawn_token)));
        let file = match driver.progress_observation_wiring(&config) {
            ProgressIngress::AgentJsonlFile(ingress) => {
                let prepared = discovery::PreparedSource::new(ingress.clone()).map_err(anyhow::Error::msg)?;
                Some((ingress, prepared.baseline.into_iter().collect()))
            }
            ProgressIngress::HookCallback(wiring) => {
                ensure!(
                    wiring.destination == HookWiringDestination::WorkerSettingsFile,
                    "driver-owned hook installation requires provisioned worker policy"
                );
                let mut settings: serde_json::Value = if spawn.settings_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_slice(&spawn.settings_json)?
                };
                boss_engine_driver::hook_settings::replace_forwarders(
                    &mut settings,
                    wiring.hooks,
                    &config.forwarder_binary,
                )
                .map_err(anyhow::Error::msg)?;
                spawn.settings_json = serde_json::to_vec(&settings)?;
                None
            }
            ProgressIngress::StdoutJsonl => anyhow::bail!("pane driver cannot use stdout ingress"),
        };
        std::fs::DirBuilder::new().mode(0o700).create(&pending)?;
        record.observation = Some(ObservationConfig {
            workspace: config.workspace_path,
            pending,
            file,
            pid_start: None,
        });
        Ok(())
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
        output_environment: BTreeMap<String, String>,
    ) -> Result<()> {
        if let Some(rules) = &spawn.agent_rules {
            initial_prompt::write_private(
                &driver.agent_rules_destination(workspace, &record.run_id),
                rules.as_bytes(),
            )?;
        }
        if let Some(path) = &record.settings_file {
            initial_prompt::write_private(path, &spawn.settings_json)?;
        }
        let plan = driver.spawn_invocation(SpawnRequest {
            model: &spawn.model,
            effort: spawn.effort.as_deref(),
            settings_path: record.settings_file.as_deref(),
            non_opus_auto_mode: false,
            permission_mode_override: spawn.permission_mode.as_deref(),
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
        for name in OUTPUT_ENV_NAMES {
            environment.remove(*name);
        }
        environment.extend(output_environment);
        if let Some((socket, binary)) = self.store.worker_api_paths() {
            environment.insert("BOSS_SOCKET_PATH".into(), socket.display().to_string());
            environment.insert("BOSS_BIN".into(), binary.display().to_string());
        }
        insert_color_environment(&mut environment);
        environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), record.spawn_token.clone());
        environment.insert(SESSION_SCHEMA_ENV.to_owned(), SESSION_SCHEMA.to_owned());
        environment.insert(RUN_ID_ENV.to_owned(), record.execution_id.clone());
        if let Some(observation) = &record.observation {
            environment.insert(EVENT_SPOOL_ENV.into(), observation.pending.display().to_string());
        }
        environment.insert(homes::ROOT_ENV.to_owned(), self.homes_root.display().to_string());
        self.tmux
            .new_session(&NewSession {
                name: record.session.clone(),
                environment,
                working_directory: workspace.to_path_buf(),
                command: pane_command(&plan),
            })
            .await?;
        boss_tmux::session_options::apply(&self.tmux, &record.session).await?;
        self.tmux
            .set_option(&record.session, SPAWN_TOKEN_OPTION, &record.spawn_token)
            .await?;
        if let Some(config) = boss_engine_driver::tmux_session_config_for(&spawn.driver_id) {
            match boss_tmux::session_options::source_driver_session_config(&self.tmux, &record.session, config).await? {
                boss_tmux::session_options::SessionConfigOutcome::Sourced => {}
                // A cosmetic scrollback setting never fails a spawn; an old or
                // unreadable tmux simply runs without it.
                boss_tmux::session_options::SessionConfigOutcome::UnsupportedTmux(_)
                | boss_tmux::session_options::SessionConfigOutcome::VersionUnreadable(_) => {}
            }
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
                adopt_pid(&mut record, (pane.pid > 0).then_some(pane.pid));
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
                        adopt_pid(&mut record, (pane.pid > 0).then_some(pane.pid));
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
        let result = self.stop_inner(command, run, stop, interrupted).await?;
        if result.code == ResultCode::Ok as i32
            && self.tmux.runner().manages_host_processes()
            && let Some(record) = self.loaded(run)?
        {
            let workspace = self.workspace_path(&record.workspace_id, &record.lease_id).await?;
            self.tmux
                .cleanup_run_processes(&record.execution_id, &workspace)
                .await?;
            self.tmux.verify_workspace_quiescent(&workspace).await?;
        }
        Ok(result)
    }

    async fn stop_inner(&self, command: &Command, run: &Run, stop: &Stop, interrupted: bool) -> Result<CommandResult> {
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
        // A Terminal record already classified the outcome. Kill any retained
        // remain-on-exit session without demoting the phase or replacing the
        // stored evidence; otherwise a later Adopt would report Missing.
        if record.phase == Phase::Terminal {
            return Ok(
                match self
                    .tmux
                    .kill_session_verified(&record.session, &record.spawn_token)
                    .await
                {
                    Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => outcome(
                        command,
                        ResultCode::Ok,
                        String::new(),
                        Some(self.observe(&record).await),
                    ),
                    Err(KillSessionError::TokenMismatch { actual, .. }) => outcome(
                        command,
                        ResultCode::Rejected,
                        format!("refusing stop: live token {actual} is not this execution"),
                        Some(self.observe(&record).await),
                    ),
                    Err(error) => outcome(
                        command,
                        ResultCode::OutcomeUncertain,
                        error.to_string(),
                        Some(self.observe(&record).await),
                    ),
                },
            );
        }
        record.phase = Phase::Stopping;
        self.persist(&record)?;
        let mut observed = self.observe(&record).await;
        if stop.grace_ms > 0 {
            let budget = Duration::from_millis(u64::from(stop.grace_ms.min(30_000)));
            let deadline = Instant::now() + budget;
            while pane_life(&observed) == PaneLife::Alive && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                observed = self.observe(&record).await;
            }
        }
        match self
            .tmux
            .kill_session_verified(&record.session, &record.spawn_token)
            .await
        {
            Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => {
                // Keep a pre-kill Exited/BriefNotDelivered observation. Killing
                // a still-alive pane records Missing from the kill outcome.
                let pane = match pane_life(&observed) {
                    PaneLife::Exited | PaneLife::BriefNotDelivered => observed,
                    _ => PaneState {
                        session: record.session.clone(),
                        pane_id: record.pane_id.clone().unwrap_or_default(),
                        pid: record.pid.unwrap_or(0),
                        boot_id: record.boot_id.clone(),
                        life: PaneLife::Missing.into(),
                        ..Default::default()
                    },
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
                let mut pane = pane;
                if pane_life(&pane) == PaneLife::Alive && !delivered {
                    pane.life = match self
                        .tmux
                        .kill_session_verified(&record.session, &record.spawn_token)
                        .await
                    {
                        Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => {
                            PaneLife::BriefNotDelivered.into()
                        }
                        _ => PaneLife::Indeterminate.into(),
                    };
                }
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
        let status = match boss_tmux::probe_pane(&self.tmux, &record.session, &record.spawn_token).await {
            PaneProbe::Absent | PaneProbe::ForeignToken => {
                pane.life = PaneLife::Missing.into();
                return pane;
            }
            PaneProbe::Unreadable { .. } => {
                pane.life = PaneLife::Indeterminate.into();
                return pane;
            }
            probe @ (PaneProbe::Alive | PaneProbe::Dead { .. }) => {
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
                match probe {
                    PaneProbe::Dead { status: None } => None,
                    PaneProbe::Dead { status: Some(text) } => match text.parse::<i32>() {
                        Ok(status) => Some(status),
                        Err(_) => {
                            pane.life = PaneLife::Indeterminate.into();
                            return pane;
                        }
                    },
                    _ => {
                        pane.life = PaneLife::Alive.into();
                        return pane;
                    }
                }
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
        if matches!(
            pane_life(pane),
            PaneLife::Exited | PaneLife::BriefNotDelivered | PaneLife::Missing
        ) {
            self.tmux.cleanup_worker_processes(&record.spawn_token).await?;
            if self.tmux.runner().manages_host_processes() {
                let workspace = self
                    .workspace_path(&record.workspace_id, &record.lease_id)
                    .await
                    .unwrap_or_default();
                self.tmux
                    .cleanup_run_processes(&record.execution_id, &workspace)
                    .await?;
            }
        }
        match record.phase {
            // An uncertain launch leaves the record here. Whatever the pane
            // shows now decides it: a session that exists is a launched
            // worker, and one that does not is a finished attempt.
            Phase::Launching => match pane_life(pane) {
                PaneLife::Alive | PaneLife::Exited | PaneLife::BriefNotDelivered => {
                    record.phase = Phase::Alive;
                    record.pane_id = (!pane.pane_id.is_empty()).then(|| pane.pane_id.clone());
                    adopt_pid(record, (pane.pid > 0).then_some(pane.pid));
                    self.persist(record)?;
                    Box::pin(self.settle(record, pane)).await
                }
                PaneLife::Missing => self.record_terminal(record, pane),
                _ => Ok(()),
            },
            Phase::Alive | Phase::Delivering => {
                if !matches!(
                    pane_life(pane),
                    PaneLife::Exited | PaneLife::BriefNotDelivered | PaneLife::Missing
                ) {
                    // Commands run one at a time, so a Delivering record seen
                    // here is left over from a send whose completion was never
                    // persisted. Return it to Alive, keeping the attempt on the
                    // record so a replay of that command still reports it as
                    // uncertain, and so later inputs are accepted.
                    if record.phase == Phase::Delivering {
                        record.phase = Phase::Alive;
                        self.persist(record)?;
                    }
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

    /// Remove the run's input home and settings file; the outputs directory is retained (see below). The outcome is recorded
    /// on the record: `reclaimed` once clean, otherwise the error to retry.
    fn reclaim(&self, record: &mut ExecutionRecord) {
        if record.reclaimed {
            return;
        }
        if record.observation.is_some() {
            match self.store.observations_drained(record) {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    record.reclaim_error = Some(error.to_string());
                    return;
                }
            }
        }
        let mut errors = Vec::new();
        if let Some(pending) = record.observation.as_ref().map(|o| o.pending.clone()) {
            if let Err(error) = std::fs::remove_dir_all(&pending)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(format!("observation directory: {error}"));
            }
            if let Err(error) = self.store.prune_observation_sources(record) {
                errors.push(format!("observation sources: {error}"));
            }
        }
        if let Some(path) = &record.settings_file
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("settings file: {error}"));
        }
        // The outputs directory is deliberately kept: it holds the files the
        // worker wrote through the output variables, which are completion
        // evidence until the hub has finalized the run. `prune_stale_outputs`
        // removes it once the retention window has passed.
        let home = if let Some(state) = &record.runtime_state {
            // Provisioning wrote files somewhere; a state naming no home the
            // spoke recognises cannot be cleaned up, so it never counts as done.
            if input::owned_home(state).is_none() {
                Err(anyhow::anyhow!(
                    "driver runtime state names no home the spoke can reclaim"
                ))
            } else {
                input::reclaim(state)
            }
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
        prune_stale_outputs(&self.homes_root, OUTPUT_RETENTION, &records);
        let mut failures = Vec::new();
        for mut record in records {
            let pane = match record.phase {
                Phase::Launching | Phase::Alive | Phase::Delivering | Phase::Stopping => self.observe(&record).await,
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

const OUTPUT_ENV_NAMES: &[&str] = &[
    "BOSS_STRUCTURED_OUTPUT",
    "BOSS_STRUCTURED_OUTPUT_KIND",
    "BOSS_PR_URL_OUTPUT",
];

/// Minimum directory age for pruning outputs of reclaimed, unassigned runs.
const OUTPUT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Remove `outputs-*` directories under `homes_root` untouched for longer than
/// `retention`, excluding assigned or unreclaimed runs. Best effort: a directory that cannot be removed is retried by
/// the next sweep.
fn prune_stale_outputs(homes_root: &Path, retention: Duration, records: &[ExecutionRecord]) {
    let protected: std::collections::BTreeSet<_> = records
        .iter()
        .filter(|record| record.phase.assigned() || !record.reclaimed)
        .map(|record| format!("outputs-{}", hex_token(&record.spawn_token)))
        .collect();
    let Ok(entries) = std::fs::read_dir(homes_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("outputs-") || protected.contains(&name) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > retention);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn outputs_root(homes_root: &Path, spawn_token: &str) -> PathBuf {
    homes_root.join(format!("outputs-{}", hex_token(spawn_token)))
}

/// Output locations are allocated on the execution host beside its private homes.
fn insert_output_environment(
    environment: &mut BTreeMap<String, String>,
    spawn: &Spawn,
    homes_root: &Path,
    spawn_token: &str,
) -> Result<()> {
    let root = outputs_root(homes_root, spawn_token);
    // The supervisor alone decides the output contract; caller values for these
    // names must never reach the worker.
    for name in OUTPUT_ENV_NAMES {
        environment.remove(*name);
    }
    if spawn.structured_output_kind.is_some() || spawn.pr_url_output {
        std::fs::create_dir_all(&root)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    use boss_engine_driver::{StructuredOutputRequest, default_structured_output_wiring};
    use boss_engine_structured_output::{StructuredOutputKind, prepare};
    let designated = spawn
        .structured_output_kind
        .as_deref()
        .map(|slug| {
            StructuredOutputKind::all()
                .find(|kind| kind.slug() == slug && *kind != StructuredOutputKind::PrUrl)
                .with_context(|| format!("unknown structured output kind: {slug}"))
        })
        .transpose()?;
    for kind in designated
        .into_iter()
        .chain(spawn.pr_url_output.then_some(StructuredOutputKind::PrUrl))
    {
        // The identifier is the hex token, never the raw token: the raw
        // value can hold `/` or `..` and would escape the private directory.
        let path = prepare(&root, &hex_token(spawn_token), kind)?;
        environment.extend(
            default_structured_output_wiring(&StructuredOutputRequest {
                kind,
                result_path: &path,
                schema: None,
            })
            .env,
        );
    }
    if let Some(kind) = designated {
        environment.insert("BOSS_STRUCTURED_OUTPUT_KIND".into(), kind.slug().into());
    }
    Ok(())
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

fn adopt_pid(record: &mut ExecutionRecord, pid: Option<u64>) {
    record.pid = pid;
    if let Some(observation) = &mut record.observation {
        observation.pid_start = pid
            .and_then(|pid| crate::process::process_snapshot(pid as i32).ok())
            .map(|process| process.start);
    }
}

#[cfg(test)]
mod tests;
