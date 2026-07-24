//! Shared OS-keychain-backed secret storage: macOS Data Protection Keychain
//! in release builds, a 0600-mode file fallback in ad-hoc dev builds, and a
//! plain `keyring` backend on other platforms.
//!
//! This crate owns *storage* only — callers decide what to store (a raw
//! token string, a JSON-serialized record, etc.) via [`KeychainStore::get_raw`]
//! / [`KeychainStore::set_raw`]. [`KeystoreBackend`] is exposed so tests can
//! inject an in-memory fake instead of touching the real keychain.

/// Error type for [`KeychainStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
}

/// Low-level storage backend abstraction. The production impl uses the
/// platform keychain; tests inject an in-memory fake.
pub trait KeystoreBackend: Send + Sync {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError>;
    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError>;
    fn delete_raw(&self) -> Result<(), TokenStoreError>;
}

/// Production backend on non-macOS: delegates to the OS credential store via
/// `keyring::Entry`.
#[cfg(not(target_os = "macos"))]
struct KeyringBackend {
    service: String,
    account: String,
}

#[cfg(not(target_os = "macos"))]
impl KeystoreBackend for KeyringBackend {
    fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        let entry = keyring::Entry::new(&self.service, &self.account)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(TokenStoreError::Keychain(e)),
        }
    }

    fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(&self.service, &self.account)?;
        entry.set_password(value).map_err(TokenStoreError::Keychain)
    }

    fn delete_raw(&self) -> Result<(), TokenStoreError> {
        let entry = keyring::Entry::new(&self.service, &self.account)?;
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
/// entitlement), [`DataProtectionKeychainBackend`] stores the secret in the
/// data-protection keychain using entitlement-based ACLs rather than
/// per-binary ACLs. This means a new (re-signed) build of the engine can
/// read the secret without triggering a macOS keychain prompt.
///
/// On dev builds (ad-hoc signed, no `keychain-access-groups` entitlement),
/// [`FileBackend`] stores the secret in a 0600-mode file under the Boss
/// Application Support directory — the same fallback strategy as
/// `APIKeyStore` in the Swift app.
///
/// # Why three prompts on the old code path
///
/// The old `keyring` backend used `SecKeychainFindGenericPassword` (the
/// *legacy* macOS keychain). Legacy keychain items carry a trusted-application
/// ACL that records the code-signing identity of each binary that is allowed
/// to access the item. A new (re-signed) binary is not in that ACL, so
/// macOS shows a prompt for every distinct keychain access from the new
/// binary.
///
/// The data-protection keychain (via `kSecUseDataProtectionKeychain = true`)
/// enforces access by entitlement rather than binary identity: any binary
/// signed with the same `keychain-access-groups` entitlement can access the
/// item without a user prompt, even after a re-sign.
#[cfg(target_os = "macos")]
mod macos_backends {
    use super::{KeystoreBackend, TokenStoreError};

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

    fn read_options(service: &str, account: &str) -> PasswordOptions {
        let mut opts = PasswordOptions::new_generic_password(service, account);
        opts.use_protected_keychain();
        opts
    }

    fn write_options(service: &str, account: &str) -> PasswordOptions {
        let mut opts = read_options(service, account);
        // Add kSecAttrAccessible = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly so
        // that the item can be read by background processes (including this engine)
        // even when the screen is locked. We use the deprecated `query` field because
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

    /// Stores the secret in the macOS Data Protection Keychain.
    ///
    /// Requires the `keychain-access-groups` entitlement (present in Developer
    /// ID release builds via `engine.entitlements`).
    pub(super) struct DataProtectionKeychainBackend {
        pub(super) service: String,
        pub(super) account: String,
    }

    impl KeystoreBackend for DataProtectionKeychainBackend {
        fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
            match generic_password(read_options(&self.service, &self.account)) {
                Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
                Err(e) if e.code() == errSecItemNotFound => Ok(None),
                Err(e) => Err(sec_err(e)),
            }
        }

        fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
            set_generic_password_options(value.as_bytes(), write_options(&self.service, &self.account)).map_err(sec_err)
        }

        fn delete_raw(&self) -> Result<(), TokenStoreError> {
            match delete_generic_password_options(read_options(&self.service, &self.account)) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == errSecItemNotFound => Ok(()),
                Err(e) => Err(sec_err(e)),
            }
        }
    }

    // ── FileBackend ────────────────────────────────────────────────────────────

    /// Stores the secret as a 0600-mode file.
    ///
    /// Used as a fallback on ad-hoc dev builds that lack the
    /// `keychain-access-groups` entitlement needed to access the Data
    /// Protection Keychain.
    pub(super) struct FileBackend {
        pub(super) path: PathBuf,
    }

    impl FileBackend {
        pub(super) fn new(file_fallback_name: &str) -> Self {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            Self {
                path: PathBuf::from(home)
                    .join("Library/Application Support/Boss")
                    .join(file_fallback_name),
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

/// Stores and retrieves a secret string in the OS keychain under a given
/// `(service, account)` pair.
///
/// Production code constructs this with [`KeychainStore::new`]; tests supply
/// a [`KeystoreBackend`] fake via [`KeychainStore::with_backend`].
pub struct KeychainStore {
    backend: Box<dyn KeystoreBackend>,
}

impl KeychainStore {
    /// Creates a store backed by the platform's native credential store.
    ///
    /// On macOS, selects between the data-protection keychain (release builds
    /// with `keychain-access-groups` entitlement) and a file-based fallback
    /// (ad-hoc dev builds without the entitlement), writing the fallback file
    /// as `~/Library/Application Support/Boss/<file_fallback_name>`. On other
    /// platforms, delegates to `keyring`.
    pub fn new(service: &str, account: &str, file_fallback_name: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            if macos_backends::data_protection_keychain_available() {
                tracing::debug!(
                    target: "boss_keychain",
                    service,
                    "keychain store: data-protection keychain (release build)"
                );
                Self {
                    backend: Box::new(macos_backends::DataProtectionKeychainBackend {
                        service: service.to_owned(),
                        account: account.to_owned(),
                    }),
                }
            } else {
                tracing::debug!(
                    target: "boss_keychain",
                    service,
                    "keychain store: file backend (dev build, no keychain-access-groups entitlement)"
                );
                Self {
                    backend: Box::new(macos_backends::FileBackend::new(file_fallback_name)),
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = file_fallback_name;
            Self {
                backend: Box::new(KeyringBackend {
                    service: service.to_owned(),
                    account: account.to_owned(),
                }),
            }
        }
    }

    /// Creates a store backed by a caller-supplied [`KeystoreBackend`] — used
    /// by downstream crates' tests to inject an in-memory fake instead of
    /// touching the real keychain.
    pub fn with_backend(backend: impl KeystoreBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
        }
    }

    /// Returns the stored secret, or `None` if none is present.
    pub fn get_raw(&self) -> Result<Option<String>, TokenStoreError> {
        self.backend.get_raw()
    }

    /// Persists `value` in the keychain, overwriting any prior value.
    pub fn set_raw(&self, value: &str) -> Result<(), TokenStoreError> {
        self.backend.set_raw(value)
    }

    /// Removes the stored secret. A no-op if none is present.
    pub fn delete_raw(&self) -> Result<(), TokenStoreError> {
        self.backend.delete_raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeStore(std::sync::Mutex<Option<String>>);

    impl FakeStore {
        fn empty() -> Self {
            Self(std::sync::Mutex::new(None))
        }

        fn prefilled(value: &str) -> Self {
            Self(std::sync::Mutex::new(Some(value.to_owned())))
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

    #[test]
    fn store_round_trips_value() {
        let store = KeychainStore::with_backend(FakeStore::empty());
        assert!(store.get_raw().unwrap().is_none());

        store.set_raw("secret_abc").unwrap();
        assert_eq!(store.get_raw().unwrap().as_deref(), Some("secret_abc"));
    }

    #[test]
    fn store_delete_removes_value() {
        let store = KeychainStore::with_backend(FakeStore::prefilled("secret_abc"));
        assert!(store.get_raw().unwrap().is_some());
        store.delete_raw().unwrap();
        assert!(store.get_raw().unwrap().is_none());
    }

    #[test]
    fn store_delete_is_idempotent_when_empty() {
        let store = KeychainStore::with_backend(FakeStore::empty());
        store.delete_raw().unwrap();
    }

    #[test]
    fn store_set_overwrites_existing_value() {
        let store = KeychainStore::with_backend(FakeStore::prefilled("old"));
        store.set_raw("new").unwrap();
        assert_eq!(store.get_raw().unwrap().as_deref(), Some("new"));
    }

    /// Behavior tests for the real filesystem-backed `FileBackend`, the macOS
    /// dev-build fallback. These construct `FileBackend { path }` directly
    /// against a tempdir and exercise the public `KeystoreBackend` interface,
    /// asserting only on observable behavior (round-trips, `None`/idempotent
    /// results) and observable filesystem state (file contents, 0600
    /// permissions, created parent dirs).
    #[cfg(target_os = "macos")]
    mod file_backend {
        use crate::KeystoreBackend;
        use crate::macos_backends::FileBackend;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        fn backend_at(dir: &TempDir, rel: &str) -> FileBackend {
            FileBackend {
                path: dir.path().join(rel),
            }
        }

        #[test]
        fn set_then_get_round_trips_value() {
            let dir = TempDir::new().unwrap();
            let backend = backend_at(&dir, "secret");

            backend.set_raw("secret_abc").unwrap();
            assert_eq!(backend.get_raw().unwrap().as_deref(), Some("secret_abc"));
        }

        #[test]
        fn get_on_missing_path_returns_none() {
            let dir = TempDir::new().unwrap();
            let backend = backend_at(&dir, "does_not_exist");

            // NotFound is mapped to Ok(None), not surfaced as an error.
            assert!(backend.get_raw().unwrap().is_none());
        }

        #[test]
        fn delete_on_missing_file_is_idempotent() {
            let dir = TempDir::new().unwrap();
            let backend = backend_at(&dir, "does_not_exist");

            // Deleting a secret that was never written must succeed.
            backend.delete_raw().unwrap();
            // ...and stay a no-op on a second call.
            backend.delete_raw().unwrap();
        }

        #[test]
        fn set_creates_missing_parent_directories() {
            let dir = TempDir::new().unwrap();
            // Parent dirs `nested/deeper/` do not exist yet.
            let backend = backend_at(&dir, "nested/deeper/secret");
            assert!(!backend.path.parent().unwrap().exists());

            backend.set_raw("secret_abc").unwrap();

            assert!(backend.path.exists());
            assert_eq!(backend.get_raw().unwrap().as_deref(), Some("secret_abc"));
        }

        #[test]
        fn set_writes_file_with_0600_permissions() {
            let dir = TempDir::new().unwrap();
            let backend = backend_at(&dir, "secret");

            backend.set_raw("secret_abc").unwrap();

            let mode = std::fs::metadata(&backend.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "expected owner-only rw, got {:o}", mode & 0o777);
        }

        #[test]
        fn set_truncates_existing_longer_value() {
            let dir = TempDir::new().unwrap();
            let backend = backend_at(&dir, "secret");

            backend.set_raw("a_much_longer_previous_value").unwrap();
            backend.set_raw("short").unwrap();

            // No trailing bytes from the longer prior write survive.
            assert_eq!(backend.get_raw().unwrap().as_deref(), Some("short"));
            let len = std::fs::metadata(&backend.path).unwrap().len();
            assert_eq!(len, "short".len() as u64);
        }
    }
}
