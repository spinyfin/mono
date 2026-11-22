//! Provider usage via the vendor CLI and the worker login-keychain bridge.
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;

use super::environment::{AgyProcessEnvironment, run_agy_bounded};
use super::home::{
    AgyRuntimeState, agy_homes_root, agy_run_container_for_run, provision_quota_home, quota_probe_run_id,
    reclaim_quota_probe_container,
};
use super::permissions;

// A timed-out cache reader must not start a second CLI in the same home
// while its blocking probe is still being reaped.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// No worker slot or model turn: /usage is a local print-mode command.
/// Errors deliberately omit CLI output, which can contain account details.
pub fn usage_json() -> anyhow::Result<String> {
    let _guard = PROBE_LOCK
        .try_lock()
        .map_err(|_| anyhow::anyhow!("agy quota check already running"))?;
    let workspace = agy_homes_root().join("quota-workspace");
    std::fs::create_dir_all(&workspace)?;
    usage_in(&workspace)
}

fn usage_in(workspace: &Path) -> anyhow::Result<String> {
    let container = agy_run_container_for_run(&quota_probe_run_id())?;
    let runtime = match provision_quota_home(workspace) {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = reclaim_quota_probe_container(&container);
            return Err(err);
        }
    };
    let result = (|| {
        install_quota_fail_closed_posture(&runtime, workspace)?;
        let environment = AgyProcessEnvironment::resolve(
            &runtime.process_home,
            &runtime.log_file,
            &runtime.session_settings_path,
            runtime.agy_binary.clone(),
        )?;
        let mut command = Command::new(environment.agy_binary());
        command
            .args(environment.launch_args())
            .arg("--settings")
            .arg(&runtime.session_settings_path)
            .args(["--print", "/usage", "--output-format", "json"])
            .current_dir(workspace)
            .stdin(Stdio::null());
        environment.apply_to_command(&mut command);
        let output = run_agy_bounded(&mut command, Duration::from_secs(20))
            .map_err(|_| anyhow::anyhow!("agy quota command failed or timed out"))?;
        anyhow::ensure!(output.status.success(), "agy quota command failed; check agy sign-in");
        String::from_utf8(output.stdout).map_err(|_| anyhow::anyhow!("agy quota output was not UTF-8"))
    })();
    let cleanup = reclaim_quota_probe_container(&container);
    match (result, cleanup) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err).context("Could not reclaim agy quota home"),
    }
}

/// Pin `agentMode` / `toolPermission` through `--settings` and install a
/// deny-all PreToolUse hook so a qualified binary that treated `/usage` as a
/// prompt cannot run an auto-approving tool turn in this HOME.
fn install_quota_fail_closed_posture(runtime: &AgyRuntimeState, workspace: &Path) -> anyhow::Result<()> {
    let workspace_abs = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let hooks_dir = runtime.process_home.join(".gemini/config");
    let rules_root = runtime
        .process_home
        .parent()
        .context("quota probe container")?
        .join("rules-root");
    let settings = permissions::render_settings(
        &permissions::SettingsPlan::builder()
            .worker_kind(crate::WorkerKind::Standard)
            .workspace(&workspace_abs)
            .hooks_dir(&hooks_dir)
            .control_dir(&runtime.control_dir)
            .rules_root(&rules_root)
            .build(),
    )?;
    std::fs::write(&runtime.session_settings_path, settings.to_json()?).with_context(|| {
        format!(
            "writing quota session settings {}",
            runtime.session_settings_path.display()
        )
    })?;
    std::fs::write(hooks_dir.join("hooks.json"), quota_deny_all_hooks_json())
        .with_context(|| format!("writing quota deny-all hooks under {}", hooks_dir.display()))?;
    Ok(())
}

