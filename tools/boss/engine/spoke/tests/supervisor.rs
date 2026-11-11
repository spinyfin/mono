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
    let record = ExecutionRecord::builder()
        .execution_id(format!("exec-{token}"))
        .run_id(format!("run-{token}"))
        .spawn_token(token)
        .assignment_generation(1)
        .lease_id("lease")
        .workspace_id("worker")
        .session(session_name(token))
        .phase(phase)
        .boot_id("boot")
        .driver_id("claude")
        .build();
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
    let homes = homes::pin(&homes::process_root()).unwrap();
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
        homes_root: homes::process_root(),
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

/// Executable fixture standing in for the `claude` CLI. The pane's `PATH` comes
/// from the hermetic test environment and ignores the session environment, so
/// the fixture lives in the private home's `.nix-profile/bin`, which that
/// `PATH` starts with. `FIXTURE_MODE=exit` makes it quit without reading the
/// brief; otherwise it satisfies the relay's readiness markers, shows the
/// paste placeholder once it has read a whole paste (the relay sends Return
/// only then), and appends every byte it receives to `$FIXTURE_OUT`.
const FIXTURE_CLAUDE: &str = r#"#!/bin/sh
[ "$FIXTURE_MODE" = exit ] && exit 0
exec python3 -c '
import os, tty
tty.setraw(0)
os.write(1, b"\x1b[?2004hClaude Code")
tail = b""
while True:
    data = os.read(0, 4096)
    if not data:
        break
    with open(os.environ["FIXTURE_OUT"], "ab") as out:
        out.write(data)
    tail = (tail + data)[-4096:]
    if b"\x1b[201~" in tail:
        tail = b""
        os.write(1, b"\r\n[Pasted text #1 +1 lines]")
'
"#;

fn install_fixture_claude() {
    use std::os::unix::fs::PermissionsExt;
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let bin = PathBuf::from(std::env::var("HOME").expect("test HOME")).join(".nix-profile/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tmp = bin.join(".claude.tmp");
        std::fs::write(&tmp, FIXTURE_CLAUDE).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, bin.join("claude")).unwrap();
    });
}

fn real_spawn(token: &str, mode: &str, out: &Path) -> v1::Command {
    install_fixture_claude();
    let mut command = spawn_command(token, "spawn-real");
    if let Some(command::Body::Spawn(spawn)) = &mut command.body {
        spawn.env = vec![
            v1::Env {
                name: "FIXTURE_MODE".into(),
                value: mode.into(),
            },
            v1::Env {
                name: "FIXTURE_OUT".into(),
                value: out.display().to_string(),
            },
        ];
    }
    command
}

fn run_of(token: &str) -> v1::Run {
    v1::Run {
        execution_id: format!("exec-{token}"),
        run_id: format!("run-{token}"),
        spawn_token: token.into(),
        assignment_generation: 1,
    }
}

fn command_of(token: &str, id: &str, body: command::Body) -> v1::Command {
    v1::Command {
        sequence: 1,
        command_id: id.into(),
        run: Some(run_of(token)),
        not_after_unix_ms: i64::MAX,
        body: Some(body),
    }
}

fn pane_of(result: &v1::CommandResult) -> v1::PaneState {
    let Some(command_result::Value::Pane(pane)) = &result.value else {
        panic!("missing pane in {result:?}");
    };
    pane.clone()
}

