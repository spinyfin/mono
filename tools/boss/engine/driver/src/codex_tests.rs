use super::*;
use crate::{AbsenceDisposition, Capability};
use boss_protocol::{ReviewModelTier, StopReason};
use tempfile::TempDir;

#[test]
fn codex_model_belongs_to_driver_recognises_codex_vocabulary() {
    for model in [
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4-mini",
        "codex-auto-review",
        "GPT-5.6-SOL",
    ] {
        assert!(
            codex_model_belongs_to_driver(model),
            "{model:?} should be recognised as a Codex model"
        );
    }
}

#[test]
fn codex_model_belongs_to_driver_rejects_other_drivers_models() {
    // The exact bug this gate exists to catch: a Claude family alias
    // reaching the Codex CLI verbatim.
    for model in ["opus", "sonnet", "claude-opus-4-7", "grok-4.6"] {
        assert!(
            !codex_model_belongs_to_driver(model),
            "{model:?} should not be recognised as a Codex model"
        );
    }
}

// Tests that mutate `BOSS_CODEX_*` go through
// [`crate::test_support::codex_homes_override`] (owns
// [`CODEX_HOMES_ENV_TEST_LOCK`]). `CODEX_AUTH_SOURCE_ENV` rides on that
// same lock — set/restore it only while a homes override is held.

fn sample_auth_json() -> String {
    serde_json::json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "id.token",
            "access_token": "access.token",
            "refresh_token": "refresh.token",
            "account_id": "acct_test"
        },
        "last_refresh": "2026-01-01T00:00:00.000Z"
    })
    .to_string()
}

fn spawn_request<'a>(model: &'a str, run_id: &'a str) -> SpawnRequest<'a> {
    SpawnRequest {
        model,
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some(run_id),
    }
}

#[test]
fn codex_descriptor_matches_design() {
    let driver = CodexDriver::default();
    let d = driver.descriptor();
    assert_eq!(d.name, "codex");
    assert_eq!(d.label, "OpenAI Codex");
    assert_eq!(d.binary, "codex");
    assert_eq!(d.config_dir, ".codex");
    assert_eq!(d.agent_rules_filename, "AGENTS.md");
    assert_eq!(d.initial_prompt_filename, "initial-prompt.txt");
    assert_eq!(d.model_menu.engine_default, "gpt-5.6-sol");
}

#[test]
fn agent_rules_require_session_polling_until_a_real_exit_status() {
    let preamble = CodexDriver::default().agent_rules_preamble();
    for required in [
        "expected to exceed roughly ten seconds",
        "exec_command` yields after at most 30 seconds;",
        "tools.write_stdin",
        "chars: \"\"",
        "text(JSON.stringify(r))",
        "completion in the same JavaScript cell",
        "let r = await tools.exec_command",
        "session_id: r.session_id",
        "yield_time_ms: 300000",
        "carries `exit_code`.",
        "own foreground timeout",
    ] {
        assert!(
            preamble.contains(required),
            "Codex worker rules must preserve the long-command contract {required:?}: {preamble}"
        );
    }
    assert!(
        preamble.contains("a result containing\n`session_id` means the command is still running"),
        "a yielded session must never be presented as a completed gate: {preamble}"
    );
    assert!(
        preamble.contains("`text(r.output)` discards that handle"),
        "the worker must retain its session handle for polling: {preamble}"
    );
    assert!(
        preamble.contains("only that invocation's output and logs") && preamble.contains("global process-name matches"),
        "the worker must not attribute unrelated global processes to its command: {preamble}"
    );
    assert!(
        preamble.contains("without the real `exit_code`"),
        "the worker must not infer success from a missing exit status: {preamble}"
    );
}

#[test]
fn agent_rules_destination_is_codex_home_not_dot_codex() {
    // Codex never reads `.codex/AGENTS.md` (verified with `codex debug
    // prompt-input`). Must route to `$CODEX_HOME/AGENTS.md`, not the
    // trait default (`<workspace>/<config_dir>/<agent_rules_filename>`).
    // Pin the homes root for the whole assertion. Both sides resolve it
    // from `CODEX_HOMES_ROOT_ENV`, so without the override a sibling test
    // installing or dropping its own override between the two resolutions
    // makes them disagree — the flake this test showed on CI.
    let tmp = TempDir::new().unwrap();
    let _homes = crate::test_support::codex_homes_override(&tmp.path().join("homes"));
    let driver = CodexDriver::default();
    let workspace = Path::new("/tmp/some-workspace");
    let destination = driver.agent_rules_destination(workspace, "run-agents-md-1");
    assert_eq!(
        destination,
        codex_home_for_run("run-agents-md-1").unwrap().join("AGENTS.md")
    );
    assert!(
        !destination.starts_with(workspace),
        "AGENTS.md must not land inside the workspace tree: {}",
        destination.display()
    );
}

#[test]
fn codex_model_menu_sourced_from_debug_models_vocabulary() {
    let driver = CodexDriver::default();
    let menu = &driver.descriptor().model_menu;
    assert_eq!((menu.effort_value_for_level)(EffortLevel::Trivial), Some("low"));
    assert_eq!((menu.effort_value_for_level)(EffortLevel::Small), Some("medium"));
    assert_eq!((menu.effort_value_for_level)(EffortLevel::Medium), Some("high"));
    assert_eq!((menu.effort_value_for_level)(EffortLevel::Large), Some("xhigh"));
    assert_eq!((menu.effort_value_for_level)(EffortLevel::Max), Some("max"));
    assert_eq!((menu.model_for_reasoning)(ReasoningMode::Standard), "gpt-5.6-terra");
    assert_eq!((menu.model_for_reasoning)(ReasoningMode::Investigation), "gpt-5.6-sol");
    assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Fast), "gpt-5.6-luna");
    assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Balanced), "gpt-5.6-terra");
    assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Strong), "gpt-5.6-sol");
    assert!(!(menu.model_requires_auto_permissions)("gpt-5.6-sol"));
}

