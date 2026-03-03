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
    /// List Robinhood accounts for an authenticated user.
    Accounts {
        /// Robinhood username to use. Uses the most recently authenticated user when omitted.
        #[arg(long, short = 'u')]
        username: Option<String>,
    },
    /// Verify stored credentials and connectivity to Robinhood APIs.
    Status {
        /// Robinhood username to check. Uses the most recently authenticated user when omitted.
        #[arg(long, short = 'u')]
        username: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Auth { verbose } => commands::auth::run(verbose).await?,
        Command::Accounts { username } => commands::accounts::run(username.as_deref()).await?,
        Command::Status { username } => commands::status::run(username.as_deref()).await?,
    }

    Ok(())
}
