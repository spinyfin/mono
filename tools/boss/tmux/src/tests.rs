use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;

#[derive(Default)]
struct StubRunner {
    outcomes: Mutex<VecDeque<std::io::Result<CommandOutput>>>,
    calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
}

impl StubRunner {
    fn replies(replies: impl IntoIterator<Item = CommandOutput>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(replies.into_iter().map(Ok).collect()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, args)| args.clone())
            .collect()
    }
}

#[async_trait]
impl CommandRunner for StubRunner {
    async fn run(&self, program: &Path, args: &[OsString]) -> std::io::Result<CommandOutput> {
        self.calls.lock().unwrap().push((
            program.to_path_buf(),
            args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect(),
        ));
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("stub runner received an unexpected tmux command")
    }
}

fn success(stdout: &str) -> CommandOutput {
    CommandOutput {
        success: true,
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn failure(stderr: &str) -> CommandOutput {
    CommandOutput {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

fn tmux(replies: impl IntoIterator<Item = CommandOutput>) -> (Tmux, Arc<StubRunner>) {
    let runner = StubRunner::replies(replies);
    let tmux = Tmux::with_runner("/opt/homebrew/bin/tmux", runner.clone()).unwrap();
    (tmux, runner)
}

#[test]
fn relative_executable_path_is_rejected() {
    let error = Tmux::with_runner("tmux", StubRunner::replies([])).unwrap_err();
    assert!(error.to_string().contains("absolute"));
}

#[test]
fn version_parser_accepts_letter_suffixes_and_enforces_floor() {
    assert_eq!(
        TmuxVersion::parse("tmux 3.6a\n").unwrap(),
        TmuxVersion { major: 3, minor: 6 }
    );
    assert!(TmuxVersion::parse("tmux 3.2\n").unwrap().supports_session_environment());
    assert!(
        !TmuxVersion::parse("tmux 3.1c\n")
            .unwrap()
            .supports_session_environment()
    );
    assert!(TmuxVersion::parse("3.6a").is_err());
}

#[tokio::test]
async fn version_uses_resolved_program() {
    let (tmux, runner) = tmux([success("tmux 3.6a\n")]);
    assert_eq!(tmux.version().await.unwrap(), TmuxVersion { major: 3, minor: 6 });
    assert_eq!(runner.calls(), vec![vec!["-L", "boss", "-V"]]);
}

#[tokio::test]
async fn new_session_is_detached_private_and_carries_environment_atomically() {
    let (tmux, runner) = tmux([success("")]);
    let session = NewSession {
        name: "boss-6-example".to_owned(),
        environment: [("BOSS_SPAWN_TOKEN".to_owned(), "secret".to_owned())].into(),
        working_directory: PathBuf::from("/workspace/lease"),
        command: "exec codex".to_owned(),
    };
    tmux.new_session(&session).await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-L",
            "boss",
            "new-session",
            "-d",
            "-s",
            "boss-6-example",
            "-e",
            "BOSS_SPAWN_TOKEN=secret",
            "-c",
            "/workspace/lease",
            "exec codex"
        ]]
    );
}

#[tokio::test]
async fn list_sessions_parses_the_token_mirror() {
    let (tmux, runner) = tmux([success("boss-1-a\ttoken-a\nboss-2-b\t\n")]);
    assert_eq!(
        tmux.list_sessions().await.unwrap(),
        vec![
            Session {
                name: "boss-1-a".to_owned(),
                spawn_token: Some("token-a".to_owned())
            },
            Session {
                name: "boss-2-b".to_owned(),
                spawn_token: None
            },
        ]
    );
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-L",
            "boss",
            "list-sessions",
            "-F",
            "#{session_name}\t#{@boss_spawn_token}"
        ]]
    );
}

#[tokio::test]
async fn no_private_server_is_an_empty_session_inventory() {
    let (tmux, _) = tmux([failure("no server running on /private/tmp/tmux-0/boss")]);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn environment_and_option_reads_distinguish_absence() {
    let (tmux, _) = tmux([
        success("BOSS_SPAWN_TOKEN=secret\n"),
        failure("unknown variable: BOSS_RUN_ID"),
        success("token\n"),
        failure("invalid option: @missing"),
    ]);
    assert_eq!(
        tmux.show_environment("boss-1", "BOSS_SPAWN_TOKEN").await.unwrap(),
        Some("secret".to_owned())
    );
    assert_eq!(tmux.show_environment("boss-1", "BOSS_RUN_ID").await.unwrap(), None);
    assert_eq!(
        tmux.show_option("boss-1", "@boss_spawn_token").await.unwrap(),
        Some("token".to_owned())
    );
    assert_eq!(tmux.show_option("boss-1", "@missing").await.unwrap(), None);
}

#[tokio::test]
async fn set_option_capture_kill_and_display_use_the_private_server() {
    let (tmux, runner) = tmux([success(""), success("pane text\n"), success(""), success("1234\n")]);
    tmux.set_option("boss-1", "@boss_spawn_token", "token").await.unwrap();
    assert_eq!(tmux.capture_pane("boss-1").await.unwrap(), "pane text\n");
    tmux.kill_session("boss-1").await.unwrap();
    assert_eq!(
        tmux.display_message("boss-1", DisplayField::PanePid).await.unwrap(),
        "1234"
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec!["-L", "boss", "set-option", "-t", "boss-1", "@boss_spawn_token", "token"],
            vec!["-L", "boss", "capture-pane", "-p", "-t", "boss-1"],
            vec!["-L", "boss", "kill-session", "-t", "boss-1"],
            vec!["-L", "boss", "display-message", "-p", "-t", "boss-1", "#{pane_pid}"],
        ]
    );
}

#[tokio::test]
async fn standard_tmux_options_are_supported() {
    let (tmux, runner) = tmux([success("")]);
    tmux.set_option("boss-1", "remain-on-exit", "on").await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![vec!["-L", "boss", "set-option", "-t", "boss-1", "remain-on-exit", "on"]]
    );
}

#[tokio::test]
async fn send_keys_chunks_utf8_then_submits_in_a_separate_command() {
    let text = format!("{}é", "x".repeat(DEFAULT_SEND_CHUNK_BYTES));
    let (tmux, runner) = tmux([success(""), success(""), success("")]);
    tmux.send_keys("boss-1", &text).await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-L",
                "boss",
                "send-keys",
                "-t",
                "boss-1",
                "-l",
                "x".repeat(DEFAULT_SEND_CHUNK_BYTES).as_str()
            ],
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "-l", "é"],
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "C-m"],
        ]
    );
}
