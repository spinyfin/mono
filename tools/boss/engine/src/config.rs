use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const DEFAULT_ACP_COMMAND: &str = "pnpm --filter @mono/claude-code-acp exec claude-code-acp";

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub anthropic_api_key: String,
    pub acp_command: String,
    pub acp_args: Vec<String>,
    pub cwd: PathBuf,
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

        let cwd = std::env::current_dir().context("failed to resolve current working directory")?;

        Ok(Self {
            anthropic_api_key,
            acp_command: acp_command.clone(),
            acp_args: acp_args.to_vec(),
            cwd,
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
