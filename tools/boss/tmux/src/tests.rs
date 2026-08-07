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
    async fn run(&self, program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none(), "tmux commands must not change the process directory");
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
async fn server_option_set_and_read_carry_no_session_target() {
    let (tmux, runner) = tmux([success(""), success("4242\n")]);
    tmux.set_server_option("@boss_engine_owner", "4242").await.unwrap();
    assert_eq!(
        tmux.show_server_option("@boss_engine_owner").await.unwrap(),
        Some("4242".to_owned())
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec!["-L", "boss", "set-option", "-s", "@boss_engine_owner", "4242"],
            vec!["-L", "boss", "show-options", "-s", "-v", "@boss_engine_owner"],
        ]
    );
}

#[tokio::test]
async fn server_option_read_distinguishes_absence() {
    let (tmux, _) = tmux([failure("invalid option: @missing")]);
    assert_eq!(tmux.show_server_option("@missing").await.unwrap(), None);
}

#[tokio::test(start_paused = true)]
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
                "--",
                "x".repeat(DEFAULT_SEND_CHUNK_BYTES).as_str()
            ],
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "-l", "--", "é"],
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "C-m"],
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn send_keys_marks_dash_prefixed_literal_input_as_an_argument() {
    let (tmux, runner) = tmux([success(""), success("")]);
    tmux.send_keys("boss-1", "-R dashdash").await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "-l", "--", "-R dashdash"],
            vec!["-L", "boss", "send-keys", "-t", "boss-1", "C-m"],
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn send_keys_never_sends_a_standalone_semicolon_chunk() {
    let text = format!("{};", "x".repeat(DEFAULT_SEND_CHUNK_BYTES));
    let (tmux, runner) = tmux([success(""), success(""), success("")]);
    tmux.send_keys("boss-1", &text).await.unwrap();
    let calls = runner.calls();
    assert_eq!(calls[0].last().unwrap(), &"x".repeat(DEFAULT_SEND_CHUNK_BYTES - 1));
    assert_eq!(calls[1].last().unwrap(), "x;");
    assert!(calls.iter().flatten().all(|argument| argument != ";"));
}

#[tokio::test(start_paused = true)]
async fn send_keys_escapes_a_standalone_semicolon() {
    let (tmux, runner) = tmux([success(""), success("")]);
    tmux.send_keys("boss-1", ";").await.unwrap();
    let calls = runner.calls();
    assert_eq!(calls[0].last().unwrap(), "\\;");
    assert!(calls.iter().flatten().all(|argument| argument != ";"));
}

#[test]
fn utf8_chunks_backtracks_to_a_character_boundary_before_a_trailing_semicolon() {
    let text = format!("{}é;", "x".repeat(DEFAULT_SEND_CHUNK_BYTES - 2));
    let chunks = utf8_chunks(&text, DEFAULT_SEND_CHUNK_BYTES);
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|chunk| *chunk != ";"));
}

#[tokio::test]
async fn new_session_validation_rejections_do_not_run_tmux() {
    let cases = [
        (
            NewSession {
                name: "boss-1".to_owned(),
                environment: Default::default(),
                working_directory: PathBuf::from("relative"),
                command: "exec codex".to_owned(),
            },
            "working directory must be absolute",
        ),
        (
            NewSession {
                name: String::new(),
                environment: Default::default(),
                working_directory: PathBuf::from("/workspace"),
                command: "exec codex".to_owned(),
            },
            "session name cannot be empty",
        ),
        (
            NewSession {
                name: "boss-1".to_owned(),
                environment: Default::default(),
                working_directory: PathBuf::from("/workspace"),
                command: String::new(),
            },
            "session command cannot be empty",
        ),
        (
            NewSession {
                name: "boss-1".to_owned(),
                environment: [("BAD=NAME".to_owned(), "value".to_owned())].into(),
                working_directory: PathBuf::from("/workspace"),
                command: "exec codex".to_owned(),
            },
            "environment name cannot contain '='",
        ),
    ];
    for (session, expected_error) in cases {
        let (tmux, runner) = tmux([]);
        let error = tmux.new_session(&session).await.unwrap_err();
        assert!(error.to_string().contains(expected_error), "error was: {error:#}");
        assert!(runner.calls().is_empty());
    }
}

#[tokio::test]
async fn command_failures_include_the_argv_and_stderr() {
    let (tmux, _) = tmux([failure("session not found: boss-1")]);
    let error = tmux.kill_session("boss-1").await.unwrap_err().to_string();
    assert!(error.contains("kill-session"), "error was: {error}");
    assert!(error.contains("session not found: boss-1"), "error was: {error}");
}

#[tokio::test]
async fn show_environment_rejects_unexpected_output() {
    let (tmux, _) = tmux([success("OTHER=value\n")]);
    let error = tmux.show_environment("boss-1", "NAME").await.unwrap_err().to_string();
    assert!(
        error.contains("unexpected tmux environment output"),
        "error was: {error}"
    );
}