fn quota_deny_all_hooks_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({"boss": {
        "SessionStart": [{"type": "command", "command": "true"}],
        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command":
            "/usr/bin/printf '%s\\n' '{\"decision\":\"deny\",\"reason\":\"quota probe forbids tools\"}'"
        }]}]
    }}))
    .expect("quota deny-all hooks are serializable")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::super::environment::{AGY_BIN_ENV, AGY_SKIP_BINARY_ASSERT_ENV};
    use super::super::home::{AGY_HOMES_ROOT_ENV, AGY_SKIP_POSTURE_ASSERT_ENV};
    use super::super::test_environment::{EnvGuard, env_for_provision_with_home_override, transcript_store_for};
    use super::*;

    fn leftover_probe_paths(homes: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let id = format!("quota-probe-{}", std::process::id());
        (homes.join(&id), transcript_store_for(homes).join(&id))
    }

    #[test]
    fn usage_runs_only_in_scoped_home_with_keychain_bridge_and_deletes_the_container() {
        let tmp = tempfile::TempDir::new().unwrap();
        let host = tmp.path().join("host");
        let keychain = host.join(crate::scoped_home::LOGIN_KEYCHAIN_LEAF);
        std::fs::create_dir_all(keychain.parent().unwrap()).unwrap();
        std::fs::write(&keychain, "dummy").unwrap();
        let _home = crate::test_support::home_override(&host);
        let homes = tmp.path().join("homes");
        let _env = env_for_provision_with_home_override(&homes);
        std::fs::create_dir_all(homes.join("bin")).unwrap();
        crate::hook_guards::write_executable(
            &homes.join("bin/agy"),
            r#"#!/bin/sh
test -L "$HOME/Library/Keychains/login.keychain-db" || exit 1
test "$AGY_CLI_DISABLE_AUTO_UPDATE" = true || exit 2
test "$AGY_CLI_HIDE_ACCOUNT_INFO" = true || exit 3
case "$HOME" in */quota-probe-*/process-home) ;; *) exit 4;; esac
settings=
print=
format=
while [ $# -gt 0 ]; do
  case "$1" in
    --settings) settings=$2; shift 2 ;;
    --print) print=$2; shift 2 ;;
    --output-format) format=$2; shift 2 ;;
    --log-file|--add-dir) shift 2 ;;
    *) exit 5 ;;
  esac
done
test "$print" = /usage || exit 5
test "$format" = json || exit 5
test -n "$settings" || exit 6
grep -q '"agentMode": ""' "$settings" || exit 7
grep -q '"toolPermission": "request-review"' "$settings" || exit 8
grep -q 'PreToolUse' "$HOME/.gemini/config/hooks.json" || exit 9
printf '%s' '{"status":"SUCCESS","num_turns":0}'
"#,
        )
        .unwrap();
        assert_eq!(usage_json().unwrap(), r#"{"status":"SUCCESS","num_turns":0}"#);
        let (container, transcripts) = leftover_probe_paths(&homes);
        assert!(
            !container.exists(),
            "quota container must be deleted: {}",
            container.display()
        );
        assert!(
            !transcripts.exists(),
            "quota probe must not leave a transcript-store entry: {}",
            transcripts.display()
        );
        assert_eq!(std::fs::read_to_string(keychain).unwrap(), "dummy");
    }

    #[test]
    fn provisioning_failure_deletes_the_quota_container_and_leaves_no_transcript_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let host = tmp.path().join("host");
        let keychain = host.join(crate::scoped_home::LOGIN_KEYCHAIN_LEAF);
        std::fs::create_dir_all(keychain.parent().unwrap()).unwrap();
        std::fs::write(&keychain, "dummy").unwrap();
        let _home = crate::test_support::home_override(&host);
        let homes = tmp.path().join("homes");
        std::fs::create_dir_all(&homes).unwrap();
        let missing = tmp.path().join("agy-not-installed");
        let _env = EnvGuard::set_with_lock(
            &[
                (AGY_HOMES_ROOT_ENV, homes.to_str().unwrap()),
                (AGY_SKIP_POSTURE_ASSERT_ENV, "1"),
                (AGY_SKIP_BINARY_ASSERT_ENV, "0"),
                (AGY_BIN_ENV, missing.to_str().unwrap()),
            ],
            None,
        )
        .with_transcript_store(&transcript_store_for(&homes));
        assert!(usage_json().is_err());
        let (container, transcripts) = leftover_probe_paths(&homes);
        assert!(
            !container.exists(),
            "partial quota container leaked: {}",
            container.display()
        );
        assert!(
            !transcripts.exists(),
            "failed quota probe left a transcript-store entry: {}",
            transcripts.display()
        );
    }
}