#[test]
fn codex_declares_design_capability_set() {
    let caps = CodexDriver::default().capabilities();
    for cap in [
        Capability::Spawn,
        Capability::WorkspaceProvisioning,
        Capability::PermissionPolicy,
        Capability::ModelAndEffortMenu,
        Capability::ProgressObservation,
        Capability::ToolUseInterception,
        Capability::TurnBoundary,
        Capability::StructuredOutput,
        Capability::TranscriptAccess,
        Capability::ControlVerbs,
        Capability::PromptComposition,
    ] {
        assert!(caps.provides(cap), "CodexDriver must provide {cap:?}");
    }
    assert!(!caps.provides(Capability::ToolProvisioning));
    assert!(!caps.provides(Capability::AwaitingInputSignal));
    assert!(!caps.provides(Capability::CommandOutcomeObservation));
    assert_eq!(
        caps.absence_disposition(Capability::ToolProvisioning),
        AbsenceDisposition::Degrade
    );
    assert_eq!(
        caps.absence_disposition(Capability::AwaitingInputSignal),
        AbsenceDisposition::Degrade
    );
    assert_eq!(
        caps.absence_disposition(Capability::CommandOutcomeObservation),
        AbsenceDisposition::Degrade,
        "Boss must not synthesize a per-command outcome Codex never observed"
    );
}

/// The `AwaitingInputSignal` omission must survive the persistent-session
/// inversion. Its original justification — "a completed turn means the
/// process is about to exit, not that anyone is blocked on a human" — is
/// dead: the driver now declares [`WorkerProcessLifetime::Persistent`],
/// and a TUI parked at its composer genuinely is awaiting input. The
/// capability stays undeclared anyway because it is a claim about the
/// *progress stream*, and every `Notification` Codex's normaliser can
/// emit means something else (unobserved command, guard trace, command
/// denial, turn abort, fatal error). Declaring it would let
/// `apply_event` promote one of those into a fabricated
/// `WaitingForInput` — Grok's precedent, same reasoning.
///
/// Asserting both facts in one test is the point: it is the pairing that
/// is deliberate, and a future reader who flips the lifetime back, or
/// declares the capability on the strength of persistence alone, breaks
/// here.
#[test]
fn codex_persistence_does_not_earn_the_awaiting_input_capability() {
    let driver = CodexDriver::default();
    assert_eq!(
        driver.worker_process_lifetime(),
        WorkerProcessLifetime::Persistent,
        "the premise that inverted the old omission reasoning",
    );
    assert!(
        !driver.capabilities().provides(Capability::AwaitingInputSignal),
        "a persistent session is a structural argument, not a measured stream signal; \
         the capability must not be declared on it",
    );
    assert_eq!(
        driver
            .capabilities()
            .absence_disposition(Capability::AwaitingInputSignal),
        AbsenceDisposition::Degrade,
        "absence must degrade, never synthesize a waiting state Codex never reported",
    );
}

#[test]
fn codex_declares_rich_progress_fidelity_without_command_outcome_observation() {
    // Rich cadence (per-tool item.started/item.completed boundaries) is
    // not the same claim as reliable per-command exit status: Codex's
    // rollout exit_code/status fields are sometimes absent, can be
    // dropped by the model's own result-projection layer, and become
    // unparseable once output is truncated. A scheduler must not infer
    // outcome observability from the fidelity tier alone.
    let driver = CodexDriver::default();
    assert_eq!(driver.progress_fidelity(), ProgressFidelity::Rich);
    assert!(!driver.capabilities().provides(Capability::CommandOutcomeObservation));
}

#[test]
fn spawn_invocation_meets_codex_tui_contract() {
    let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-terra", "run-spawn-1"));
    assert!(
        !plan.command.contains("--color"),
        "forbids --color: hard argument error on the bare TUI: {}",
        plan.command
    );
    assert!(
        !plan.command.contains("--json"),
        "forbids --json: never existed on the bare TUI as anything but a hard error: {}",
        plan.command
    );
    assert!(
        plan.command.contains("--strict-config"),
        "requires --strict-config: {}",
        plan.command
    );
    assert!(
        !plan.command.contains("--skip-git-repo-check"),
        "forbids --skip-git-repo-check: hard argument error on the bare TUI \
         (the retired `codex exec` shape needed it; the TUI does not perform \
         the check it bypassed): {}",
        plan.command
    );
    assert!(
        plan.command.contains("--no-alt-screen"),
        "requires --no-alt-screen: viewport/screen reads must diverge so scrollback \
         accumulates across turns instead of capping at one screenful: {}",
        plan.command
    );
    assert!(
        plan.command.contains("-a never"),
        "requires -a never: a persistent session must never block on an approval \
         prompt Boss cannot answer: {}",
        plan.command
    );
    assert!(
        !plan.command.starts_with("exec ") && !plan.command.trim_start().starts_with("exec "),
        "must not wrap the command in a shell `exec` prefix: {}",
        plan.command
    );
    assert!(
        !plan.command.contains("codex exec"),
        "must not invoke the `exec` subcommand: {}",
        plan.command
    );
    assert!(
        !plan.command.contains("< /dev/null"),
        "must not redirect stdin from /dev/null: a TUI needs the tty: {}",
        plan.command
    );
    assert!(
        plan.command.contains(&format!("-m {}", shell_quote("gpt-5.6-terra"))),
        "must pass shell-quoted model: {}",
        plan.command
    );
    assert!(
        plan.command
            .contains(&format!("model_reasoning_effort={}", shell_quote("high"))),
        "must pass shell-quoted effort: {}",
        plan.command
    );
    assert!(
        plan.env.iter().any(|d| matches!(
            d,
            EnvDirective::Set(k, v) if k == "CODEX_HOME" && v.contains("run-spawn-1")
        )),
        "must export CODEX_HOME for the run: {:?}",
        plan.env
    );
}

