//! Storage for the Trunk org API token, mirroring the GitHub OAuth
//! [`KeychainTokenStore`] pattern (`tools/boss/github_tracker/src/github_oauth.rs`):
//! macOS Data Protection Keychain in release builds, a 0600-mode file
//! fallback in ad-hoc dev builds, and a plain `keyring` backend on other
//! platforms.
//!
//! Per the design (`docs/designs/trunk-merge-queue-integration-*.md`
//! §"Auth"), the token is never persisted anywhere else — no DB row, no
//! repo file, and it is never logged. [`BOSS_TRUNK_API_TOKEN_ENV`] lets a
//! developer override the keychain for local testing.
//!
//! This crate owns *storage* only; [`boss_trunk_client::TrunkTokenProvider`]
//! is implemented here so the engine can hand a [`TrunkTokenStore`]
//! straight to a `TrunkClient`.

use boss_trunk_client::{SecretString, TrunkError, TrunkTokenProvider};

/// Env var that overrides the keychain for local development. Checked
/// before the keychain on every read.
pub const BOSS_TRUNK_API_TOKEN_ENV: &str = "BOSS_TRUNK_API_TOKEN";

/// OS keychain coordinates for the stored Trunk API token.
const KEYCHAIN_SERVICE: &str = "dev.spinyfin.boss.trunk";
const KEYCHAIN_ACCOUNT: &str = "api-token@trunk.io";

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
    Keychain(#[from] keyring::Error),
}

/// Low-level storage backend abstraction. The production impl uses the
/// platform keychain; tests inject [`FakeStore`].
trait KeystoreBackend: Send + Sync {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError>;
    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError>;
    fn delete_raw(&self) -> Result<(), TokenStoreError>;
}

/// Production backend on non-macOS: delegates to the OS credential store via
/// `keyring::Entry`.
#[cfg(not(target_os = "macos"))]
struct KeyringBackend;

#[cfg(not(target_os = "macos"))]
impl KeystoreBackend for KeyringBackend {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(TokenStoreError::Keychain(e)),
        }
    }

    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        entry.set_password(value).map_err(TokenStoreError::Keychain)
    }

    fn delete_raw(&self) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(TokenStoreError::Keychain(e)),
        }
    }
}

/// macOS production backends. See `github_oauth.rs`'s `macos_backends`
/// module for the full rationale (data-protection keychain vs legacy
/// per-binary ACLs); this mirrors it exactly, with Trunk's service/account.
#[cfg(target_os = "macos")]
mod macos_backends {
    use super::{KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE, KeystoreBackend, TokenStoreError};

    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8};
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password_options, generic_password, set_generic_password_options,
    };
    use security_framework_sys::access_control::kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly;
    use security_framework_sys::base::errSecItemNotFound;
    use std::ffi::CString;
    use std::path::PathBuf;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecAttrAccessible: CFStringRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecTaskCreateFromSelf(error: *const std::ffi::c_void) -> CFTypeRef;
        fn SecTaskCopyValueForEntitlement(
            task: CFTypeRef,
            entitlement: CFStringRef,
            error: *mut std::ffi::c_void,
        ) -> CFTypeRef;
    }

    /// Returns `true` when the `keychain-access-groups` entitlement is present
    /// in the running process (i.e. this is a Developer ID release build).
    pub(super) fn data_protection_keychain_available() -> bool {
        // SAFETY: identical CF ownership discipline as
        // `github_oauth::macos_backends::data_protection_keychain_available`.
        unsafe {
            let task = SecTaskCreateFromSelf(std::ptr::null());
            if task.is_null() {
                return false;
            }

            let entitlement = CString::new("keychain-access-groups").unwrap();
            let cf_entitlement =
                CFStringCreateWithCString(std::ptr::null_mut(), entitlement.as_ptr(), kCFStringEncodingUTF8);
            if cf_entitlement.is_null() {
                CFRelease(task);
                return false;
            }

            let value = SecTaskCopyValueForEntitlement(task, cf_entitlement, std::ptr::null_mut());
            CFRelease(task);
            CFRelease(cf_entitlement as CFTypeRef);

            let present = !value.is_null();
            if present {
                CFRelease(value);
            }
            present
        }
    }

    fn read_options() -> PasswordOptions {
        let mut opts = PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        opts.use_protected_keychain();
        opts
    }

    fn write_options() -> PasswordOptions {
        let mut opts = read_options();
        #[allow(deprecated)]
        opts.query.push((
            // SAFETY: kSecAttrAccessible is a permanent static string in Security.framework.
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccessible) },
            // SAFETY: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly is a permanent
            // static string in Security.framework; cast from CFStringRef to CFTypeRef is valid.
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as CFStringRef) }
                .into_CFType(),
        ));
        opts
    }

    fn sec_err(e: security_framework::base::Error) -> TokenStoreError {
        TokenStoreError::Keychain(keyring::Error::PlatformFailure(Box::new(e)))
    }

    pub(super) struct DataProtectionKeychainBackend;

    impl KeystoreBackend for DataProtectionKeychainBackend {
        fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
            match generic_password(read_options()) {
                Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
                Err(e) if e.code() == errSecItemNotFound => Ok(None),
                Err(e) => Err(sec_err(e)),
            }
        }

        fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
            set_generic_password_options(value.as_bytes(), write_options()).map_err(sec_err)
        }

        fn delete_raw(&self) -> Result<(), TokenStoreError> {
            match delete_generic_password_options(read_options()) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == errSecItemNotFound => Ok(()),
                Err(e) => Err(sec_err(e)),
            }
        }
    }

    /// Stores the Trunk token as a 0600-mode file. Used as a fallback on
    /// ad-hoc dev builds that lack the `keychain-access-groups` entitlement.
    pub(super) struct FileBackend {
        path: PathBuf,
    }

    impl FileBackend {
        pub(super) fn new() -> Self {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            Self {
                path: PathBuf::from(home)
                    .join("Library/Application Support/Boss")
                    .join("trunk-api-token"),
            }
        }
    }

    fn io_err(e: std::io::Error) -> TokenStoreError {
        TokenStoreError::Keychain(keyring::Error::PlatformFailure(Box::new(e)))
    }

    impl KeystoreBackend for FileBackend {
        fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
            match std::fs::read_to_string(&self.path) {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io_err(e)),
            }
        }

        fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)
                .map_err(io_err)?;
            file.write_all(value.as_bytes()).map_err(io_err)
        }

        fn delete_raw(&self) -> Result<(), TokenStoreError> {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(e)),
            }
        }
    }
}

