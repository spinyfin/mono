//! The idempotent bootstrap installer.
//!
//! It runs as root on the worker host, verifies a signed release before it
//! executes or unpacks anything, and lays out: an immutable release directory,
//! a `current` pointer, the service user's owner-only state, the hub's CA, the
//! service definition and a small install record. It **never enrolls** the host
//! and never starts a service that has no client certificate: enrollment is a
//! separate step the person takes as the service user.
//!
//! Running it again with the same inputs changes nothing and restarts nothing.
//! Running it with a newer release switches `current`, then restarts an
//! enrolled host's service. The service definitions leave the tmux server and
//! its worker panes running through that restart, and the daemon re-adopts them.
//!
//! All the host-changing steps that cannot be done with plain file operations
//! (creating the user, talking to systemd or launchd, running a command as the
//! service user) go through [`Host`], so the whole flow is exercised in tests
//! against a staging root without being root.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::archive::{self, INSTALLER_BINARY, SPOKE_BINARY};
use crate::manifest::{self, Platform, TrustedKey};
use crate::service::{self, Spec};

pub const DEFAULT_USER: &str = "boss-worker";

/// Where everything goes. Paths are the real ones; `root` stages them under a
/// directory instead of `/` (tests, image builds), and the service definitions
/// still name the real paths.
#[derive(Debug, Clone)]
pub struct Layout {
    pub platform: Platform,
    pub user: String,
    pub root: Option<PathBuf>,
}

impl Layout {
    pub fn on_disk(&self, logical: &Path) -> PathBuf {
        match &self.root {
            Some(root) => root.join(logical.strip_prefix("/").unwrap_or(logical)),
            None => logical.to_path_buf(),
        }
    }

    pub fn prefix(&self) -> PathBuf {
        PathBuf::from(if self.platform.is_macos() {
            "/usr/local/lib/boss-spoke"
        } else {
            "/opt/boss-spoke"
        })
    }

    pub fn etc(&self) -> PathBuf {
        PathBuf::from("/etc/boss-spoke")
    }

    pub fn home(&self) -> PathBuf {
        PathBuf::from(if self.platform.is_macos() { "/Users" } else { "/var/lib" }).join(&self.user)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home().join("spoke")
    }

    pub fn release_dir(&self, release_id: &str) -> PathBuf {
        self.prefix().join("releases").join(release_id)
    }

    pub fn current(&self) -> PathBuf {
        self.prefix().join("current")
    }

    pub fn previous(&self) -> PathBuf {
        self.prefix().join("previous")
    }

    pub fn request_dir(&self) -> PathBuf {
        self.state_dir().join("update-request")
    }

    pub fn request_path(&self) -> PathBuf {
        self.request_dir().join("request.json")
    }

    pub fn status_path(&self) -> PathBuf {
        self.etc().join("upgrade-status.json")
    }

    pub fn in_progress_path(&self) -> PathBuf {
        self.etc().join("upgrade-in-progress.json")
    }

    pub fn health_path(&self) -> PathBuf {
        self.state_dir().join("update-health.json")
    }

    pub fn attempt_path(&self) -> PathBuf {
        self.etc().join("update-attempt.json")
    }

    pub fn helper_definition(&self) -> PathBuf {
        if self.platform.is_macos() {
            Path::new("/Library/LaunchDaemons").join(format!("{}.plist", service::LAUNCHD_UPDATE_LABEL))
        } else {
            Path::new("/etc/systemd/system").join(service::SYSTEMD_UPDATE_SERVICE)
        }
    }

    pub fn helper_path_definition(&self) -> Option<PathBuf> {
        if self.platform.is_macos() {
            None
        } else {
            Some(Path::new("/etc/systemd/system").join(service::SYSTEMD_UPDATE_PATH))
        }
    }

