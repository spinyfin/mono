use anyhow::{Context, Result};
use broker_robinhood::RobinhoodClient;
use broker_robinhood::client::RobinhoodAccount;

use crate::creds;

pub async fn run(username: Option<&str>) -> Result<()> {
    let (_, access_token) = creds::load_access_token(username)?;

    let client = RobinhoodClient::new().context("failed to initialize Robinhood client")?;
    let accounts = client
        .fetch_accounts(&access_token)
        .await
        .context("failed to fetch Robinhood accounts")?;

    if accounts.is_empty() {
        println!("No Robinhood accounts found.");
        return Ok(());
    }

    for account in &accounts {
        println!("{}", format_account_line(account));
    }

    Ok(())
}

fn format_account_line(account: &RobinhoodAccount) -> String {
    let account_type = account
        .brokerage_account_type
        .as_deref()
        .unwrap_or("Unknown");
    let suffix = if account.is_default {
        " [default]"
    } else {
        ""
    };
    format!("{} ({account_type}){suffix}", account.account_number)
}

#[cfg(test)]
mod tests {
    use broker_robinhood::client::RobinhoodAccount;

    use super::format_account_line;

    #[test]
    fn format_account_line_includes_default_marker() {
        let account = RobinhoodAccount {
            account_number: "1234".to_string(),
            brokerage_account_type: Some("Cash".to_string()),
            is_default: true,
        };

        assert_eq!(format_account_line(&account), "1234 (Cash) [default]");
    }

    #[test]
    fn format_account_line_handles_missing_account_type() {
        let account = RobinhoodAccount {
            account_number: "5678".to_string(),
            brokerage_account_type: None,
            is_default: false,
        };

        assert_eq!(format_account_line(&account), "5678 (Unknown)");
    }
}
