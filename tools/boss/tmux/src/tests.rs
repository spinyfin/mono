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
    stdin: Mutex<Vec<Vec<u8>>>,
}

impl StubRunner {
    fn replies(replies: impl IntoIterator<Item = CommandOutput>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(replies.into_iter().map(Ok).collect()),
            calls: Mutex::new(Vec::new()),
            stdin: Mutex::new(Vec::new()),
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

    fn stdin(&self) -> Vec<Vec<u8>> {
        self.stdin.lock().unwrap().clone()
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

    async fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none(), "tmux commands must not change the process directory");
        self.calls.lock().unwrap().push((
            program.to_path_buf(),
            args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect(),
        ));
        self.stdin.lock().unwrap().push(stdin.to_vec());
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
    let tmux = Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), TEST_SOCKET_PATH).unwrap();
    (tmux, runner)
}

#[test]
fn relative_executable_path_is_rejected() {
    let error = Tmux::with_runner_and_socket("tmux", StubRunner::replies([]), TEST_SOCKET_PATH).unwrap_err();
    assert!(error.to_string().contains("absolute"));
}

#[tokio::test]
async fn explicit_socket_path_scopes_commands_without_tmp_label_resolution() {
    let runner = StubRunner::replies([success("tmux 3.6\n")]);
    let tmux = Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), TEST_SOCKET_PATH).unwrap();

    tmux.version().await.unwrap();

    assert_eq!(runner.calls(), vec![vec!["-S", TEST_SOCKET_PATH, "-V"]]);
}

#[tokio::test]
async fn legacy_label_server_still_emits_the_pre_move_argv() {
    let runner = StubRunner::replies([success("tmux 3.6\n")]);
    let tmux = Tmux::for_legacy_label_server_with_runner("/opt/homebrew/bin/tmux", runner.clone()).unwrap();
    tmux.version().await.unwrap();
    assert_eq!(runner.calls(), vec![vec!["-L", "boss", "-V"]]);
    assert_eq!(tmux.socket_path(), None);
    assert_eq!(tmux.server_identity(), SERVER_LABEL);
}

#[test]
fn socket_handle_exposes_the_path_as_server_identity() {
    let tmux =
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", StubRunner::replies([]), TEST_SOCKET_PATH).unwrap();
    assert_eq!(tmux.socket_path(), Some(Path::new(TEST_SOCKET_PATH)));
    assert_eq!(tmux.server_identity(), TEST_SOCKET_PATH);
    assert_eq!(tmux.operator_prefix(), format!("tmux -S {TEST_SOCKET_PATH}"));
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
    assert_eq!(runner.calls(), vec![vec!["-S", TEST_SOCKET_PATH, "-V"]]);
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
            "-S",
            TEST_SOCKET_PATH,
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

#[test]
fn attach_session_command_uses_resolved_program_and_omits_exec() {
    let (tmux, _runner) = tmux([]);
    let command = tmux.attach_session_command("boss-worker-3");
    assert_eq!(
        command,
        format!("{} -L boss attach-session -t boss-worker-3", tmux.program().display())
    );
    assert!(
        !command.starts_with("exec "),
        "operator paste must not replace the shell: {command}"
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
            "-S",
            TEST_SOCKET_PATH,
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

/// A host reboot clears `/tmp`, so the private socket is absent rather than
/// stale and tmux reports a `connect(2)` ENOENT instead of "no server
/// running". Every coordinator recovery path funnels through
/// `list_sessions`, so this must read as an empty inventory — an `Err` here
/// strands the coordinator pane blank until the socket reappears.
#[tokio::test]
async fn a_missing_private_socket_is_an_empty_session_inventory() {
    let (tmux, _) = tmux([failure(
        "error connecting to /private/tmp/tmux-501/boss (No such file or directory)",
    )]);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

/// The inverse guard: a socket that exists but refuses us is not evidence
/// that no sessions exist, so it must stay a hard error.
#[tokio::test]
async fn an_unreadable_private_socket_is_still_an_error() {
    let (tmux, _) = tmux([failure(
        "error connecting to /private/tmp/tmux-501/boss (Permission denied)",
    )]);
    assert!(tmux.list_sessions().await.is_err());
}

#[tokio::test]
async fn kill_session_verified_treats_a_missing_private_socket_as_already_torn_down() {
    let (tmux, runner) = tmux([failure(
        "error connecting to /private/tmp/tmux-501/boss (No such file or directory)",
    )]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Absent);
    assert_eq!(
        runner.calls().len(),
        1,
        "a missing socket must never issue kill-session",
    );
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
async fn set_option_capture_and_display_use_the_private_server() {
    let (tmux, runner) = tmux([
        success(""),
        success("pane text\n"),
        success("1234\n"),
        success("claude\n"),
    ]);
    tmux.set_option("boss-1", "@boss_spawn_token", "token").await.unwrap();
    assert_eq!(tmux.capture_pane("boss-1").await.unwrap(), "pane text\n");
    assert_eq!(
        tmux.display_message("boss-1", DisplayField::PanePid).await.unwrap(),
        "1234"
    );
    assert_eq!(
        tmux.display_message("boss-1", DisplayField::PaneCurrentCommand)
            .await
            .unwrap(),
        "claude"
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "set-option",
                "-t",
                "boss-1",
                "@boss_spawn_token",
                "token"
            ],
            vec!["-S", TEST_SOCKET_PATH, "capture-pane", "-p", "-t", "boss-1"],
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1",
                "#{pane_pid}"
            ],
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1",
                "#{pane_current_command}",
            ],
        ]
    );
}

#[tokio::test]
async fn kill_session_verified_kills_only_on_an_exact_token_match() {
    let (tmux, runner) = tmux([success("BOSS_SPAWN_TOKEN=secret\n"), success("")]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Killed);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1",
                "BOSS_SPAWN_TOKEN"
            ],
            vec!["-S", TEST_SOCKET_PATH, "kill-session", "-t", "boss-1"],
        ]
    );
}

