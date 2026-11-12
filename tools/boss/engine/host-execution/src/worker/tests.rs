use super::worker_paths::outputs_root;
use super::*;
use crate::cube::CubeJsonTransport;
use async_trait::async_trait;
use boss_tmux::{CommandOutput, CommandRunner};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

type StoredExecutions = HashMap<(String, String), Vec<u8>>;

#[derive(Clone, Default)]
struct MemoryStore(Arc<Mutex<StoredExecutions>>, Arc<std::sync::atomic::AtomicBool>);

impl ExecutionStore for MemoryStore {
    fn worker_api_paths(&self) -> Option<(PathBuf, PathBuf)> {
        Some(("/host/worker.sock".into(), "/host/bin/boss".into()))
    }
    fn observations_drained(&self, _record: &ExecutionRecord) -> Result<bool> {
        Ok(self.1.load(std::sync::atomic::Ordering::SeqCst))
    }
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
    heartbeat_args: Mutex<Vec<Vec<String>>>,
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
                self.heartbeat_args
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|a| (*a).to_owned()).collect());
                Ok(json!({}))
            }
            _ => Ok(json!({})),
        }
    }
}

#[derive(Default, bon::Builder)]
struct FakeTmux {
    #[builder(default)]
    delivered_home: Mutex<Option<PathBuf>>,
    #[builder(default)]
    sessions: Mutex<HashMap<String, String>>,
    #[builder(default)]
    pane_dead: Mutex<HashMap<String, (String, Option<i32>)>>,
    #[builder(default)]
    mismatch: Mutex<HashMap<String, String>>,
    #[builder(default)]
    killed: Mutex<Vec<String>>,
    #[builder(default)]
    show_env_error: Mutex<bool>,
    #[builder(default)]
    dead_error: Mutex<bool>,
    #[builder(default)]
    kill_error: Mutex<bool>,
    #[builder(default)]
    sent: Mutex<Vec<Vec<String>>>,
    #[builder(default)]
    envs: Mutex<HashMap<String, Vec<String>>>,
    #[builder(default)]
    launches: Mutex<Vec<String>>,
    #[builder(default)]
    manage_processes: std::sync::atomic::AtomicBool,
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
    fn manages_host_processes(&self) -> bool {
        self.manage_processes.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn run(&self, _program: &Path, args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let rest = subcommand(&args);
        let Some(verb) = rest.first().map(String::as_str) else {
            return Ok(ok(""));
        };
        match verb {
            "new-session" => {
                self.launches.lock().unwrap().push(rest.last().unwrap().clone());
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
                if let Some(home) = self.delivered_home.lock().unwrap().as_ref() {
                    std::fs::write(home.join(initial_prompt::DELIVERED_MARKER), "").unwrap();
                }
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
fn tighten_owner_only(root: &Path) {
    // `pin` creates a 0700 leaf only when the path is missing. The
    // hermetic wrapper's TMPDIR is 0755, and a sibling `create_dir_all`
    // or leftover temp-backed root can leave `process_root()` already
    // present and too open — Linux CI then panics in `pin`.
    if let Ok(meta) = std::fs::symlink_metadata(root)
        && meta.is_dir()
        && !meta.file_type().is_symlink()
    {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn shared_homes() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = homes::process_root();
        tighten_owner_only(&root);
        homes::pin(&root).unwrap()
    })
}

#[test]
fn shared_homes_root_is_owner_only() {
    let mode = std::fs::symlink_metadata(shared_homes()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

async fn supervisor(root: &Path, fake: Arc<FakeTmux>) -> Supervisor<ListingCube, MemoryStore> {
    supervisor_with(root, fake, MemoryStore::default()).await
}

async fn supervisor_with<S: ExecutionStore>(root: &Path, fake: Arc<FakeTmux>, store: S) -> Supervisor<ListingCube, S> {
    let homes_root = shared_homes();
    tighten_owner_only(homes_root);
    let homes = homes::pin(homes_root).unwrap();
    let workspace = root.join("worker");
    std::fs::create_dir_all(&workspace).unwrap();
    Supervisor {
        cube: ListingCube {
            path: workspace,
            heartbeats: Mutex::new(Vec::new()),
            heartbeat_args: Mutex::new(Vec::new()),
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
        observation: None,
        worker_outputs: BTreeMap::new(),
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
async fn sweep_adoption_captures_process_start_for_hook_authentication() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    seed(&supervisor.store, Phase::Launching);
    let mut record = stored(&supervisor.store);
    record.observation = Some(ObservationConfig {
        workspace: root.path().into(),
        pending: root.path().join("pending"),
        file: None,
        pid_start: None,
    });
    let pid = std::process::id() as u64;
    supervisor
        .settle(
            &mut record,
            &PaneState {
                life: PaneLife::Alive as i32,
                pid,
                pane_id: "%1".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let saved = stored(&supervisor.store);
    assert_eq!(saved.phase, Phase::Alive);
    assert_eq!(saved.pid, Some(pid));
    assert_eq!(
        saved.observation.unwrap().pid_start,
        Some(crate::process::process_snapshot(pid as i32).unwrap().start)
    );
}

#[tokio::test]
async fn observation_wiring_preserves_policy_and_retains_home_until_durable_drain() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let supervisor = supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    seed(&supervisor.store, Phase::Alive);
    let mut record = stored(&supervisor.store);
    record.spawn_token = "observation-token".into();
    record.home = Some(run_home("observation-home"));
    let original = json!({"matcher":"Bash", "hooks":[{"type":"command", "command":"policy-guard"}]});
    let mut spawn = Spawn {
        settings_json: serde_json::to_vec(&json!({"hooks":{"PreToolUse":[original.clone()]}})).unwrap(),
        ..Default::default()
    };
    supervisor
        .prepare_observation(
            &mut record,
            &mut spawn,
            &boss_engine_driver::ClaudeDriver,
            boss_engine_driver::ProgressObservationConfig {
                events_socket_path: root.path().join("events.sock"),
                forwarder_binary: "/test/boss-event".into(),
                lease_id: "lease".into(),
                run_id: "run-test".into(),
                workspace_path: root.path().into(),
            },
        )
        .unwrap();
    let settings: Value = serde_json::from_slice(&spawn.settings_json).unwrap();
    assert!(
        settings["hooks"]["PreToolUse"][0]
            .to_string()
            .contains("/test/boss-event"),
        "forwarder stays ahead of policy guards"
    );
    assert!(settings["hooks"]["PreToolUse"].as_array().unwrap().contains(&original));
    assert!(settings["hooks"]["Stop"].to_string().contains("/test/boss-event"));
    let observation = record.observation.as_ref().unwrap();
    assert!(observation.file.is_none());
    assert_eq!(
        std::fs::metadata(&observation.pending).unwrap().permissions().mode() & 0o777,
        0o700
    );
    record.phase = Phase::Terminal;
    let pending = observation.pending.clone();
    supervisor.reclaim(&mut record);
    assert!(!record.reclaimed);
    assert!(record.home.as_ref().unwrap().exists());
    assert!(pending.exists());
    supervisor.store.1.store(true, std::sync::atomic::Ordering::SeqCst);
    supervisor.reclaim(&mut record);
    assert!(record.reclaimed);
    assert!(!record.home.as_ref().unwrap().exists());
    assert!(!pending.exists());
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

#[tokio::test]
async fn heartbeat_sends_the_engine_owned_lease_ttl() {
    assert_eq!(HEARTBEAT_TTL_SECONDS, 86400);
    assert!(HEARTBEAT_INTERVAL.as_secs() * 10 < HEARTBEAT_TTL_SECONDS);
    let root = tempfile::tempdir().unwrap();
    let supervisor = supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    seed(&supervisor.store, Phase::Alive);
    heartbeat_leases(&supervisor.cube, &supervisor.store).await;
    let sent = supervisor.cube.heartbeat_args.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].windows(2).any(|w| w[0] == "--ttl-seconds" && w[1] == "86400"),
        "{:?}",
        sent[0]
    );
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
async fn launching_record_with_a_live_session_is_promoted_by_adopt() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Launching);
    let result = supervisor.execute(&adopt_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32);
    assert_eq!(pane_life(&pane_of(&result)), PaneLife::Alive);
    assert_eq!(stored(&supervisor.store).phase, Phase::Alive);
    let delivered = supervisor
        .execute(&deliver_command("d1", DeliveryKind::Nudge, "hello"), false, 1)
        .await;
    assert_eq!(delivered.code, ResultCode::Ok as i32, "{}", delivered.reason);
}

#[tokio::test]
async fn launching_record_whose_pane_exited_is_reclaimed_by_the_sweep() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    fake.pane_dead
        .lock()
        .unwrap()
        .insert(session_name("token"), ("1".into(), Some(0)));
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Launching);
    let home = run_home("launching-sweep-home");
    update(&supervisor.store, |r| r.home = Some(home.clone()));
    assert!(supervisor.sweep().await.is_empty());
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert!(record.reclaimed);
    assert!(!home.exists());
}

#[tokio::test]
async fn stale_delivering_record_returns_to_alive_keeping_the_attempt() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Delivering);
    update(&supervisor.store, |r| {
        r.delivery = Some(DeliveryProgress {
            command_id: "d1".into(),
            completed: false,
        })
    });
    let result = supervisor.execute(&adopt_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32);
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Alive);
    assert!(record.delivery.is_some());
    let replay = supervisor
        .execute(&deliver_command("d1", DeliveryKind::Nudge, "x"), false, 1)
        .await;
    assert_eq!(replay.code, ResultCode::OutcomeUncertain as i32);
}

#[tokio::test]
async fn grok_state_outside_the_private_root_is_not_owned() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    let state = |home: &Path| {
        boss_engine_driver::grok::GrokRuntimeState::builder()
            .grok_home(home.join("grok-home"))
            .process_home(home.join("process-home"))
            .auth_source_path(PathBuf::from("/auth.json"))
            .session_id("s")
            .workspace_path(PathBuf::from("/ws"))
            .build()
            .to_driver_runtime_state()
    };
    let outside = state(Path::new("/tmp/boss-grok-homes/run-1"));
    assert!(supervisor.check_owned_home(&outside).unwrap_err().contains("outside"));
    let inside = state(&supervisor.homes_root.join(".grok").join("run-1"));
    assert!(supervisor.check_owned_home(&inside).is_ok());
    assert!(input::owned_home(&inside).unwrap().ends_with("grok-home"));
    let unknown = DriverRuntimeState::new(json!({"unrelated": true}));
    assert!(supervisor.check_owned_home(&unknown).is_err());
}

/// Pins Grok's per-run container root, auth source and transcript store for
/// one test, restoring the previous values on drop.
struct GrokEnv {
    _homes: boss_engine_driver::test_support::GrokHomesOverride,
    _transcripts: boss_engine_driver::test_support::TranscriptStoreOverride,
    prior_auth: Option<std::ffi::OsString>,
    prior_skip: Option<std::ffi::OsString>,
}

fn grok_env(container_root: &Path, scratch: &Path) -> GrokEnv {
    let homes = boss_engine_driver::test_support::grok_homes_override(container_root);
    let transcripts = boss_engine_driver::test_support::transcript_store_override(&scratch.join("transcripts"));
    let auth = scratch.join("auth.json");
    std::fs::write(&auth, r#"{"token":"test-only"}"#).unwrap();
    let prior_auth = std::env::var_os(boss_engine_driver::grok::GROK_AUTH_SOURCE_ENV);
    let prior_skip = std::env::var_os(boss_engine_driver::grok::GROK_SKIP_POSTURE_ASSERT_ENV);
    // SAFETY: the Grok homes lock held by `_homes` serialises every test
    // that touches these process-wide variables.
    unsafe {
        std::env::set_var(boss_engine_driver::grok::GROK_AUTH_SOURCE_ENV, &auth);
        std::env::set_var(boss_engine_driver::grok::GROK_SKIP_POSTURE_ASSERT_ENV, "1");
    }
    GrokEnv {
        _homes: homes,
        _transcripts: transcripts,
        prior_auth,
        prior_skip,
    }
}

impl Drop for GrokEnv {
    fn drop(&mut self) {
        // SAFETY: the lock is still held until the fields drop after this body.
        unsafe {
            match &self.prior_auth {
                Some(value) => std::env::set_var(boss_engine_driver::grok::GROK_AUTH_SOURCE_ENV, value),
                None => std::env::remove_var(boss_engine_driver::grok::GROK_AUTH_SOURCE_ENV),
            }
            match &self.prior_skip {
                Some(value) => std::env::set_var(boss_engine_driver::grok::GROK_SKIP_POSTURE_ASSERT_ENV, value),
                None => std::env::remove_var(boss_engine_driver::grok::GROK_SKIP_POSTURE_ASSERT_ENV),
            }
        }
    }
}

fn grok_spawn_command() -> Command {
    let mut command = spawn_command();
    if let Some(command::Body::Spawn(spawn)) = &mut command.body {
        spawn.driver_id = "grok".into();
        spawn.model = "grok-4.6".into();
    }
    command
}

/// A supervisor whose private-home root is its own fresh directory.
async fn grok_supervisor(root: &Path, fake: Arc<FakeTmux>) -> Supervisor<ListingCube, MemoryStore> {
    let mut supervisor = supervisor(root, fake).await;
    supervisor.homes_root = homes::pin(&root.join("homes")).unwrap();
    supervisor
}

#[tokio::test]
async fn grok_spawn_records_its_home_and_stop_reclaims_the_container() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = grok_supervisor(root.path(), fake.clone()).await;
    *fake.delivered_home.lock().unwrap() = Some(
        homes::grok_root(&supervisor.homes_root)
            .join("run-test")
            .join("grok-home"),
    );
    let _env = grok_env(&homes::grok_root(&supervisor.homes_root), root.path());
    let result = supervisor.execute(&grok_spawn_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Alive);
    let home = record.home.clone().expect("grok home recorded");
    assert!(home.ends_with("grok-home") && home.is_dir(), "{home:?}");
    assert!(record.runtime_state.is_some());
    let container = home.parent().unwrap().to_path_buf();
    assert!(container.starts_with(homes::grok_root(&supervisor.homes_root)));
    let stopped = supervisor.execute(&stop_command(), false, 1).await;
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert!(!container.exists(), "stop must remove the whole container");
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert!(record.reclaimed);
}

#[tokio::test]
async fn grok_spawn_stops_waiting_once_the_delivered_marker_appears() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let mut supervisor = grok_supervisor(root.path(), fake.clone()).await;
    supervisor.brief_budget = Duration::from_secs(60);
    let grok_home = homes::grok_root(&supervisor.homes_root)
        .join("run-test")
        .join("grok-home");
    let _env = grok_env(&homes::grok_root(&supervisor.homes_root), root.path());
    let marker = grok_home.join(initial_prompt::DELIVERED_MARKER);
    let started = Instant::now();
    let command = grok_spawn_command();
    let (result, ()) = tokio::join!(supervisor.execute(&command, false, 1), async {
        // The launcher writes its marker from inside the pane, so only
        // after the session exists. Provisioning clears any marker an
        // earlier attempt left in the home.
        for _ in 0..500 {
            if !fake.sessions.lock().unwrap().is_empty() {
                std::fs::write(&marker, "").unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the pane was never started");
    });
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the spawn waited out the brief budget instead of seeing the marker in the Grok home"
    );
}

#[test]
fn brief_budget_covers_every_launcher_stage_with_a_margin() {
    assert_eq!(
        BRIEF_DELIVERY_BUDGET,
        initial_prompt::DELIVERY_BUDGET + Duration::from_secs(30)
    );
}

#[tokio::test]
async fn live_pane_without_a_delivered_brief_is_not_a_successful_delivery() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Alive);
    let pane = supervisor.wait_for_brief(&stored(&supervisor.store)).await;
    assert_eq!(pane_life(&pane), PaneLife::BriefNotDelivered);
}