/// The pivot's opposite pane-launch invariant: unlike the retired
/// `codex exec` shape (which wrapped the body in a shell `exec` so no
/// shell survived to consume tty-buffered injects), a persistent session
/// is typed as a plain command at the pane's shell prompt — the same
/// shape Claude and Grok already use — so the shell survives and can go
/// on to accept later `SendToPane` turns.
#[test]
fn pane_launch_spec_does_not_use_shell_exec() {
    let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-sol", "run-pane-a"));
    let trimmed = plan.command.trim_start();
    assert!(
        trimmed.starts_with("codex "),
        "pane launch must type a plain `codex` command line, not shell-exec into it; got: {}",
        plan.command
    );
    assert!(
        !trimmed.starts_with("exec "),
        "must not wrap the command in a shell `exec` prefix: {}",
        plan.command
    );
}

/// Driven by a test-owned current-thread runtime rather than
/// `#[tokio::test]`, because the homes-root override must stay held for
/// the whole provision → reclaim sequence. Releasing it after the initial
/// `set_var` left reclaim reading a root any parallel test in this binary
/// could move (or clear) out from under it — CI saw reclaim refuse a home
/// under the test temp tree when `codex_homes_root()` had flipped back to
/// the default `$TMPDIR/boss-codex-homes`. `block_on` keeps the guard
/// inside one blocking call so we never hold a `MutexGuard` across
/// `.await` (`clippy::await_holding_lock`).
#[test]
fn provision_workspace_creates_owned_home_and_snapshots_auth() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    // Make workspace a git repo so project trust stamps are meaningful.
    let _ = Command::new("git").args(["init"]).current_dir(&workspace).output();

    let auth_src = tmp.path().join("source-auth.json");
    fs::write(&auth_src, sample_auth_json()).unwrap();

    // Point homes + auth source at the temp tree; never touch ~/.codex.
    let _auth = crate::test_support::codex_auth_source_override(&homes, &auth_src);
    let _transcripts = crate::test_support::transcript_store_override(&tmp.path().join("transcripts"));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(provision_workspace_creates_owned_home_and_snapshots_auth_body(
            &homes, &workspace,
        ));
}

async fn provision_workspace_creates_owned_home_and_snapshots_auth_body(
    homes: &std::path::Path,
    workspace: &std::path::Path,
) {
    let driver = CodexDriver::default();
    let state = driver
        .provision_workspace(workspace, "hello prompt", "run-prov-1")
        .await
        .expect("provision")
        .expect("Codex must return runtime state");

    let runtime = CodexRuntimeState::from_driver_runtime_state(&state).unwrap();
    assert!(runtime.codex_home.starts_with(homes));
    assert!(runtime.codex_home.join("auth.json").is_file());
    assert!(runtime.codex_home.join("config.toml").is_file());

    let config = fs::read_to_string(runtime.codex_home.join("config.toml")).unwrap();
    assert!(
        config.contains("[notice.external_config_migration_prompts]") && config.contains("home = true"),
        "must suppress the external-agent config-migration notice via the real \
         nested key (not a nonexistent top-level field): {config}"
    );
    assert!(
        config.contains("[features]") && config.contains("external_agent_memory_import = false"),
        "must pin the memory-import feature off: {config}"
    );
    assert!(
        config.contains("trust_level = \"trusted\""),
        "must stamp project trust: {config}"
    );

    let prompt = workspace.join(".codex/initial-prompt.txt");
    assert_eq!(fs::read_to_string(prompt).unwrap(), "hello prompt");

    // Interactive home must not have been created/scanned/mutated as CODEX_HOME.
    assert_ne!(
        runtime.codex_home,
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex")
    );

    // Teardown adopts auth but retains the home for policy-based reclaim.
    driver
        .teardown_workspace(Some(workspace), "run-prov-1", Some(&state))
        .await
        .unwrap();
    assert!(
        runtime.codex_home.exists(),
        "teardown must retain CODEX_HOME as terminal-run evidence"
    );

    // Explicit reclaim (what the retention sweep does) removes only this root.
    // Must run while the homes-root override is still held so
    // `codex_homes_root()` still matches the path provision recorded.
    reclaim_codex_home(&runtime.codex_home).unwrap();
    assert!(!runtime.codex_home.exists(), "reclaim must remove the recorded home");
    // Idempotent.
    reclaim_codex_home(&runtime.codex_home).unwrap();
}

#[test]
fn base_config_escapes_workspace_paths_with_spaces() {
    let toml = render_base_config_toml(Path::new("/Users/a b/ws"));
    assert!(toml.contains("\"/Users/a b/ws\""), "{toml}");
    assert!(toml.contains("[notice.external_config_migration_prompts]"));
    assert!(toml.contains("[features]"));
    assert!(toml.contains("external_agent_memory_import = false"));
    // No stray unnested/unquoted occurrence of the old, invalid top-level
    // scalar form this config once emitted.
    assert!(!toml.contains("\nexternal_config_migration_prompts = false\n"));
}

#[test]
fn base_config_grants_bazel_sandbox_permissions() {
    let toml = render_base_config_toml(Path::new("/ws"));
    assert!(toml.contains("[sandbox_workspace_write]"), "{toml}");
    assert!(toml.contains("network_access = true"), "{toml}");
    // The table must land before [projects.*] so a duplicate top-level
    // key introduced later in the format string can't silently shadow it.
    let sandbox_pos = toml.find("[sandbox_workspace_write]").unwrap();
    let projects_pos = toml.find("[projects.").unwrap();
    assert!(sandbox_pos < projects_pos, "{toml}");
    // writable_roots is present iff the real environment resolves at
    // least one root (it always does on a dev/CI host with HOME set;
    // this stays non-brittle if some future test host truly lacks one).
    let roots = bazel_writable_roots();
    if roots.is_empty() {
        assert!(!toml.contains("writable_roots"), "{toml}");
    } else {
        let quoted: Vec<String> = roots
            .iter()
            .map(|r| toml_basic_string(&r.display().to_string()))
            .collect();
        assert!(
            toml.contains(&format!("writable_roots = [{}]", quoted.join(", "))),
            "{toml}"
        );
    }
}