    pub fn definition(&self) -> PathBuf {
        if self.platform.is_macos() {
            Path::new("/Library/LaunchDaemons").join(format!("{}.plist", service::LAUNCHD_LABEL))
        } else {
            Path::new("/etc/systemd/system").join(service::SYSTEMD_UNIT)
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.etc().join("install.json")
    }

    pub fn pinned_key_path(&self) -> PathBuf {
        self.etc().join("release-verify-key.pub")
    }

    fn path_env(&self) -> String {
        let home = self.home().display().to_string();
        if self.platform.is_macos() {
            format!("{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        } else {
            format!("{home}/.local/bin:/usr/local/bin:/usr/bin:/bin")
        }
    }

    pub fn spec(&self, host_id: &str, hub_url: &str) -> Spec {
        Spec::builder()
            .user(self.user.clone())
            .home(self.home())
            .state_dir(self.state_dir())
            .binary(self.current().join(SPOKE_BINARY))
            .host_id(host_id)
            .hub_url(hub_url)
            .path(self.path_env())
            .build()
    }

    /// The service environment with every path pointed at the staging root, for
    /// running checks against what was actually written.
    pub fn disk_environment(&self, spec: &Spec) -> Vec<(String, String)> {
        spec.environment()
            .into_iter()
            .map(|(key, value)| match key.as_str() {
                "PATH" => (key, value),
                _ => (key, self.on_disk(Path::new(&value)).display().to_string()),
            })
            .collect()
    }

    fn base(&self) -> &Path {
        self.root.as_deref().unwrap_or(Path::new("/"))
    }
}

/// What the installer remembers about this host, root-owned and world-readable.
/// Nothing in it is secret.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct InstallRecord {
    pub schema: u32,
    pub release_id: String,
    pub platform: String,
    pub user: String,
    pub host_id: String,
    pub hub_url: String,
    /// HTTPS origin prefix later upgrades must fetch from. Empty on records
    /// written before the updater; the next verified install fills it.
    #[builder(default)]
    #[serde(default)]
    pub package_origin: String,
}

impl InstallRecord {
    pub fn read(path: &Path) -> Result<Self> {
        match Self::load(path)? {
            Some(record) => Ok(record),
            None => bail!(
                "{} is unreadable; has the spoke package been installed on this host?",
                path.display()
            ),
        }
    }

    /// `Ok(None)` only when the file is absent. Any other read or parse
    /// problem is an error: a corrupt record must not be treated as a first
    /// install.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => bail!(
                "{} cannot be read ({error}); inspect it before installing again",
                path.display()
            ),
        };
        let record: Self = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is not a valid install record; inspect it before installing again",
                path.display()
            )
        })?;
        ensure!(
            record.schema == 1,
            "unsupported install record schema {}",
            record.schema
        );
        Ok(Some(record))
    }

    pub fn layout(&self) -> Result<Layout> {
        Ok(Layout {
            platform: Platform::parse(&self.platform)?,
            user: self.user.clone(),
            root: None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
}

pub struct RunOutput {
    pub success: bool,
    pub output: String,
}

/// The parts of installing that need the operating system's cooperation.
pub trait Host {
    fn lookup_user(&self, name: &str) -> Result<Option<Account>>;
    /// Creates the unprivileged service account with `home` as its home.
    fn create_user(&self, name: &str, home: &Path) -> Result<Account>;
    /// Makes the service manager aware of a new or changed definition. It must
    /// not start a host that has no client certificate.
    fn register_service(&self, definition: &Path) -> Result<()>;
    /// Whether the service is registered and enabled, regardless of whether
    /// the definition bytes on disk changed.
    fn service_registered(&self) -> Result<bool>;
    fn restart_service(&self) -> Result<()>;
    /// Stops the daemon without starting it again, so the journal has no writer.
    fn stop_service(&self) -> Result<()>;
    fn service_running(&self) -> Result<bool>;
    /// PID and service-manager launch count, absent when no instance is running.
    fn service_instance(&self) -> Result<Option<(u32, u64)>>;
    /// Runs `program` as `account` with exactly `env` and nothing inherited.
    fn run_as(&self, account: &Account, env: &[(String, String)], program: &Path, args: &[String])
    -> Result<RunOutput>;
}

/// How long to wait for a restarted service to prove itself.
#[derive(Debug, Clone, Copy)]
pub struct Health {
    pub deadline: Duration,
    pub poll: Duration,
    pub stable: Duration,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(120),
            poll: Duration::from_secs(1),
            stable: Duration::from_secs(10),
        }
    }
}

#[derive(bon::Builder)]
pub struct Request<'a> {
    pub manifest: &'a [u8],
    pub signature: &'a str,
    pub package: &'a [u8],
    pub trusted_key: Option<TrustedKey>,
    pub host_id: &'a str,
    pub hub_url: &'a str,
    pub hub_ca: &'a [u8],
    pub platform: Platform,
    pub layout: Layout,
    #[builder(default)]
    pub health: Health,
}

#[derive(Debug, Default)]
pub struct Report {
    pub release_id: String,
    pub key_fingerprint: String,
    /// Each step that changed the host. Empty when the host already matched.
    pub changed: Vec<&'static str>,
    pub enrolled: bool,
    pub restarted: bool,
}