/// Stores and retrieves the Trunk org API token in the OS keychain, and
/// implements [`TrunkTokenProvider`] by preferring [`BOSS_TRUNK_API_TOKEN_ENV`]
/// over the stored value.
pub struct TrunkTokenStore {
    backend: Box<dyn KeystoreBackend>,
}

impl Default for TrunkTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrunkTokenStore {
    /// Creates a store backed by the platform's native credential store. See
    /// `github_oauth::KeychainTokenStore::new` for the backend-selection
    /// rationale (data-protection keychain vs file fallback on macOS).
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            if macos_backends::data_protection_keychain_available() {
                tracing::debug!(
                    target: "boss_trunk_auth",
                    "trunk token store: data-protection keychain (release build)"
                );
                Self {
                    backend: Box::new(macos_backends::DataProtectionKeychainBackend),
                }
            } else {
                tracing::debug!(
                    target: "boss_trunk_auth",
                    "trunk token store: file backend (dev build, no keychain-access-groups entitlement)"
                );
                Self {
                    backend: Box::new(macos_backends::FileBackend::new()),
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                backend: Box::new(KeyringBackend),
            }
        }
    }

    #[cfg(test)]
    fn with_backend(backend: impl KeystoreBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
        }
    }

    /// Returns the stored token, or `None` if none is present.
    pub fn get(&self) -> Result<Option<String>, TokenStoreError> {
        self.backend.get_raw()
    }

    /// Persists `token` in the keychain, overwriting any prior value.
    pub fn set(&self, token: &str) -> Result<(), TokenStoreError> {
        self.backend.set_raw(token)
    }

    /// Removes the stored token. A no-op if none is present.
    pub fn delete(&self) -> Result<(), TokenStoreError> {
        self.backend.delete_raw()
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

// ── FakeStore (test-only) ─────────────────────────────────────────────────

#[cfg(test)]
struct FakeStore(std::sync::Mutex<Option<String>>);

#[cfg(test)]
impl FakeStore {
    fn empty() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    fn prefilled(token: &str) -> Self {
        Self(std::sync::Mutex::new(Some(token.to_owned())))
    }
}

#[cfg(test)]
impl KeystoreBackend for FakeStore {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = Some(value.to_owned());
        Ok(())
    }

    fn delete_raw(&self) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
