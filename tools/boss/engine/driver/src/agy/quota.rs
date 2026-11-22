//! Provider usage via the vendor CLI and the worker login-keychain bridge.
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;

use super::environment::{AgyProcessEnvironment, run_agy_bounded};
use super::home::{
    QUOTA_PROBE_RUN_ID, agy_homes_root, agy_run_container_for_run, provision_quota_home, reclaim_quota_probe_container,
    reclaim_stale_quota_probe_containers,
};
use super::probe_isolation;

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
    reclaim_stale_quota_probe_containers()?;
    let container = agy_run_container_for_run(QUOTA_PROBE_RUN_ID)?;
    let runtime = match provision_quota_home(workspace) {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = reclaim_quota_probe_container(&container);
            return Err(err);
        }
    };
    let result = (|| {
        let attempts = runtime.control_dir.join("tool-attempts.log");
        probe_isolation::install_standard_fail_closed_posture(
            &runtime,
            workspace,
            &runtime.session_settings_path,
            "true",
            &attempts,
        )?;
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

#[cfg(test)]
mod tests {
    use super::super::environment::{AGY_BIN_ENV, AGY_SKIP_BINARY_ASSERT_ENV};
    use super::super::home::{AGY_HOMES_ROOT_ENV, AGY_SKIP_POSTURE_ASSERT_ENV};
    use super::super::test_environment::{EnvGuard, env_for_provision_with_home_override, transcript_store_for};
    use super::*;

    fn leftover_probe_paths(homes: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            homes.join(QUOTA_PROBE_RUN_ID),
            transcript_store_for(homes).join(QUOTA_PROBE_RUN_ID),
        )
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
case "$HOME" in */quota-probe/process-home) ;; *) exit 4;; esac
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
grep -q '/bin/cat' "$HOME/.gemini/config/hooks.json" || exit 10
grep -q 'exit 2' "$HOME/.gemini/config/hooks.json" || exit 11
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

    #[test]
    fn stale_quota_probe_container_is_removed_before_provision() {
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
printf '%s' '{"status":"SUCCESS","num_turns":0}'
"#,
        )
        .unwrap();
        let stale = homes.join(QUOTA_PROBE_RUN_ID);
        let leftover_pid = homes.join("quota-probe-99999");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("marker"), "old").unwrap();
        std::fs::create_dir_all(&leftover_pid).unwrap();
        std::fs::write(leftover_pid.join("marker"), "old").unwrap();
        assert_eq!(usage_json().unwrap(), r#"{"status":"SUCCESS","num_turns":0}"#);
        assert!(!stale.exists(), "canonical stale quota container leaked");
        assert!(!leftover_pid.exists(), "pid-keyed leftover quota container leaked");
        let (container, transcripts) = leftover_probe_paths(&homes);
        assert!(!container.exists(), "quota container must be deleted after the probe");
        assert!(
            !transcripts.exists(),
            "quota probe must not leave a transcript-store entry"
        );
    }
}