#[tokio::test]
async fn kill_session_verified_treats_a_session_destroyed_just_before_the_kill_as_absent() {
    // The token read matches, but the session dies on its own (the
    // ordinary worker-completion race) before the kill-session call
    // lands — tmux reports it the same way it would report a session
    // that was already gone at the token read. That must still resolve
    // to Absent, not surface as a hard Tmux error.
    let (tmux, runner) = tmux([
        success("BOSS_SPAWN_TOKEN=secret\n"),
        failure("can't find session: boss-1"),
    ]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Absent);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1",
                "BOSS_SPAWN_TOKEN"
            ],
            vec!["-S", TEST_SOCKET_PATH, "kill-session", "-t", "boss-1"],
        ],
    );
}

#[tokio::test]
async fn kill_session_verified_refuses_a_token_mismatch_and_never_kills() {
    let (tmux, runner) = tmux([success("BOSS_SPAWN_TOKEN=someone-elses-token\n")]);
    let error = tmux.kill_session_verified("boss-1", "secret").await.unwrap_err();
    match error {
        KillSessionError::TokenMismatch {
            session,
            expected,
            actual,
        } => {
            assert_eq!(session, "boss-1");
            assert_eq!(expected, "secret");
            assert_eq!(actual, "someone-elses-token");
        }
        other => panic!("expected TokenMismatch, got {other:?}"),
    }
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1",
            "BOSS_SPAWN_TOKEN"
        ]],
        "a mismatch must never issue kill-session",
    );
}

#[tokio::test]
async fn kill_session_verified_treats_an_absent_session_as_already_torn_down() {
    let (tmux, runner) = tmux([failure("unknown variable: BOSS_SPAWN_TOKEN")]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Absent);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1",
            "BOSS_SPAWN_TOKEN"
        ]],
        "an absent session must never issue kill-session",
    );
}

#[tokio::test]
async fn kill_session_verified_treats_a_missing_session_as_already_torn_down() {
    let (tmux, runner) = tmux([failure("can't find session: boss-1")]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Absent);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1",
            "BOSS_SPAWN_TOKEN"
        ]],
        "a missing session must never issue kill-session",
    );
}

#[tokio::test]
async fn kill_session_verified_treats_a_dead_server_as_already_torn_down() {
    let (tmux, runner) = tmux([failure("no server running on /tmp/tmux-501/boss")]);
    let outcome = tmux.kill_session_verified("boss-1", "secret").await.unwrap();
    assert_eq!(outcome, KillSessionOutcome::Absent);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1",
            "BOSS_SPAWN_TOKEN"
        ]],
        "a dead server must never issue kill-session",
    );
}

#[tokio::test]
async fn standard_tmux_options_are_supported() {
    let (tmux, runner) = tmux([success("")]);
    tmux.set_option("boss-1", "remain-on-exit", "on").await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            TEST_SOCKET_PATH,
            "set-option",
            "-t",
            "boss-1",
            "remain-on-exit",
            "on"
        ]]
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
            vec!["-S", TEST_SOCKET_PATH, "set-option", "-s", "@boss_engine_owner", "4242"],
            vec!["-S", TEST_SOCKET_PATH, "show-options", "-s", "-v", "@boss_engine_owner"],
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
                "-S",
                TEST_SOCKET_PATH,
                "send-keys",
                "-t",
                "boss-1",
                "-l",
                "--",
                "x".repeat(DEFAULT_SEND_CHUNK_BYTES).as_str()
            ],
            vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "-l", "--", "é"],
            vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "C-m"],
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
            vec![
                "-S",
                TEST_SOCKET_PATH,
                "send-keys",
                "-t",
                "boss-1",
                "-l",
                "--",
                "-R dashdash"
            ],
            vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "C-m"],
        ]
    );
}

