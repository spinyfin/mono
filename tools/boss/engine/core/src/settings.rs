//! Per-installation engine settings.
//!
//! Backed by a TOML file at `<state_root>/settings.toml`. Mirrors the
//! `feature_flags` design: a static registry declares every known key
//! with a default; the file overrides only touched keys. Missing file
//! = all defaults. Atomic writes via temp-then-rename.
//!
//! ## Adding a setting
//!
//! Append a [`SettingSpec`] entry to [`REGISTRY`] with the key,
//! human-readable description, and default. Read at consumer sites via
//! [`SettingsStore::is_enabled`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One registered setting entry. Boolean-valued for v1; extend the
/// value type if future settings need strings or numbers.
#[derive(Debug, Clone)]
pub struct SettingSpec {
    pub key: &'static str,
    pub description: &'static str,
    pub default_enabled: bool,
}

/// One registered *text*-valued setting entry, for keys whose natural value
/// isn't a bool (a cron expression, an autonomy mode, a numeric cap stored as
/// a string). Kept as a separate registry rather than widening
/// [`SettingSpec`] into an enum: the existing bool registry backs the
/// `GetSettings`/`SetSetting` RPCs the macOS Settings window renders, and
/// widening that wire shape would require an app-side change no consumer of
/// this registry needs yet. Boothby's scheduler settings are read/written
/// through their own bespoke RPCs instead (`GetBoothbyState`,
/// `SetBoothbyMode`).
#[derive(Debug, Clone)]
pub struct TextSettingSpec {
    pub key: &'static str,
    pub description: &'static str,
    pub default: &'static str,
}

/// Static registry for text-valued settings. Append here, read/write with
/// [`SettingsStore::get_text`] / [`SettingsStore::set_text`].
pub const TEXT_REGISTRY: &[TextSettingSpec] = &[
    TextSettingSpec {
        key: "boothby.mode",
        description: "Boothby's autonomy level: 'off' (scheduler never fires a pass), 'propose' \
            (every intended action becomes a human-approved proposal), or 'auto' (per-verb \
            autonomy from the action catalogue). Ships defaulting to propose.",
        default: "propose",
    },
    TextSettingSpec {
        key: "boothby.schedule",
        description: "Cron expression (UTC) for Boothby's periodic wake-up cadence.",
        default: "*/30 * * * *",
    },
    TextSettingSpec {
        key: "boothby.event_delay_secs",
        description: "Seconds an event-triggered wake-up waits before arming a pass, so the \
            sweeps' own churn guards have time to settle first.",
        default: "300",
    },
    TextSettingSpec {
        key: "boothby.min_pass_gap_secs",
        description: "Minimum seconds between the start of one Boothby pass and the next, \
            regardless of trigger.",
        default: "900",
    },
    TextSettingSpec {
        key: "boothby.pass_timeout_secs",
        description: "Seconds a Boothby pass may run before the runtime force-closes it as \
            timed_out.",
        default: "900",
    },
];

/// Static registry. Append here, read with `SettingsStore::is_enabled`.
pub const REGISTRY: &[SettingSpec] = &[
    SettingSpec {
        key: "default_pr_draft_mode",
        description: "Workers will pass --draft to gh pr create unless the chore description overrides.",
        default_enabled: false,
    },
    SettingSpec {
        key: "workers.non_opus_permission_mode",
        // false = --dangerously-skip-permissions (personal laptop default).
        // true  = --permission-mode auto (corp laptop: dangerously-skip is
        // forbidden, but auto mode works for Sonnet/Haiku too).
        // Opus workers always get --permission-mode auto regardless of this
        // setting (corp env does not default to auto for Opus either).
        description: "Permission mode for Sonnet/Haiku workers. Disabled (default): --dangerously-skip-permissions. Enabled: --permission-mode auto.",
        default_enabled: false,
    },
    SettingSpec {
        key: "coordinator.direct_developer_mode",
        // false (default) = coordinator uses 'boss shake' for Boss bugs/features
        //                   (files a GitHub issue in spinyfin/mono).
        // true            = coordinator prefers filing a chore against the Boss
        //                   product directly; 'boss shake' is used only when the
        //                   user explicitly requests a GitHub issue.
        // Intended for the machine where Boss is actively developed using Boss.
        description: "Coordinator files Boss bugs/features as chores against the Boss product instead of GitHub issues. Use on a machine where you develop Boss with Boss.",
        default_enabled: false,
    },
];

