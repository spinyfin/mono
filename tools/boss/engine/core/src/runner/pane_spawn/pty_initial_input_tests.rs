//! Tests for the MAX_CANON fix: the guard (`check_initial_input_length`),
//! the file-indirection helper (`write_initial_input_script`), and — driven
//! through each real driver's own `spawn_invocation` — proof that the typed
//! `initial_input` line stays under the canonical-mode limit even in each
//! driver's worst realistic case (a long workspace path, and for Grok the
//! full structural `--deny` rule set).

use super::{
    ExecArgLimits, MAX_CANON_LINE_BYTES, check_arg_max_budget, check_initial_input_length,
    check_launch_command_arg_max, check_launch_command_arg_max_for_bytes, estimated_launch_argv_bytes, local_arg_max,
    path_prepend_clause, render_env_directive, write_initial_input_script,
};
use crate::driver::{
    AgentDriver, ClaudeDriver, CodexDriver, EnvDirective, GrokDriver, PermissionInput, SpawnRequest, WorkerKind,
    apply_permission_extra_args, codex::codex_sandbox_extra_args,
};
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn short_line_passes_the_guard() {
    check_initial_input_length(". .boss/initial-input.sh\n", "claude").unwrap();
}

#[test]
fn line_exactly_at_the_cap_passes() {
    let line = format!("{}\n", "a".repeat(MAX_CANON_LINE_BYTES - 1));
    assert_eq!(line.len(), MAX_CANON_LINE_BYTES);
    check_initial_input_length(&line, "claude").unwrap();
}

#[test]
fn line_one_byte_over_the_cap_fails_loudly_naming_bytes_and_driver() {
    let line = format!("{}\n", "a".repeat(MAX_CANON_LINE_BYTES));
    assert_eq!(line.len(), MAX_CANON_LINE_BYTES + 1);
    let err = check_initial_input_length(&line, "grok").expect_err("must fail, not silently proceed");
    let msg = err.to_string();
    assert!(msg.contains("grok"), "error must name the driver: {msg}");
    assert!(
        msg.contains(&(MAX_CANON_LINE_BYTES + 1).to_string()),
        "error must name the byte count: {msg}",
    );
}

#[test]
fn write_initial_input_script_returns_a_short_fixed_line_regardless_of_script_size() {
    let workspace = TempDir::new().unwrap();
    let huge_script = format!("export FOO=bar; {}\n", "x".repeat(5000));
    let line = write_initial_input_script(workspace.path(), &huge_script).unwrap();
    assert_eq!(line, ". .boss/initial-input.sh\n");
    check_initial_input_length(&line, "grok").unwrap();

    let written = std::fs::read_to_string(workspace.path().join(".boss").join("initial-input.sh")).unwrap();
    assert_eq!(written, huge_script);
}

/// Assemble exactly what `run_execution` assembles from a `SpawnPlan`,
/// then write it through the same file-indirection + guard path. Shared
/// by every driver-specific "stays under the limit" test below.
fn assembled_initial_input(workspace_path: &std::path::Path, env: &[EnvDirective], command: &str) -> String {
    let env_prefix: String = env.iter().map(render_env_directive).collect();
    let assembled = format!(
        "{}{}{env_prefix}{}",
        path_prepend_clause("BOSS_BIN_DIR"),
        path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
        command,
    );
    write_initial_input_script(workspace_path, &assembled).unwrap()
}

#[test]
fn claude_initial_input_stays_under_the_limit_with_a_long_settings_path() {
    // Claude's own permission mechanism is a single `--settings <file>`
    // JSON file, not CLI extra_args — a long settings path (worker
    // settings live under the per-user system temp dir, keyed by
    // workspace name) is this driver's main lever on command length.
    let long_settings_path = PathBuf::from("/")
        .join("a".repeat(80))
        .join("b".repeat(80))
        .join("claude-worker-settings.json");
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: "claude-sonnet-4-6-with-a-deliberately-long-model-slug",
        effort: Some("xhigh"),
        settings_path: Some(&long_settings_path),
        non_opus_auto_mode: false,
        permission_mode_override: Some("auto"),
        run_id: Some("run-claude-length-1"),
    });

    let workspace = TempDir::new().unwrap();
    let initial_input = assembled_initial_input(workspace.path(), &plan.env, &plan.command);
    check_initial_input_length(&initial_input, "claude").expect("typed line must stay under MAX_CANON");
    assert!(
        initial_input.len() < 64,
        "typed line must stay a small fixed string, got: {initial_input:?}",
    );
}

