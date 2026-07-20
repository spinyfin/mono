//! Token handling. This crate knows nothing about *where* the Trunk org API
//! token lives (macOS Keychain, env var, config file) — that is the caller's
//! concern. [`TrunkTokenProvider`] is the only seam; [`SecretString`] just
//! keeps the token out of accidental `{:?}`/log output.

use crate::error::TrunkError;

/// A string that redacts itself in `Debug` output. This is not a
/// cryptographic protection — it only prevents the token from leaking into a
/// stray `tracing`/`{:?}` line.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The real token value, for building a request.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Supplies the Trunk org API token on demand. Implementations live on the
/// caller's side (e.g. an engine-side Keychain-backed provider); this crate
/// never reads a keychain, env var, or config file itself.
pub trait TrunkTokenProvider: Send + Sync {
    /// Return the current token, or [`TrunkError::Auth`] if none is
    /// available (e.g. not yet provisioned). Called once per request so a
    /// provider can rotate its token without the client being rebuilt.
    fn token(&self) -> Result<SecretString, TrunkError>;
}

/// A [`TrunkTokenProvider`] that always returns the same fixed token — the
/// common case (an env var or a value read once at startup).
#[derive(Clone)]
pub struct StaticTokenProvider(SecretString);

impl StaticTokenProvider {
    pub fn new(token: impl Into<SecretString>) -> Self {
        Self(token.into())
    }
}

impl TrunkTokenProvider for StaticTokenProvider {
    fn token(&self) -> Result<SecretString, TrunkError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_token() {
        let secret = SecretString::new("super-secret-token");
        assert_eq!(format!("{secret:?}"), "SecretString(REDACTED)");
    }

    #[test]
    fn expose_secret_returns_the_real_value() {
        let secret = SecretString::new("super-secret-token");
        assert_eq!(secret.expose_secret(), "super-secret-token");
    }

    #[test]
    fn static_provider_returns_its_fixed_token() {
        let provider = StaticTokenProvider::new("fixed-token");
        assert_eq!(provider.token().unwrap().expose_secret(), "fixed-token");
    }
}
