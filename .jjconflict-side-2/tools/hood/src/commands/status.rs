use broker_robinhood::RobinhoodClient;
use broker_robinhood::RobinhoodClientError;
use console::{set_colors_enabled, style};
use thiserror::Error;

use crate::creds;

type Result<T> = std::result::Result<T, StatusError>;

#[derive(Debug, Error)]
pub enum StatusError {
    #[error(transparent)]
    Credentials(#[from] creds::CredentialsError),
    #[error(transparent)]
    RobinhoodClient(#[from] RobinhoodClientError),
}

pub async fn run(username: Option<&str>, _account: &str) -> Result<()> {
    set_colors_enabled(true);

    let (_, access_token) = match creds::load_access_token(username) {
        Ok(value) => {
            print_check(true, "Authenticated");
            value
        }
        Err(error) => {
            print_check(false, "Authenticated");
            print_check(false, "Connection successful");
            return Err(StatusError::Credentials(error));
        }
    };

    match verify_connection(&access_token).await {
        Ok(_) => {
            print_check(true, "Connection successful");
            Ok(())
        }
        Err(error) => {
            print_check(false, "Connection successful");
            Err(error)
        }
    }
}

async fn verify_connection(access_token: &str) -> Result<()> {
    let client = RobinhoodClient::new()?;
    client.fetch_accounts(access_token).await?;
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
