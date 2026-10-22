//! Tests for the MAX_CANON fix: the guard (`check_initial_input_length`),
//! the file-indirection helper (`write_initial_input_script`), and — driven
//! through each real driver's own `spawn_invocation` — proof that the typed
//! `initial_input` line stays under the canonical-mode limit even in each
//! driver's worst realistic case (a long workspace path, and for Grok the
//! full structural `--deny` rule set).

use super::{
    MAX_CANON_LINE_BYTES, check_arg_max_budget, check_initial_input_length, check_launch_command_arg_max,
    local_arg_max, path_prepend_clause, render_env_directive, write_initial_input_script,
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

/// A long workspace path (embedded in `--cwd` and in `GROK_HOME`/`HOME`)
/// plus the FULL structural `--deny` rule set (design T-17) pushed the
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
        model: "grok-4.6",
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
// ARG_MAX preflight (`check_launch_command_arg_max`): the brief travels over
// the launcher's PTY, so the exec budget covers only the driver's flags.
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
fn check_launch_command_arg_max_fails_for_a_command_over_this_hosts_real_arg_max() {
    let arg_max = local_arg_max().unwrap();
    let command = crate::driver::initial_prompt::launcher_command(
        ".grok",
        &format!("grok --model 'grok-4.6' --settings '{}'", "w".repeat(arg_max + 4096)),
    );
    let err =
        check_launch_command_arg_max(&command, "grok").expect_err("must fail, not silently proceed with a doomed exec");
    let msg = err.to_string();
    assert!(msg.contains("grok"), "error must name the driver: {msg}");
    assert!(msg.contains("ARG_MAX") || msg.contains("MAX_ARG_STRLEN"), "{msg}");
}

/// Rename the driver binary in a real `spawn_invocation` command so the
/// hermetic sandbox can exec the stub (it denies `claude` / `codex` by name).
fn rename_driver_binary(command: &str, from: &str, to: &str) -> String {
    let needle = format!("initial-prompt.py {from} ");
    assert!(command.contains(&needle), "missing terminal launcher: {command}");
    command.replacen(&needle, &format!("initial-prompt.py {to} "), 1)
}

fn write_driver_stub(stub_dir: &std::path::Path) {
    std::fs::create_dir_all(stub_dir).unwrap();
    let stub_path = stub_dir.join("boss-driver-stub");
    std::fs::write(
        &stub_path,
        r#"#!/usr/bin/env python3
import json, os, sys, tty
from pathlib import Path
record = Path(os.environ['BOSS_STUB_RECORD'])
(record / 'argv').write_text(json.dumps(sys.argv))
(record / 'environment').write_text(json.dumps(dict(os.environ)))
tty.setraw(0)
os.write(1, b'\x1b[6n')
response = b''
while not response.endswith(b'R'):
    response += os.read(0, 4096)
assert response == b'\x1b[1;1R', response
os.write(1, b'\x1b[?2004h' + os.environ['BOSS_STUB_CHROME'].encode())
data = b''
while not data.endswith(b'\x1b[201~\r'):
    data += os.read(0, 65536)
assert data.startswith(b'\x1b[200~'), data[:40]
(record / 'received').write_bytes(data[6:-7])
(record / 'turn_started').touch()
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

struct DeliveryEnvironment {
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl Drop for DeliveryEnvironment {
    fn drop(&mut self) {
        for (key, value) in &self.prior {
            // SAFETY: the caller holds the Grok environment lock until drop.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn per_driver_small_and_large_prompts_are_delivered_and_start_the_turn() {
    use crate::driver::{grok, test_support};
    let root = TempDir::new().unwrap();
    let auth = root.path().join("auth.json");
    std::fs::write(&auth, r#"{"OPENAI_API_KEY":"test-key"}"#).unwrap();
    let _codex = test_support::codex_auth_source_override(&root.path().join("codex"), &auth);
    let _grok = test_support::grok_homes_override(&root.path().join("grok"));
    let _transcripts = test_support::transcript_store_override(&root.path().join("transcripts"));
    let mut environment = DeliveryEnvironment { prior: Vec::new() };
    for (key, value) in [
        (grok::GROK_AUTH_SOURCE_ENV, auth.as_os_str()),
        (grok::GROK_SKIP_POSTURE_ASSERT_ENV, std::ffi::OsStr::new("1")),
    ] {
        environment.prior.push((key, std::env::var_os(key)));
        // SAFETY: `_grok` holds the shared environment lock through block_on.
        unsafe { std::env::set_var(key, value) };
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(per_driver_delivery_body());
}

/// Small and ~600 KB prompts, per driver: take the real `spawn_invocation`
/// command, rewrite the binary to a stub the sandbox can exec, write the
/// actual `.boss/initial-input.sh`, source it from `/bin/sh`, and assert
/// the stub received the prompt intact through terminal stdin and emitted
/// the turn-start signal confirmation consumes.
async fn per_driver_delivery_body() {
    use crate::runner::spawn_confirmation::{confirm_spawn_started, pane_shows_driver_ready};
    use std::time::Duration;

    struct Case {
        driver_name: &'static str,
        binary: &'static str,
        config_dir: &'static str,
        filename: &'static str,
        chrome: &'static str,
        spec: boss_protocol::PaneMonitorSpec,
        spawn: fn(&str) -> crate::driver::SpawnPlan,
    }

    let cases = [
        Case {
            driver_name: "claude",
            binary: "claude",
            config_dir: ".claude",
            filename: "initial-prompt.txt",
            chrome: "Claude Code 2.1.283\nauto mode on\n❯ ",
            spec: ClaudeDriver.pane_monitor_spec().expect("claude spec"),
            spawn: |run_id| {
                ClaudeDriver.spawn_invocation(SpawnRequest {
                    model: "opus",
                    effort: Some("high"),
                    settings_path: None,
                    non_opus_auto_mode: false,
                    permission_mode_override: Some("auto"),
                    run_id: Some(run_id),
                })
            },
        },
        Case {
            driver_name: "codex",
            binary: "codex",
            config_dir: ".codex",
            filename: "initial-prompt.txt",
            chrome: ">_ OpenAI Codex (v0.15)\n• Working (1s • esc to interrupt)",
            spec: CodexDriver::default().pane_monitor_spec().expect("codex spec"),
            spawn: |run_id| {
                CodexDriver::default().spawn_invocation(SpawnRequest {
                    model: "gpt-5.6-terra",
                    effort: Some("high"),
                    settings_path: None,
                    non_opus_auto_mode: false,
                    permission_mode_override: None,
                    run_id: Some(run_id),
                })
            },
        },
        Case {
            driver_name: "grok",
            binary: "grok",
            config_dir: ".grok",
            filename: "initial-prompt.txt",
            chrome: "Grok 4.6  Shift+Tab:mode  always-approve\n│ ❯ ",
            spec: GrokDriver::default().pane_monitor_spec().expect("grok spec"),
            spawn: |run_id| {
                GrokDriver::default().spawn_invocation(SpawnRequest {
                    model: "grok-4.6",
                    effort: Some("high"),
                    settings_path: None,
                    non_opus_auto_mode: false,
                    permission_mode_override: None,
                    run_id: Some(run_id),
                })
            },
        },
    ];

    for case in cases {
        for prompt_bytes in [20_000usize, 600_000, local_arg_max().unwrap() + 4096] {
            let workspace = TempDir::new().unwrap();
            let run_id = format!("run-{}-{prompt_bytes}", case.driver_name);
            let sentinel = "BRIEF_SENTINEL_PRIVATE_INITIAL_INPUT";
            let prompt = format!(
                "{sentinel}\n{}\nquotes: '\" $() `command` 雪\n\n",
                "p".repeat(prompt_bytes)
            );
            let driver = crate::driver::DriverRegistry::default()
                .require(case.driver_name)
                .unwrap();
            let config_dir = workspace.path().join(case.config_dir);
            driver
                .provision_workspace(workspace.path(), &prompt, &run_id)
                .await
                .unwrap();
            assert!(
                std::fs::symlink_metadata(config_dir.join(case.filename))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let mut plan = (case.spawn)(&run_id);
            assert!(!plan.command.contains(sentinel));
            assert!(!plan.command.contains("$(cat"));

            plan.command = rename_driver_binary(&plan.command, case.binary, "boss-driver-stub");
            let env_prefix: String = plan.env.iter().map(render_env_directive).collect();
            let assembled = format!(
                "{}{}{env_prefix}{}",
                path_prepend_clause("BOSS_BIN_DIR"),
                path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
                plan.command,
            );
            let typed = write_initial_input_script(workspace.path(), &assembled).unwrap();
            check_initial_input_length(&typed, case.driver_name).unwrap();
            assert_eq!(typed, ". .boss/initial-input.sh\n");

            let preflight = check_launch_command_arg_max(&plan.command, case.driver_name);
            preflight.unwrap_or_else(|err| {
                panic!(
                    "{} {}-byte prompt must pass this host's preflight: {err}",
                    case.driver_name, prompt_bytes
                )
            });

            let stub_root = TempDir::new().unwrap();
            write_driver_stub(stub_root.path());
            let record_dir = stub_root.path().join("record");
            std::fs::create_dir_all(&record_dir).unwrap();
            let path = format!("{}:{}", stub_root.path().display(), std::env::var("PATH").unwrap());
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(". .boss/initial-input.sh")
                .current_dir(workspace.path())
                .env("PATH", &path)
                .env_remove("BOSS_BIN_DIR")
                .env_remove(boss_engine_worker_bin::WORKER_BIN_DIR_ENV)
                .env("BOSS_STUB_RECORD", &record_dir)
                .env("BOSS_STUB_CHROME", case.chrome)
                .output()
                .expect("sh must be able to source the launch script");
            assert!(
                output.status.success(),
                "{} {}-byte prompt must exec without E2BIG; status={} stderr={}",
                case.driver_name,
                prompt_bytes,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );

            for surface in ["argv", "environment"] {
                let observed = std::fs::read_to_string(record_dir.join(surface)).unwrap();
                assert!(
                    !observed.contains(sentinel),
                    "{} leaked brief in {surface}",
                    case.driver_name
                );
            }
            let received = std::fs::read_to_string(record_dir.join("received")).unwrap();
            assert_eq!(
                received, prompt,
                "{} stub must receive the prompt intact",
                case.driver_name
            );
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                pane_shows_driver_ready(&stdout, &case.spec),
                "{} stub stdout must count as composer-ready: {stdout}",
                case.driver_name
            );
            assert!(
                record_dir.join("turn_started").exists(),
                "{} stub must emit the turn-start signal confirmation consumes",
                case.driver_name
            );
            confirm_spawn_started(
                case.driver_name,
                &run_id,
                Duration::from_millis(20),
                Duration::from_millis(20),
                Duration::from_millis(5),
                || {
                    let stdout = stdout.clone();
                    let spec = case.spec.clone();
                    async move { pane_shows_driver_ready(&stdout, &spec) }
                },
                || {
                    let marker = record_dir.join("turn_started");
                    async move { marker.exists() }
                },
            )
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "{} confirmation must observe the stub's turn-start for a {}-byte prompt: {err}",
                    case.driver_name, prompt_bytes
                )
            });
        }
    }
}