#[test]
fn codex_initial_input_stays_under_the_limit_for_reviewer() {
    let mut plan = CodexDriver::default().spawn_invocation(SpawnRequest {
        model: "gpt-5.6-terra-with-a-deliberately-long-model-slug",
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some("run-codex-length-1"),
    });
    plan.command = apply_permission_extra_args(&plan.command, &codex_sandbox_extra_args(WorkerKind::Reviewer, false));

    let workspace = TempDir::new().unwrap();
    let long_workspace = workspace.path().join("a".repeat(60)).join("b".repeat(60));
    std::fs::create_dir_all(&long_workspace).unwrap();

    let initial_input = assembled_initial_input(&long_workspace, &plan.env, &plan.command);
    check_initial_input_length(&initial_input, "codex").expect("typed line must stay under MAX_CANON");
    assert!(
        initial_input.len() < 64,
        "typed line must stay a small fixed string, got: {initial_input:?}",
    );
}

/// The motivating worst case from the regression report: a long
/// workspace path (embedded in `--cwd` and in `GROK_HOME`/`HOME`) plus
/// the FULL structural `--deny` rule set (design T-17) pushed the
/// previous typed-line-inline behaviour to ~1150-1177 bytes — past
/// MAX_CANON. Confirms the fix holds even here.
#[test]
fn grok_initial_input_stays_under_the_limit_with_long_workspace_path_and_full_deny_rule_set() {
    use crate::driver::grok::{
        GROK_HOMES_ENV_TEST_LOCK, GROK_HOMES_ROOT_ENV, GROK_SKIP_POSTURE_ASSERT_ENV, grok_home_for_run,
    };

    let workspace = TempDir::new().unwrap();
    let long_workspace_path = workspace
        .path()
        .join("a".repeat(50))
        .join("b".repeat(50))
        .join("c".repeat(50))
        .join("d".repeat(50));
    std::fs::create_dir_all(&long_workspace_path).unwrap();

    // Stamp a disposable $GROK_HOME (session id + workspace-path files)
    // so `spawn_invocation` builds a real command without running the
    // network-touching `provision_workspace` — same shape as
    // `grok::tests::spawn_invocation_matches_execution_shape`, reachable
    // here only through `grok`'s public re-exports.
    let _lock = GROK_HOMES_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prior_homes_env = std::env::var_os(GROK_HOMES_ROOT_ENV);
    let prior_skip_env = std::env::var_os(GROK_SKIP_POSTURE_ASSERT_ENV);
    let homes_root = TempDir::new().unwrap();
    // SAFETY: serialised by `_lock`, held for this whole test.
    unsafe {
        std::env::set_var(GROK_HOMES_ROOT_ENV, homes_root.path());
        // This command-length fixture stamps only the two files consumed by
        // spawn; it deliberately does not provision a real Cube workspace or
        // OAuth credential and therefore must not run the live preflight.
        std::env::set_var(GROK_SKIP_POSTURE_ASSERT_ENV, "1");
    }
    let run_id = "run-grok-length-1";
    let grok_home = grok_home_for_run(run_id).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();
    std::fs::write(
        grok_home.join("boss-session-id"),
        "11111111-2222-4333-8444-555555555555\n",
    )
    .unwrap();
    std::fs::write(
        grok_home.join("boss-workspace-path"),
        format!("{}\n", long_workspace_path.display()),
    )
    .unwrap();

    let mut plan = GrokDriver::default().spawn_invocation(SpawnRequest {
        model: "grok-4.7",
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some(run_id),
    });

    // A plausible long Boss-data-dir path: the Read/Edit deny pair
    // embeds this string twice, so a realistic length matters here too.
    let boss_data_dir = long_workspace_path.join("boss-events-socket-parent-dir-for-this-run");
    let permission_input = PermissionInput {
        frontend_socket_path: None,
        worker_kind: WorkerKind::Standard,
        workspace_path: long_workspace_path.clone(),
        events_socket_path: boss_data_dir.join("events.sock"),
        boss_event_path: PathBuf::from("/opt/homebrew/bin/boss-event"),
        run_id: run_id.to_owned(),
        lease_id: "lease-grok-length-1".to_owned(),
        execution_kind: "chore_implementation".to_owned(),
        task_kind: None,
        is_remote: false,
        path_guard_script: None,
        checkleft_guard_script: None,
        codex_sandbox_enforced: false,
    };
    let dest_dir = TempDir::new().unwrap();
    let artifacts = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(GrokDriver::default().write_permission_config(&permission_input, dest_dir.path()))
        .expect("grok write_permission_config must succeed against a stamped GROK_HOME");

    let deny_count = artifacts.extra_args.iter().filter(|a| a.as_str() == "--deny").count();
    assert_eq!(
        deny_count, 10,
        "expected the full structural deny-rule set (design T-17): {:?}",
        artifacts.extra_args,
    );
    plan.command = apply_permission_extra_args(&plan.command, &artifacts.extra_args);
    for (key, value) in &artifacts.env {
        if !plan
            .env
            .iter()
            .any(|d| matches!(d, EnvDirective::Set(k, _) if k == key))
        {
            plan.env.push(EnvDirective::Set(key.clone(), value.clone()));
        }
    }

    let env_prefix: String = plan.env.iter().map(render_env_directive).collect();
    let assembled = format!(
        "{}{}{env_prefix}{}",
        path_prepend_clause("BOSS_BIN_DIR"),
        path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
        plan.command,
    );
    // Sanity: this fixture really does reproduce an oversized command —
    // confirm that before proving the typed line stays small regardless.
    assert!(
        assembled.len() > 900,
        "fixture should reproduce a realistically oversized command, got {} bytes: {assembled}",
        assembled.len(),
    );

    let initial_input = write_initial_input_script(&long_workspace_path, &assembled).unwrap();
    check_initial_input_length(&initial_input, "grok").expect("typed line must stay under MAX_CANON");
    assert!(
        initial_input.len() < 64,
        "typed line must stay a small fixed string regardless of driver, permission-rule \
         count, or workspace path length, got: {initial_input:?}",
    );

    let written = std::fs::read_to_string(long_workspace_path.join(".boss").join("initial-input.sh")).unwrap();
    assert_eq!(
        written, assembled,
        "the full command must survive, relocated into the script"
    );
    assert!(written.contains("--sandbox"), "{written}");
    assert!(written.contains("--deny"), "{written}");
    assert!(written.contains("--cwd"), "{written}");

    // SAFETY: still serialised by `_lock`.
    match prior_homes_env {
        Some(v) => unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, v) },
        None => unsafe { std::env::remove_var(GROK_HOMES_ROOT_ENV) },
    }
    match prior_skip_env {
        Some(v) => unsafe { std::env::set_var(GROK_SKIP_POSTURE_ASSERT_ENV, v) },
        None => unsafe { std::env::remove_var(GROK_SKIP_POSTURE_ASSERT_ENV) },
    }
}

