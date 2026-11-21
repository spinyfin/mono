//! Host-side authorization proof, run by `write_permission_config` before the
//! pane command can be built.

use std::fs;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use super::{
    AuthorizationProbeReport, HOOK_GROUP, authorization_hooks_map, expected_authorization_policy, hooks_file_path,
    observation_from_permission, policy_path, probe_authorization, process_home_for_run, session_settings_path_for_run,
    verify_attestation,
};
use crate::{PermissionInput, ToolUseInterceptionConfig};

/// Verify the installed chain for the expected run before sending any brief.
///
/// `AgyDriver::write_permission_config` calls this after permission
/// installation, with the same [`PermissionInput`] used to arm the run, and
/// fails the run if it fails. This compares the installed policy
/// to the shared expected renderer (guard commands, matchers, timeouts, required
/// artifact paths, revision flag, run identity) and the session settings to the
/// per-kind render, then checks exact lifecycle wiring and executes the existing
/// harmless positive, structural-deny and reused-guard-deny probes. Recheck the
/// binding afterwards so changes during the probe cannot produce a successful
/// result. No SessionStart event, hook count, or cached success is accepted as
/// proof.
///
/// This proves the installed host-side authorization path, not CLI discovery.
/// The launcher proves discovery before it pastes the brief: it waits for the
/// CLI's own startup log to show the rendered settings and `loaded 1 named
/// hooks from 1 hooks.json file(s)`, and refuses the run if the CLI reports an
/// unparseable hooks file. The persisted launch flag controls allocated traffic;
/// these startup checks apply to both allocated and explicitly pinned launches.
pub fn verify_startup_authorization(input: &PermissionInput) -> anyhow::Result<AuthorizationProbeReport> {
    let expected = observation_from_permission(input);
    let interception = ToolUseInterceptionConfig::from_permission_input(input);
    let home = process_home_for_run(&expected.run_id)?;
    super::super::home::assert_agy_posture(&home, &expected.workspace_path)?;
    let before = verify_attestation(&home).context("agy startup authorization binding")?;
    let policy: Value =
        serde_json::from_slice(&fs::read(policy_path(&home)).context("reading agy startup authorization policy")?)
            .context("agy startup authorization policy is malformed")?;
    let expected_policy = expected_authorization_policy(&home, input.worker_kind, &expected, &interception)?;
    if policy != expected_policy {
        bail!("agy startup authorization policy does not match the expected guard chain");
    }
    let installed: Value =
        serde_json::from_slice(&fs::read(hooks_file_path(&home)).context("reading agy startup hooks")?)
            .context("agy startup hooks are malformed")?;
    if installed != json!({ HOOK_GROUP: authorization_hooks_map(&expected)? }) {
        bail!("agy startup hooks do not match the complete expected lifecycle wiring");
    }
    let settings_path = session_settings_path_for_run(&expected.run_id)?;
    let installed_settings = super::super::permissions::parse_rendered(
        &fs::read_to_string(&settings_path).context("reading agy startup session settings")?,
    )
    .context("agy startup session settings are malformed")?;
    let expected_settings = super::super::permissions::render_settings_for_input(input)
        .context("rendering expected agy session settings")?;
    if installed_settings != expected_settings {
        bail!("agy startup session settings do not match the expected per-kind render");
    }
    let report = probe_authorization(&home).context("agy startup authorization liveness")?;
    if before != verify_attestation(&home)? {
        bail!("agy startup authorization binding changed during the probe");
    }
    Ok(report)
}