#[tokio::test]
async fn spawn_whose_grok_home_is_outside_the_private_root_is_rejected_and_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = grok_supervisor(root.path(), fake.clone()).await;
    let elsewhere = root.path().join("elsewhere");
    let _env = grok_env(&elsewhere, root.path());
    let result = supervisor.execute(&grok_spawn_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
    assert!(result.reason.contains("outside"), "{}", result.reason);
    assert!(fake.sessions.lock().unwrap().is_empty(), "no session may be created");
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert!(record.reclaimed, "{:?}", record.reclaim_error);
    assert!(
        !elsewhere.join("run-test").exists(),
        "the brief must not be left behind"
    );
}

fn grok_state_in(container: &Path) -> DriverRuntimeState {
    boss_engine_driver::grok::GrokRuntimeState::builder()
        .grok_home(container.join("grok-home"))
        .process_home(container.join("process-home"))
        .auth_source_path(PathBuf::from("/auth.json"))
        .session_id("s")
        .workspace_path(PathBuf::from("/ws"))
        .build()
        .to_driver_runtime_state()
}

#[tokio::test]
async fn rejected_home_that_cannot_be_removed_is_kept_and_retried_by_the_sweep() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = grok_supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    let container = root.path().join("elsewhere").join("run-test");
    std::fs::create_dir_all(container.join("grok-home")).unwrap();
    std::fs::write(container.join("grok-home").join("initial-prompt.txt"), "brief").unwrap();
    seed(&supervisor.store, Phase::Intent);
    let mut record = stored(&supervisor.store);
    record.driver_id = "grok".into();
    record.runtime_state = Some(grok_state_in(&container));
    record.home = Some(container.join("grok-home"));
    {
        // Cleanup refuses: the container is not under the Grok root in force.
        let _env = grok_env(&homes::grok_root(&supervisor.homes_root), root.path());
        let result = supervisor
            .reject_unowned_home(&grok_spawn_command(), record, "outside the private-home root".into())
            .unwrap();
        assert_eq!(result.code, ResultCode::Rejected as i32);
    }
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert!(!record.reclaimed, "a failed cleanup must not be recorded as done");
    assert!(record.reclaim_error.is_some());
    assert!(record.runtime_state.is_some(), "the cleanup handle must be kept");
    assert!(container.exists());
    {
        let _env = grok_env(&root.path().join("elsewhere"), root.path());
        assert!(supervisor.sweep().await.is_empty());
    }
    let record = stored(&supervisor.store);
    assert!(record.reclaimed, "{:?}", record.reclaim_error);
    assert!(record.reclaim_error.is_none());
    assert!(!container.exists());
}

