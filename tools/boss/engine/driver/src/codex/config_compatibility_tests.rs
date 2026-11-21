//! Exercise the production permission path with Codex's semantic config loader.
//! Bazel supplies the checksum-pinned deployed version on every CI platform.

use super::*;
use boss_engine_codex_hook_trust::read_attestation_file;

#[test]
fn every_worker_config_arms_hooks_on_pinned_codex() {
    let version = Command::new(resolve_codex_bin()).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("codex-cli {}", env!("CODEX_CLI_VERSION")),
    );

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let _homes = crate::test_support::codex_homes_override(&root.join("homes"));
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    // These externally supplied scripts are only content-bound, never executed
    // by hooks/list. All Codex-owned guards and wrappers are production output.
    let path_guard = root.join("path-guard.py");
    let checkleft_guard = root.join("checkleft-guard.py");
    write_executable(&path_guard, "#!/usr/bin/env python3\n").unwrap();
    write_executable(&checkleft_guard, "#!/usr/bin/env python3\n").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let driver = CodexDriver::default();

    for kind in [
        WorkerKind::Standard,
        WorkerKind::Reviewer,
        WorkerKind::Triage,
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        for remote in [false, true] {
            for (execution_kind, task_kind) in [
                ("task_implementation", None),
                ("revision_implementation", None),
                ("task_implementation", Some("revision")),
            ] {
                let run_id = format!("{kind:?}-{remote}-{execution_kind}-{task_kind:?}");
                let input = PermissionInput::builder()
                    .worker_kind(kind)
                    .workspace_path(workspace.clone())
                    .events_socket_path(root.join("events.sock"))
                    .frontend_socket_path(root.join("frontend.sock"))
                    .boss_event_path(root.join("boss-event"))
                    .run_id(&run_id)
                    .lease_id("compatibility-lease")
                    .execution_kind(execution_kind)
                    .maybe_task_kind(task_kind)
                    .is_remote(remote)
                    .path_guard_script(path_guard.clone())
                    .checkleft_guard_script(checkleft_guard.clone())
                    .build();
                let home = codex_home_for_run(&run_id).unwrap();
                fs::create_dir_all(&home).unwrap();
                let artifacts = rt
                    .block_on(driver.write_permission_config(&input, &root))
                    .unwrap_or_else(|error| panic!("{run_id}: permission materialization failed: {error:#}"));
                assert_eq!(artifacts.config_files, vec![home.join("config.toml")]);
                let attestation = read_attestation_file(&guard_chain::attestation_path(&home)).unwrap();
                let expected = expected_guards(
                    kind,
                    remote,
                    execution_kind == "revision_implementation" || task_kind.is_some(),
                );
                assert_eq!(attestation.hooks.len(), expected.len(), "{run_id}");
                for (index, (hook, name)) in attestation.hooks.iter().zip(expected).enumerate() {
                    assert_eq!(hook.event, "pre_tool_use", "{run_id}: {name}");
                    assert_eq!(hook.observed_trust_status, "trusted", "{run_id}: {name}");
                    assert!(hook.guard_content_sha256.is_some(), "{run_id}: {name}");
                    assert_eq!(
                        Path::new(&hook.command).file_name().unwrap().to_str().unwrap(),
                        format!("{index:02}_{name}.sh"),
                        "{run_id}",
                    );
                }
            }
        }
    }
}

fn expected_guards(kind: WorkerKind, remote: bool, revision: bool) -> Vec<&'static str> {
    let mut names = Vec::new();
    if !remote {
        names.push("path_guard");
    }
    names.extend(["boss_launch_guard", "codex_tool_surface_guard"]);
    // Exhaustive so a new worker kind requires an explicit contract decision.
    match kind {
        WorkerKind::Standard => {
            names.push("pr_redirect_guard");
            if !remote {
                names.push("checkleft_push_guard");
            }
        }
        WorkerKind::Reviewer => names.extend(["reviewer_static_analysis_guard", "reviewer_publish_guard"]),
        WorkerKind::ReviewGuide => names.push("review_guide_guard"),
        WorkerKind::Progression => names.push("progression_guard"),
        WorkerKind::Triage | WorkerKind::AnswerAgent => {}
    }
    if revision {
        names.push("revision_pr_guard");
    }
    names
}