pub fn install(host: &dyn Host, request: &Request<'_>) -> Result<Report> {
    let layout = &request.layout;
    let mut report = Report::default();

    let (key, newly_pinned) = resolve_trust(layout, request.trusted_key.clone())?;
    report.key_fingerprint = key.fingerprint();
    let verified = manifest::verify_release(
        &key,
        request.manifest,
        request.signature,
        request.package,
        request.platform,
    )?;
    let release_id = verified.manifest.release_id.clone();
    report.release_id = release_id.clone();

    let spec = layout.spec(request.host_id, request.hub_url);
    spec.validate()?;
    ensure!(
        request.hub_ca.windows(27).any(|w| w == b"-----BEGIN CERTIFICATE-----"),
        "the hub CA must be a PEM certificate"
    );

    // A host's journal is keyed by its id; changing it would strand the journal.
    let existing = InstallRecord::load(&layout.on_disk(&layout.config_path()))?;
    if let Some(existing) = &existing {
        ensure!(
            existing.host_id == request.host_id,
            "this host is installed as {}; a host id never changes because the journal belongs to it",
            existing.host_id
        );
        if !existing.package_origin.is_empty() {
            manifest::require_origin(&verified.manifest.package.url, &existing.package_origin)?;
        }
    }

    // Nothing is installed before this point.
    let account = match host.lookup_user(&layout.user)? {
        Some(account) => account,
        None => {
            report.changed.push("service user created");
            host.create_user(&layout.user, &layout.home())?
        }
    };
    if ensure_root_dir(layout, &layout.etc())? {
        report.changed.push("etc directory");
    }
    if newly_pinned {
        write_logical(layout, &layout.pinned_key_path(), key.to_text().as_bytes(), 0o644)?;
        report.changed.push("release key pinned");
    }
    if ensure_state(layout, &account)? {
        report.changed.push("service state directories");
    }
    if install_release(layout, &release_id, request.package, &verified.manifest.package.sha256)? {
        report.changed.push("release unpacked");
    }

    // Prove, as the service user and in the service's own environment, that the
    // state is private and the driver-home root is pinned inside it, before any
    // service definition points at this release.
    let checker = layout.on_disk(&layout.release_dir(&release_id).join(INSTALLER_BINARY));
    let args = vec![
        "preflight".to_owned(),
        "--inline".into(),
        "--storage-only".into(),
        "--state-dir".into(),
        layout.on_disk(&layout.state_dir()).display().to_string(),
    ];
    let checked = host.run_as(&account, &layout.disk_environment(&spec), &checker, &args)?;
    ensure!(
        checked.success,
        "the service user cannot use its state directory:\n{}",
        checked.output.trim_end()
    );

    if point_current(layout, &release_id)? {
        report.changed.push("current release switched");
    }
    let credentials = spec.credentials_dir();
    let credentials_dir = secure_fs::open_dir(layout.base(), &credentials)
        .context("the credentials directory was replaced after it was verified")?;
    secure_fs::verify_user_dir(&credentials_dir, &account, true, "credentials")?;
    let trust_changed = secure_fs::write_file_at(&credentials_dir, "ca.pem", request.hub_ca, 0o600, Some(&account))?;
    if trust_changed {
        report.changed.push("hub trust installed");
    }
    let package_origin = match &existing {
        Some(existing) if !existing.package_origin.is_empty() => existing.package_origin.clone(),
        _ => manifest::pinned_origin(&verified.manifest.package.url)?,
    };
    let record = InstallRecord::builder()
        .schema(1)
        .release_id(release_id.clone())
        .platform(verified.platform.as_str())
        .user(layout.user.clone())
        .host_id(request.host_id)
        .hub_url(request.hub_url)
        .package_origin(package_origin)
        .build();
    let mut record_bytes = serde_json::to_vec_pretty(&record)?;
    record_bytes.push(b'\n');
    if write_logical(layout, &layout.config_path(), &record_bytes, 0o644)? {
        report.changed.push("install record");
    }
    let definition = if layout.platform.is_macos() {
        spec.render_launchd()
    } else {
        spec.render_systemd()
    };
    let definition_path = layout.definition();
    let definition_parent = definition_path.parent().context("service definition has no parent")?;
    ensure_root_dir(layout, definition_parent)?;
    // Only acknowledge startup inputs after registration and any required restart.
    // A failed/interrupted attempt leaves the previous acknowledgement intact.
    let reconciled = serde_json::to_vec(&serde_json::json!({
        "definition": definition,
        "release_id": release_id,
        "hub_ca_sha256": manifest::sha256_hex(request.hub_ca),
    }))?;
    let reconciled_path = layout.etc().join("service-reconciled");
    let etc = secure_fs::open_installer_dir(layout.base(), &layout.etc())?;
    let pending = match secure_fs::read_file_at(&etc, "service-reconciled") {
        Ok((bytes, metadata)) => {
            secure_fs::verify_installer_file(&metadata, "service reconciliation record")?;
            bytes != reconciled
        }
        Err(error) if secure_fs::is_not_found(&error) => true,
        Err(error) => return Err(error),
    };
    let definition_changed = write_logical(layout, &definition_path, definition.as_bytes(), 0o644)?;
    if definition_changed {
        report.changed.push("service definition");
    }
    let helper_changed = install_update_helper(layout)?;
    if helper_changed {
        report.changed.push("update helper");
    }
    let registration_pending = match secure_fs::read_file_at(&etc, "definition-registered") {
        Ok((bytes, metadata)) => {
            secure_fs::verify_installer_file(&metadata, "definition registration record")?;
            bytes != definition.as_bytes()
        }
        Err(error) if secure_fs::is_not_found(&error) => true,
        Err(error) => return Err(error),
    };
    if registration_pending || definition_changed || helper_changed || !host.service_registered()? {
        host.register_service(&layout.definition())?;
        write_logical(
            layout,
            &layout.etc().join("definition-registered"),
            definition.as_bytes(),
            0o644,
        )?;
        if !definition_changed {
            report.changed.push("service registration");
        }
    }

    let credentials_on_disk = layout.on_disk(&credentials);
    report.enrolled = ["cert.pem", "key.pem"]
        .iter()
        .all(|name| credentials_on_disk.join(name).is_file());
    let release_changed = report.changed.contains(&"current release switched");
    // The daemon reads its definition, binary and trust anchor only at startup.
    if report.enrolled
        && (pending || definition_changed || release_changed || trust_changed || !host.service_running()?)
    {
        host.restart_service()?;
        wait_healthy(host, &request.health, layout)?;
        report.restarted = true;
    }
    write_logical(layout, &reconciled_path, &reconciled, 0o644)?;
    Ok(report)
}