#[tokio::test]
async fn rejected_unrecognised_driver_state_is_never_recorded_as_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    seed(&supervisor.store, Phase::Intent);
    let mut record = stored(&supervisor.store);
    record.runtime_state = Some(DriverRuntimeState::new(json!({"unrelated": true})));
    supervisor
        .reject_unowned_home(&spawn_command(), record, "not one the spoke can own".into())
        .unwrap();
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert!(!record.reclaimed);
    assert!(record.reclaim_error.as_deref().unwrap().contains("reclaim"));
    assert_eq!(
        supervisor.sweep().await.len(),
        1,
        "the failure stays visible to the sweep"
    );
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

/// Codex strips `*TOKEN*` from tool shells, so a reparented background
/// shell carries only the run id. Stop must still signal it.
#[tokio::test]
async fn stop_signals_a_reparented_shell_that_carries_only_the_run_id() {
    use std::io::BufRead;
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    fake.manage_processes.store(true, std::sync::atomic::Ordering::Relaxed);
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Alive);
    let mut shell = std::process::Command::new("python3")
        .args([
            "-u",
            "-c",
            "import os,time; os.setsid(); print('ready'); time.sleep(600)",
        ])
        .current_dir(root.path().join("worker"))
        .env("BOSS_RUN_ID", "exec_test")
        .env_remove("BOSS_SPAWN_TOKEN")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(shell.stdout.as_mut().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let result = supervisor.execute(&stop_command(), false, 1).await;
    let exited = shell.try_wait().unwrap().is_some();
    let _ = shell.kill();
    let _ = shell.wait();
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert!(exited, "the run-id-only shell was signalled by stop");
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
async fn stop_on_terminal_exited_keeps_evidence_and_adopt_reports_exited() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake.clone()).await;
    seed(&supervisor.store, Phase::Terminal);
    update(&supervisor.store, |r| {
        r.terminal = Some(TerminalEvidence {
            life: PaneLife::Exited.into(),
            exit_code: Some(64),
        });
        r.reclaimed = true;
    });
    let result = supervisor.execute(&stop_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert_eq!(pane_life(&pane_of(&result)), PaneLife::Exited);
    let record = stored(&supervisor.store);
    assert_eq!(record.phase, Phase::Terminal);
    assert_eq!(
        record.terminal,
        Some(TerminalEvidence {
            life: PaneLife::Exited.into(),
            exit_code: Some(64)
        })
    );
    assert!(!fake.killed.lock().unwrap().is_empty());
    heartbeat_leases(&supervisor.cube, &supervisor.store).await;
    assert!(supervisor.cube.heartbeats.lock().unwrap().is_empty());
    let adopted = supervisor.execute(&adopt_command(), false, 1).await;
    assert_eq!(adopted.code, ResultCode::Ok as i32);
    assert_eq!(pane_life(&pane_of(&adopted)), PaneLife::Exited);
    assert_eq!(stored(&supervisor.store).phase, Phase::Terminal);
}