// ---------------------------------------------------------------------------
// ARG_MAX preflight (`check_launch_command_arg_max`): each driver embeds its
// initial prompt into the CLI's own argv via `"$(cat <file>)"` command
// substitution, expanded when the pane's shell sources
// `write_initial_input_script`'s output — unlike the MAX_CANON guard above,
// which bounds what gets *typed*, this bounds what the kernel accepts at
// the driver CLI's own `execve()`, after that expansion.
// ---------------------------------------------------------------------------

#[test]
fn check_arg_max_budget_under_the_limit_passes() {
    check_arg_max_budget(1000, 1000, 20000, "fake").unwrap();
}

#[test]
fn check_arg_max_budget_environment_can_push_a_fitting_argv_over_the_limit() {
    let err = check_arg_max_budget(1000, 20000, 20000, "fake").expect_err("must fail, not silently proceed");
    let msg = err.to_string();
    assert!(msg.contains("fake"), "error must name the driver: {msg}");
    assert!(msg.contains("environment 20000 bytes"), "{msg}");
    assert!(msg.contains("ARG_MAX 20000 bytes"), "{msg}");
}

#[test]
fn estimated_launch_argv_bytes_substitutes_the_real_prompt_files_size_for_the_placeholder() {
    let workspace = TempDir::new().unwrap();
    std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
    let prompt = "y".repeat(12_345);
    std::fs::write(workspace.path().join(".claude").join("initial-prompt.txt"), &prompt).unwrap();

    let command = "claude --model opus \"$(cat .claude/initial-prompt.txt)\"\n";
    let placeholder_len = "\"$(cat .claude/initial-prompt.txt)\"".len();
    let bytes = estimated_launch_argv_bytes(command, workspace.path(), ".claude", "initial-prompt.txt").unwrap();

    assert_eq!(bytes, command.len() - placeholder_len + prompt.len());
}

