//! Provider usage via the vendor CLI and the worker login-keychain bridge.
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use super::environment::{AgyProcessEnvironment, run_agy_bounded};
use super::home::{agy_homes_root, provision_quota_home, reclaim_agy_run};

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
    let runtime = provision_quota_home(workspace).map_err(|_| {
        anyhow::anyhow!("Could not provision isolated agy quota home; check agy installation and sign-in")
    })?;
    let result = (|| {
        let environment = AgyProcessEnvironment::resolve(
            &runtime.process_home,
            &runtime.log_file,
            &runtime.session_settings_path,
            runtime.agy_binary.clone(),
        )?;
        let mut command = Command::new(environment.agy_binary());
        command
            .args(environment.launch_args())
            .args(["--print", "/usage", "--output-format", "json"])
            .current_dir(workspace)
            .stdin(Stdio::null());
        environment.apply_to_command(&mut command);
        let output = run_agy_bounded(&mut command, Duration::from_secs(20))
            .map_err(|_| anyhow::anyhow!("agy quota command failed or timed out"))?;
        anyhow::ensure!(output.status.success(), "agy quota command failed; check agy sign-in");
        String::from_utf8(output.stdout).map_err(|_| anyhow::anyhow!("agy quota output was not UTF-8"))
    })();
    // Preserve diagnostic logs using the same retention policy as workers.
    let cleanup = reclaim_agy_run(&runtime.process_home);
    cleanup.map_err(|_| anyhow::anyhow!("Could not reclaim agy quota home"))?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agy::test_environment::env_for_provision_with_home_override;

    #[test]
    fn usage_runs_only_in_scoped_home_with_keychain_bridge_and_reclaims_it() {
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
shift 4
test "$*" = '--print /usage --output-format json' || exit 5
printf '%s' '{"status":"SUCCESS","num_turns":0}'
"#,
        )
        .unwrap();
        assert_eq!(usage_json().unwrap(), r#"{"status":"SUCCESS","num_turns":0}"#);
        assert!(
            !homes
                .join(format!("quota-probe-{}/process-home", std::process::id()))
                .exists()
        );
        assert_eq!(std::fs::read_to_string(keychain).unwrap(), "dummy");
    }
}
