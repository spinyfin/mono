//! Is the armed `PreToolUse` guard chain *still* there, right now?
//!
//! # The question this answers, and why it is not the one the trace answers
//!
//! [`super::guard_trace`] records what a guard **did** when it ran. It is
//! evidence in the past tense, and it only exists when a guard was actually
//! invoked. That leaves one condition it structurally cannot report: a turn in
//! which Codex invoked no guard *because there was nothing left to invoke*.
//! Codex's hook failures are silent and fail-open — an unexecutable handler
//! produces no stream event, no log line and no error — so "the guard approved
//! nothing this turn" and "the guard no longer exists" produce byte-identical
//! observations from the trace alone.
//!
//! A guard record is evidence about the turn that wrote it, not about the
//! chain's present state, so liveness is established per turn rather than
//! remembered. Over a session spanning hours and many turns the difference is
//! measurable:
//!
//! ```text
//! turn 1   `echo one-canary`   → 5 guard records, command runs
//! (between turns: $CODEX_HOME/guards removed)
//! turn 2   `echo two-canary`   → 0 guard records, command runs anyway
//! ```
//!
//! Measured on codex-cli 0.145.0 against a live TUI session; see
//! `tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`.
//! Turn 2 ran unguarded and nothing in Codex's stream said so.
//!
//! # How liveness is established instead
//!
//! The property being asserted — "the guards are armed and reachable" — is
//! directly checkable, because Boss materialised the chain itself and wrote
//! [`super::HOOK_TRUST_ATTESTATION_FILENAME`] naming every hook command and
//! its content hash, alongside the config that declares and trusts them. So
//! the reader re-checks liveness against disk at each turn boundary rather
//! than inferring it from history.
//!
//! The check is orthogonal to the trace, not a replacement for it: it stops at
//! the first bad entry, so a run that lost one wrapper of five still has four
//! guards running and recording. The caller reports both.

use std::path::{Path, PathBuf};

use boss_engine_codex_hook_trust::{read_attestation_file, verify_armed_chain_on_disk};

use super::HOOK_TRUST_ATTESTATION_FILENAME;

/// Whether the guards Codex would invoke are still on disk, unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ArmedChainStatus {
    /// No Boss-owned `CODEX_HOME` is known to this reader, so there is nothing
    /// to check. The stateless path and every rollout fixture land here.
    Unknown,
    /// Every attested hook command is still a regular executable file with the
    /// bytes that were attested.
    Intact,
    /// The chain is gone or altered. Carries the operator-facing detail.
    Broken(String),
}

/// Absolute path of the arming attestation for a run's `CODEX_HOME`.
pub(super) fn attestation_path(codex_home: &Path) -> PathBuf {
    codex_home.join(HOOK_TRUST_ATTESTATION_FILENAME)
}

