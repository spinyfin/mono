use anyhow::{Context, Result, bail};
use broker_robinhood::RobinhoodClient;
use console::{set_colors_enabled, style};

use crate::creds;

pub async fn run(username: Option<&str>) -> Result<()> {
    set_colors_enabled(true);

    let (_, access_token) = match creds::load_access_token(username) {
        Ok(value) => {
            print_check(true, "Authenticated");
            value
        }
        Err(error) => {
            print_check(false, "Authenticated");
            print_check(false, "Connection successful");
            bail!("{error:#}");
        }
    };

    match verify_connection(&access_token).await {
        Ok(_) => {
            print_check(true, "Connection successful");
            Ok(())
        }
        Err(error) => {
            print_check(false, "Connection successful");
            bail!("{error:#}");
        }
    }
}

async fn verify_connection(access_token: &str) -> Result<()> {
    let client = RobinhoodClient::new().context("failed to initialize Robinhood client")?;
    client
        .fetch_accounts(access_token)
        .await
        .context("authenticated API call failed")?;
    Ok(())
}

fn print_check(ok: bool, message: &str) {
    let status = if ok {
        style("[OK]").green()
    } else {
        style("[ERR]").red()
    };
    println!("{status} {message}");
}
