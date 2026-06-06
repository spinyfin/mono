use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Result, bail};

use super::ExternalCheckCapabilities;

const GLOBAL_COMMAND_CEILING: &[&str] = &["cat", "grep", "sed", "wc"];
const BLOCKED_SHELL_BINARIES: &[&str] = &["sh", "bash", "zsh"];
const BLOCKED_PYTHON_BINARIES: &[&str] = &["python", "python3"];
const BLOCKED_NODE_BINARIES: &[&str] = &["node", "nodejs"];

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_MAX_STDOUT_BYTES: usize = 65_536;
const COMMAND_MAX_STDERR_BYTES: usize = 65_536;

// ─────────────────────────────────────────────────────────────────────────────
// PROTOTYPE-ONLY relaxations — spike: "buildifier check as a wasm external check"
//
// These constants and the `from_manifest_prototype` constructor below exist ONLY
// to let the throwaway buildifier-wasm spike run end-to-end. They are NOT the
// production command policy and MUST NOT be wired into the default path:
//
//   * `from_manifest` (the production constructor) is unchanged — it still
//     intersects manifest commands with the tight `GLOBAL_COMMAND_CEILING`.
//   * `from_manifest_prototype` is selected by the runtime ONLY when the env var
//     `CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1` is set (see runtime.rs).
//
// The real, trust-tiered capability model is a separate future design. See
// tools/checkleft/external-checks/buildifier-wasm/PROTOTYPE-NOTES.md.
//
// What is relaxed and why:
//   * ceiling: add `buildifier` (the check needs it) and `bazel` (so the guest
//     *could* resolve `buildifier_target`; the prototype guest uses a direct
//     path, but `bazel` is allowed so the target-resolution route is testable).
//   * timeout: 2s -> 120s, because `bazel build` of the buildifier target can
//     exceed 2s on a cold cache.
//   * stdout cap: 64KiB -> 4MiB, because buildifier `--format=json` on a large
//     BUILD/.bzl file (or `bazel`'s own chatter) can exceed 64KiB.
// ─────────────────────────────────────────────────────────────────────────────
const PROTOTYPE_COMMAND_CEILING: &[&str] = &["buildifier", "bazel"];
const PROTOTYPE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOTYPE_COMMAND_MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const PROTOTYPE_COMMAND_MAX_STDERR_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommandCapabilities {
    allowed_commands: Vec<String>,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
}

impl ExternalCommandCapabilities {
    pub fn from_manifest(capabilities: &ExternalCheckCapabilities) -> Result<Self> {
        let mut allowed = BTreeSet::new();
        for command in &capabilities.commands {
            validate_shell_escape_binary(command)?;
            if GLOBAL_COMMAND_CEILING.contains(&command.as_str()) {
                allowed.insert(command.clone());
            }
        }

        Ok(Self {
            allowed_commands: allowed.into_iter().collect(),
            timeout: COMMAND_TIMEOUT,
            max_stdout_bytes: COMMAND_MAX_STDOUT_BYTES,
            max_stderr_bytes: COMMAND_MAX_STDERR_BYTES,
        })
    }

    /// PROTOTYPE-ONLY. Builds capabilities using the relaxed prototype ceiling,
    /// timeout, and stdout/stderr caps so the buildifier-wasm spike can invoke
    /// `buildifier` (and, optionally, `bazel`). The runtime selects this only when
    /// `CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1`. Never use in production.
    ///
    /// Commands are still intersected with a ceiling — the union of the production
    /// `GLOBAL_COMMAND_CEILING` and `PROTOTYPE_COMMAND_CEILING` — so a manifest
    /// still cannot request arbitrary binaries, and shell binaries remain blocked.
    pub fn from_manifest_prototype(capabilities: &ExternalCheckCapabilities) -> Result<Self> {
        let mut allowed = BTreeSet::new();
        for command in &capabilities.commands {
            validate_shell_escape_binary(command)?;
            if GLOBAL_COMMAND_CEILING.contains(&command.as_str())
                || PROTOTYPE_COMMAND_CEILING.contains(&command.as_str())
            {
                allowed.insert(command.clone());
            }
        }

        Ok(Self {
            allowed_commands: allowed.into_iter().collect(),
            timeout: PROTOTYPE_COMMAND_TIMEOUT,
            max_stdout_bytes: PROTOTYPE_COMMAND_MAX_STDOUT_BYTES,
            max_stderr_bytes: PROTOTYPE_COMMAND_MAX_STDERR_BYTES,
        })
    }

    pub fn allowed_commands(&self) -> &[String] {
        &self.allowed_commands
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX)
    }

    pub fn max_stdout_bytes(&self) -> usize {
        self.max_stdout_bytes
    }

    pub fn max_stderr_bytes(&self) -> usize {
        self.max_stderr_bytes
    }

    pub fn validate_invocation(&self, program: &str, args: &[String]) -> Result<()> {
        if !self
            .allowed_commands
            .iter()
            .any(|allowed| allowed == program)
        {
            bail!("command `{program}` is not allowed for this check");
        }
        validate_shell_escape_invocation(program, args)
    }
}