#[test]
fn estimated_launch_argv_bytes_errors_when_the_prompt_file_is_missing() {
    let workspace = TempDir::new().unwrap();
    let command = "claude --model opus \"$(cat .claude/initial-prompt.txt)\"\n";
    let err = estimated_launch_argv_bytes(command, workspace.path(), ".claude", "initial-prompt.txt")
        .expect_err("must fail, not silently proceed with an unknown size");
    assert!(err.to_string().contains("initial-prompt.txt"), "{err}");
}

/// End-to-end against this host's real `ARG_MAX`: a ~600 KB prompt — the
/// motivating large-prompt size from the regression report — must still
/// fit comfortably. Regressing the estimate back to counting every argv
/// byte as if it needed its own pointer (rather than one pointer per
/// shell argument) would wrongly reject this.
#[test]
fn check_launch_command_arg_max_passes_for_a_600kb_prompt_on_this_host() {
    let workspace = TempDir::new().unwrap();
    std::fs::create_dir_all(workspace.path().join(".codex")).unwrap();
    std::fs::write(
        workspace.path().join(".codex").join("initial-prompt.txt"),
        "z".repeat(600_000),
    )
    .unwrap();

    let command = "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n";
    let result = check_launch_command_arg_max(command, "codex", workspace.path(), ".codex", "initial-prompt.txt");
    if crate::runner::spawn_launch_limits::local_max_arg_strlen().is_some_and(|limit| 600_000 > limit) {
        let err = result.expect_err("Linux MAX_ARG_STRLEN must refuse a 600KB single argument");
        let msg = err.to_string();
        assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
        return;
    }
    result.expect("a ~600KB prompt must fit under this host's real ARG_MAX");
}

/// A prompt sized to exceed this host's real, measured `ARG_MAX` must be
/// refused before spawn — not left to fail silently inside the pane's
/// sourced script as `execve`'s `E2BIG`.
#[test]
fn check_launch_command_arg_max_fails_for_a_prompt_over_this_hosts_real_arg_max() {
    let workspace = TempDir::new().unwrap();
    std::fs::create_dir_all(workspace.path().join(".grok")).unwrap();
    let arg_max = local_arg_max().unwrap();
    std::fs::write(
        workspace.path().join(".grok").join("initial-prompt.txt"),
        "w".repeat(arg_max + 4096),
    )
    .unwrap();

    let command = "grok --model 'grok-4.7' \"$(cat .grok/initial-prompt.txt)\"\n";
    let err = check_launch_command_arg_max(command, "grok", workspace.path(), ".grok", "initial-prompt.txt")
        .expect_err("must fail, not silently proceed with a doomed exec");
    let msg = err.to_string();
    assert!(msg.contains("grok"), "error must name the driver: {msg}");
    assert!(msg.contains("ARG_MAX"), "{msg}");
}

// ---------------------------------------------------------------------------
// `check_launch_command_arg_max_for_bytes`: the same preflight for the
// remote spawn path (`host_adapter.rs`), which ships the initial prompt to
// a remote host over SSH from an in-memory `String` rather than a
// workspace-relative file this process can `stat` — so it takes the
// prompt's byte length directly instead of reading it off disk.
// ---------------------------------------------------------------------------

#[test]
fn check_launch_command_arg_max_for_bytes_passes_for_a_600kb_prompt_on_this_host() {
    let command = "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n";
    let result = check_launch_command_arg_max_for_bytes(
        command,
        "codex",
        ".codex",
        "initial-prompt.txt",
        600_000,
        ExecArgLimits::local().unwrap(),
    );
    if crate::runner::spawn_launch_limits::local_max_arg_strlen().is_some_and(|limit| 600_000 > limit) {
        let err = result.expect_err("Linux MAX_ARG_STRLEN must refuse a 600KB single argument");
        let msg = err.to_string();
        assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
        return;
    }
    result.expect("a ~600KB prompt must fit under this host's real ARG_MAX");
}