/// Wire/display snapshot of one setting's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingSnapshot {
    pub key: String,
    pub description: String,
    pub default_enabled: bool,
    pub enabled: bool,
}

/// On-disk file shape: flat key → bool mapping, plus a nested `[text]`
/// table for [`TEXT_REGISTRY`] overrides. The nested table keeps the
/// existing flat bool shape byte-for-byte backward compatible — an
/// installation's existing `settings.toml` parses unchanged; `text` is
/// simply absent until a Boothby setting is first overridden.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(flatten)]
    settings: HashMap<String, bool>,
    #[serde(default)]
    text: HashMap<String, String>,
}

/// Thread-safe store. In-memory overrides keyed by setting key;
/// falls back to registry default for any key not in the map.
pub struct SettingsStore {
    path: PathBuf,
    state: Mutex<HashMap<String, bool>>,
    text_state: Mutex<HashMap<String, String>>,
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(HashMap::new()),
            text_state: Mutex::new(HashMap::new()),
        }
    }

    pub fn default_path(state_root: &Path) -> PathBuf {
        state_root.join("settings.toml")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file into memory. Missing file = empty overrides
    /// (all defaults). A parse error returns `Err` without touching the
    /// in-memory map.
    pub fn load(&self) -> Result<()> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut guard = self.state.lock().expect("settings lock poisoned");
                guard.clear();
                let mut text_guard = self.text_state.lock().expect("settings text lock poisoned");
                text_guard.clear();
                return Ok(());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("read settings file: {}", self.path.display()));
            }
        };
        let parsed: FileShape =
            toml::from_str(&contents).with_context(|| format!("parse settings file: {}", self.path.display()))?;
        let mut guard = self.state.lock().expect("settings lock poisoned");
        guard.clear();
        for (key, value) in parsed.settings {
            // workers.always_use_opus was superseded by workers.non_opus_permission_mode
            // (T462 → this chore). If the old key is still in the file it is a no-op;
            // log once so operators know to clean it up.
            if key == "workers.always_use_opus" {
                tracing::warn!(
                    "settings: ignoring obsolete key 'workers.always_use_opus' \
                     (superseded by 'workers.non_opus_permission_mode'); \
                     you can remove it from settings.toml"
                );
                continue;
            }
            if REGISTRY.iter().any(|spec| spec.key == key) {
                guard.insert(key, value);
            }
        }
        drop(guard);
        let mut text_guard = self.text_state.lock().expect("settings text lock poisoned");
        text_guard.clear();
        for (key, value) in parsed.text {
            if TEXT_REGISTRY.iter().any(|spec| spec.key == key) {
                text_guard.insert(key, value);
            }
        }
        Ok(())
    }

    /// Current value for `key`. Returns the registry default when no
    /// override exists; `None` when the key is unknown.
    pub fn get(&self, key: &str) -> Option<bool> {
        let spec = REGISTRY.iter().find(|spec| spec.key == key)?;
        let guard = self.state.lock().expect("settings lock poisoned");
        Some(guard.get(key).copied().unwrap_or(spec.default_enabled))
    }

    /// Convenience for the one-line consumer check.
    pub fn is_enabled(&self, key: &str) -> bool {
        self.get(key).unwrap_or(false)
    }

    /// Set `key` to `enabled` and atomically persist.
    pub fn set(&self, key: &str, enabled: bool) -> Result<()> {
        if !REGISTRY.iter().any(|spec| spec.key == key) {
            anyhow::bail!("unknown setting: {key}");
        }
        {
            let mut guard = self.state.lock().expect("settings lock poisoned");
            guard.insert(key.to_owned(), enabled);
        }
        self.write_to_disk()
    }

    /// Current value for a [`TEXT_REGISTRY`] key. Returns the registry
    /// default when no override exists; `None` when the key is unknown.
    pub fn get_text(&self, key: &str) -> Option<String> {
        let spec = TEXT_REGISTRY.iter().find(|spec| spec.key == key)?;
        let guard = self.text_state.lock().expect("settings text lock poisoned");
        Some(guard.get(key).cloned().unwrap_or_else(|| spec.default.to_owned()))
    }

    /// Convenience for a text setting whose value is really an integer
    /// (event-delay / min-pass-gap / pass-timeout, all stored as strings so
    /// they share the same on-disk shape as the cron/mode settings). Falls
    /// back to the registry default's parse on a corrupt override rather
    /// than erroring — the default is always valid by construction.
    pub fn get_text_i64(&self, key: &str) -> Option<i64> {
        let spec = TEXT_REGISTRY.iter().find(|spec| spec.key == key)?;
        match self.get_text(key)?.parse() {
            Ok(value) => Some(value),
            Err(_) => Some(
                spec.default
                    .parse()
                    .unwrap_or_else(|_| panic!("TEXT_REGISTRY default for {key} is not a valid i64")),
            ),
        }
    }

    /// Set a [`TEXT_REGISTRY`] key and atomically persist.
    pub fn set_text(&self, key: &str, value: String) -> Result<()> {
        if !TEXT_REGISTRY.iter().any(|spec| spec.key == key) {
            anyhow::bail!("unknown setting: {key}");
        }
        {
            let mut guard = self.text_state.lock().expect("settings text lock poisoned");
            guard.insert(key.to_owned(), value);
        }
        self.write_to_disk()
    }

    /// Snapshot of every registered setting in registry order.
    pub fn snapshot_all(&self) -> Vec<SettingSnapshot> {
        let guard = self.state.lock().expect("settings lock poisoned");
        REGISTRY
            .iter()
            .map(|spec| SettingSnapshot {
                key: spec.key.to_owned(),
                description: spec.description.to_owned(),
                default_enabled: spec.default_enabled,
                enabled: guard.get(spec.key).copied().unwrap_or(spec.default_enabled),
            })
            .collect()
    }

    fn write_to_disk(&self) -> Result<()> {
        let serialized = {
            let guard = self.state.lock().expect("settings lock poisoned");
            let text_guard = self.text_state.lock().expect("settings text lock poisoned");
            let shape = FileShape {
                settings: guard.clone(),
                text: text_guard.clone(),
            };
            toml::to_string_pretty(&shape).context("serialize settings to TOML")?
        };

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create settings parent dir: {}", parent.display()))?;
        }

        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, serialized).with_context(|| format!("write settings temp file: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} → {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store(tmp: &TempDir) -> SettingsStore {
        SettingsStore::new(tmp.path().join("settings.toml"))
    }

    #[test]
    fn missing_file_returns_registry_default() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(!store.is_enabled("default_pr_draft_mode"));
    }

    #[test]
    fn set_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set("default_pr_draft_mode", true).unwrap();
        assert!(store.is_enabled("default_pr_draft_mode"));
        let store2 = make_store(&tmp);
        store2.load().unwrap();
        assert!(store2.is_enabled("default_pr_draft_mode"));
    }

    #[test]
    fn unknown_key_set_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let err = store.set("not_a_real_setting", true).unwrap_err();
        assert!(err.to_string().contains("not_a_real_setting"));
    }

    #[test]
    fn unknown_key_is_enabled_is_false() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(!store.is_enabled("not_a_real_setting"));
    }

    #[test]
    fn snapshot_lists_every_registered_setting() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        let snap = store.snapshot_all();
        assert_eq!(snap.len(), REGISTRY.len());
        let draft = snap.iter().find(|s| s.key == "default_pr_draft_mode").unwrap();
        assert!(!draft.default_enabled);
        assert!(!draft.enabled);
    }

    #[test]
    fn unknown_key_in_file_is_dropped_on_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "default_pr_draft_mode = true\nstale_setting = false\n").unwrap();
        let store = SettingsStore::new(path);
        store.load().unwrap();
        assert!(store.is_enabled("default_pr_draft_mode"));
        assert!(store.get("stale_setting").is_none());
    }

    #[test]
    fn non_opus_permission_mode_defaults_to_false() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(!store.is_enabled("workers.non_opus_permission_mode"));
    }

    #[test]
    fn direct_developer_mode_defaults_to_false() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert!(!store.is_enabled("coordinator.direct_developer_mode"));
    }

    #[test]
    fn direct_developer_mode_can_be_toggled() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set("coordinator.direct_developer_mode", true).unwrap();
        assert!(store.is_enabled("coordinator.direct_developer_mode"));
        let store2 = make_store(&tmp);
        store2.load().unwrap();
        assert!(store2.is_enabled("coordinator.direct_developer_mode"));
    }

    #[test]
    fn obsolete_always_use_opus_key_is_ignored_on_load() {
        // The old workers.always_use_opus key from T462 must not cause an error;
        // it is silently skipped (and a tracing warning is emitted, not tested here).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(
            &path,
            "\"workers.always_use_opus\" = true\ndefault_pr_draft_mode = true\n",
        )
        .unwrap();
        let store = SettingsStore::new(path);
        store.load().unwrap();
        assert!(store.is_enabled("default_pr_draft_mode"));
        assert!(store.get("workers.always_use_opus").is_none());
    }

    #[test]
    fn set_persists_only_to_target_path() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set("default_pr_draft_mode", true).unwrap();
        let on_disk = std::fs::read_to_string(store.path()).unwrap();
        assert!(on_disk.contains("default_pr_draft_mode"));
        assert!(on_disk.contains("true"));
        assert!(!tmp.path().join("settings.toml.tmp").exists());
    }

    #[test]
    fn text_setting_missing_file_returns_registry_default() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        assert_eq!(store.get_text("boothby.mode").as_deref(), Some("propose"));
        assert_eq!(store.get_text_i64("boothby.min_pass_gap_secs"), Some(900));
    }

    #[test]
    fn text_setting_set_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set_text("boothby.mode", "auto".to_owned()).unwrap();
        assert_eq!(store.get_text("boothby.mode").as_deref(), Some("auto"));
        let store2 = make_store(&tmp);
        store2.load().unwrap();
        assert_eq!(store2.get_text("boothby.mode").as_deref(), Some("auto"));
    }

    #[test]
    fn text_setting_unknown_key_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let err = store.set_text("not_a_real_text_setting", "x".to_owned()).unwrap_err();
        assert!(err.to_string().contains("not_a_real_text_setting"));
        assert!(store.get_text("not_a_real_text_setting").is_none());
    }

    #[test]
    fn text_setting_does_not_disturb_bool_settings_on_disk() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set("default_pr_draft_mode", true).unwrap();
        store.set_text("boothby.schedule", "0 * * * *".to_owned()).unwrap();

        let store2 = make_store(&tmp);
        store2.load().unwrap();
        assert!(store2.is_enabled("default_pr_draft_mode"));
        assert_eq!(store2.get_text("boothby.schedule").as_deref(), Some("0 * * * *"));
    }

    #[test]
    fn existing_flat_bool_file_still_parses_with_no_text_table() {
        // Back-compat: an installation's settings.toml written before the
        // `[text]` table existed must still load cleanly.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "default_pr_draft_mode = true\n").unwrap();
        let store = SettingsStore::new(path);
        store.load().unwrap();
        assert!(store.is_enabled("default_pr_draft_mode"));
        assert_eq!(store.get_text("boothby.mode").as_deref(), Some("propose"));
    }
}