#[tokio::test]
async fn stop_on_alive_exited_pane_records_exited_evidence() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    fake.pane_dead
        .lock()
        .unwrap()
        .insert(session_name("token"), ("1".into(), Some(0)));
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Alive);
    let home = run_home("stop-already-exited");
    update(&supervisor.store, |r| r.home = Some(home.clone()));
    let result = supervisor.execute(&stop_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert_eq!(pane_life(&pane_of(&result)), PaneLife::Exited);
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
    let adopted = supervisor.execute(&adopt_command(), false, 1).await;
    assert_eq!(pane_life(&pane_of(&adopted)), PaneLife::Exited);
}

#[tokio::test]
async fn stop_on_alive_brief_not_delivered_pane_keeps_that_life() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    fake.pane_dead
        .lock()
        .unwrap()
        .insert(session_name("token"), ("1".into(), Some(64)));
    let supervisor = supervisor(root.path(), fake).await;
    seed(&supervisor.store, Phase::Alive);
    let home = run_home("stop-brief-not-delivered");
    update(&supervisor.store, |r| r.home = Some(home.clone()));
    let result = supervisor.execute(&stop_command(), false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert_eq!(pane_life(&pane_of(&result)), PaneLife::BriefNotDelivered);
    let record = stored(&supervisor.store);
    assert_eq!(
        record.terminal,
        Some(TerminalEvidence {
            life: PaneLife::BriefNotDelivered.into(),
            exit_code: Some(64)
        })
    );
    let adopted = supervisor.execute(&adopt_command(), false, 1).await;
    assert_eq!(pane_life(&pane_of(&adopted)), PaneLife::BriefNotDelivered);
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
async fn spawn_applies_forced_permission_mode_and_agent_rules() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = supervisor(root.path(), fake.clone()).await;
    let mut command = spawn_command();
    let Some(command::Body::Spawn(spawn)) = &mut command.body else {
        unreachable!()
    };
    spawn.permission_mode = Some("dontAsk".into());
    spawn.agent_rules = Some("Read-only reviewer rules".into());
    spawn.required_features.push(boss_protocol_spoke::v1::Feature {
        name: "spawn-worker-posture".into(),
        revision: 1,
    });
    let result = supervisor.execute(&command, false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    let launches = fake.launches.lock().unwrap();
    assert!(launches.iter().any(|launch| launch.contains("dontAsk")), "{launches:?}");
    assert!(
        launches
            .iter()
            .all(|launch| !launch.contains("dangerously-skip-permissions"))
    );
    let driver = DriverRegistry::default().require("claude").unwrap();
    let rules = driver.agent_rules_destination(&root.path().join("worker"), "run-test");
    assert_eq!(std::fs::read_to_string(rules).unwrap(), "Read-only reviewer rules");
}

#[tokio::test]
async fn unknown_output_kind_is_rejected_before_launch_intent() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = supervisor(root.path(), fake.clone()).await;
    let mut command = spawn_command();
    let Some(command::Body::Spawn(spawn)) = &mut command.body else {
        unreachable!()
    };
    spawn.structured_output_kind = Some("future-unknown-output".into());
    let result = supervisor.execute(&command, false, 1).await;
    assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
    assert!(result.reason.contains("unknown structured output kind"));
    assert!(fake.launches.lock().unwrap().is_empty());
    assert!(supervisor.store.load("run-test", "token").unwrap().is_none());
}

