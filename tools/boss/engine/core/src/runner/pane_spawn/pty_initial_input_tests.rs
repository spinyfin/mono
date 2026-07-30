//! Tests for the MAX_CANON fix: the guard (`check_initial_input_length`),
//! the file-indirection helper (`write_initial_input_script`), and — driven
//! through each real driver's own `spawn_invocation` — proof that the typed
//! `initial_input` line stays under the canonical-mode limit even in each
//! driver's worst realistic case (a long workspace path, and for Grok the
//! full structural `--deny` rule set).

use super::{
    MAX_CANON_LINE_BYTES, check_initial_input_length, path_prepend_clause, render_env_directive,
    write_initial_input_script,
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
fn codex_initial_input_stays_under_the_limit_with_reviewer_sandbox_extra_args() {
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
    use crate::driver::grok::{GROK_HOMES_ENV_TEST_LOCK, GROK_HOMES_ROOT_ENV, grok_home_for_run};

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
    let homes_root = TempDir::new().unwrap();
    // SAFETY: serialised by `_lock`, held for this whole test.
    unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, homes_root.path()) };
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
        model: "grok-4.5",
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
}
