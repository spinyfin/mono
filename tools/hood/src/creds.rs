use anyhow::{Context, Result};
use keyring::Entry;
use serde_json::Value;

const KEYRING_SERVICE: &str = "hood.robinhood.oauth";

pub fn store_credentials(username: &str, token: &Value) -> Result<()> {
    let entry = keyring_entry(username)?;
    entry
        .set_password(&serde_json::to_string(token).context("failed to serialize credentials")?)
        .context("failed to write credentials to keychain")?;
    Ok(())
}

#[allow(dead_code)]
pub fn load_credentials(username: &str) -> Result<Value> {
    let entry = keyring_entry(username)?;
    let raw = entry
        .get_password()
        .context("failed to read credentials from keychain")?;
    serde_json::from_str(&raw).context("failed to parse credentials from keychain")
}

fn keyring_entry(username: &str) -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, username).context("failed to open keychain entry")
}