#[tokio::test]
async fn pruning_preserves_assigned_and_unreclaimed_evidence() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = grok_supervisor(root.path(), Arc::new(FakeTmux::default())).await;
    seed(&supervisor.store, Phase::Alive);
    let mut record = stored(&supervisor.store);
    let dir = outputs_root(&supervisor.homes_root, &record.spawn_token);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("result.json"), "evidence").unwrap();
    let old = std::time::SystemTime::now() - OUTPUT_RETENTION - Duration::from_secs(60);
    std::fs::File::open(&dir)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    for phase in [Phase::Alive, Phase::Terminal] {
        record.phase = phase;
        record.reclaimed = false;
        prune_stale_outputs(&supervisor.homes_root, OUTPUT_RETENTION, &[record.clone()]);
        assert!(dir.join("result.json").exists());
    }
    record.reclaimed = true;
    prune_stale_outputs(&supervisor.homes_root, OUTPUT_RETENTION, &[record]);
    assert!(!dir.exists());
}

#[test]
fn output_paths_stay_under_the_outputs_directory_for_hostile_tokens() {
    let root = tempfile::tempdir().unwrap();
    let homes = root.path().join("homes");
    std::fs::create_dir_all(&homes).unwrap();
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, "keep").unwrap();
    for token in [
        format!("{}/outside", root.path().display()),
        "../outside".to_owned(),
        "a/b/c".to_owned(),
    ] {
        let spawn = Spawn {
            structured_output_kind: Some("review-result".into()),
            pr_url_output: true,
            ..Default::default()
        };
        let mut env = BTreeMap::new();
        insert_output_environment(&mut env, &spawn, &homes, &token).unwrap();
        let dir = outputs_root(&homes, &token);
        assert!(dir.starts_with(&homes) && dir.is_dir(), "{token}");
        for name in ["BOSS_STRUCTURED_OUTPUT", "BOSS_PR_URL_OUTPUT"] {
            assert!(Path::new(&env[name]).starts_with(&dir), "{token}: {name}={}", env[name]);
        }
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
}