#[test]
fn missing_review_guide_permissions_reports_loader_error() {
    use boss_engine_codex_hook_trust::{ArmRequest, CommandHookSpec, HookEvent, arm_and_attest};
    use std::os::unix::fs::PermissionsExt;
    let binary = PathBuf::from(std::env::var_os("BOSS_TEST_CODEX").expect("Bazel must provide pinned Codex"))
        .canonicalize()
        .unwrap();
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("codex-cli {}", env!("CODEX_CLI_VERSION")),
    );

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    let workspace = root.join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let guard = root.join("guard.sh");
    fs::write(&guard, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = render_review_guide_config(&workspace, &root.join("guide.sock"));
    config.push_str(&format!(
        "\n[[hooks.PreToolUse]]\nmatcher = \".*\"\nhooks = [{{ type = \"command\", command = {} }}]\n",
        super::toml_basic_string(&guard.display().to_string()),
    ));
    let request = ArmRequest {
        codex_home: home.clone(),
        config_path: home.join("config.toml"),
        cwd: workspace,
        hooks: vec![
            CommandHookSpec::builder()
                .event(HookEvent::PreToolUse)
                .matcher(".*")
                .command(guard)
                .build(),
        ],
        codex_bin: binary,
    };

    // Reproduce the pre-fix failure through the unchanged production gate.
    // No CLI overrides: this is exactly how the pre-spawn observer loads it.
    fs::write(
        &request.config_path,
        config.replace("default_permissions = \"review-guide\"\n", ""),
    )
    .unwrap();
    let error = arm_and_attest(&request).expect_err("missing profile selection must refuse the worker");
    let text = error.to_string();
    assert!(text.contains("hooks/list returned no hook entries"), "{text}");
    assert!(
        text.contains("default_permissions"),
        "refusal must carry Codex's loader error, got {text}"
    );

    fs::write(&request.config_path, config).unwrap();
    let attestation = arm_and_attest(&request).expect("rendered guide config must load and arm its guard");
    assert_eq!(attestation.hooks.len(), 1);
    assert_eq!(attestation.hooks[0].observed_trust_status, "trusted");
    assert!(attestation.hooks[0].guard_content_sha256.is_some());
}

/// A progression run provisioned by the production path in a working
/// directory whose path crosses a symlink, as its scratch directory under the
/// system temp directory does on macOS (`/var/folders` is a symlink to
/// `/private/var/folders`).
struct SymlinkedProgressionRun {
    /// The environment overrides the run was provisioned under, innermost
    /// lock first so it is released first.
    _env: (
        crate::test_support::TranscriptStoreOverride,
        crate::test_support::CodexAuthSourceOverride,
    ),
    root: PathBuf,
    home: PathBuf,
    /// The rollout source the engine arms for this run before the pane exists.
    ingress: crate::discovery::PreparedSource,
    /// Dropped last: everything above lives under it.
    _temp: tempfile::TempDir,
}

const SYMLINKED_RUN_BRIEF: &str = "Reply with the single word OK and nothing else.";

