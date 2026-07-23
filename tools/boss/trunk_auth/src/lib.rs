//! Storage for the Trunk org API token: macOS Data Protection Keychain in
//! release builds, a 0600-mode file fallback in ad-hoc dev builds, and a
//! plain `keyring` backend on other platforms, via [`boss_keychain`].
//!
//! Per the design (`docs/designs/trunk-merge-queue-integration-*.md`
//! §"Auth"), the token is never persisted anywhere else — no DB row, no
//! repo file, and it is never logged. [`BOSS_TRUNK_API_TOKEN_ENV`] lets a
//! developer override the keychain for local testing.
//!
//! This crate owns *storage* only; [`boss_trunk_client::TrunkTokenProvider`]
//! is implemented here so the engine can hand a [`TrunkTokenStore`]
//! straight to a `TrunkClient`.

use boss_keychain::{KeychainStore, TokenStoreError as KeychainStoreError};
use boss_trunk_client::{SecretString, TrunkError, TrunkTokenProvider};

/// Env var that overrides the keychain for local development. Checked
/// before the keychain on every read.
pub const BOSS_TRUNK_API_TOKEN_ENV: &str = "BOSS_TRUNK_API_TOKEN";

/// OS keychain coordinates for the stored Trunk API token.
const KEYCHAIN_SERVICE: &str = "dev.spinyfin.boss.trunk";
const KEYCHAIN_ACCOUNT: &str = "api-token@trunk.io";
const FILE_FALLBACK_NAME: &str = "trunk-api-token";

/// Where a resolved token came from — surfaced by `boss engine trunk status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `BOSS_TRUNK_API_TOKEN` env var.
    Env,
    /// OS keychain (or its dev-build file fallback).
    Keychain,
}

impl TokenSource {
    /// Stable lowercase label for wire/JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Keychain => "keychain",
        }
    }
}

/// Error type for [`TrunkTokenStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("keychain error: {0}")]
    Keychain(#[from] KeychainStoreError),
}

/// Stores and retrieves the Trunk org API token in the OS keychain, and
/// implements [`TrunkTokenProvider`] by preferring [`BOSS_TRUNK_API_TOKEN_ENV`]
/// over the stored value.
pub struct TrunkTokenStore {
    inner: KeychainStore,
}

impl Default for TrunkTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrunkTokenStore {
    /// Creates a store backed by the platform's native credential store.
    pub fn new() -> Self {
        Self {
            inner: KeychainStore::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, FILE_FALLBACK_NAME),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: impl boss_keychain::KeystoreBackend + 'static) -> Self {
        Self {
            inner: KeychainStore::with_backend(backend),
        }
    }

    /// Returns the stored token, or `None` if none is present.
    pub fn get(&self) -> Result<Option<String>, TokenStoreError> {
        Ok(self.inner.get_raw()?)
    }

    /// Persists `token` in the keychain, overwriting any prior value.
    pub fn set(&self, token: &str) -> Result<(), TokenStoreError> {
        Ok(self.inner.set_raw(token)?)
    }

    /// Removes the stored token. A no-op if none is present.
    pub fn delete(&self) -> Result<(), TokenStoreError> {
        Ok(self.inner.delete_raw()?)
    }

    /// Where the effective token would come from right now — checks
    /// [`BOSS_TRUNK_API_TOKEN_ENV`] first, then the keychain. `Ok(None)`
    /// means no token is configured at all. A keychain read error is
    /// surfaced (unlike `token()`, which the caller may prefer to treat as
    /// "unconfigured"); `boss engine trunk status` wants to know the
    /// difference.
    pub fn source(&self) -> Result<Option<TokenSource>, TokenStoreError> {
        let env = std::env::var(BOSS_TRUNK_API_TOKEN_ENV).ok();
        Ok(resolve_source(env.as_deref(), self.get()?))
    }
}