#[tokio::test]
async fn terminal_settle_keeps_an_already_written_output_file() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = supervisor(root.path(), fake.clone()).await;
    seed(&supervisor.store, Phase::Alive);
    let mut record = stored(&supervisor.store);
    let dir = outputs_root(&supervisor.homes_root, &record.spawn_token);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("result.json"), "evidence").unwrap();
    supervisor.reclaim(&mut record);
    assert!(record.reclaimed, "{:?}", record.reclaim_error);
    assert_eq!(std::fs::read_to_string(dir.join("result.json")).unwrap(), "evidence");
    prune_stale_outputs(&supervisor.homes_root, OUTPUT_RETENTION, &[]);
    assert!(
        dir.join("result.json").exists(),
        "fresh output survives the retention sweep"
    );
    prune_stale_outputs(&supervisor.homes_root, Duration::ZERO, &[]);
    assert!(!dir.exists(), "output past retention is removed");
}

#[test]
fn caller_cannot_set_output_variables_when_no_output_is_designated() {
    let root = tempfile::tempdir().unwrap();
    let mut env: BTreeMap<String, String> = [
        ("BOSS_STRUCTURED_OUTPUT", "/etc/passwd"),
        ("BOSS_STRUCTURED_OUTPUT_KIND", "forged"),
        ("BOSS_PR_URL_OUTPUT", "/etc/passwd"),
        ("KEEP_ME", "yes"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    insert_output_environment(&mut env, &Spawn::default(), root.path(), "token").unwrap();
    assert_eq!(env.keys().map(String::as_str).collect::<Vec<_>>(), ["KEEP_ME"]);
    assert!(!outputs_root(root.path(), "token").exists());
}

#[tokio::test]
async fn spawn_stamps_identity_last_and_keeps_settings_out_of_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    let supervisor = supervisor(root.path(), fake.clone()).await;
    let mut command = spawn_command();
    // Other spawn tests reclaim their homes concurrently. Keep this
    // delivery-marker fixture in its own run home.
    let run_id = root.path().file_name().unwrap().to_str().unwrap().replace('.', "_");
    command.run.as_mut().unwrap().run_id = run_id.clone();
    *fake.delivered_home.lock().unwrap() =
        Some(boss_engine_driver::codex::codex_home_for_run(&format!("claude-{run_id}")).unwrap());
    if let Some(command::Body::Spawn(spawn)) = &mut command.body {
        spawn.settings_json = br#"{"hooks":{}}"#.to_vec();
        spawn.effort = Some("medium".into());
        spawn.structured_output_kind = Some("review-result".into());
        spawn.pr_url_output = true;
        for (name, value) in [
            ("BOSS_SPAWN_TOKEN", "forged"),
            ("BOSS_RUN_ID", "forged"),
            ("BOSS_SESSION_SCHEMA", "forged"),
            ("BOSS_SOCKET_PATH", "/tmp/forged.sock"),
            ("BOSS_BIN", "/tmp/forged-boss"),
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
    for gateway in ["BOSS_SOCKET_PATH=/host/worker.sock", "BOSS_BIN=/host/bin/boss"] {
        assert!(envs.contains(&gateway.to_owned()), "{envs:?}");
    }
    assert!(fake.launches.lock().unwrap()[0].contains("--effort medium"));
    let output = envs
        .iter()
        .find_map(|env| env.strip_prefix("BOSS_STRUCTURED_OUTPUT="))
        .unwrap();
    assert!(Path::new(output).is_absolute());
    assert!(Path::new(output).starts_with(shared_homes()));
    assert!(envs.contains(&"BOSS_STRUCTURED_OUTPUT_KIND=review-result".into()));
    assert!(envs.iter().any(|env| env.starts_with("BOSS_PR_URL_OUTPUT=")));
    assert!(envs.contains(&"KEEP_ME=yes".to_owned()), "{envs:?}");
    assert!(envs.iter().filter(|e| e.contains("forged")).count() == 0, "{envs:?}");
    assert!(!workspace.join(".boss-spoke-settings.json").exists());
    let run = command.run.as_ref().unwrap();
    let record = decode(&supervisor.store.load(&run.run_id, &run.spawn_token).unwrap().unwrap()).unwrap();
    let settings = record.settings_file.expect("settings recorded");
    assert!(settings.starts_with(shared_homes()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Stop removes it with the home.
    let mut stop = stop_command();
    stop.run = command.run.clone();
    let stopped = supervisor.execute(&stop, false, 1).await;
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert!(!settings.exists());
}

#[test]
fn brief_not_delivered_is_distinct_from_a_generic_exit() {
    assert!(is_brief_not_delivered(Some(64), None, None));
    assert!(!is_brief_not_delivered(Some(1), None, None));
}