#[tokio::test]
async fn send_keys_pastes_multiline_text_then_submits_once() {
    let (tmux, runner) = tmux([success(""), success(""), success("")]);
    tmux.send_keys("boss-1", "first\nsecond\n").await.unwrap();
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0][..3], ["-S", TEST_SOCKET_PATH, "load-buffer"]);
    assert_eq!(calls[0][3], "-b");
    let buffer_name = calls[0][4].clone();
    assert!(
        buffer_name.starts_with("boss-deliver-boss-1-"),
        "unexpected buffer name: {buffer_name}"
    );
    assert_eq!(calls[0][5], "-");
    assert_eq!(
        calls[1],
        vec![
            "-S",
            TEST_SOCKET_PATH,
            "paste-buffer",
            "-b",
            buffer_name.as_str(),
            "-p",
            "-d",
            "-t",
            "boss-1",
        ]
    );
    assert_eq!(
        calls[2],
        vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "C-m"]
    );
    assert_eq!(runner.stdin(), vec![b"first\nsecond".to_vec()]);
}

#[tokio::test(start_paused = true)]
async fn send_keys_strips_trailing_newlines_on_single_line_path() {
    let (tmux, runner) = tmux([success(""), success("")]);
    tmux.send_keys("boss-1", "hello\r\n").await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![
            vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "-l", "--", "hello"],
            vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "C-m"],
        ]
    );
}

#[tokio::test]
async fn send_key_uses_a_single_named_keypress_without_return() {
    let (tmux, runner) = tmux([success("")]);
    tmux.send_key("boss-1", "Escape").await.unwrap();
    assert_eq!(
        runner.calls(),
        vec![vec!["-S", TEST_SOCKET_PATH, "send-keys", "-t", "boss-1", "Escape"]]
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
    let error = tmux.capture_pane("boss-1").await.unwrap_err().to_string();
    assert!(error.contains("capture-pane"), "error was: {error}");
    assert!(error.contains("session not found: boss-1"), "error was: {error}");
}

#[tokio::test]
async fn kill_session_verified_surfaces_a_kill_session_failure_after_a_matched_token() {
    // A genuine kill-session failure — not one of the absent-session stderr
    // shapes `is_absent_session_stderr` recognizes — must still surface as
    // a hard error rather than being swallowed as Absent.
    let (tmux, _) = tmux([
        success("BOSS_SPAWN_TOKEN=secret\n"),
        failure("server exited unexpectedly"),
    ]);
    let error = tmux.kill_session_verified("boss-1", "secret").await.unwrap_err();
    let KillSessionError::Tmux(err) = error else {
        panic!("expected KillSessionError::Tmux, got {error:?}");
    };
    let text = err.to_string();
    assert!(text.contains("kill-session"), "error was: {text}");
    assert!(text.contains("server exited unexpectedly"), "error was: {text}");
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

/// A row sanitized by tmux (no UTF-8 locale on the client) is otherwise
/// indistinguishable from a session name containing an underscore, so the
/// error must name the locale cause rather than just echoing the row.
#[tokio::test]
async fn a_sanitized_session_row_error_names_the_locale_cause() {
    let (tmux, _) = tmux([success("boss-coordinator_deadbeef\n")]);
    let error = format!("{:#}", tmux.list_sessions().await.unwrap_err());
    assert!(error.contains("utf8_sanitize"), "{error}");
    assert!(error.contains("LC_CTYPE"), "{error}");
}

/// The generic message is still used when the row is malformed for some
/// reason other than sanitization.
#[tokio::test]
async fn an_unparseable_row_without_underscores_keeps_the_plain_error() {
    let (tmux, _) = tmux([success("boss-coordinator\n")]);
    let error = format!("{:#}", tmux.list_sessions().await.unwrap_err());
    assert!(error.contains("unexpected tmux list-sessions row"), "{error}");
    assert!(!error.contains("utf8_sanitize"), "{error}");
}

#[tokio::test]
async fn list_sessions_treats_connecting_enoent_as_an_absent_server() {
    let (tmux, _) = tmux([failure(
        "error connecting to /state/boss/tmux.sock (No such file or directory)",
    )]);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_sessions_treats_connecting_refused_as_an_absent_server() {
    let (tmux, _) = tmux([failure(
        "error connecting to /state/boss/tmux.sock (Connection refused)",
    )]);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_sessions_still_fails_on_a_generic_connecting_error() {
    let (tmux, _) = tmux([failure("error connecting to /tmp/tmux-0/default (boss-1)")]);
    let error = tmux.list_sessions().await.unwrap_err().to_string();
    assert!(error.contains("error connecting to"), "error was: {error}");
}

#[test]
fn unlink_stale_socket_removes_a_dead_unix_socket() {
    let path = std::env::temp_dir().join(format!("boss-tmux-unlink-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(listener);
    assert!(path.exists());
    let tmux = Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", StubRunner::replies([]), &path).unwrap();
    assert!(tmux.unlink_stale_socket_file().unwrap());
    assert!(!path.exists());
}

#[test]
fn unlink_stale_socket_leaves_a_regular_file_alone() {
    let path = std::env::temp_dir().join(format!("boss-tmux-unlink-regular-{}", std::process::id()));
    std::fs::write(&path, b"not a socket").unwrap();
    let tmux = Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", StubRunner::replies([]), &path).unwrap();
    assert!(!tmux.unlink_stale_socket_file().unwrap());
    assert!(path.exists());
    std::fs::remove_file(&path).unwrap();
}
