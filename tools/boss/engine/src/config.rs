use anyhow::{Context, Result};

pub struct RuntimeConfig {
    pub anthropic_api_key: String,
}

impl RuntimeConfig {
    pub fn load_from_env() -> Result<Self> {
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY must be set before starting boss-engine")?;

        Ok(Self { anthropic_api_key })
    }
}