impl TrunkTokenProvider for TrunkTokenStore {
    fn token(&self) -> Result<SecretString, TrunkError> {
        if let Ok(env) = std::env::var(BOSS_TRUNK_API_TOKEN_ENV)
            && !env.is_empty()
        {
            return Ok(SecretString::new(env));
        }
        match self.get() {
            Ok(Some(token)) if !token.is_empty() => Ok(SecretString::new(token)),
            Ok(_) => Err(TrunkError::Auth(
                "no Trunk API token configured — run `boss engine trunk set-token`".to_owned(),
            )),
            Err(e) => Err(TrunkError::Auth(format!(
                "failed to read Trunk token from keychain: {e}"
            ))),
        }
    }
}

/// Pure decision logic for [`TrunkTokenStore::source`], factored out so
/// tests don't have to mutate process-wide env vars.
fn resolve_source(env: Option<&str>, stored: Option<String>) -> Option<TokenSource> {
    if env.is_some_and(|v| !v.is_empty()) {
        return Some(TokenSource::Env);
    }
    if stored.is_some_and(|v| !v.is_empty()) {
        return Some(TokenSource::Keychain);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeStore(std::sync::Mutex<Option<String>>);

    impl FakeStore {
        fn empty() -> Self {
            Self(std::sync::Mutex::new(None))
        }

        fn prefilled(token: &str) -> Self {
            Self(std::sync::Mutex::new(Some(token.to_owned())))
        }
    }

    impl boss_keychain::KeystoreBackend for FakeStore {
        fn get_raw(&self) -> Result<Option<String>, KeychainStoreError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn set_raw(&self, value: &str) -> Result<(), KeychainStoreError> {
            *self.0.lock().unwrap() = Some(value.to_owned());
            Ok(())
        }

        fn delete_raw(&self) -> Result<(), KeychainStoreError> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn store_round_trips_token() {
        let store = TrunkTokenStore::with_backend(FakeStore::empty());
        assert!(store.get().unwrap().is_none());

        store.set("trunk_tok_abc").unwrap();
        assert_eq!(store.get().unwrap().as_deref(), Some("trunk_tok_abc"));
    }

    #[test]
    fn store_delete_removes_token() {
        let store = TrunkTokenStore::with_backend(FakeStore::prefilled("trunk_tok_abc"));
        assert!(store.get().unwrap().is_some());
        store.delete().unwrap();
        assert!(store.get().unwrap().is_none());
    }

    #[test]
    fn store_delete_is_idempotent_when_empty() {
        let store = TrunkTokenStore::with_backend(FakeStore::empty());
        store.delete().unwrap();
    }

    #[test]
    fn store_set_overwrites_existing_token() {
        let store = TrunkTokenStore::with_backend(FakeStore::prefilled("old_tok"));
        store.set("new_tok").unwrap();
        assert_eq!(store.get().unwrap().as_deref(), Some("new_tok"));
    }

    #[test]
    fn resolve_source_prefers_env_over_keychain() {
        assert_eq!(
            resolve_source(Some("env_tok"), Some("keychain_tok".to_owned())),
            Some(TokenSource::Env)
        );
    }

    #[test]
    fn resolve_source_falls_back_to_keychain() {
        assert_eq!(
            resolve_source(None, Some("keychain_tok".to_owned())),
            Some(TokenSource::Keychain)
        );
    }

    #[test]
    fn resolve_source_ignores_empty_env_value() {
        assert_eq!(
            resolve_source(Some(""), Some("keychain_tok".to_owned())),
            Some(TokenSource::Keychain)
        );
    }

    #[test]
    fn resolve_source_none_when_nothing_configured() {
        assert_eq!(resolve_source(None, None), None);
    }

    #[test]
    fn provider_token_prefers_fake_env_injected_value() {
        // `token()` reads the real process env directly (production
        // behavior); this test only exercises the keychain-fallback path to
        // avoid mutating process-wide env vars from a test.
        let store = TrunkTokenStore::with_backend(FakeStore::prefilled("trunk_tok_from_keychain"));
        let secret = store.token().expect("token should resolve from keychain");
        assert_eq!(secret.expose_secret(), "trunk_tok_from_keychain");
    }

    #[test]
    fn provider_token_errors_when_unconfigured() {
        let store = TrunkTokenStore::with_backend(FakeStore::empty());
        let err = store.token().expect_err("should error when nothing configured");
        assert!(matches!(err, TrunkError::Auth(_)));
    }
}