#[test]
fn check_launch_command_arg_max_for_bytes_fails_for_a_prompt_over_this_hosts_real_arg_max() {
    let arg_max = local_arg_max().unwrap();
    let command = "grok --model 'grok-4.7' \"$(cat .grok/initial-prompt.txt)\"\n";
    let err = check_launch_command_arg_max_for_bytes(
        command,
        "grok",
        ".grok",
        "initial-prompt.txt",
        arg_max + 4096,
        ExecArgLimits::local().unwrap(),
    )
    .expect_err("must fail, not silently proceed with a doomed remote exec");
    let msg = err.to_string();
    assert!(msg.contains("grok"), "error must name the driver: {msg}");
    assert!(msg.contains("ARG_MAX"), "{msg}");
}

/// Small and ~600 KB prompts, per driver: write the argv-delivery script,
/// run the ARG_MAX / MAX_ARG_STRLEN preflight, then confirm composer +
/// turn-start against that driver's agent chrome.
#[tokio::test]
async fn per_driver_small_and_large_prompts_go_through_script_preflight_and_confirmation() {
    use crate::runner::spawn_confirmation::{confirm_spawn_started, pane_shows_driver_ready};
    use std::time::Duration;

    struct Case {
        driver_name: &'static str,
        config_dir: &'static str,
        filename: &'static str,
        command: &'static str,
        chrome: &'static str,
        spec: boss_protocol::PaneMonitorSpec,
    }

    let cases = [
        Case {
            driver_name: "claude",
            config_dir: ".claude",
            filename: "initial-prompt.txt",
            command: "claude --model opus \"$(cat .claude/initial-prompt.txt)\"\n",
            chrome: "Claude Code 2.1.283\nauto mode on\n❯ ",
            spec: ClaudeDriver.pane_monitor_spec().expect("claude spec"),
        },
        Case {
            driver_name: "codex",
            config_dir: ".codex",
            filename: "initial-prompt.txt",
            command: "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n",
            chrome: ">_ OpenAI Codex (v0.15)\n• Working (1s • esc to interrupt)",
            spec: CodexDriver::default().pane_monitor_spec().expect("codex spec"),
        },
        Case {
            driver_name: "grok",
            config_dir: ".grok",
            filename: "initial-prompt.txt",
            command: "grok --model 'grok-4.7' \"$(cat .grok/initial-prompt.txt)\"\n",
            chrome: "Grok 4.6  Shift+Tab:mode  always-approve\n│ ❯ ",
            spec: GrokDriver::default().pane_monitor_spec().expect("grok spec"),
        },
    ];

    for case in cases {
        for prompt_bytes in [20_000usize, 600_000] {
            let workspace = TempDir::new().unwrap();
            std::fs::create_dir_all(workspace.path().join(case.config_dir)).unwrap();
            std::fs::write(
                workspace.path().join(case.config_dir).join(case.filename),
                "p".repeat(prompt_bytes),
            )
            .unwrap();
            let typed = write_initial_input_script(workspace.path(), case.command).unwrap();
            check_initial_input_length(&typed, case.driver_name).unwrap();
            assert_eq!(typed, ". .boss/initial-input.sh\n");

            let preflight = check_launch_command_arg_max(
                case.command,
                case.driver_name,
                workspace.path(),
                case.config_dir,
                case.filename,
            );
            if crate::runner::spawn_launch_limits::local_max_arg_strlen().is_some_and(|limit| prompt_bytes > limit) {
                let err = preflight.expect_err("over MAX_ARG_STRLEN must refuse before spawn");
                let msg = err.to_string();
                assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
                assert!(msg.contains(case.driver_name), "{msg}");
            } else {
                preflight.unwrap_or_else(|err| {
                    panic!(
                        "{} {}-byte prompt must pass this host's preflight: {err}",
                        case.driver_name, prompt_bytes
                    )
                });
            }

            assert!(
                pane_shows_driver_ready(case.chrome, &case.spec),
                "{} chrome must count as composer-ready: {}",
                case.driver_name,
                case.chrome
            );
            confirm_spawn_started(
                case.driver_name,
                "exec-launch-path",
                Duration::from_millis(20),
                Duration::from_millis(20),
                Duration::from_millis(5),
                || async { pane_shows_driver_ready(case.chrome, &case.spec) },
                || async { true },
            )
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "{} confirmation must pass for a {}-byte prompt: {err}",
                    case.driver_name, prompt_bytes
                )
            });
        }
    }
}