#[test]
fn bazel_writable_roots_prefers_test_tmpdir() {
    assert_eq!(
        bazel_writable_roots_impl(Some("/scratch/test-tmp"), Some("/Users/test-home"), None),
        vec![PathBuf::from("/scratch/test-tmp")]
    );
}

#[test]
fn bazel_writable_roots_falls_back_to_platform_cache_dirs() {
    let roots = bazel_writable_roots_impl(None, Some("/Users/test-home"), None);
    let expected = if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/Users/test-home/Library/Caches/bazel"),
            PathBuf::from("/Users/test-home/.cache"),
        ]
    } else {
        vec![PathBuf::from("/Users/test-home/.cache/bazel")]
    };
    assert_eq!(roots, expected);
}

#[test]
fn bazel_writable_roots_prefers_xdg_cache_home_on_non_macos() {
    if cfg!(target_os = "macos") {
        return;
    }
    assert_eq!(
        bazel_writable_roots_impl(None, Some("/Users/test-home"), Some("/custom/cache")),
        vec![PathBuf::from("/custom/cache/bazel")]
    );
}

#[test]
fn bazel_writable_roots_empty_without_home_or_test_tmpdir() {
    assert_eq!(bazel_writable_roots_impl(None, None, None), Vec::<PathBuf>::new());
    assert_eq!(bazel_writable_roots_impl(Some(""), None, None), Vec::<PathBuf>::new());
}

/// Lay out a fake cube secondary jj workspace: `<workspace>/.jj/repo`
/// pointing at `<repos_root>/<repo>/.jj/repo`, mirroring what `jj` itself
/// writes for a real cube-leased checkout.
fn write_cube_jj_pointer(workspace: &Path, repo_root: &Path) {
    fs::create_dir_all(workspace.join(".jj")).unwrap();
    fs::write(
        workspace.join(".jj").join("repo"),
        repo_root.join(".jj").join("repo").display().to_string(),
    )
    .unwrap();
}

#[test]
fn cube_repo_store_root_reads_the_jj_pointer_file() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspaces").join("mono-agent-1");
    let repo_root = tmp.path().join("repos").join("mono");
    write_cube_jj_pointer(&workspace, &repo_root);

    assert_eq!(cube_repo_store_root(&workspace), Some(repo_root));
}

#[test]
fn cube_repo_store_root_none_without_pointer_file() {
    let tmp = TempDir::new().unwrap();
    // Plain checkout: no .jj at all.
    assert_eq!(cube_repo_store_root(tmp.path()), None);
}

#[test]
fn cube_repo_store_root_none_for_unexpected_pointer_shape() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(workspace.join(".jj")).unwrap();
    fs::write(workspace.join(".jj").join("repo"), "/not/a/jj/store/path").unwrap();
    assert_eq!(cube_repo_store_root(&workspace), None);
}

#[test]
fn cube_repo_store_root_resolves_relative_pointer_against_workspace_jj_dir() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspaces").join("mono-agent-1");
    let repo_root = tmp.path().join("repos").join("mono");
    fs::create_dir_all(workspace.join(".jj")).unwrap();
    // jj itself resolves a relative pointer relative to the workspace's
    // own `.jj` directory (tmp/workspaces/mono-agent-1/.jj here), so
    // reaching tmp/repos/mono/.jj/repo takes three `..` hops up to `tmp`.
    let relative_pointer = Path::new("../../../repos/mono/.jj/repo");
    fs::write(
        workspace.join(".jj").join("repo"),
        relative_pointer.display().to_string(),
    )
    .unwrap();

    let resolved = cube_repo_store_root(&workspace).unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(resolved, repo_root);
}

/// The regression this task exists for: a Codex worker in a cube
/// workspace must be granted write access to the shared jj store, or
/// every `jj describe`/`jj git fetch` in the sandbox dies with
/// `Operation not permitted` on the store's lock files.
#[test]
fn codex_config_grants_cube_shared_store_as_writable_root() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspaces").join("mono-agent-1");
    let repo_root = tmp.path().join("repos").join("mono");
    write_cube_jj_pointer(&workspace, &repo_root);

    let toml = render_base_config_toml(&workspace);
    let quoted_repo_root = toml_basic_string(&repo_root.display().to_string());
    assert!(
        toml.contains(&quoted_repo_root),
        "writable_roots must include the cube shared repo store: {toml}"
    );
}

/// Codex's workspace-write sandbox name-excludes `.git` from every
/// writable root it is granted, so granting the cube store root alone is
/// not enough: `jj git fetch`'s `FETCH_HEAD` write and `jj new`'s loose
/// object writes both land under `<store root>/.git` and get denied with
/// `Operation not permitted` even though the store root itself is
/// writable. An explicit `<store root>/.git` entry is its own top-level
/// writable root and is not subject to that auto-exclusion.
#[test]
fn render_sandbox_workspace_write_toml_grants_store_root_git_dir() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspaces").join("mono-agent-1");
    let repo_root = tmp.path().join("repos").join("mono");
    write_cube_jj_pointer(&workspace, &repo_root);

    let toml = render_sandbox_workspace_write_toml(&workspace);
    let quoted_repo_root = toml_basic_string(&repo_root.display().to_string());
    let quoted_git_dir = toml_basic_string(&repo_root.join(".git").display().to_string());
    assert!(
        toml.contains(&quoted_repo_root),
        "writable_roots must include the cube shared repo store root: {toml}"
    );
    assert!(
        toml.contains(&quoted_git_dir),
        "writable_roots must include the store root's .git dir explicitly, since Codex \
         auto-excludes .git from every granted root: {toml}"
    );
}

