use anyhow::Result;

use crate::cli::{Cli, Mode};
use crate::config::RuntimeConfig;

pub async fn run(cli: Cli) -> Result<()> {
    let _cfg = RuntimeConfig::load_from_env()?;

    match cli.mode {
        Mode::Cli => {
            println!("boss-engine cli mode scaffold initialized");
        }
        Mode::Server => {
            println!("boss-engine server mode scaffold initialized");
        }
    }

    Ok(())
}
