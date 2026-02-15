use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use boss_engine::app;
use boss_engine::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    app::run(cli).await
}
