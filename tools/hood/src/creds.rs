use anyhow::{Context, Result};
use keyring::Entry;
use serde_json::Value;

const KEYRING_SERVICE: &str = "hood.robinhood.oauth";
const KEYRING_METADATA_SERVICE: &str = "hood.robinhood.meta";
const LAST_USERNAME_KEY: &str = "last_username";

pub fn store_credentials(username: &str, token: &Value) -> Result<()> {
    let entry = oauth_entry(username)?;
    entry
        .set_password(&serde_json::to_string(token).context("failed to serialize credentials")?)
        .context("failed to write credentials to keychain")?;
    store_last_username(username)?;
    Ok(())
}

pub fn load_credentials(username: &str) -> Result<Value> {
    let entry = oauth_entry(username)?;
    let raw = entry
        .get_password()
        .context("failed to read credentials from keychain")?;
    serde_json::from_str(&raw).context("failed to parse credentials from keychain")
}

pub fn load_latest_credentials() -> Result<(String, Value)> {
    let username = load_last_username()?;
    let credentials = load_credentials(&username)?;
    Ok((username, credentials))
}

fn oauth_entry(username: &str) -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, username).context("failed to open keychain entry")
}

fn metadata_entry(key: &str) -> Result<Entry> {
    Entry::new(KEYRING_METADATA_SERVICE, key).context("failed to open metadata keychain entry")
}

fn store_last_username(username: &str) -> Result<()> {
    metadata_entry(LAST_USERNAME_KEY)?
        .set_password(username)
        .context("failed to store latest username in keychain")
}

fn load_last_username() -> Result<String> {
    metadata_entry(LAST_USERNAME_KEY)?
        .get_password()
        .context("failed to read latest username from keychain")
}
