use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod commands;
mod creds;

#[derive(Debug, Parser)]
#[command(name = "hood", about = "Robinhood CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Args)]
struct CommonFlags {
    /// Robinhood username to use. Uses the most recently authenticated user when omitted.
    #[arg(long, short = 'u')]
    username: Option<String>,

    /// Robinhood account number to use. `default` is an alias for the default account.
    #[arg(long, default_value = "default")]
    account: String,
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
    Accounts(CommonFlags),
    /// Verify stored credentials and connectivity to Robinhood APIs.
    Status(CommonFlags),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Auth { verbose } => commands::auth::run(verbose).await?,
        Command::Accounts(common) => {
            commands::accounts::run(common.username.as_deref(), &common.account).await?
        }
        Command::Status(common) => {
            commands::status::run(common.username.as_deref(), &common.account).await?
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn accounts_defaults_account_to_default_alias() {
        let cli = Cli::parse_from(["hood", "accounts"]);

        match cli.command {
            Command::Accounts(common) => {
                assert_eq!(common.account, "default");
                assert_eq!(common.username, None);
            }
            _ => panic!("expected accounts command"),
        }
    }

    #[test]
    fn status_allows_overriding_common_flags() {
        let cli = Cli::parse_from([
            "hood",
            "status",
            "--username",
            "alice",
            "--account",
            "12345678",
        ]);

        match cli.command {
            Command::Status(common) => {
                assert_eq!(common.username.as_deref(), Some("alice"));
                assert_eq!(common.account, "12345678");
            }
            _ => panic!("expected status command"),
        }
    }
}