#[test]
fn codex_progress_ingress_is_run_correlated_rollout_jsonl() {
    let config = ProgressObservationConfig {
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        lease_id: "lease".into(),
        run_id: "run".into(),
        workspace_path: PathBuf::from("/ws"),
        forwarder_binary: PathBuf::from("/bin/boss-event"),
    };
    let driver = CodexDriver::default();
    match driver.progress_observation_wiring(&config) {
        ProgressIngress::AgentJsonlFile(file) => {
            assert_eq!(
                file.directory,
                durable_sessions_dir(&transcript_store_root().unwrap(), "codex", "run").unwrap()
            );
            assert_eq!(file.workspace_path, PathBuf::from("/ws"));
            assert_eq!(file.filename_prefix, "rollout-");
            assert_eq!(file.filename_suffix, ".jsonl");
        }
        ProgressIngress::StdoutJsonl | ProgressIngress::HookCallback(_) => {
            panic!("Codex progress must use the rollout file, not pane stdout or hooks")
        }
    }
    assert_eq!(driver.progress_fidelity(), ProgressFidelity::Rich);
}

#[test]
fn codex_pr_capture_reads_rollout_payload_output_shapes() {
    let driver = CodexDriver::default();
    let input = serde_json::json!({"command":"gh pr create --title rollout"});

    let function_output = driver
        .pr_url_capture_feed(
            "Bash",
            &input,
            &serde_json::json!("https://github.com/example/repo/pull/41\n"),
        )
        .unwrap();
    assert_eq!(function_output.command, "gh pr create --title rollout");
    assert_eq!(function_output.output_text, "https://github.com/example/repo/pull/41\n");

    let custom_output = driver
        .pr_url_capture_feed(
            "Bash",
            &input,
            &serde_json::json!([
                {"type":"input_text","text":"created"},
                {"type":"input_text","text":"https://github.com/example/repo/pull/42"}
            ]),
        )
        .unwrap();
    assert_eq!(
        custom_output.output_text,
        "created\nhttps://github.com/example/repo/pull/42"
    );
    assert!(
        driver
            .pr_url_capture_feed("Read", &input, &serde_json::json!("ignored"))
            .is_none()
    );
}

/// `continuation` is a property of the boundary itself — "something
/// pulled the agent back after it had already stopped" — not of whether
/// the process survives it. Codex has no stop-hook surface, so no rollout
/// record can ever mark a `task_complete` as re-entrant, and this stays
/// `false` even when the incoming event carries `stop_hook_active: true`
/// (which only a Claude-shaped payload would). Pinned deliberately: the
/// driver is now a persistent session, and "the process continues" must
/// not be mistaken for "the boundary was a continuation".
#[test]
fn codex_turn_boundary_on_stop_is_non_continuation() {
    let event = WorkerEvent::Stop {
        session_id: "thread-1".into(),
        stop_hook_active: true,
        stop_reason: StopReason::Completed,
    };
    let driver = CodexDriver::default();
    let boundary = driver.turn_boundary(&event).expect("Stop is a boundary");
    assert_eq!(boundary.session_id, "thread-1");
    assert_eq!(boundary.reason, StopReason::Completed);
    assert!(!boundary.continuation);
    assert!(
        driver
            .turn_boundary(&WorkerEvent::SessionStart {
                session_id: "thread-1".into(),
                source: boss_protocol::SessionStartSource::Startup,
                model: None,
            })
            .is_none()
    );
}

#[test]
fn normalize_progress_event_requires_session_meta_before_turn_events() {
    let raw = serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}});
    assert!(matches!(
        CodexDriver::default().normalize_progress_event(&raw),
        Err(NormalizeError::MissingField("session_meta.payload.id"))
    ));
}

#[test]
fn append_hooks_toml_emits_pre_tool_use_groups() {
    let base = render_base_config_toml(Path::new("/ws"));
    let guards = vec![MaterializedGuard {
        command_path: PathBuf::from("/tmp/guard.sh"),
        matcher: Some(".*"),
    }];
    let full = append_hooks_toml(&base, &guards);
    assert!(full.contains("[[hooks.PreToolUse]]"));
    assert!(full.contains("matcher = \".*\""));
    assert!(full.contains("command = \"/tmp/guard.sh\""));
}

#[test]
fn python_c_to_script_extracts_body() {
    let body = python_c_to_script(r#"python3 -c "print(1)""#).unwrap();
    assert!(body.contains("#!/usr/bin/env python3"));
    assert!(body.contains("print(1)"));
}

#[test]
fn materialize_guards_writes_executables() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let path_guard = tmp.path().join("path_guard.py");
    fs::write(&path_guard, "print('ok')\n").unwrap();
    let config = ToolUseInterceptionConfig {
        data_dir: Some(tmp.path().to_path_buf()),
        path_guard_script: Some(path_guard),
        checkleft_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: false,
        run_id: Some("r".into()),
        workspace_path: Some(tmp.path().to_path_buf()),
    };
    let guards = materialize_guards(&home, &config).unwrap();
    // path + boss-launch + codex tool-surface at minimum
    assert!(guards.len() >= 3);
    for g in &guards {
        assert!(g.command_path.is_file(), "{:?}", g.command_path);
    }
}

#[test]
fn materialize_guards_adds_static_analysis_guard_for_reviewer() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let config = ToolUseInterceptionConfig {
        data_dir: None,
        path_guard_script: None,
        checkleft_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: true,
        run_id: Some("r".into()),
        workspace_path: Some(tmp.path().to_path_buf()),
    };
    let guards = materialize_guards(&home, &config).unwrap();
    assert!(
        guards.iter().any(|guard| guard
            .command_path
            .to_string_lossy()
            .contains("reviewer_static_analysis_guard")),
        "reviewer static-analysis guard must be materialised: {guards:?}"
    );
}

