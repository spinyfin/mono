use anyhow::{Context, Result, bail};
use broker_robinhood::RobinhoodClient;
use serde_json::Value;

use crate::creds;

const ROBINHOOD_API_BASE_URL: &str = "https://api.robinhood.com";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

pub async fn run(username: Option<&str>) -> Result<()> {
    let (username, credentials) = match load_stored_credentials(username) {
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

    let access_token = match extract_access_token(&credentials) {
        Some(token) => token,
        None => {
            print_check(false, "Connection successful");
            bail!("stored credentials for `{username}` are missing a valid access token");
        }
    };

    match verify_connection(access_token).await {
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

fn load_stored_credentials(username: Option<&str>) -> Result<(String, Value)> {
    match username {
        Some(username) => creds::load_credentials(username)
            .with_context(|| format!("failed to load credentials for `{username}`"))
            .map(|credentials| (username.to_string(), credentials)),
        None => creds::load_latest_credentials()
            .context("failed to load credentials for the most recently authenticated user"),
    }
}

fn extract_access_token(credentials: &Value) -> Option<&str> {
    credentials
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

async fn verify_connection(access_token: &str) -> Result<()> {
    let client = RobinhoodClient::new(ROBINHOOD_API_BASE_URL)
        .context("failed to initialize Robinhood client")?;
    client
        .fetch_accounts(access_token)
        .await
        .context("authenticated API call failed")?;
    Ok(())
}

fn print_check(ok: bool, message: &str) {
    let (color, status) = if ok { (GREEN, "[OK]") } else { (RED, "[ERR]") };
    println!("{color}{status}{RESET} {message}");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_access_token;

    #[test]
    fn extract_access_token_returns_none_when_missing() {
        let credentials = json!({});
        assert_eq!(extract_access_token(&credentials), None);
    }

    #[test]
    fn extract_access_token_returns_none_when_blank() {
        let credentials = json!({ "access_token": "   " });
        assert_eq!(extract_access_token(&credentials), None);
    }

    #[test]
    fn extract_access_token_returns_trimmed_token() {
        let credentials = json!({ "access_token": "  abc123  " });
        assert_eq!(extract_access_token(&credentials), Some("abc123"));
    }
}