fn symlinked_progression_run(run_id: &str) -> SymlinkedProgressionRun {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("real").join("ws")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let workspace = root.join("link").join("ws");
    assert_ne!(workspace, workspace.canonicalize().unwrap());

    // An API key the pinned CLI accepts as signed in. No request it makes can
    // succeed: the test sandbox has no external network, and the launch below
    // points the model endpoint at a closed local port.
    let auth = root.join("auth.json");
    fs::write(&auth, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-not-a-real-key"}"#).unwrap();
    let auth_override = crate::test_support::codex_auth_source_override(&root.join("homes"), &auth);
    let transcripts = crate::test_support::transcript_store_override(&root.join("transcripts"));

    let path_guard = root.join("path-guard.py");
    write_executable(&path_guard, "#!/usr/bin/env python3\n").unwrap();
    let driver = CodexDriver::default();
    let input = PermissionInput::builder()
        .worker_kind(WorkerKind::Progression)
        .workspace_path(workspace.clone())
        .events_socket_path(root.join("events.sock"))
        .frontend_socket_path(root.join("frontend.sock"))
        .boss_event_path(root.join("boss-event"))
        .run_id(run_id)
        .lease_id("")
        .execution_kind("progression")
        .is_remote(false)
        .path_guard_script(path_guard)
        .build();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            driver
                .provision_workspace(&workspace, SYMLINKED_RUN_BRIEF, run_id)
                .await
                .expect("provision the run");
            driver
                .write_permission_config(&input, &root)
                .await
                .expect("arm the progression profile and guards");
        });
    let home = codex_home_for_run(run_id).unwrap();

    let ProgressIngress::AgentJsonlFile(ingress) = driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: root.join("events.sock"),
        lease_id: String::new(),
        run_id: run_id.to_owned(),
        workspace_path: workspace.clone(),
        forwarder_binary: root.join("boss-event"),
    }) else {
        panic!("Codex progress rides its rollout file");
    };
    let ingress = crate::discovery::PreparedSource::new(ingress).expect("arm rollout discovery");

    // `codex` resolved to the pinned release, for the spawn line's bare name.
    fs::create_dir_all(root.join("bin")).unwrap();
    std::os::unix::fs::symlink(resolve_codex_bin(), root.join("bin").join("codex")).unwrap();

    SymlinkedProgressionRun {
        _env: (transcripts, auth_override),
        root,
        home,
        ingress,
        _temp: temp,
    }
}

impl SymlinkedProgressionRun {
    /// The workspace as the engine names it: through the symlink.
    fn workspace(&self) -> PathBuf {
        self.root.join("link").join("ws")
    }

    fn path_with_pinned_codex(&self) -> std::ffi::OsString {
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        std::env::join_paths(paths).unwrap()
    }

    /// The header of the trust entry Codex itself looks up: the workspace's
    /// physical path.
    fn physical_trust_header(&self) -> String {
        format!(
            "[projects.{}]",
            toml_basic_string(&self.workspace().canonicalize().unwrap().display().to_string())
        )
    }
}

