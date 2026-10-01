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
        WorkerKind::Triage | WorkerKind::AnswerAgent => {}
    }
    if revision {
        names.push("revision_pr_guard");
    }
    names
}