fn resolve_trust(layout: &Layout, supplied: Option<TrustedKey>) -> Result<(TrustedKey, bool)> {
    let etc = match secure_fs::open_installer_dir(layout.base(), &layout.etc()) {
        Ok(dir) => {
            secure_fs::verify_installer_dir(&dir, &layout.etc().display().to_string())?;
            dir
        }
        Err(error) if secure_fs::is_not_found(&error) => {
            let key = supplied.context(
                "no release key is pinned on this host; pass --trusted-key with the release owner's public key \
                 and confirm its fingerprint with them first",
            )?;
            return Ok((key, true));
        }
        Err(error) => return Err(error),
    };
    match secure_fs::read_file_at(&etc, "release-verify-key.pub") {
        Ok((bytes, metadata)) => {
            secure_fs::verify_installer_file(&metadata, &layout.pinned_key_path().display().to_string())?;
            let pinned = TrustedKey::parse(&String::from_utf8(bytes).context("pinned release key is not UTF-8")?)?;
            if let Some(supplied) = supplied {
                ensure!(
                    supplied == pinned,
                    "trusted_key_changed: this host pins release key {} and was given {}; \
                     remove {} as root to trust a different key",
                    pinned.fingerprint(),
                    supplied.fingerprint(),
                    layout.pinned_key_path().display()
                );
            }
            Ok((pinned, false))
        }
        Err(error) if secure_fs::is_not_found(&error) => {
            let key = supplied.context(
                "no release key is pinned on this host; pass --trusted-key with the release owner's public key \
                 and confirm its fingerprint with them first",
            )?;
            Ok((key, true))
        }
        Err(error) => Err(error).context("pinned release key is not a trusted installer-owned file"),
    }
}

/// An installer-owned directory anyone may read. Created when missing; an
/// existing one must be a real directory, owned by the installer, that others
/// cannot write to.
fn ensure_root_dir(layout: &Layout, logical: &Path) -> Result<bool> {
    let (_, created) = secure_fs::ensure_installer_dir(layout.base(), logical)?;
    Ok(created)
}

/// The service user's home, then its owner-only spoke state, credential and
/// driver-home directories. An existing directory is verified, never repaired:
/// the service user controls these paths, so root must not chown or chmod them.
fn ensure_state(layout: &Layout, account: &Account) -> Result<bool> {
    let home = layout.home();
    let parent = home.parent().context("home has no parent")?;
    let parent_dir = {
        let (dir, _) = secure_fs::ensure_installer_dir(layout.base(), parent)?;
        dir
    };
    let home_name = home
        .file_name()
        .and_then(|name| name.to_str())
        .context("home is not a plain name")?;
    let (home_dir, mut changed) = secure_fs::ensure_user_dir_at(&parent_dir, home_name, account, false)?;
    let state_path = layout.state_dir();
    let state_name = state_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("state directory is not a plain name")?;
    let (state_dir, state_created) = secure_fs::ensure_user_dir_at(&home_dir, state_name, account, true)?;
    changed |= state_created;
    for name in ["credentials", "homes", "update-request"] {
        let (_, created) = secure_fs::ensure_user_dir_at(&state_dir, name, account, true)?;
        changed |= created;
    }
    Ok(changed)
}

/// Writes `bytes` into an installer-owned directory walked from the layout
/// base. The parent is re-checked (owner, no group/other write, not a symlink)
/// before the file is created.
const SERVICE_FILE_MAX: u64 = 64 * 1024;