/// The cause of the failure this file's next test fixes, pinned against the
/// real CLI: a workspace trusted only under the engine's spelling makes Codex
/// open its folder-trust dialog. The dialog is watched through the production
/// launcher's own terminal view, which also pins that the launcher's readiness
/// check, with the production Codex markers, refuses to deliver while the
/// dialog is on screen.
#[test]
fn codex_asks_to_trust_a_folder_stamped_only_under_the_engines_spelling() {
    let run = symlinked_progression_run("progression-untrusted-symlink");
    let config_path = run.home.join("config.toml");
    let rendered = fs::read_to_string(&config_path).unwrap();
    let physical_trust = format!("{}\ntrust_level = \"trusted\"\n\n", run.physical_trust_header());
    let logical_only = rendered.replace(&physical_trust, "");
    assert_ne!(
        logical_only, rendered,
        "the rendered config must trust the physical path"
    );
    fs::write(&config_path, &logical_only).unwrap();

    // Nothing is ever delivered: the readiness check is replaced by one that
    // only watches. The run ends the moment the dialog is fully drawn, so no
    // step depends on how fast the CLI starts.
    let harness = r#"
import importlib.util, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("launcher", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
config = json.loads((root / "initial-prompt.json").read_text())
dialog = sys.argv[2].encode()
judge = launcher.ready_for_prompt

class DialogShown(Exception):
    pass

def watch(view, raw, paste_enabled, markers, starting_markers):
    screen = view.tail()
    # Fully drawn once its selected row, which carries the composer's prefix,
    # is up beside the question.
    if dialog in screen and any(marker in screen for marker in markers):
        raise DialogShown("refuses" if not judge(view, True, True, markers, starting_markers) else "delivers")
    return False

launcher.ready_for_prompt = watch
try:
    launcher.run(sys.argv[3:], b"never delivered",
                 [marker.encode() for marker in config["agent_markers"]],
                 [marker.encode() for marker in config["starting_markers"]], readiness_timeout=40)
    outcome = "the driver exited"
except DialogShown as shown:
    outcome = f"dialog shown; the launcher {shown}"
except Exception as error:
    outcome = f"{type(error).__name__}: {error}"
(root / "outcome.txt").write_text(outcome)
"#;
    let config_dir = run.workspace().join(".codex");
    let mut pane = boss_engine_test_process::spawn_supervised("python3", |command| {
        command
            .args(["-I", "-c", harness])
            .arg(&config_dir)
            .arg(CODEX_FOLDER_TRUST_DIALOG_MARKER)
            .args([
                "codex",
                "--strict-config",
                "--no-alt-screen",
                "-a",
                "never",
                "-m",
                ASTRA_MODEL,
            ])
            .current_dir(run.workspace())
            .env("CODEX_HOME", &run.home)
            .env("PATH", run.path_with_pinned_codex())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while pane.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the CLI showed neither the dialog nor an error"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    assert_eq!(
        fs::read_to_string(config_dir.join("outcome.txt")).expect("the harness records what happened"),
        "dialog shown; the launcher refuses"
    );
    // Accepting the dialog makes Codex save the decision under the physical path.
    assert!(
        !fs::read_to_string(&config_path)
            .unwrap()
            .contains(&run.physical_trust_header()),
        "nothing answered the dialog"
    );
    let scan = crate::discovery::scan_once(&run.ingress).unwrap();
    assert!(
        scan.matched.is_empty() && scan.rejected.is_empty(),
        "no turn can have started"
    );
}

/// The production launch of a Codex progression run in a symlinked working
/// directory: the spawn line, the rendered profile, the launcher and the
/// armed rollout source are all the engine's own. The brief must become a
/// turn whose rollout the engine's discovery attaches. This is the
/// reproduction of "no driver hook or session event arrived within 120s after
/// prompt delivery" for every Codex progression spawn on macOS.
#[test]
fn a_progression_launch_in_a_symlinked_workspace_attaches_its_rollout() {
    let run = symlinked_progression_run("progression-symlinked-attach");
    let driver = CodexDriver::default();
    let plan = driver.spawn_invocation(SpawnRequest {
        model: ASTRA_MODEL,
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some("progression-symlinked-attach"),
    });
    let extra_args = codex_sandbox_extra_args(WorkerKind::Progression, false);
    // The production pane command, plus one override that keeps the turn's
    // model request on a closed local port.
    let command = format!(
        "{} -c 'openai_base_url=\"http://127.0.0.1:1/v1\"'",
        crate::apply_permission_extra_args(&plan.command, &extra_args).trim_end()
    );
    let mut pane = boss_engine_test_process::spawn_supervised("sh", |pane_command| {
        pane_command
            .args(["-c", &command])
            .current_dir(run.workspace())
            .env("PATH", run.path_with_pinned_codex())
            .stdout(std::process::Stdio::null())
            .stderr(fs::File::create(run.root.join("pane.err")).unwrap());
        for directive in &plan.env {
            match directive {
                EnvDirective::Set(key, value) => pane_command.env(key, value),
                EnvDirective::Unset(key) => pane_command.env_remove(key),
            };
        }
    });

    let home = crate::initial_prompt::delivery_home(&run.workspace().join(".codex"));
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let attached = loop {
        // A scan that races the first write is retried, as discovery does.
        if let Ok(scan) = crate::discovery::scan_once(&run.ingress)
            && let Some(candidate) = scan.matched.into_iter().next()
        {
            break candidate;
        }
        let exited = pane.try_wait().unwrap();
        assert!(
            exited.is_none() && std::time::Instant::now() < deadline,
            "no rollout attached (pane exit {exited:?}); launcher steps {:?}; launcher stderr {:?}",
            crate::initial_prompt::read_delivery_log(&home),
            fs::read_to_string(run.root.join("pane.err")).unwrap_or_default(),
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Discovery attaches on the rollout's first line; the turn's own record
    // follows it.
    while !fs::read_to_string(&attached.path)
        .unwrap()
        .contains(SYMLINKED_RUN_BRIEF)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the attached rollout never recorded the brief as a turn"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(pane);

    let steps: Vec<String> = crate::initial_prompt::read_delivery_log(&home)
        .into_iter()
        .map(|step| step.step)
        .collect();
    assert_eq!(steps, ["ready", "paste_sent", "paste_landed", "submit_sent"]);
    assert!(
        attached.path.starts_with(run.root.join("transcripts")),
        "the rollout must land in the run's durable transcript directory: {}",
        attached.path.display()
    );
}
