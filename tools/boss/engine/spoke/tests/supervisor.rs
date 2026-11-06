//! Real-tmux coverage for the spoke worker supervisor.
mod hub;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use boss_host_execution::{
    cube::CubeJsonTransport,
    homes,
    worker::{ExecutionRecord, ExecutionStore, Phase, Supervisor, session_name},
};
use boss_protocol_spoke::v1::{self, ResultCode, command, command_result};
use boss_tmux::{DisplayField, NewSession, OptionScope, OptionSetting, TMUX_SPAWN_TOKEN_ENV, Tmux};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn declared_tmux_binary() -> Result<PathBuf> {
    let test_srcdir = PathBuf::from(std::env::var("TEST_SRCDIR")?);
    let host_tmux_runfiles = std::fs::read_dir(&test_srcdir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("host_tmux"))
        })
        .ok_or_else(|| anyhow!("Bazel did not provide the declared host tmux runfiles"))?;
    let tmux = host_tmux_runfiles.join("tmux");
    if !tmux.is_file() {
        bail!("the declared tmux binary is unavailable at {}", tmux.display());
    }
    Ok(tmux)
}

struct TmuxServerGuard {
    program: PathBuf,
    socket: PathBuf,
}

impl TmuxServerGuard {
    fn new(program: PathBuf, socket: PathBuf) -> Self {
        Self { program, socket }
    }
}

impl Drop for TmuxServerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.program)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

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
                    "lease_id": "lease"
                }]
            })),
            ["--json", "workspace", "heartbeat", "--lease", ..] => Ok(json!({})),
            _ => Ok(json!({})),
        }
    }
}

async fn live_tmux(root: &Path) -> (Tmux, TmuxServerGuard) {
    let program = declared_tmux_binary().expect("host tmux");
    let socket = root.join("tmux.sock");
    let tmux = Tmux::from_path_with_socket(&program, &socket).unwrap();
    tmux.start_server_with_options(&[
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
    .await
    .expect("start tmux server");
    (tmux, TmuxServerGuard::new(program, socket))
}

fn seed(store: &MemoryStore, token: &str, phase: Phase) {
    let record = ExecutionRecord {
        execution_id: format!("exec-{token}"),
        run_id: format!("run-{token}"),
        spawn_token: token.into(),
        assignment_generation: 1,
        lease_id: "lease".into(),
        workspace_id: "worker".into(),
        session: session_name(token),
        phase,
        boot_id: "boot".into(),
        pane_id: None,
        pid: None,
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

fn spawn_command(token: &str, command_id: &str) -> v1::Command {
    v1::Command {
        sequence: 1,
        command_id: command_id.into(),
        run: Some(v1::Run {
            execution_id: format!("exec-{token}"),
            run_id: format!("run-{token}"),
            spawn_token: token.into(),
            assignment_generation: 1,
        }),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Spawn(v1::Spawn {
            lease_id: "lease".into(),
            workspace_id: "worker".into(),
            driver_id: "claude".into(),
            model: "sonnet".into(),
            prompt_utf8: b"do the thing".to_vec(),
            ..Default::default()
        })),
    }
}

async fn create_session(tmux: &Tmux, token: &str, command: &str) {
    let mut environment = BTreeMap::new();
    environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), token.to_owned());
    tmux.new_session(&NewSession {
        name: session_name(token),
        environment,
        working_directory: std::env::temp_dir(),
        command: command.to_owned(),
    })
    .await
    .unwrap();
    tmux.set_option(&session_name(token), "@boss_spawn_token", token)
        .await
        .unwrap();
}

fn supervisor(root: &Path, tmux: Tmux) -> Supervisor<ListingCube, MemoryStore> {
    let homes = homes::pin(&root.join("homes")).unwrap();
    let workspace = root.join("worker");
    std::fs::create_dir_all(&workspace).unwrap();
    Supervisor {
        cube: ListingCube { path: workspace },
        tmux,
        store: MemoryStore::default(),
        host_id: "host".into(),
        homes_root: homes,
        boot_id: "boot".into(),
        brief_budget: Duration::ZERO,
    }
}

#[tokio::test]
async fn duplicate_spawn_is_rejected_while_the_session_lives() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    create_session(&tmux, "alpha", "sleep 3600").await;
    let supervisor = supervisor(root.path(), tmux.clone());
    seed(&supervisor.store, "alpha", Phase::Alive);
    let result = supervisor.execute(&spawn_command("alpha", "spawn-2"), false, 1).await;
    assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
    assert!(result.reason.contains("duplicate"), "{}", result.reason);
    assert!(
        tmux.list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|session| session.name == session_name("alpha"))
    );
}