pub(crate) fn read_service_file(layout: &Layout, logical: &Path, account: &Account) -> Result<Option<Vec<u8>>> {
    let parent = logical.parent().context("path has no parent")?;
    let name = logical
        .file_name()
        .and_then(|name| name.to_str())
        .context("path has no name")?;
    let dir = match secure_fs::open_dir(layout.base(), parent) {
        Ok(dir) => dir,
        Err(error) if secure_fs::is_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    match secure_fs::read_file_at_capped(&dir, name, SERVICE_FILE_MAX) {
        Ok((bytes, metadata)) => {
            ensure!(
                metadata.st_uid == account.uid,
                "{} is not owned by the service user",
                logical.display()
            );
            Ok(Some(bytes))
        }
        Err(error) if secure_fs::is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn unlink_service_file(layout: &Layout, logical: &Path) -> Result<()> {
    let parent = logical.parent().context("path has no parent")?;
    let name = logical
        .file_name()
        .and_then(|name| name.to_str())
        .context("path has no name")?;
    let dir = match secure_fs::open_dir(layout.base(), parent) {
        Ok(dir) => dir,
        Err(error) if secure_fs::is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    secure_fs::unlink_file_at(&dir, name)
}

pub(crate) fn journal_sqlite_path(layout: &Layout, account: &Account) -> Result<Option<PathBuf>> {
    let logical = layout.state_dir().join("journal.db");
    let parent = logical.parent().context("journal has no parent")?;
    let dir = match secure_fs::open_dir(layout.base(), parent) {
        Ok(dir) => dir,
        Err(error) if secure_fs::is_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    match secure_fs::open_regular_at(&dir, "journal.db") {
        Ok((_, metadata)) => {
            ensure!(
                metadata.st_uid == account.uid,
                "journal.db is not owned by the service user"
            );
            ensure!(
                metadata.st_size >= 100,
                "journal.db is too short to be a SQLite database"
            );
            Ok(Some(layout.on_disk(&logical)))
        }
        Err(error) if secure_fs::is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_logical(layout: &Layout, logical: &Path, bytes: &[u8], mode: u32) -> Result<bool> {
    let parent = logical.parent().context("destination has no parent")?;
    let name = logical
        .file_name()
        .and_then(|name| name.to_str())
        .context("destination has no name")?;
    let dir = secure_fs::open_installer_dir(layout.base(), parent)?;
    secure_fs::verify_installer_dir(&dir, &parent.display().to_string())?;
    secure_fs::write_file_at(&dir, name, bytes, mode, None)
}

/// Unpacks into a staging directory next to the final one and renames it into
/// place, so a release directory is either absent or complete. A release id
/// names one set of bytes forever.
pub(crate) fn install_release(layout: &Layout, release_id: &str, package: &[u8], digest: &str) -> Result<bool> {
    let target = layout.on_disk(&layout.release_dir(release_id));
    let releases_logical = layout
        .release_dir(release_id)
        .parent()
        .context("release directory has no parent")?
        .to_path_buf();
    ensure_root_dir(layout, &releases_logical)?;
    let releases = target.parent().context("release directory has no parent")?;
    if fs::symlink_metadata(&target).is_ok() {
        let recorded = fs::read_to_string(target.join(".package-sha256")).unwrap_or_default();
        ensure!(
            recorded.trim() == digest,
            "release {release_id} is already installed with different contents; a release id is never reused"
        );
        return Ok(false);
    }
    let staging = tempfile::Builder::new().prefix(".staging-").tempdir_in(releases)?;
    archive::unpack(package, staging.path())?;
    fs::write(staging.path().join(".package-sha256"), format!("{digest}\n"))?;
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o755))?;
    fs::rename(staging.keep(), &target)?;
    Ok(true)
}

/// Points `current` at the release with an atomic rename of a relative link.
pub(crate) fn point_current(layout: &Layout, release_id: &str) -> Result<bool> {
    point_link(layout, &layout.current(), ".current.new", release_id)
}

pub(crate) fn point_previous(layout: &Layout, release_id: &str) -> Result<bool> {
    point_link(layout, &layout.previous(), ".previous.new", release_id)
}

pub(crate) fn current_release(layout: &Layout) -> Option<String> {
    release_of(&layout.on_disk(&layout.current()))
}

fn release_of(link: &Path) -> Option<String> {
    fs::read_link(link)
        .ok()
        .and_then(|target| target.file_name().map(|name| name.to_string_lossy().into_owned()))
}

fn point_link(layout: &Layout, logical: &Path, temp_name: &str, release_id: &str) -> Result<bool> {
    let link = layout.on_disk(logical);
    let target = Path::new("releases").join(release_id);
    if fs::read_link(&link).is_ok_and(|existing| existing == target) {
        return Ok(false);
    }
    let temp = link.with_file_name(temp_name);
    let _ = fs::remove_file(&temp);
    symlink(&target, &temp)?;
    fs::rename(&temp, &link)?;
    Ok(true)
}

pub(crate) fn write_status(layout: &Layout, bytes: &[u8]) -> Result<bool> {
    write_logical(layout, &layout.status_path(), bytes, 0o644)
}

pub(crate) fn write_in_progress(layout: &Layout, bytes: &[u8]) -> Result<bool> {
    write_logical(layout, &layout.in_progress_path(), bytes, 0o644)
}

pub(crate) fn clear_in_progress(layout: &Layout) -> Result<()> {
    let path = layout.on_disk(&layout.in_progress_path());
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn install_update_helper(layout: &Layout) -> Result<bool> {
    let installer = layout.current().join(INSTALLER_BINARY);
    let mut changed = false;
    if layout.platform.is_macos() {
        let body = service::render_update_launchd(&installer, &layout.config_path(), &layout.request_dir());
        changed |= write_logical(layout, &layout.helper_definition(), body.as_bytes(), 0o644)?;
    } else {
        let service_body = service::render_update_systemd(&installer, &layout.config_path());
        changed |= write_logical(layout, &layout.helper_definition(), service_body.as_bytes(), 0o644)?;
        let path_body = service::render_update_path(&layout.request_dir());
        changed |= write_logical(
            layout,
            &layout.helper_path_definition().expect("linux helper path unit"),
            path_body.as_bytes(),
            0o644,
        )?;
    }
    Ok(changed)
}

pub(crate) fn resolve_pinned_key(layout: &Layout) -> Result<crate::manifest::TrustedKey> {
    let (key, _) = resolve_trust(layout, None)?;
    Ok(key)
}

pub(crate) fn wait_healthy(host: &dyn Host, health: &Health, layout: &Layout) -> Result<()> {
    let started = Instant::now();
    loop {
        if let Some(instance) = host.service_instance()? {
            std::thread::sleep(health.stable);
            if host.service_instance()? == Some(instance) {
                return Ok(());
            }
        }
        if started.elapsed() >= health.deadline {
            bail!(
                "the spoke service did not stay running within {:?} of its restart. Nothing was rolled back: \
                 the previous release is still in {}. Read the service log, then reinstall the previous signed package.",
                health.deadline,
                layout.prefix().join("releases").display()
            );
        }
        std::thread::sleep(health.poll);
    }
}

mod secure_fs {
    /// Path operations that never follow a swapped symlink.
    ///
    /// Root walks from a trusted base (`/` or the staging root), opens each
    /// component with `O_NOFOLLOW|O_DIRECTORY`, and creates or writes through
    /// `mkdirat` / `openat` / `renameat` on those descriptors. A service user who
    /// replaces a directory with a symlink after the walk cannot redirect a later
    /// root write.
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, fchown};
    use std::path::{Component, Path};

    use anyhow::{Context, Result, bail, ensure};

    use super::Account;

    pub struct DirFd(OwnedFd);

    impl DirFd {
        /// Opens `path` itself with `O_DIRECTORY`. `nofollow` applies to the last
        /// component; `/` is opened without it because the root cannot be swapped.
        pub fn open(path: &Path, nofollow: bool) -> Result<Self> {
            let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
            if nofollow {
                flags |= libc::O_NOFOLLOW;
            }
            Ok(Self(open_path(path, flags)?))
        }

        pub fn open_at(&self, name: &str) -> Result<Self> {
            self.open_at_flags(name, true)
        }

        fn open_at_flags(&self, name: &str, nofollow: bool) -> Result<Self> {
            let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
            if nofollow {
                flags |= libc::O_NOFOLLOW;
            }
            Ok(Self(open_at(&self.0, name, flags, 0)?))
        }

        /// Opens a child directory. A symlink is followed only when the target is
        /// installer-owned and not writable by others (`/etc` → `/private/etc` on
        /// macOS). User-controlled paths must call [`Self::open_at`] instead.
        pub fn open_installer_at(&self, name: &str) -> Result<Self> {
            match self.open_at(name) {
                Ok(child) => Ok(child),
                Err(error) if is_symlink_error(&error) => {
                    let child = self.open_at_flags(name, false)?;
                    verify_installer_dir(&child, name)?;
                    Ok(child)
                }
                Err(error) => Err(error),
            }
        }

        pub fn mkdir_at(&self, name: &str, mode: u32) -> Result<()> {
            let c = cstr(name)?;
            let rc = unsafe { libc::mkdirat(self.0.as_raw_fd(), c.as_ptr(), mode as libc::mode_t) };
            if rc != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| format!("cannot create {name}"));
                }
            }
            Ok(())
        }

        pub fn stat(&self) -> Result<libc::stat> {
            fstat(self.0.as_raw_fd())
        }
    }

    /// The uid that must own installer-controlled paths: the process that is
    /// installing. Production runs as root (0); `install --root` runs as the
    /// person inspecting a release.
    pub fn installer_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    pub fn verify_installer_dir(dir: &DirFd, what: &str) -> Result<()> {
        let metadata = dir.stat()?;
        ensure!(mode_is_dir(metadata.st_mode), "{what} must be a directory");
        ensure!(
            metadata.st_uid == installer_uid(),
            "{what} must be owned by the installer"
        );
        ensure!(metadata.st_mode & 0o022 == 0, "{what} is writable by others");
        Ok(())
    }

    pub fn verify_user_dir(dir: &DirFd, account: &Account, private: bool, what: &str) -> Result<()> {
        let metadata = dir.stat()?;
        ensure!(
            mode_is_dir(metadata.st_mode),
            "{what} must be a real directory, not a symlink or file"
        );
        ensure!(
            metadata.st_uid == account.uid,
            "{what} is not owned by the service user"
        );
        ensure!(
            !private || metadata.st_mode & 0o077 == 0,
            "{what} must be owner-only (0700); fix its mode and run the installer again"
        );
        Ok(())
    }

    /// Opens `logical` under `base`, creating any missing installer-owned
    /// components. `logical` is an absolute path (`/etc/boss-spoke`); `base` is
    /// `/` or the staging root.
    pub fn ensure_installer_dir(base: &Path, logical: &Path) -> Result<(DirFd, bool)> {
        let mut dir = DirFd::open(base, base != Path::new("/"))?;
        verify_installer_dir(&dir, &base.display().to_string())?;
        let mut created = false;
        for name in components(logical)? {
            match dir.open_installer_at(&name) {
                Ok(child) => {
                    verify_installer_dir(&child, &name)?;
                    dir = child;
                }
                Err(error) if is_not_found(&error) => {
                    dir.mkdir_at(&name, 0o755)?;
                    let child = dir.open_at(&name)?;
                    fchmod(&child.0, 0o755)?;
                    verify_installer_dir(&child, &name)?;
                    dir = child;
                    created = true;
                }
                Err(error) => {
                    return Err(error).context(format!("cannot open {name}"));
                }
            }
        }
        Ok((dir, created))
    }

    pub fn ensure_user_dir_at(parent: &DirFd, name: &str, account: &Account, private: bool) -> Result<(DirFd, bool)> {
        match parent.open_at(name) {
            Ok(child) => {
                verify_user_dir(&child, account, private, name)?;
                Ok((child, false))
            }
            Err(error) if is_not_found(&error) => {
                parent.mkdir_at(name, 0o700)?;
                let child = parent.open_at(name)?;
                fchown(&child.0, Some(account.uid), Some(account.gid))
                    .with_context(|| format!("cannot chown {name}"))?;
                fchmod(&child.0, if private { 0o700 } else { 0o755 })?;
                verify_user_dir(&child, account, private, name)?;
                Ok((child, true))
            }
            Err(error) => Err(error).context(format!("{name} must be a real directory, not a symlink or file")),
        }
    }

    /// Opens an existing directory by walking from `base`. Fails if any component
    /// is a symlink or missing. For installer-owned trees (`/etc`, `/opt`, …)
    /// use [`open_installer_dir`], which may follow a root-owned symlink.
    pub fn open_dir(base: &Path, logical: &Path) -> Result<DirFd> {
        walk_existing(base, logical, false)
    }

    pub fn open_installer_dir(base: &Path, logical: &Path) -> Result<DirFd> {
        walk_existing(base, logical, true)
    }

    fn walk_existing(base: &Path, logical: &Path, installer: bool) -> Result<DirFd> {
        let mut dir = DirFd::open(base, base != Path::new("/"))?;
        for name in components(logical)? {
            dir = if installer {
                dir.open_installer_at(&name)
                    .with_context(|| format!("cannot open {name}"))?
            } else {
                dir.open_at(&name)
                    .with_context(|| format!("{} is a symlink or not a directory", name))?
            };
        }
        Ok(dir)
    }

    pub fn write_file_at(dir: &DirFd, name: &str, bytes: &[u8], mode: u32, owner: Option<&Account>) -> Result<bool> {
        if file_matches(dir, name, bytes, mode, owner)? {
            return Ok(false);
        }
        let temp = format!(".{name}.{}.tmp", std::process::id());
        let _ = unlink_at(dir, &temp);
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = open_at(&dir.0, &temp, flags, mode)?;
        let mut file = File::from(fd);
        file.write_all(bytes).with_context(|| format!("cannot write {name}"))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        if let Some(account) = owner {
            fchown(&file, Some(account.uid), Some(account.gid)).with_context(|| format!("cannot chown {name}"))?;
        }
        file.sync_all()?;
        drop(file);
        rename_at(dir, &temp, name)?;
        Ok(true)
    }

    pub fn read_file_at(dir: &DirFd, name: &str) -> Result<(Vec<u8>, libc::stat)> {
        read_file_at_capped(dir, name, 1024 * 1024)
    }

    pub fn read_file_at_capped(dir: &DirFd, name: &str, max_bytes: u64) -> Result<(Vec<u8>, libc::stat)> {
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        let fd = open_at(&dir.0, name, flags, 0)?;
        let metadata = fstat(fd.as_raw_fd())?;
        ensure!(
            metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
            "{name} must be a regular file, not a symlink, fifo or device"
        );
        ensure!(
            metadata.st_size as u64 <= max_bytes,
            "{name} is larger than {max_bytes} bytes"
        );
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= max_bytes,
            "{name} is larger than {max_bytes} bytes"
        );
        Ok((bytes, metadata))
    }

    pub fn unlink_file_at(dir: &DirFd, name: &str) -> Result<()> {
        unlink_at(dir, name)
    }

    pub fn open_regular_at(dir: &DirFd, name: &str) -> Result<(OwnedFd, libc::stat)> {
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        let fd = open_at(&dir.0, name, flags, 0)?;
        let metadata = fstat(fd.as_raw_fd())?;
        ensure!(
            metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
            "{name} must be a regular file, not a symlink, fifo or device"
        );
        Ok((fd, metadata))
    }

    pub fn verify_installer_file(metadata: &libc::stat, what: &str) -> Result<()> {
        ensure!(
            metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
            "{what} must be a regular file, not a symlink"
        );
        ensure!(
            metadata.st_uid == installer_uid(),
            "{what} must be owned by the installer"
        );
        ensure!(metadata.st_mode & 0o022 == 0, "{what} is writable by others");
        Ok(())
    }

    fn file_matches(dir: &DirFd, name: &str, bytes: &[u8], mode: u32, owner: Option<&Account>) -> Result<bool> {
        let (existing, metadata) = match read_file_at(dir, name) {
            Ok(value) => value,
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(_) => return Ok(false),
        };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Ok(false);
        }
        if metadata.st_mode & 0o7777 != mode as libc::mode_t {
            return Ok(false);
        }
        if owner.is_some_and(|account| metadata.st_uid != account.uid) {
            return Ok(false);
        }
        Ok(existing == bytes)
    }

    fn components(logical: &Path) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for part in logical.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .with_context(|| format!("{} is not a plain name", logical.display()))?;
                    names.push(name.to_owned());
                }
                _ => bail!("{} has a component that is not a plain name", logical.display()),
            }
        }
        Ok(names)
    }

    fn open_path(path: &Path, flags: i32) -> Result<OwnedFd> {
        let c = CString::new(path.as_os_str().as_bytes()).context("path contains an interior NUL")?;
        let fd = unsafe { libc::open(c.as_ptr(), flags, 0o755) };
        if fd < 0 {
            return Err(map_open_error(io::Error::last_os_error(), &path.display().to_string()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn open_at(dir: &OwnedFd, name: &str, flags: i32, mode: u32) -> Result<OwnedFd> {
        let c = cstr(name)?;
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, mode) };
        if fd < 0 {
            return Err(map_open_error(io::Error::last_os_error(), name));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn map_open_error(error: io::Error, what: &str) -> anyhow::Error {
        if error.raw_os_error() == Some(libc::ELOOP) {
            anyhow::anyhow!("{what} is a symlink")
        } else {
            anyhow::Error::from(error).context(format!("cannot open {what}"))
        }
    }

    fn unlink_at(dir: &DirFd, name: &str) -> Result<()> {
        let c = cstr(name)?;
        let rc = unsafe { libc::unlinkat(dir.0.as_raw_fd(), c.as_ptr(), 0) };
        if rc != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error).with_context(|| format!("cannot remove {name}"));
            }
        }
        Ok(())
    }

    fn rename_at(dir: &DirFd, from: &str, to: &str) -> Result<()> {
        let from_c = cstr(from)?;
        let to_c = cstr(to)?;
        let rc = unsafe { libc::renameat(dir.0.as_raw_fd(), from_c.as_ptr(), dir.0.as_raw_fd(), to_c.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("cannot replace {to}"));
        }
        Ok(())
    }

    fn fstat(fd: i32) -> Result<libc::stat> {
        let mut metadata = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut metadata) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("cannot stat");
        }
        Ok(metadata)
    }

    fn fchmod(fd: &OwnedFd, mode: u32) -> Result<()> {
        let rc = unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("cannot chmod");
        }
        Ok(())
    }

    fn cstr(name: &str) -> Result<CString> {
        CString::new(name).context("path component contains an interior NUL")
    }

    fn mode_is_dir(mode: libc::mode_t) -> bool {
        mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_not_found(error: &anyhow::Error) -> bool {
        error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<io::Error>())
            .any(|cause| cause.kind() == io::ErrorKind::NotFound)
    }

    fn is_symlink_error(error: &anyhow::Error) -> bool {
        error.to_string().contains("is a symlink")
            || error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<io::Error>())
                .any(|cause| cause.raw_os_error() == Some(libc::ELOOP))
    }
}