async fn wait_for_file(path: &Path, wanted: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    for _ in 0..200 {
        let bytes = std::fs::read(path).unwrap_or_default();
        if wanted(&bytes) {
            return bytes;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "condition never met; file holds {:?}",
        std::fs::read(path).unwrap_or_default()
    );
}

#[tokio::test]
async fn real_spawn_reaches_alive_without_waiting_out_the_brief_budget_and_delivers_input() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    let out = root.path().join("received.bin");
    let mut supervisor = supervisor(root.path(), tmux.clone());
    supervisor.brief_budget = Duration::from_secs(30);
    let started = std::time::Instant::now();
    let result = supervisor
        .execute(&real_spawn("real-alive", "echo", &out), false, 1)
        .await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "spawn waited out the delivery budget: {:?}",
        started.elapsed()
    );
    let pane = pane_of(&result);
    assert_eq!(
        pane.life,
        v1::PaneLife::Alive as i32,
        "pane output: {}",
        tmux.capture_pane(&session_name("real-alive")).await.unwrap_or_default()
    );
    assert_eq!(pane.session, session_name("real-alive"));
    assert_eq!(
        tmux.show_environment(&session_name("real-alive"), TMUX_SPAWN_TOKEN_ENV)
            .await
            .unwrap()
            .as_deref(),
        Some("real-alive")
    );
    // The brief itself reached the driver through the relay.
    let received = wait_for_file(&out, |bytes| bytes.windows(12).any(|w| w == b"do the thing")).await;
    let mark = received.len();

    // Single-line and multi-line input go to the exact session.
    for (id, text) in [("d-one", "ping"), ("d-two", "first\nsecond")] {
        let deliver = command_of(
            "real-alive",
            id,
            command::Body::Deliver(v1::Deliver {
                kind: v1::DeliveryKind::Nudge as i32,
                input_utf8: text.as_bytes().to_vec(),
            }),
        );
        let delivered = supervisor.execute(&deliver, false, 1).await;
        assert_eq!(delivered.code, ResultCode::Ok as i32, "{}", delivered.reason);
    }
    // Wait for Deliver's trailing Return after "second" before taking the
    // Interrupt snapshot; otherwise the Return can land in the tail and
    // fail a strict `\x1b` equality. Multi-line Deliver uses paste-buffer -p,
    // so the fixture may see `second\x1b[201~\r` rather than contiguous
    // `second\r`.
    let received = wait_for_file(&out, |bytes| {
        if !bytes.windows(4).any(|w| w == b"ping") {
            return false;
        }
        let Some(pos) = bytes.windows(6).position(|w| w == b"second") else {
            return false;
        };
        bytes[pos + 6..].contains(&b'\r')
    })
    .await;

    // Interrupt is the driver's Escape, with no Return after it.
    let mark = mark.max(received.len());
    let interrupt = command_of(
        "real-alive",
        "i-one",
        command::Body::Deliver(v1::Deliver {
            kind: v1::DeliveryKind::Interrupt as i32,
            input_utf8: Vec::new(),
        }),
    );
    let interrupted = supervisor.execute(&interrupt, false, 1).await;
    assert_eq!(interrupted.code, ResultCode::Ok as i32, "{}", interrupted.reason);
    let after = wait_for_file(&out, |bytes| bytes[mark..].contains(&b'\x1b')).await;
    let tail = &after[mark..];
    let escape = tail.iter().position(|b| *b == b'\x1b').expect("interrupt Escape");
    assert_eq!(
        tail.iter().filter(|b| **b == b'\x1b').count(),
        1,
        "interrupt must be one Escape, got {tail:?}"
    );
    assert!(
        !tail[escape + 1..].contains(&b'\r'),
        "no Return may follow the interrupt Escape, got {tail:?}"
    );

    // Stop removes the run's input home and the settings file with it.
    let home = {
        let record = supervisor.store.load("run-real-alive", "real-alive").unwrap().unwrap();
        serde_json::from_slice::<ExecutionRecord>(&record).unwrap().home
    }
    .expect("run input home recorded");
    assert!(home.exists());
    let stopped = supervisor
        .execute(
            &command_of("real-alive", "stop-real", command::Body::Stop(v1::Stop { grace_ms: 0 })),
            false,
            1,
        )
        .await;
    assert_eq!(stopped.code, ResultCode::Ok as i32, "{}", stopped.reason);
    assert!(!home.exists(), "input home survived Stop");
}

#[tokio::test]
async fn real_spawn_whose_relay_exits_64_is_brief_not_delivered() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    let out = root.path().join("received.bin");
    let mut supervisor = supervisor(root.path(), tmux);
    supervisor.brief_budget = Duration::from_secs(30);
    let result = supervisor
        .execute(&real_spawn("real-undelivered", "exit", &out), false, 1)
        .await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    let pane = pane_of(&result);
    assert_eq!(pane.life, v1::PaneLife::BriefNotDelivered as i32, "{pane:?}");
    assert_eq!(pane.exit_code, Some(64));
    let record: ExecutionRecord = serde_json::from_slice(
        &supervisor
            .store
            .load("run-real-undelivered", "real-undelivered")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record.phase, Phase::Terminal);
    assert!(record.reclaimed);
}