/// Interception config for a local Standard worker with every guard
/// available, so a test sees the full materialised set.
fn full_interception(tmp: &Path) -> (PathBuf, ToolUseInterceptionConfig) {
    let path_guard = tmp.join("boss-path-guard.py");
    fs::write(&path_guard, "print('ok')\n").unwrap();
    let checkleft = tmp.join("boss-checkleft-push-guard.py");
    fs::write(&checkleft, "print('ok')\n").unwrap();
    let home = tmp.join("home");
    fs::create_dir_all(&home).unwrap();
    (
        home,
        ToolUseInterceptionConfig {
            data_dir: Some(tmp.to_path_buf()),
            path_guard_script: Some(path_guard),
            checkleft_guard_script: Some(checkleft),
            is_revision: true,
            is_standard_worker: true,
            is_reviewer: false,
            run_id: Some("r".into()),
            workspace_path: Some(tmp.to_path_buf()),
        },
    )
}

#[test]
fn every_guard_is_armed_through_the_trace_shim() {
    // Observability contract: Codex's rollout carries no hook record, so
    // the only way "did the guard fire?" is answerable is for every guard
    // to run under the trace shim, which records each decision and turns a
    // broken guard into a block rather than Codex's silent approval.
    let tmp = TempDir::new().unwrap();
    let (home, config) = full_interception(tmp.path());
    let guards = materialize_guards(&home, &config).unwrap();

    let shim = home.join("guards").join(GUARD_TRACE_SHIM_FILENAME);
    assert!(shim.is_file(), "the trace shim must be materialised");
    let trace = guard_trace_path(&home);
    for guard in &guards {
        let body = fs::read_to_string(&guard.command_path).unwrap();
        assert!(
            body.contains(&shim.display().to_string()),
            "guard {:?} must be invoked through the trace shim: {body}",
            guard.command_path
        );
        assert!(
            body.contains(&trace.display().to_string()),
            "guard {:?} must be told where to record: {body}",
            guard.command_path
        );
        assert!(body.contains("BOSS_GUARD_NAME="), "guard must be labelled: {body}");
        // The wrapper is the only link the attestation content-binds, so it
        // is what must vouch for the shim's bytes: otherwise replacing the
        // shim disarms every guard with every hash still valid.
        assert!(
            body.contains(&sha256_hex_prefixed(GUARD_TRACE_SHIM_SCRIPT.as_bytes())),
            "guard {:?} must verify the shim's content hash before running it: {body}",
            guard.command_path
        );
    }
}

#[test]
fn codex_tool_surface_guard_is_always_armed_on_every_tool() {
    // It is the only guard that sees non-Bash tool names, which is what
    // makes the `mcp__*` route reachable at all — a `matcher = "Bash"`
    // guard never sees a GitHub app tool call.
    let tmp = TempDir::new().unwrap();
    let (home, mut config) = full_interception(tmp.path());
    config.is_standard_worker = false;
    config.is_revision = false;
    config.data_dir = None;
    config.path_guard_script = None;
    config.checkleft_guard_script = None;

    let guards = materialize_guards(&home, &config).unwrap();
    let surface = guards
        .iter()
        .find(|guard| {
            guard
                .command_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("codex_tool_surface_guard"))
        })
        .expect("the tool-surface guard must be armed for every Codex worker kind");
    assert_eq!(
        surface.matcher,
        Some(".*"),
        "it must see every tool name, not just Bash"
    );
}

#[test]
fn path_guard_keeps_its_data_dir_env_through_the_wrapper() {
    // The path gate reads BOSS_DATA_DIR from its environment; wrapping it
    // in the trace shim must not drop that.
    let tmp = TempDir::new().unwrap();
    let (home, config) = full_interception(tmp.path());
    let guards = materialize_guards(&home, &config).unwrap();
    let path_guard = guards
        .iter()
        .find(|guard| {
            guard
                .command_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("path_guard"))
        })
        .expect("path guard must be armed for a local worker");
    let body = fs::read_to_string(&path_guard.command_path).unwrap();
    assert!(body.contains("export BOSS_DATA_DIR="), "{body}");
}

#[test]
fn guard_matchers_match_the_tool_names_codex_actually_emits() {
    // Empirically (codex-cli 0.145.0, gpt-5.6-terra) a code-mode cell's
    // `tools.exec_command` reaches PreToolUse as `tool_name: "Bash"`, so
    // the command guards keep Claude's matcher; `apply_patch` and `mcp__*`
    // arrive under their own names, which only a `.*` guard sees.
    let tmp = TempDir::new().unwrap();
    let (home, config) = full_interception(tmp.path());
    let guards = materialize_guards(&home, &config).unwrap();
    let by_name: Vec<(String, Option<&str>)> = guards
        .iter()
        .map(|guard| {
            (
                guard.command_path.file_name().unwrap().to_string_lossy().into_owned(),
                guard.matcher,
            )
        })
        .collect();
    for (name, matcher) in &by_name {
        let expected = if name.contains("path_guard") || name.contains("tool_surface") {
            ".*"
        } else {
            "Bash"
        };
        assert_eq!(matcher.unwrap(), expected, "unexpected matcher for {name}");
    }
    assert!(
        by_name.iter().any(|(name, _)| name.contains("pr_redirect_guard")),
        "a Standard worker must carry the PR-redirect guard: {by_name:?}"
    );
    assert!(
        by_name.iter().any(|(name, _)| name.contains("revision_pr_guard")),
        "a revision worker must carry the revision guard: {by_name:?}"
    );
    assert!(
        by_name.iter().any(|(name, _)| name.contains("checkleft_push_guard")),
        "a local Standard worker must carry the checkleft gate: {by_name:?}"
    );
}

#[test]
fn empty_run_id_refused_for_codex_home() {
    let err = codex_home_for_run("").expect_err("empty run_id must fail");
    assert!(
        err.to_string().contains("empty"),
        "expected empty-run_id error, got {err:#}"
    );
    assert!(sanitize_run_id_for_home("").is_err());
}

