//! Token-identified tmux worker sessions: Spawn, Adopt, Deliver, Stop.

use crate::{
    cube::{self, CubeJsonTransport},
    homes, input,
};
use anyhow::{Context, Result, ensure};
use boss_engine_driver::{
    DriverRegistry, EnvDirective, SpawnPlan, SpawnRequest,
    initial_prompt::{self, delivered_marker_present, is_brief_not_delivered},
};
use boss_protocol::DriverRuntimeState;
use boss_protocol_spoke::{
    Validate,
    v1::{Command, CommandResult, Deliver, PaneLife, PaneState, ResultCode, Run, Spawn, Stop, command, command_result},
};
use boss_tmux::{
    DisplayField, KillSessionError, KillSessionOutcome, NewSession, OptionScope, OptionSetting, TMUX_SPAWN_TOKEN_ENV,
    Tmux, quote_for_shell,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
pub const HEARTBEAT_TTL_SECONDS: u64 = 300;
pub const BRIEF_DELIVERY_BUDGET: Duration = Duration::from_secs(35);
const SESSION_SCHEMA: &str = "1";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
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
        };
        self.persist(&record)?;
        if interrupted {
            return self.reconcile_launch(command, record).await;
        }
        let workspace = match self.workspace_path(&spawn.workspace_id, &spawn.lease_id).await {
            Ok(path) => path,
            Err(error) => {
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        let driver = match DriverRegistry::default().require(&spawn.driver_id) {
            Ok(driver) => driver,
            Err(error) => {
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        let runtime = match input::provision(driver.as_ref(), &workspace, &spawn.prompt_utf8, &run.run_id).await {
            Ok(runtime) => runtime,
            Err(error) => {
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Rejected, error.to_string(), None));
            }
        };
        record.runtime_state = runtime.clone();
        record.home = runtime.as_ref().and_then(initial_prompt::owned_home_from_runtime_state);
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
        if matches!(
            pane_life(&pane),
            PaneLife::Exited | PaneLife::BriefNotDelivered | PaneLife::Missing
        ) {
            self.reclaim(&record);
            record.phase = Phase::Terminal;
            self.persist(&record)?;
        }
        let code = match pane_life(&pane) {
            PaneLife::Indeterminate => ResultCode::OutcomeUncertain,
            PaneLife::Missing => ResultCode::Absent,
            _ => ResultCode::Ok,
        };
        Ok(outcome(command, code, String::new(), Some(pane)))
    }

    async fn launch(
        &self,
        record: &ExecutionRecord,
        spawn: &Spawn,
        driver: &dyn boss_engine_driver::AgentDriver,
        workspace: &Path,
    ) -> Result<()> {
        let settings_path = if spawn.settings_json.is_empty() {
            None
        } else {
            let path = workspace.join(".boss-spoke-settings.json");
            std::fs::write(&path, &spawn.settings_json)?;
            Some(path)
        };
        let plan = driver.spawn_invocation(SpawnRequest {
            model: &spawn.model,
            effort: None,
            settings_path: settings_path.as_deref(),
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(&record.run_id),
        });
        self.tmux
            .start_server_with_options(&[
                OptionSetting {
                    scope: OptionScope::Server,
                    option: "exit-empty",
                    value: "off",
                },
                OptionSetting {
                    scope: OptionScope::Global,
                    option: "remain-on-exit",
                    value: "on",
                },
            ])
            .await?;
        let mut environment = BTreeMap::new();
        environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), record.spawn_token.clone());
        environment.insert(SESSION_SCHEMA_ENV.to_owned(), SESSION_SCHEMA.to_owned());
        environment.insert("BOSS_RUN_ID".to_owned(), record.execution_id.clone());
        environment.insert(homes::ROOT_ENV.to_owned(), self.homes_root.display().to_string());
        environment.insert("CLAUDE_CODE_TMUX_TRUECOLOR".to_owned(), "1".to_owned());
        environment.insert("COLORTERM".to_owned(), "truecolor".to_owned());
        for env in &spawn.env {
            if HOST_ENV_DENY.contains(&env.name.as_str()) {
                continue;
            }
            environment.insert(env.name.clone(), env.value.clone());
        }
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
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    format!("launch may have happened: {error:#}"),
                    Some(pane),
                ))
            }
            _ => {
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::OutcomeUncertain,
                    format!("launch outcome uncertain: {error:#}"),
                    Some(pane),
                ))
            }
        }
    }

    async fn reconcile_launch(&self, command: &Command, mut record: ExecutionRecord) -> Result<CommandResult> {
        match record.phase {
            Phase::Intent => {
                record.phase = Phase::Terminal;
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
                        record.phase = Phase::Terminal;
                        self.persist(&record)?;
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
        let Some(record) = self.loaded(run)? else {
            return Ok(outcome(command, ResultCode::Absent, "no execution record", None));
        };
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
        if interrupted && record.phase == Phase::Delivering {
            return Ok(outcome(
                command,
                ResultCode::OutcomeUncertain,
                "deliver interrupted after attempting",
                Some(self.observe(&record).await),
            ));
        }
        let text = std::str::from_utf8(&deliver.input_utf8).context("deliver input must be UTF-8")?;
        record.phase = Phase::Delivering;
        self.persist(&record)?;
        match self.tmux.send_keys(&record.session, text).await {
            Ok(()) => {
                record.phase = Phase::Alive;
                self.persist(&record)?;
                Ok(outcome(
                    command,
                    ResultCode::Ok,
                    String::new(),
                    Some(self.observe(&record).await),
                ))
            }
            Err(error) => Ok(outcome(
                command,
                ResultCode::OutcomeUncertain,
                format!("deliver attempted: {error:#}"),
                Some(self.observe(&record).await),
            )),
        }
    }

    async fn stop(&self, command: &Command, run: &Run, stop: &Stop, interrupted: bool) -> Result<CommandResult> {
        let Some(mut record) = self.loaded(run)? else {
            return Ok(outcome(command, ResultCode::Absent, "no execution record", None));
        };
        if interrupted && record.phase == Phase::Stopping {
            let pane = self.observe(&record).await;
            if pane_life(&pane) == PaneLife::Missing {
                self.reclaim(&record);
                record.phase = Phase::Terminal;
                self.persist(&record)?;
                return Ok(outcome(command, ResultCode::Ok, String::new(), Some(pane)));
            }
            return Ok(outcome(
                command,
                ResultCode::OutcomeUncertain,
                "stop interrupted",
                Some(pane),
            ));
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
                self.reclaim(&record);
                record.phase = Phase::Terminal;
                self.persist(&record)?;
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
                PaneLife::BriefNotDelivered | PaneLife::Exited | PaneLife::Missing | PaneLife::Indeterminate => {
                    return pane;
                }
                PaneLife::Alive if delivered || Instant::now() >= deadline => return pane,
                _ => {}
            }
            if Instant::now() >= deadline {
                return pane;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn observe(&self, record: &ExecutionRecord) -> PaneState {
        let mut pane = PaneState {
            session: record.session.clone(),
            pane_id: record.pane_id.clone().unwrap_or_default(),
            pid: record.pid.unwrap_or(0),
            boot_id: record.boot_id.clone(),
            ..Default::default()
        };
        match self.tmux.show_environment(&record.session, TMUX_SPAWN_TOKEN_ENV).await {
            Ok(Some(token)) if token == record.spawn_token => {}
            Ok(Some(_)) => {
                pane.life = PaneLife::Missing.into();
                return pane;
            }
            Ok(None) => {
                pane.life = PaneLife::Missing.into();
                return pane;
            }
            Err(_) => {
                pane.life = PaneLife::Missing.into();
                return pane;
            }
        }
        if let Ok(id) = self.tmux.display_message(&record.session, DisplayField::PaneId).await {
            pane.pane_id = id;
        }
        if let Ok(pid) = self.tmux.display_message(&record.session, DisplayField::PanePid).await {
            pane.pid = pid.parse().unwrap_or(pane.pid);
        }
        let dead = self
            .tmux
            .display_message(&record.session, DisplayField::PaneDead)
            .await
            .unwrap_or_default();
        if dead != "1" {
            pane.life = PaneLife::Alive.into();
            return pane;
        }
        let status = self
            .tmux
            .display_message(&record.session, DisplayField::PaneDeadStatus)
            .await
            .ok()
            .and_then(|text| text.parse::<i32>().ok());
        pane.exit_code = status;
        pane.life = if is_brief_not_delivered(status, record.home.as_deref(), None) {
            PaneLife::BriefNotDelivered.into()
        } else {
            PaneLife::Exited.into()
        };
        pane
    }

    fn reclaim(&self, record: &ExecutionRecord) {
        if let Some(state) = &record.runtime_state {
            let _ = input::reclaim(state);
        } else if let Some(home) = &record.home {
            let _ = input::reclaim_home(home);
        }
    }
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
    let env_prefix: String = plan
        .env
        .iter()
        .map(|directive| match directive {
            EnvDirective::Set(key, value) => format!("export {key}={}; ", quote_for_shell(value)),
            EnvDirective::Unset(key) => format!("unset {key}; "),
        })
        .collect();
    format!(
        "/bin/sh -lc {}",
        quote_for_shell(&format!("{env_prefix}{}", plan.command))
    )
}

fn pane_life(pane: &PaneState) -> PaneLife {
    PaneLife::try_from(pane.life).unwrap_or_default()
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
                "show-environment" => {
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

    async fn supervisor(root: &Path, fake: Arc<FakeTmux>) -> Supervisor<ListingCube, MemoryStore> {
        let homes = homes::pin(&root.join("homes")).unwrap();
        let workspace = root.join("worker");
        std::fs::create_dir_all(&workspace).unwrap();
        Supervisor {
            cube: ListingCube {
                path: workspace,
                heartbeats: Mutex::new(Vec::new()),
            },
            tmux: Tmux::with_runner_and_socket("/usr/bin/tmux", fake, root.join("tmux.sock")).unwrap(),
            store: MemoryStore::default(),
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

    #[test]
    fn brief_not_delivered_is_distinct_from_a_generic_exit() {
        assert!(is_brief_not_delivered(Some(64), None, None));
        assert!(!is_brief_not_delivered(Some(1), None, None));
    }
}
