use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod creds;

#[derive(Debug, Parser)]
#[command(name = "hood", about = "Robinhood CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Authenticate with Robinhood and store OAuth credentials in the system keychain.
    Auth {
        /// Print extra diagnostics with sensitive fields redacted.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Auth { verbose } => commands::auth::run(verbose).await?,
    }

    Ok(())
}