/// The root/home pair must come from one read of the homes-root env, so
/// every containment comparison downstream
/// sees a home that is genuinely a child of the root it was handed.
/// Pairing an independent `codex_homes_root()`
/// call with `codex_home_for_run()` is what made a sibling test's env
/// mutation land between the two reads and reject a valid run id.
#[test]
fn homes_root_and_home_pair_is_self_consistent() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = crate::test_support::codex_homes_override(&homes);

    let (root, home) = codex_homes_root_and_home_for_run("run-pair-1").unwrap();
    assert_eq!(root, homes);
    assert_eq!(home.parent(), Some(root.as_path()));
    assert_eq!(home, codex_home_for_run("run-pair-1").unwrap());
    assert!(codex_homes_root_and_home_for_run("").is_err());
}

#[test]
fn reviewer_sandbox_extra_args_are_output_only_workspace_write() {
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::Reviewer, false),
        "workspace-write"
    );
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::Reviewer, true),
        "workspace-write"
    );
    assert_eq!(
        codex_sandbox_extra_args(WorkerKind::Reviewer, false),
        vec!["--sandbox".to_owned(), "workspace-write".to_owned()]
    );
    // The reviewer's output-root `--cd` argument is supplied only when a
    // complete PermissionInput is available.
    let output_dir = boss_engine_structured_output::default_dir();
    assert_eq!(
        reviewer_output_sandbox_extra_args(&output_dir),
        vec!["--cd".to_owned(), output_dir.display().to_string(),]
    );
    // The rendered rules file must point `jj` navigation at the checkout the
    // reviewer prompt names, not at the `--cd` output root Codex's sandbox
    // cwd is relocated to -- the two are different paths by construction, so
    // a reviewer following the rules file's `jj log -R <path>` guidance
    // lands on real source, not empty engine scratch.
    let workspace = Path::new("/tmp/reviewer-workspace-for-jj");
    let cd_args = reviewer_output_sandbox_extra_args(&output_dir);
    assert_ne!(
        cd_args[1],
        workspace.display().to_string(),
        "the --cd output root must differ from the checkout the rules file names"
    );
    let rules = boss_pr_review::render_reviewer_claude_md("lease-1", &workspace.display().to_string(), "");
    assert!(
        rules.contains(&format!("jj log -R {}", workspace.display())),
        "rules file must instruct jj against the checkout, not the --cd root: {rules}"
    );
    assert!(
        !rules.contains(&cd_args[1]),
        "rules file must not point jj navigation at the --cd output root: {rules}"
    );

    // Final command preserves workspace-write and moves the sandbox root out
    // of the checkout through the permission artifact's `--cd` argument.
    let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-terra", "run-review-sandbox"));
    assert!(
        plan.command.contains("--sandbox workspace-write"),
        "spawn default is workspace-write: {}",
        plan.command
    );
    let merged =
        crate::apply_permission_extra_args(&plan.command, &codex_sandbox_extra_args(WorkerKind::Reviewer, false));
    assert!(
        merged.contains("--sandbox") && merged.contains("workspace-write"),
        "Reviewer must get --sandbox workspace-write after extra_args apply: {merged}"
    );
}

#[test]
fn reviewer_config_keeps_checkout_out_of_writable_roots() {
    let workspace = Path::new("/tmp/reviewer-workspace");
    let output_dir = boss_engine_structured_output::default_dir();
    let config = render_reviewer_base_config_toml(workspace, &output_dir);
    // network_access = true is required for the boss propose Unix-socket
    // report channel; exclude_tmpdir_env_var/exclude_slash_tmp keep the
    // filesystem grant narrowed to the --cd output root despite that, and
    // writable_roots grants that root explicitly rather than relying solely
    // on Codex's cwd default, in case the exclusions are applied as deny
    // rules over the surrounding $TMPDIR subtree.
    assert!(config.contains("network_access = true"), "{config}");
    assert!(config.contains("exclude_tmpdir_env_var = true"), "{config}");
    assert!(config.contains("exclude_slash_tmp = true"), "{config}");
    let writable_roots_line = config
        .lines()
        .find(|line| line.starts_with("writable_roots"))
        .unwrap_or_else(|| panic!("expected a writable_roots line: {config}"));
    assert!(
        writable_roots_line.contains(&toml_basic_string(&output_dir.display().to_string())),
        "writable_roots must grant the --cd output dir: {writable_roots_line}"
    );
    assert!(
        !writable_roots_line.contains(&toml_basic_string(&workspace.display().to_string())),
        "writable_roots must not grant the checkout: {writable_roots_line}"
    );
    assert!(
        config.contains(&toml_basic_string(&workspace.display().to_string())),
        "{config}"
    );
    assert!(
        config.contains(&toml_basic_string(&output_dir.display().to_string())),
        "{config}"
    );
}

#[test]
fn standard_worker_sandbox_defaults_to_danger_full_access() {
    // codex_sandbox_enforced=false (the feature-flag default): Standard,
    // Triage, and AnswerAgent all get danger-full-access, matching the
    // Claude driver's no-OS-sandbox posture.
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::Standard, false),
        "danger-full-access"
    );
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::Triage, false),
        "danger-full-access"
    );
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::AnswerAgent, false),
        "danger-full-access"
    );
    // codex_sandbox_enforced=true restores the OS-enforced fence.
    assert_eq!(
        codex_sandbox_for_worker_kind(WorkerKind::Standard, true),
        "workspace-write"
    );
    assert_eq!(
        codex_sandbox_extra_args(WorkerKind::Standard, true),
        vec!["--sandbox".to_owned(), "workspace-write".to_owned()]
    );
    assert_eq!(
        codex_sandbox_extra_args(WorkerKind::Standard, false),
        vec!["--sandbox".to_owned(), "danger-full-access".to_owned()]
    );
}