#[tokio::test]
async fn a_worker_that_exits_after_spawn_returned_is_settled_by_the_sweep() {
    let root = tempfile::tempdir().unwrap();
    let (tmux, _guard) = live_tmux(root.path()).await;
    create_session(&tmux, "later", "sleep 1").await;
    let supervisor = supervisor(root.path(), tmux);
    seed(&supervisor.store, "later", Phase::Alive);
    assert!(supervisor.sweep().await.is_empty());
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        supervisor.sweep().await;
        let record: ExecutionRecord =
            serde_json::from_slice(&supervisor.store.load("run-later", "later").unwrap().unwrap()).unwrap();
        if record.phase == Phase::Terminal {
            assert!(record.terminal.is_some());
            return;
        }
    }
    panic!("the finished worker was never settled");
}

/// Drives one command over the hub connection and returns its result.
async fn run_over_hub(connection: &mut hub::Connection, command: v1::Command) -> v1::CommandResult {
    use boss_protocol_spoke::v1::{hub_frame, record, spoke_frame};
    let id = command.command_id.clone();
    connection.send(hub_frame::Body::Command(command)).await;
    loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            connection.receipt(record.sequence).await;
            if let Some(record::Body::Result(result)) = record.body
                && result.command_id == id
            {
                return result;
            }
        }
    }
}

#[tokio::test]
async fn daemon_restart_adopts_the_same_live_session_without_a_second_launch() {
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

    struct LogOnPanic(PathBuf);
    impl Drop for LogOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("DAEMON LOG: {:?}", std::fs::read_to_string(&self.0));
            }
        }
    }
    let _log = LogOnPanic(hub.dir.path().join("daemon.log"));
    // A live token session on the daemon's tmux server, and the durable
    // journal record a previous daemon wrote for it.
    let state = hub.root();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tmux = Tmux::from_path_with_socket(&tmux_bin, state.join("tmux.sock")).unwrap();
    let _guard = TmuxServerGuard::new(tmux_bin.clone(), state.join("tmux.sock"));
    tmux.start_private_server().await.expect("start tmux server");
    create_session(&tmux, "survivor", "sleep 3600").await;
    let pane_id = tmux
        .display_message(&session_name("survivor"), DisplayField::PaneId)
        .await
        .unwrap();
    let pane_pid = tmux
        .display_message(&session_name("survivor"), DisplayField::PanePid)
        .await
        .unwrap();
    let record = ExecutionRecord::builder()
        .execution_id("exec-survivor")
        .run_id("run-survivor")
        .spawn_token("survivor")
        .assignment_generation(1)
        .lease_id("lease")
        .workspace_id("worker")
        .session(session_name("survivor"))
        .phase(Phase::Alive)
        .boot_id("old-boot")
        .pane_id(pane_id.clone())
        .driver_id("claude")
        .build();
    {
        let mut journal =
            boss_spoke_journal::Journal::open(&state, "host", boss_spoke_journal::Limits::default()).unwrap();
        journal
            .save_execution("run-survivor", "survivor", &serde_json::to_vec(&record).unwrap())
            .unwrap();
    }

    for (sequence, (generation, command_id)) in [("first", "adopt-first"), ("restarted", "adopt-restarted")]
        .into_iter()
        .enumerate()
    {
        let daemon = hub.start_with_tmux(&cube, &tmux_bin);
        let mut connection = hub.connection().await;
        connection.welcome(0).await;
        let mut adopt = command_of("survivor", command_id, command::Body::Adopt(v1::Empty {}));
        adopt.sequence = sequence as u64 + 1;
        let result = run_over_hub(&mut connection, adopt).await;
        assert_eq!(result.code, ResultCode::Ok as i32, "{generation}: {}", result.reason);
        let pane = pane_of(&result);
        assert_eq!(pane.life, v1::PaneLife::Alive as i32, "{generation}: {pane:?}");
        assert_eq!(pane.pane_id, pane_id, "{generation}");
        assert_eq!(pane.session, session_name("survivor"), "{generation}");
        assert!(
            connection
                .hello
                .features
                .iter()
                .any(|feature| feature.name == "execution"),
            "{:?}",
            connection.hello.features
        );
        drop(connection);
        drop(daemon);
    }

    // Same session, same token, same pane process: nothing was launched again.
    let sessions = tmux.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(
        tmux.show_environment(&session_name("survivor"), TMUX_SPAWN_TOKEN_ENV)
            .await
            .unwrap()
            .as_deref(),
        Some("survivor")
    );
    assert_eq!(
        tmux.display_message(&session_name("survivor"), DisplayField::PanePid)
            .await
            .unwrap(),
        pane_pid
    );
}
