//! `[backup.offsite]` section of `settings.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Human-readable section name included in every error to name the setting to fix.
pub const CONFIG_SECTION: &str = "[backup.offsite]";

const DEFAULT_KEEP_HOURLY: usize = 24;
const DEFAULT_KEEP_DAILY: usize = 14;

/// Off-machine backup settings. Disabled unless `enabled = true`.
///
/// ```toml
/// [backup.offsite]
/// enabled = true
/// destination = "/Users/me/Library/CloudStorage/GoogleDrive-me@example.com/My Drive/boss-backups"
/// keep_hourly = 24
/// keep_daily = 14
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsiteConfig {
    pub enabled: bool,
    pub destination: Option<PathBuf>,
    /// Number of most recent distinct hours that keep their newest copy.
    pub keep_hourly: usize,
    /// Number of most recent distinct days that keep their newest copy.
    pub keep_daily: usize,
}

impl Default for OffsiteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: None,
            keep_hourly: DEFAULT_KEEP_HOURLY,
            keep_daily: DEFAULT_KEEP_DAILY,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileShape {
    #[serde(default)]
    backup: BackupTable,
}

#[derive(Debug, Default, Deserialize)]
struct BackupTable {
    #[serde(default)]
    offsite: RawOffsite,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOffsite {
    #[serde(default)]
    enabled: bool,
    destination: Option<String>,
    #[serde(default = "default_keep_hourly")]
    keep_hourly: usize,
    #[serde(default = "default_keep_daily")]
    keep_daily: usize,
}

impl Default for RawOffsite {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: None,
            keep_hourly: DEFAULT_KEEP_HOURLY,
            keep_daily: DEFAULT_KEEP_DAILY,
        }
    }
}

fn default_keep_hourly() -> usize {
    DEFAULT_KEEP_HOURLY
}

fn default_keep_daily() -> usize {
    DEFAULT_KEEP_DAILY
}

/// A destination that passed [`OffsiteConfig::validate`]: the per-host
/// directory the copies go into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDestination {
    pub host_dir: PathBuf,
}

impl OffsiteConfig {
    /// Parse the `[backup.offsite]` section out of a `settings.toml` body.
    /// A missing section yields the disabled default. Other top-level keys
    /// are ignored (they belong to the boolean settings registry).
    pub fn from_toml_str(contents: &str) -> Result<Self> {
        let parsed: FileShape = toml::from_str(contents).with_context(|| format!("parse {CONFIG_SECTION}"))?;
        let raw = parsed.backup.offsite;
        let config = Self {
            enabled: raw.enabled,
            destination: raw.destination.filter(|d| !d.trim().is_empty()).map(PathBuf::from),
            keep_hourly: raw.keep_hourly,
            keep_daily: raw.keep_daily,
        };
        if config.enabled && config.keep_hourly == 0 && config.keep_daily == 0 {
            bail!("{CONFIG_SECTION} keep_hourly and keep_daily are both 0; that would delete every copy");
        }
        Ok(config)
    }

    /// Load from a `settings.toml` path. A missing file is the disabled default.
    pub fn load(settings_path: &Path) -> Result<Self> {
        match std::fs::read_to_string(settings_path) {
            Ok(contents) => Self::from_toml_str(&contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read settings file {}", settings_path.display())),
        }
    }

    /// The no-I/O part of validation: `destination` must be set and absolute.
    pub fn destination_setting(&self) -> Result<&Path> {
        let Some(destination) = &self.destination else {
            bail!("{CONFIG_SECTION} is enabled but `destination` is not set");
        };
        if !destination.is_absolute() {
            bail!(
                "{CONFIG_SECTION} destination {} must be an absolute path",
                destination.display()
            );
        }
        Ok(destination)
    }

    /// Check that an enabled config points at a usable destination, creating
    /// the per-host subfolder. Errors name the offending setting. Call at
    /// each copy on the destination worker: a sync folder can be unmounted
    /// and remounted while the engine runs.
    pub fn validate(&self, host: &str) -> Result<ValidatedDestination> {
        let destination = self.destination_setting()?;
        match std::fs::metadata(destination) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{CONFIG_SECTION} destination {} is not a directory",
                destination.display()
            ),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "{CONFIG_SECTION} destination {} is missing or unreadable",
                        destination.display()
                    )
                });
            }
        }
        let host_dir = destination.join(sanitize_host_component(host));
        std::fs::create_dir_all(&host_dir).with_context(|| {
            format!(
                "{CONFIG_SECTION} destination {} is not writable (cannot create {})",
                destination.display(),
                host_dir.display()
            )
        })?;
        let probe = host_dir.join(".write-probe.partial");
        std::fs::write(&probe, b"probe")
            .and_then(|()| std::fs::remove_file(&probe))
            .with_context(|| format!("{CONFIG_SECTION} destination {} is not writable", host_dir.display()))?;
        Ok(ValidatedDestination { host_dir })
    }
}

