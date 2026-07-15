//! Durable storage for the GitHub OAuth user token.
//!
//! The device-flow controller that mints the token lives in `boss-engine`
//! (it needs the work DB to record org-authorization state); this module
//! owns only the storage half, which has no such dependency. Keeping it
//! here lets [`crate::credentials::KeychainOAuthResolver`] read a stored
//! token without the resolver having to reach back up into the engine.

use serde::{Deserialize, Serialize};

// ── TokenRecord ───────────────────────────────────────────────────────────────

/// Captured token with identity metadata.  Persisted in the macOS keychain
/// by [`KeychainTokenStore`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub token: String,
    pub login: String,
    pub granted_scopes: Vec<String>,
    pub obtained_at: i64,
}

// ── KeychainTokenStore ────────────────────────────────────────────────────────

/// OS keychain coordinates for the stored OAuth token.
pub(crate) const KEYCHAIN_SERVICE: &str = "dev.spinyfin.boss.github";
pub(crate) const KEYCHAIN_ACCOUNT: &str = "oauth-user-token@github.com";

/// Error type for [`KeychainTokenStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    #[error("token record (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Low-level storage backend abstraction.  The production impl uses
/// [`keyring::Entry`]; tests inject [`FakeStore`] to avoid touching the
/// real keychain.
pub trait KeystoreBackend: Send + Sync {
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

/// macOS production backends.
///
/// On release builds (Developer ID-signed with `keychain-access-groups`
/// entitlement), [`DataProtectionKeychainBackend`] stores the token in the
/// data-protection keychain using entitlement-based ACLs rather than
/// per-binary ACLs.  This means a new (re-signed) build of the engine can
/// read the token without triggering a macOS keychain prompt.
///
/// On dev builds (ad-hoc signed, no `keychain-access-groups` entitlement),
/// [`FileBackend`] stores the token in a 0600-mode file under the Boss
/// Application Support directory — the same fallback strategy as
/// `APIKeyStore` in the Swift app.
///
/// # Why three prompts on the old code path
///
/// The old `keyring` backend used `SecKeychainFindGenericPassword` (the
/// *legacy* macOS keychain).  Legacy keychain items carry a trusted-application
/// ACL that records the code-signing identity of each binary that is allowed
/// to access the item.  A new (re-signed) binary is not in that ACL, so
/// macOS shows a prompt for every distinct keychain access from the new
/// binary.  At engine startup there are three such accesses:
///
/// 1. `GitHubAuthController::restore_from_store()` reads the stored token.
/// 2. The `KeychainOAuthResolver` reads the same token when the issue-sync
///    tracker resolves credentials for its first sync cycle.
/// 3. A third read happens when the org-auth probe runs after restoring state.
///
/// The data-protection keychain (via `kSecUseDataProtectionKeychain = true`)
/// enforces access by entitlement rather than binary identity: any binary
/// signed with the same `keychain-access-groups` entitlement can access the
/// item without a user prompt, even after a re-sign.
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