#[tokio::test]
async fn interrupted_launch_without_a_session_is_uncertain() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    let supervisor = supervisor(root.path(), tmux.clone());
    seed(&supervisor.store, "ghost", Phase::Launching);
    let result = supervisor.execute(&spawn_command("ghost", "spawn-1"), true, 1).await;
    assert_eq!(result.code, ResultCode::OutcomeUncertain as i32, "{}", result.reason);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn restart_adopts_the_token_session() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    create_session(&tmux, "persist", "sleep 3600").await;
    let first = supervisor(root.path(), tmux.clone());
    seed(&first.store, "persist", Phase::Alive);
    let snapshot = first.store.load("run-persist", "persist").unwrap().unwrap();
    drop(first);
    let restarted = Supervisor {
        cube: ListingCube {
            path: root.path().join("worker"),
        },
        tmux: tmux.clone(),
        store: MemoryStore::default(),
        host_id: "host".into(),
        homes_root: root.path().join("homes"),
        boot_id: "boot".into(),
        brief_budget: Duration::ZERO,
    };
    restarted.store.save("run-persist", "persist", &snapshot).unwrap();
    let command = v1::Command {
        sequence: 1,
        command_id: "adopt-1".into(),
        run: Some(v1::Run {
            execution_id: "exec-persist".into(),
            run_id: "run-persist".into(),
            spawn_token: "persist".into(),
            assignment_generation: 1,
        }),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Adopt(v1::Empty {})),
    };
    let result = restarted.execute(&command, false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    let Some(command_result::Value::Pane(pane)) = result.value else {
        panic!("missing pane");
    };
    assert_eq!(pane.life, v1::PaneLife::Alive as i32);
    assert_eq!(pane.session, session_name("persist"));
    assert_eq!(
        tmux.display_message(&session_name("persist"), DisplayField::PaneDead)
            .await
            .unwrap(),
        "0"
    );
}

#[tokio::test]
async fn stop_is_token_verified_and_leaves_siblings() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    create_session(&tmux, "keep", "sleep 3600").await;
    create_session(&tmux, "drop", "sleep 3600").await;
    let supervisor = supervisor(root.path(), tmux.clone());
    seed(&supervisor.store, "keep", Phase::Alive);
    seed(&supervisor.store, "drop", Phase::Alive);
    let command = v1::Command {
        sequence: 1,
        command_id: "stop-drop".into(),
        run: Some(v1::Run {
            execution_id: "exec-drop".into(),
            run_id: "run-drop".into(),
            spawn_token: "drop".into(),
            assignment_generation: 1,
        }),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Stop(v1::Stop { grace_ms: 0 })),
    };
    let result = supervisor.execute(&command, false, 1).await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    let names: Vec<_> = tmux
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .map(|session| session.name)
        .collect();
    assert!(names.contains(&session_name("keep")), "{names:?}");
    assert!(!names.contains(&session_name("drop")), "{names:?}");
}

#[tokio::test]
async fn daemon_restart_adopts_the_live_session() {
    use std::os::unix::fs::PermissionsExt;
    let tmux_bin = declared_tmux_binary().expect("host tmux");
    let mut hub = hub::Fixture::new().await;
    let workspace = hub.dir.path().join("worker");
    std::fs::create_dir_all(&workspace).unwrap();
    let cube = hub.dir.path().join("cube-fixture");
    std::fs::write(
        &cube,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n*\"workspace list\"*) printf '%s\\n' '{{\"workspaces\":[{{\"repo\":\"repo\",\"workspace_id\":\"worker\",\"workspace_path\":\"{}\",\"state\":\"leased\",\"lease_id\":\"lease\"}}]}}' ;;\n*\"workspace heartbeat\"*) printf '%s\\n' '{{}}' ;;\n*) exit 9 ;;\nesac\n",
            workspace.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cube, std::fs::Permissions::from_mode(0o700)).unwrap();
    let daemon = hub.start_with_tmux(&cube, &tmux_bin);
    let connection = hub.connection().await;
    connection.welcome(0).await;
    drop(daemon);
    drop(connection);
    let _restarted = hub.start_with_tmux(&cube, &tmux_bin);
    let connection = hub.connection().await;
    assert!(
        connection
            .hello
            .features
            .iter()
            .any(|feature| feature.name == "execution"),
        "{:?}",
        connection.hello.features
    );
    connection.welcome(0).await;
}