/// Reduce a hostname to a safe single path component.
pub fn sanitize_host_component(host: &str) -> String {
    let cleaned: String = host
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        "unknown-host".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn disabled_by_default() {
        let cfg = OffsiteConfig::from_toml_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg, OffsiteConfig::default());
        let cfg = OffsiteConfig::from_toml_str("default_pr_draft_mode = true\n").unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn missing_settings_file_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let cfg = OffsiteConfig::load(&tmp.path().join("settings.toml")).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn parses_full_section() {
        let cfg = OffsiteConfig::from_toml_str(
            "[backup.offsite]\nenabled = true\ndestination = \"/mnt/x\"\nkeep_hourly = 5\nkeep_daily = 2\n",
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.destination, Some(PathBuf::from("/mnt/x")));
        assert_eq!((cfg.keep_hourly, cfg.keep_daily), (5, 2));
    }

    #[test]
    fn unknown_field_in_section_is_an_error() {
        let err = OffsiteConfig::from_toml_str("[backup.offsite]\nenabled = true\ndestinaton = \"/x\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("destinaton"), "{err:#}");
    }

    #[test]
    fn zero_retention_is_rejected_when_enabled() {
        let err = OffsiteConfig::from_toml_str("[backup.offsite]\nenabled = true\nkeep_hourly = 0\nkeep_daily = 0\n")
            .unwrap_err();
        assert!(format!("{err:#}").contains("keep_hourly"));
    }

    #[test]
    fn enabled_without_destination_names_the_setting() {
        let cfg = OffsiteConfig::from_toml_str("[backup.offsite]\nenabled = true\n").unwrap();
        let err = cfg.validate("host").unwrap_err();
        assert!(format!("{err:#}").contains("destination"), "{err:#}");
        assert!(format!("{err:#}").contains(CONFIG_SECTION));
    }

    #[test]
    fn relative_destination_is_rejected() {
        let cfg = OffsiteConfig {
            enabled: true,
            destination: Some(PathBuf::from("relative/dir")),
            ..OffsiteConfig::default()
        };
        assert!(format!("{:#}", cfg.validate("h").unwrap_err()).contains("absolute"));
    }

    #[test]
    fn missing_destination_directory_is_rejected_and_not_created() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("not-mounted");
        let cfg = OffsiteConfig {
            enabled: true,
            destination: Some(dest.clone()),
            ..OffsiteConfig::default()
        };
        let err = cfg.validate("h").unwrap_err();
        assert!(format!("{err:#}").contains("missing"), "{err:#}");
        assert!(!dest.exists(), "must never create the destination itself");
    }

    #[test]
    fn file_destination_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("afile");
        std::fs::write(&dest, b"x").unwrap();
        let cfg = OffsiteConfig {
            enabled: true,
            destination: Some(dest),
            ..OffsiteConfig::default()
        };
        assert!(format!("{:#}", cfg.validate("h").unwrap_err()).contains("not a directory"));
    }

    #[test]
    fn valid_destination_creates_host_subfolder() {
        let tmp = TempDir::new().unwrap();
        let cfg = OffsiteConfig {
            enabled: true,
            destination: Some(tmp.path().to_owned()),
            ..OffsiteConfig::default()
        };
        let v = cfg.validate("my/host").unwrap();
        assert_eq!(v.host_dir, tmp.path().join("my_host"));
        assert!(v.host_dir.is_dir());
        assert!(!v.host_dir.join(".write-probe.partial").exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_destination_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("ro");
        std::fs::create_dir(&dest).unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o555)).unwrap();
        let cfg = OffsiteConfig {
            enabled: true,
            destination: Some(dest.clone()),
            ..OffsiteConfig::default()
        };
        let result = cfg.validate("h");
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root can write anywhere; only assert when the OS actually denied it.
        if let Err(err) = result {
            assert!(format!("{err:#}").contains("not writable"), "{err:#}");
        }
    }

    #[test]
    fn host_component_is_sanitized() {
        assert_eq!(sanitize_host_component("Brian's Mac.local"), "Brian_s_Mac.local");
        assert_eq!(sanitize_host_component("../etc"), "_etc");
        assert_eq!(sanitize_host_component(""), "unknown-host");
    }
}