    // `kSecAttrAccessible` is not re-exported by `security-framework-sys`, but
    // it is a plain `extern` symbol in Security.framework.
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecAttrAccessible: CFStringRef;
    }

    // `SecTask*` APIs for checking the `keychain-access-groups` entitlement.
    // These functions are in Security.framework but not wrapped by any crate
    // we use.
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
    ///
    /// Mirrors `APIKeyStore.dataProtectionKeychainAvailable()` in the Swift app.
    pub(super) fn data_protection_keychain_available() -> bool {
        // SAFETY: all pointer ops follow CF ownership rules:
        //   - SecTaskCreateFromSelf returns an owned ref (Create rule) → CFRelease
        //   - CFStringCreateWithCString returns an owned ref → CFRelease
        //   - SecTaskCopyValueForEntitlement returns an owned ref (if non-null) → CFRelease
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
        // Add kSecAttrAccessible = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly so
        // that the item can be read by background processes (including this engine)
        // even when the screen is locked.  We use the deprecated `query` field because
        // `PasswordOptions` has no public setter for this attribute.
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

    /// Converts a `security_framework` error to `TokenStoreError` by wrapping
    /// it as a `keyring` platform failure.
    fn sec_err(e: security_framework::base::Error) -> TokenStoreError {
        TokenStoreError::Keychain(keyring::Error::PlatformFailure(Box::new(e)))
    }

    // ── DataProtectionKeychainBackend ──────────────────────────────────────────

    /// Stores the OAuth token in the macOS Data Protection Keychain.
    ///
    /// Requires the `keychain-access-groups` entitlement (present in Developer
    /// ID release builds via `engine.entitlements`).
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

    // ── FileBackend ────────────────────────────────────────────────────────────

    /// Stores the OAuth token as a 0600-mode JSON file.
    ///
    /// Used as a fallback on ad-hoc dev builds that lack the
    /// `keychain-access-groups` entitlement needed to access the Data
    /// Protection Keychain.
    pub(super) struct FileBackend {
        path: PathBuf,
    }

    impl FileBackend {
        pub(super) fn new() -> Self {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            Self {
                path: PathBuf::from(home)
                    .join("Library/Application Support/Boss")
                    .join("github-oauth-token"),
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

/// Stores and retrieves a [`TokenRecord`] in the OS keychain.
///
/// The value at rest is a JSON blob serialised from / into [`TokenRecord`].
/// Production code constructs this with [`KeychainTokenStore::new`]; tests
/// supply a [`FakeStore`] via [`KeychainTokenStore::with_backend`].
pub struct KeychainTokenStore {
    backend: Box<dyn KeystoreBackend>,
}

impl Default for KeychainTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainTokenStore {
    /// Creates a store backed by the platform's native credential store.
    ///
    /// On macOS, selects between the data-protection keychain (release builds
    /// with `keychain-access-groups` entitlement) and a file-based fallback
    /// (ad-hoc dev builds without the entitlement).  On other platforms,
    /// delegates to `keyring`.
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            if macos_backends::data_protection_keychain_available() {
                tracing::debug!(
                    target: "boss_engine::external_tracker::github_oauth",
                    "github token store: data-protection keychain (release build)"
                );
                Self {
                    backend: Box::new(macos_backends::DataProtectionKeychainBackend),
                }
            } else {
                tracing::debug!(
                    target: "boss_engine::external_tracker::github_oauth",
                    "github token store: file backend (dev build, no keychain-access-groups entitlement)"
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

    /// Creates a store backed by a caller-supplied [`KeystoreBackend`].
    ///
    /// Tests pass [`FakeStore`] here to stay off the real keychain. This is
    /// `pub` rather than `#[cfg(test)]` because the device-flow tests that
    /// exercise it live in `boss-engine`, and a `cfg(test)` item is not
    /// visible outside its own crate's test build.
    pub fn with_backend(backend: impl KeystoreBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
        }
    }

    /// Returns the stored [`TokenRecord`], or `None` if no token is present.
    pub fn get(&self) -> Result<Option<TokenRecord>, TokenStoreError> {
        match self.backend.get_raw()? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    /// Persists a [`TokenRecord`] in the keychain, overwriting any prior value.
    pub fn set(&self, record: &TokenRecord) -> Result<(), TokenStoreError> {
        let s = serde_json::to_string(record)?;
        self.backend.set_raw(&s)
    }

    /// Removes the stored token.  A no-op if none is present.
    pub fn delete(&self) -> Result<(), TokenStoreError> {
        self.backend.delete_raw()
    }
}

// ── FakeStore (test support) ──────────────────────────────────────────────────

/// In-memory [`KeystoreBackend`] for tests.  Never touches the real keychain.
///
/// Exported (rather than `#[cfg(test)]`) for the same reason as
/// [`KeychainTokenStore::with_backend`]: the device-flow tests in
/// `boss-engine` inject it from another crate.
pub struct FakeStore(std::sync::Mutex<Option<String>>);

impl FakeStore {
    pub fn empty() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    pub fn prefilled(record: &TokenRecord) -> Self {
        let s = serde_json::to_string(record).expect("TokenRecord should serialize");
        Self(std::sync::Mutex::new(Some(s)))
    }
}

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> TokenRecord {
        TokenRecord {
            token: "gho_sample".to_owned(),
            login: "octocat".to_owned(),
            granted_scopes: vec!["repo".to_owned(), "project".to_owned()],
            obtained_at: 1_700_000_000,
        }
    }

    #[test]
    fn keychain_store_round_trips_token_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        assert!(store.get().unwrap().is_none());

        let record = sample_record();
        store.set(&record).unwrap();

        let got = store.get().unwrap().expect("should have a record");
        assert_eq!(got.token, record.token);
        assert_eq!(got.login, record.login);
        assert_eq!(got.granted_scopes, record.granted_scopes);
        assert_eq!(got.obtained_at, record.obtained_at);
    }

    #[test]
    fn keychain_store_delete_removes_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record()));
        assert!(store.get().unwrap().is_some());

        store.delete().unwrap();
        assert!(store.get().unwrap().is_none());
    }

    #[test]
    fn keychain_store_delete_is_idempotent_when_empty() {
        let store = KeychainTokenStore::with_backend(FakeStore::empty());
        store.delete().unwrap(); // should not error
    }

    #[test]
    fn keychain_store_set_overwrites_existing_record() {
        let store = KeychainTokenStore::with_backend(FakeStore::prefilled(&sample_record()));
        let new_record = TokenRecord {
            token: "gho_new_token".to_owned(),
            login: "newuser".to_owned(),
            granted_scopes: vec!["repo".to_owned()],
            obtained_at: 1_800_000_000,
        };
        store.set(&new_record).unwrap();

        let got = store.get().unwrap().expect("should have a record");
        assert_eq!(got.token, "gho_new_token");
        assert_eq!(got.login, "newuser");
    }
}