/// Same isolation pattern as
/// [`provision_workspace_creates_owned_home_and_snapshots_auth`]: hold
/// the homes-root override for the whole check, not only around
/// `set_var`. The previous set-then-release shape also always
/// `remove_var`'d on cleanup (not restore-prior), which could clear a
/// parallel test's override mid-flight.
#[test]
fn teardown_refuses_codex_home_outside_homes_root() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    fs::create_dir_all(&homes).unwrap();
    let outside = tmp.path().join("not-a-boss-home");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("marker"), "keep").unwrap();

    let _homes = crate::test_support::codex_homes_override(&homes);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(teardown_refuses_codex_home_outside_homes_root_body(
            tmp.path(),
            &homes,
            &outside,
        ));
}

async fn teardown_refuses_codex_home_outside_homes_root_body(
    tmp: &std::path::Path,
    homes: &std::path::Path,
    outside: &std::path::Path,
) {
    let state = CodexRuntimeState {
        codex_home: outside.to_path_buf(),
        auth_source_path: tmp.join("auth.json"),
        auth_fingerprint: "fp".into(),
        auth_policy: "SnapshotWithRefreshAdoption".into(),
    }
    .to_driver_runtime_state();

    let err = CodexDriver::default()
        .teardown_workspace(None, "run-bad", Some(&state))
        .await
        .expect_err("teardown must refuse out-of-root home");
    assert!(
        err.to_string().contains("outside") || err.to_string().contains("refusing"),
        "expected containment error, got {err:#}"
    );
    assert!(
        outside.join("marker").is_file(),
        "must not delete a path outside homes root"
    );

    // Homes root itself must never be deleted.
    let root_state = CodexRuntimeState {
        codex_home: homes.to_path_buf(),
        auth_source_path: tmp.join("auth.json"),
        auth_fingerprint: "fp".into(),
        auth_policy: "SnapshotWithRefreshAdoption".into(),
    }
    .to_driver_runtime_state();
    let err = CodexDriver::default()
        .teardown_workspace(None, "run-root", Some(&root_state))
        .await
        .expect_err("teardown must refuse homes root");
    assert!(
        err.to_string().contains("equals") || err.to_string().contains("refusing"),
        "expected root-equals error, got {err:#}"
    );
    assert!(homes.is_dir(), "homes root must remain");
}

/// The bare TUI buffers mid-turn pane input — measured by injecting
/// through the engine's own `submitText` path into a live turn, which
/// produced Codex's `Messages to be submitted after next tool call`
/// affordance and no tty leak (codex-tui-pivot-pricing V4). Declared
/// explicitly rather than inherited from the trait default, so this stays
/// asserted even if the default ever changes.
#[test]
fn codex_buffers_mid_turn_pane_input() {
    let driver = CodexDriver::default();
    assert_eq!(driver.mid_turn_pane_input(), MidTurnPaneInput::Buffers);
    assert!(driver.mid_turn_pane_input().buffers());
}

#[test]
fn codex_control_verbs_match_existing_engine_paths() {
    let driver = CodexDriver::default();
    assert_eq!(driver.probe(), ProbeDelivery::PaneText);
    assert_eq!(driver.interrupt(), InterruptDelivery::PaneEsc);
    assert_eq!(driver.stop(), StopDelivery::ProcessOnly);
    assert_eq!(driver.reap(), ReapDelivery::ProcessGroup);
}

/// The declaration the engine's process-liveness reapers key off. Codex
/// is now Persistent like Claude and Grok, so its foreground process
/// exiting is always a death — no exemption for a delivered turn
/// boundary, unlike the retired `codex exec` shape.
#[test]
fn codex_declares_persistent_lifetime() {
    let driver = CodexDriver::default();
    assert_eq!(driver.worker_process_lifetime(), WorkerProcessLifetime::Persistent);
}

/// The exact transcript captured live in
/// `docs/investigations/codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md`
/// section 2: a Codex reviewer whose artifact write was denied by
/// `--sandbox read-only`, narrating the failure and then delivering the
/// probe's fenced-JSON backstop. Before this fallback was implemented,
/// this text yielded zero candidates and the fenced JSON was discarded
/// even though the model complied with the probe exactly as asked.
#[test]
fn codex_structured_output_fallback_recovers_review_result_from_fenced_json() {
    let driver = CodexDriver::default();
    let text = r#"
I completed the read-only review. I attempted to write the review result to
.../review-result.json but I could not write it because the sandbox rejected
the write with operation not permitted. I'll provide the required fenced JSON
backstop in the final response.

```json
{
  "pr_url": "https://github.com/example/spike/pull/1",
  "head_sha": "bbbbbbb",
  "summary": "The PR changes add() from subtraction to addition, which is the intended behaviour change.",
  "revision_warranted": false,
  "findings": [],
  "regression_check": { "performed": true, "suspected_deletions": [] }
}
```
"#;

    let candidates = driver.structured_output_fallback(StructuredOutputKind::ReviewResult, text);
    assert!(
        !candidates.is_empty(),
        "fenced ReviewResult JSON must round-trip through the fallback"
    );
    assert!(
        candidates[0].explicit,
        "a fenced ```json block is an explicit candidate"
    );
    assert!(candidates[0].payload.contains("\"pr_url\""));
}

/// Codex has no prose-scrape convention for the other structured-output
/// kinds yet — those must keep returning no candidates so callers fall
/// through to the file channel rather than mis-scraping arbitrary JSON.
#[test]
fn codex_structured_output_fallback_empty_for_unimplemented_kinds() {
    let driver = CodexDriver::default();
    let text = r#"automation: task T1\n```json\n{"pr_url":"https://example.com/pr/1"}\n```"#;
    assert!(
        driver
            .structured_output_fallback(StructuredOutputKind::PrUrl, text)
            .is_empty()
    );
    assert!(
        driver
            .structured_output_fallback(StructuredOutputKind::TriageDecision, text)
            .is_empty()
    );
    assert!(
        driver
            .structured_output_fallback(StructuredOutputKind::Followups, text)
            .is_empty()
    );
    assert!(
        driver
            .structured_output_fallback(StructuredOutputKind::PostmortemFollowups, text)
            .is_empty()
    );
}
