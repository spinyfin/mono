//! Host-side authorization prerequisite for a future pane launcher.

use std::fs;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use super::{
    AuthorizationProbeReport, HOOK_GROUP, approval_for, authorization_hooks_map, hooks_file_path, policy_path,
    probe_authorization, process_home_for_run, verify_attestation, worker_kind_name,
};
use crate::{ProgressObservationConfig, WorkerKind};

/// Verify the installed chain for the expected run before sending any brief.
///
/// Call after permission installation, immediately before launch. This checks
/// exact lifecycle wiring and run identity, then executes the existing harmless
/// positive, structural-deny and reused-guard-deny probes. Recheck the binding
/// afterwards so changes during the probe cannot produce a successful result.
/// No SessionStart event, hook count, or cached success is accepted as proof.
///
/// This proves the installed host-side authorization path, not CLI discovery
/// or review-mode failure semantics. Pane qualification must still establish
/// those properties and verify the CLI settings log before delivering a brief;
/// `AgyDriver::launch_ready` remains false until that complete path is qualified.
pub fn verify_startup_authorization(
    expected: &ProgressObservationConfig,
    worker_kind: WorkerKind,
) -> anyhow::Result<AuthorizationProbeReport> {
    let home = process_home_for_run(&expected.run_id)?;
    super::super::home::assert_agy_posture(&home, &expected.workspace_path)?;
    let before = verify_attestation(&home).context("agy startup authorization binding")?;
    let policy: Value = serde_json::from_slice(&fs::read(policy_path(&home))?)?;
    if policy["run_id"] != expected.run_id
        || policy["worker_kind"] != worker_kind_name(worker_kind)
        || policy["approval"] != approval_for(worker_kind)
        || policy["process_home"] != json!(home)
        || policy["workspace_path"] != json!(expected.workspace_path)
        || policy["forwarder"] != json!(expected.forwarder_binary)
    {
        bail!("agy startup authorization policy does not match the expected run");
    }
    let installed: Value = serde_json::from_slice(&fs::read(hooks_file_path(&home))?)?;
    if installed != json!({ HOOK_GROUP: authorization_hooks_map(expected)? }) {
        bail!("agy startup hooks do not match the complete expected lifecycle wiring");
    }
    let report = probe_authorization(&home).context("agy startup authorization liveness")?;
    if before != verify_attestation(&home)? {
        bail!("agy startup authorization binding changed during the probe");
    }
    Ok(report)
}