fn validate_shell_escape_binary(program: &str) -> Result<()> {
    if BLOCKED_SHELL_BINARIES.contains(&program) {
        bail!("command `{program}` is blocked by sandbox policy");
    }
    Ok(())
}

fn validate_shell_escape_invocation(program: &str, args: &[String]) -> Result<()> {
    if BLOCKED_SHELL_BINARIES.contains(&program) {
        bail!("command `{program}` is blocked by sandbox policy");
    }

    if BLOCKED_PYTHON_BINARIES.contains(&program) && args.iter().any(|arg| arg == "-c") {
        bail!("inline python execution (`{program} -c ...`) is blocked by sandbox policy");
    }

    if BLOCKED_NODE_BINARIES.contains(&program)
        && args.iter().any(|arg| arg == "-e" || arg == "--eval")
    {
        bail!("inline node execution (`{program} -e/--eval`) is blocked by sandbox policy");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ExternalCommandCapabilities;
    use crate::external::ExternalCheckCapabilities;

    #[test]
    fn intersects_manifest_commands_with_global_ceiling() {
        let capabilities = ExternalCheckCapabilities {
            commands: vec!["grep".to_owned(), "curl".to_owned(), "sed".to_owned()],
        };

        let resolved = ExternalCommandCapabilities::from_manifest(&capabilities).expect("resolve");
        assert_eq!(
            resolved.allowed_commands(),
            &["grep".to_owned(), "sed".to_owned()]
        );
    }

    #[test]
    fn rejects_shell_binary_in_manifest() {
        let capabilities = ExternalCheckCapabilities {
            commands: vec!["sh".to_owned()],
        };

        let error = ExternalCommandCapabilities::from_manifest(&capabilities).expect_err("reject");
        assert!(error.to_string().contains("blocked by sandbox policy"));
    }

    #[test]
    fn validate_invocation_rejects_unlisted_program() {
        let resolved = ExternalCommandCapabilities::from_manifest(&ExternalCheckCapabilities {
            commands: vec!["grep".to_owned()],
        })
        .expect("resolve");

        let error = resolved
            .validate_invocation("sed", &[])
            .expect_err("must reject");
        assert!(error.to_string().contains("not allowed"));
    }

    #[test]
    fn validate_invocation_blocks_python_c_flag() {
        let resolved = ExternalCommandCapabilities {
            allowed_commands: vec!["python".to_owned()],
            timeout: std::time::Duration::from_secs(1),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        };

        let error = resolved
            .validate_invocation("python", &["-c".to_owned(), "print(1)".to_owned()])
            .expect_err("must reject");
        assert!(error.to_string().contains("inline python execution"));
    }

    #[test]
    fn validate_invocation_blocks_node_eval() {
        let resolved = ExternalCommandCapabilities {
            allowed_commands: vec!["node".to_owned()],
            timeout: std::time::Duration::from_secs(1),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        };

        let error = resolved
            .validate_invocation("node", &["-e".to_owned(), "console.log(1)".to_owned()])
            .expect_err("must reject");
        assert!(error.to_string().contains("inline node execution"));
    }

    // ── PROTOTYPE-ONLY: relaxed ceiling tests ────────────────────────────────

    #[test]
    fn prototype_ceiling_admits_buildifier_and_bazel_but_not_arbitrary() {
        let capabilities = ExternalCheckCapabilities {
            commands: vec![
                "buildifier".to_owned(),
                "bazel".to_owned(),
                "cat".to_owned(),
                "curl".to_owned(),
            ],
        };

        let resolved =
            ExternalCommandCapabilities::from_manifest_prototype(&capabilities).expect("resolve");
        // buildifier + bazel admitted via the prototype ceiling, cat via the global
        // ceiling; curl is admitted by neither.
        assert_eq!(
            resolved.allowed_commands(),
            &["bazel".to_owned(), "buildifier".to_owned(), "cat".to_owned()]
        );
        // Relaxed limits are in effect.
        assert_eq!(resolved.timeout(), super::PROTOTYPE_COMMAND_TIMEOUT);
        assert_eq!(
            resolved.max_stdout_bytes(),
            super::PROTOTYPE_COMMAND_MAX_STDOUT_BYTES
        );
    }

    #[test]
    fn production_ceiling_still_rejects_buildifier() {
        // The production constructor must NOT have been widened by the prototype.
        let capabilities = ExternalCheckCapabilities {
            commands: vec!["buildifier".to_owned()],
        };
        let resolved = ExternalCommandCapabilities::from_manifest(&capabilities).expect("resolve");
        assert!(
            resolved.allowed_commands().is_empty(),
            "production policy must not admit buildifier; got {:?}",
            resolved.allowed_commands()
        );
        assert_eq!(resolved.timeout(), super::COMMAND_TIMEOUT);
    }

    #[test]
    fn prototype_ceiling_still_blocks_shell_binaries() {
        let capabilities = ExternalCheckCapabilities {
            commands: vec!["bash".to_owned()],
        };
        let error = ExternalCommandCapabilities::from_manifest_prototype(&capabilities)
            .expect_err("reject");
        assert!(error.to_string().contains("blocked by sandbox policy"));
    }
}