/// Re-check the armed chain under `codex_home`.
///
/// A missing or unparseable attestation is [`ArmedChainStatus::Broken`], not
/// [`ArmedChainStatus::Unknown`]: `write_hooks_and_attest` writes that file
/// before the worker starts, so for a run that has one at all, its absence
/// later means Boss can no longer prove anything about its own guardrails.
/// Fail closed, exactly as the guards themselves do.
pub(super) fn armed_chain_status(codex_home: Option<&Path>) -> ArmedChainStatus {
    let Some(codex_home) = codex_home else {
        return ArmedChainStatus::Unknown;
    };
    let path = attestation_path(codex_home);
    let attestation = match read_attestation_file(&path) {
        Ok(attestation) => attestation,
        Err(err) => return ArmedChainStatus::Broken(err.to_string()),
    };
    match verify_armed_chain_on_disk(&attestation) {
        Ok(()) => ArmedChainStatus::Intact,
        Err(err) => ArmedChainStatus::Broken(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use boss_engine_codex_hook_trust::{
        HookAttestationEntry, HookTrustAttestation, ObservationProof, sha256_hex_prefixed, write_attestation_file,
    };
    use tempfile::TempDir;

    use super::*;

    /// A `CODEX_HOME` shaped like one `write_hooks_and_attest` leaves behind:
    /// a config that declares and trusts one hook, an executable wrapper, and
    /// an attestation binding its bytes.
    fn armed_home(dir: &Path) -> PathBuf {
        let guards = dir.join("guards");
        fs::create_dir_all(&guards).unwrap();
        let wrapper = guards.join("00_path_guard.sh");
        let body = "#!/bin/sh\nexit 0\n";
        fs::write(&wrapper, body).unwrap();
        fs::write(
            dir.join("config.toml"),
            format!(
                "[[hooks.PreToolUse]]\n\
                 matcher = \".*\"\n\
                 [[hooks.PreToolUse.hooks]]\n\
                 type = \"command\"\n\
                 command = \"{}\"\n\
                 \n\
                 [hooks.state.\"k\"]\n\
                 trusted_hash = \"sha256:whatever\"\n",
                wrapper.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let attestation = HookTrustAttestation {
            codex_home: dir.display().to_string(),
            config_path: dir.join("config.toml").display().to_string(),
            generated_at_unix: 0,
            hooks: vec![
                HookAttestationEntry::builder()
                    .key("k")
                    .event("pre_tool_use")
                    .command(wrapper.display().to_string())
                    .matcher(".*")
                    .trusted_hash("sha256:whatever")
                    .guard_content_sha256(sha256_hex_prefixed(body.as_bytes()))
                    .observed_trust_status("trusted")
                    .build(),
            ],
            observation: ObservationProof::HooksList { codex_version: None },
        };
        write_attestation_file(&attestation_path(dir), &attestation).unwrap();
        wrapper
    }

    #[test]
    fn no_codex_home_is_unknown_not_broken() {
        // Fixtures and the stateless path have nothing to check; reporting a
        // disarmed chain for them would be a fabricated alarm.
        assert_eq!(armed_chain_status(None), ArmedChainStatus::Unknown);
    }

    #[test]
    fn a_freshly_armed_home_is_intact() {
        let dir = TempDir::new().unwrap();
        armed_home(dir.path());
        assert_eq!(armed_chain_status(Some(dir.path())), ArmedChainStatus::Intact);
    }

    #[test]
    fn a_removed_guard_chain_is_broken() {
        // The measured mid-session failure: the wrapper Codex invokes is gone,
        // so Codex skips the hook silently and every guardrail is inert.
        let dir = TempDir::new().unwrap();
        armed_home(dir.path());
        fs::remove_dir_all(dir.path().join("guards")).unwrap();
        assert!(matches!(
            armed_chain_status(Some(dir.path())),
            ArmedChainStatus::Broken(_)
        ));
    }

    #[test]
    fn an_edited_guard_is_broken() {
        // Content binding, not just existence: a wrapper swapped for one that
        // approves everything keeps the path Codex invokes intact.
        let dir = TempDir::new().unwrap();
        let wrapper = armed_home(dir.path());
        fs::write(&wrapper, "#!/bin/sh\necho '{\"decision\":\"approve\"}'\n").unwrap();
        assert!(matches!(
            armed_chain_status(Some(dir.path())),
            ArmedChainStatus::Broken(_)
        ));
    }

    #[test]
    fn a_config_that_lost_its_trust_state_is_broken() {
        // The wrappers are untouched, but Codex skips an untrusted hook
        // silently — so the chain is as inert as if they had been deleted.
        let dir = TempDir::new().unwrap();
        armed_home(dir.path());
        let config = dir.path().join("config.toml");
        let raw = fs::read_to_string(&config).unwrap();
        fs::write(&config, raw.replace("sha256:whatever", "sha256:0000")).unwrap();
        assert!(matches!(
            armed_chain_status(Some(dir.path())),
            ArmedChainStatus::Broken(_)
        ));
    }

    #[test]
    fn a_missing_attestation_is_broken() {
        // Fail closed: the file is written before the worker starts, so its
        // absence means Boss cannot prove its guardrails are armed.
        let dir = TempDir::new().unwrap();
        armed_home(dir.path());
        fs::remove_file(attestation_path(dir.path())).unwrap();
        assert!(matches!(
            armed_chain_status(Some(dir.path())),
            ArmedChainStatus::Broken(_)
        ));
    }
}
