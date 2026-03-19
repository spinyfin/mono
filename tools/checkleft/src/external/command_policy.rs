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

    pub fn allowed_commands(&self) -> &[String] {
        &self.allowed_commands
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
}
