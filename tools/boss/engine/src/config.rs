use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const DEFAULT_ACP_COMMAND: &str = "npx -y @zed-industries/claude-code-acp@0.16.1";

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub anthropic_api_key: String,
    pub acp_command: String,
    pub acp_args: Vec<String>,
    pub cwd: PathBuf,
    pub db_path: PathBuf,
}

impl RuntimeConfig {
    pub fn load_from_env() -> Result<Self> {
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY must be set before starting boss-engine")?;

        let acp_command_line =
            std::env::var("BOSS_ACP_CMD").unwrap_or_else(|_| DEFAULT_ACP_COMMAND.to_owned());
        let parts = shlex::split(&acp_command_line)
            .with_context(|| format!("could not parse BOSS_ACP_CMD: {acp_command_line}"))?;

        let Some((acp_command, acp_args)) = parts.split_first() else {
            bail!("BOSS_ACP_CMD resolved to an empty command");
        };

        let cwd = resolve_runtime_cwd()?;
        let db_path = match std::env::var_os("BOSS_DB_PATH") {
            Some(path) => PathBuf::from(path),
            None => default_db_path()?,
        };

        Ok(Self {
            anthropic_api_key,
            acp_command: acp_command.clone(),
            acp_args: acp_args.to_vec(),
            cwd,
            db_path,
        })
    }

    pub fn preflight(&self) -> Result<()> {
        if self.acp_command.contains('/') {
            let candidate = PathBuf::from(&self.acp_command);
            if !candidate.exists() {
                bail!("ACP command does not exist: {}", candidate.display());
            }
            return Ok(());
        }

        which::which(&self.acp_command).with_context(|| {
            format!(
                "ACP command not found on PATH: {} (set BOSS_ACP_CMD to override)",
                self.acp_command
            )
        })?;

        Ok(())
    }
}

fn resolve_runtime_cwd() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    std::env::current_dir().context("failed to resolve current working directory")
}

fn default_db_path() -> Result<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        bail!("HOME must be set to derive the default Boss database path");
    };

    Ok(PathBuf::from(home).join("Library/Application Support/Boss/state.db"))
}

#[cfg(test)]
mod tests {
    use super::resolve_runtime_cwd;
    use std::path::PathBuf;

    #[test]
    fn prefers_bazel_workspace_directory_when_present() {
        let original = std::env::var_os("BUILD_WORKSPACE_DIRECTORY");
        let tempdir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("BUILD_WORKSPACE_DIRECTORY", tempdir.path());
        }

        let cwd = resolve_runtime_cwd().unwrap();
        assert_eq!(cwd, PathBuf::from(tempdir.path()));

        match original {
            Some(value) => unsafe {
                std::env::set_var("BUILD_WORKSPACE_DIRECTORY", value);
            },
            None => unsafe {
                std::env::remove_var("BUILD_WORKSPACE_DIRECTORY");
            },
        }
    }
}
