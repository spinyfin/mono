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

use std::collections::{BTreeSet, HashMap};
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

/// Settings key whose value is the set of worker pools hosted by tmux.
///
/// Unlike the boolean settings in [`REGISTRY`], this is deliberately a set:
/// migration proceeds review → automation → interactive, and a pool can be
/// rolled back without changing the hosting mode of another pool.
pub const TMUX_HOSTING_SETTING: &str = "workers.tmux_hosting";

/// A worker pool eligible for tmux hosting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TmuxHostingPool {
    Interactive,
    Automation,
    Review,
}

impl TmuxHostingPool {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "automation" => Ok(Self::Automation),
            "review" => Ok(Self::Review),
            _ => anyhow::bail!(
                "invalid {TMUX_HOSTING_SETTING} pool {value:?} (expected interactive, automation, or review)"
            ),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Automation => "automation",
            Self::Review => "review",
        }
    }

    fn from_attributed_pool(value: &str) -> Option<Self> {
        match value {
            // Live worker state calls the primary interactive pool "main";
            // the settings vocabulary keeps the operator-facing name from the
            // tmux migration design.
            "main" | "interactive" => Some(Self::Interactive),
            "automation" => Some(Self::Automation),
            "review" => Some(Self::Review),
            _ => None,
        }
    }
}

/// The configured set of pools whose workers launch in detached tmux
/// sessions. Every local pool is enabled by default for this release; an
/// explicit empty set remains the rollback control for the legacy app-hosted
/// path until this setting is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxHostingPools(BTreeSet<TmuxHostingPool>);

impl TmuxHostingPools {
    /// Build a pool set from an arbitrary collection of pools — the
    /// building block for staged, pool-by-pool enablement outside the
    /// operator-facing all-or-nothing switch (see
    /// [`SettingsStore::set_tmux_hosting_pools`]).
    pub fn from_pools(pools: impl IntoIterator<Item = TmuxHostingPool>) -> Self {
        Self(pools.into_iter().collect())
    }

    /// Whether an attributed worker pool (the internal `main` / `automation`
    /// / `review` labels) is configured for tmux hosting.
    pub fn contains_attributed_pool(&self, pool: &str) -> bool {
        TmuxHostingPool::from_attributed_pool(pool).is_some_and(|pool| self.0.contains(&pool))
    }

    fn from_toml(value: &toml::Value) -> Result<Self> {
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("{TMUX_HOSTING_SETTING} must be an array of pool names"))?;
        let mut pools = BTreeSet::new();
        for value in values {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{TMUX_HOSTING_SETTING} entries must be strings"))?;
            pools.insert(TmuxHostingPool::parse(value)?);
        }
        Ok(Self(pools))
    }

    fn as_toml(&self) -> toml::Value {
        toml::Value::Array(
            self.0
                .iter()
                .map(|pool| toml::Value::String(pool.as_str().to_owned()))
                .collect(),
        )
    }

    /// Every known pool — what the operator-facing on/off switch enables
    /// when flipped on.
    fn all() -> Self {
        Self(
            [
                TmuxHostingPool::Interactive,
                TmuxHostingPool::Automation,
                TmuxHostingPool::Review,
            ]
            .into_iter()
            .collect(),
        )
    }

    fn empty() -> Self {
        Self(BTreeSet::new())
    }
}

impl Default for TmuxHostingPools {
    fn default() -> Self {
        Self::all()
    }
}

/// Description shown in the Boss UI settings toggle for the operator-facing
/// tmux-hosting switch. Lives here (not in `engine_meta`) since only this
/// module knows the pool-set representation the boolean is a projection of.
const TMUX_HOSTING_DESCRIPTION: &str = "Deprecated temporary rollback control; scheduled for \
     removal after this release. Enabled by default, it hosts worker panes (review, automation, \
     interactive) in detached tmux sessions that survive an app or engine restart. Disabling it \
     affects only new dispatches; already-running tmux workers keep their durable teardown path. \
     The coordinator's tmux session is unconditional and is not controlled by this setting.";

/// Per-pool tmux-hosting snapshot for the visibility surfaces (the dispatch
/// event stamp and `bossctl doctor`): whether each pool currently launches
/// its workers in tmux. Independent of the operator-facing boolean switch,
/// which enables/disables all three atomically — this reflects whatever the
/// underlying pool set actually holds, including a hand-edited or
/// mid-acceptance-sweep partial state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmuxHostingPoolSnapshot {
    pub review: bool,
    pub automation: bool,
    pub interactive: bool,
}

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

/// On-disk file shape. The established settings remain booleans, while
/// `workers.tmux_hosting` is a deliberately typed pool set.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(flatten)]
    settings: HashMap<String, toml::Value>,
}

#[derive(Debug, Default)]
struct SettingsState {
    booleans: HashMap<String, bool>,
    tmux_hosting: TmuxHostingPools,
    tmux_hosting_overridden: bool,
}

/// Thread-safe store. In-memory overrides keyed by setting key;
/// falls back to registry default for any key not in the map.
pub struct SettingsStore {
    path: PathBuf,
    state: Mutex<SettingsState>,
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(SettingsState::default()),
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
                *guard = SettingsState::default();
                return Ok(());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("read settings file: {}", self.path.display()));
            }
        };
        let parsed: FileShape =
            toml::from_str(&contents).with_context(|| format!("parse settings file: {}", self.path.display()))?;
        let mut next = SettingsState::default();
        for (key, value) in parsed.settings {
            if key == TMUX_HOSTING_SETTING {
                next.tmux_hosting = TmuxHostingPools::from_toml(&value)?;
                next.tmux_hosting_overridden = true;
                continue;
            }
            // `workers.always_use_opus` was replaced by
            // `workers.non_opus_permission_mode`. If the old key is still in the
            // file it is a no-op; log once so it can be cleaned up.
            if key == "workers.always_use_opus" {
                tracing::warn!(
                    "settings: ignoring obsolete key 'workers.always_use_opus' \
                     (superseded by 'workers.non_opus_permission_mode'); \
                     you can remove it from settings.toml"
                );
                continue;
            }
            if REGISTRY.iter().any(|spec| spec.key == key) {
                let value = value
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("setting {key:?} must be a boolean, got {value}"))?;
                next.booleans.insert(key, value);
            }
        }
        let mut guard = self.state.lock().expect("settings lock poisoned");
        *guard = next;
        Ok(())
    }

    /// Current value for `key`. Returns the registry default when no
    /// override exists; `None` when the key is unknown.
    pub fn get(&self, key: &str) -> Option<bool> {
        let spec = REGISTRY.iter().find(|spec| spec.key == key)?;
        let guard = self.state.lock().expect("settings lock poisoned");
        Some(guard.booleans.get(key).copied().unwrap_or(spec.default_enabled))
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
            guard.booleans.insert(key.to_owned(), enabled);
        }
        self.write_to_disk()
    }

    /// Set the pools that launch workers in tmux and atomically persist them.
    /// This is separate from [`Self::set`] because the settings RPC models
    /// boolean toggles only. [`Self::set_tmux_hosting_enabled`] is the
    /// boolean-shaped entry point the UI actually calls, mapping its on/off
    /// switch onto the full pool set; this method stays available directly
    /// for staged, pool-by-pool enablement outside the UI (e.g. an
    /// acceptance sweep or a hand edit of `settings.toml`) without going
    /// through the all-or-nothing switch.
    pub fn set_tmux_hosting_pools(&self, pools: TmuxHostingPools) -> Result<()> {
        {
            let mut guard = self.state.lock().expect("settings lock poisoned");
            guard.tmux_hosting = pools;
            guard.tmux_hosting_overridden = true;
        }
        self.write_to_disk()
    }

    /// Set (or clear) tmux hosting for every worker pool at once — the
    /// operator-facing switch surfaced in the Boss UI settings window.
    /// `true` enables review + automation + interactive together. `false`
    /// clears the set, so subsequent dispatches take the legacy app-hosted
    /// path; it does not tear anything down. Already-running tmux-hosted
    /// workers keep their sessions and are reaped by
    /// `ServerState::reap_tmux_worker` when they terminate, the same as any
    /// other tmux-hosted run — teardown keys on the durably-recorded tmux
    /// identity columns on `work_runs`, not on this setting, so in-flight
    /// runs are unaffected by the flip either way. The coordinator's tmux
    /// session is unconditional (see `coordinator_tmux`) and is not
    /// affected by this setting.
    pub fn set_tmux_hosting_enabled(&self, enabled: bool) -> Result<()> {
        self.set_tmux_hosting_pools(if enabled {
            TmuxHostingPools::all()
        } else {
            TmuxHostingPools::empty()
        })
    }

    /// Whether the given attributed pool should use the tmux-hosted spawn
    /// path. Unknown pool labels intentionally remain on the legacy path.
    pub fn tmux_hosting_enabled_for(&self, pool: &str) -> bool {
        self.state
            .lock()
            .expect("settings lock poisoned")
            .tmux_hosting
            .contains_attributed_pool(pool)
    }

    /// Snapshot of the operator-facing tmux-hosting boolean, for
    /// `GetSettings`. `enabled` is true only when every known pool is
    /// currently configured for tmux hosting — a partially-migrated set
    /// (e.g. left over from a hand-edited `settings.toml` or a sweep in
    /// progress) reads as off until the operator flips the switch, since
    /// the UI control itself is all-or-nothing.
    pub fn tmux_hosting_snapshot(&self) -> SettingSnapshot {
        let guard = self.state.lock().expect("settings lock poisoned");
        SettingSnapshot {
            key: TMUX_HOSTING_SETTING.to_owned(),
            description: TMUX_HOSTING_DESCRIPTION.to_owned(),
            default_enabled: true,
            enabled: guard.tmux_hosting == TmuxHostingPools::all(),
        }
    }

    /// Per-pool tmux-hosting snapshot, for the dispatch-event stamp and
    /// `bossctl doctor` — see [`TmuxHostingPoolSnapshot`]. Reads the pool set
    /// under a single lock acquisition so a concurrent `set_tmux_hosting_*`
    /// call can never produce a torn combination that never actually existed.
    pub fn tmux_hosting_pool_snapshot(&self) -> TmuxHostingPoolSnapshot {
        let guard = self.state.lock().expect("settings lock poisoned");
        TmuxHostingPoolSnapshot {
            review: guard.tmux_hosting.contains_attributed_pool("review"),
            automation: guard.tmux_hosting.contains_attributed_pool("automation"),
            interactive: guard.tmux_hosting.contains_attributed_pool("interactive"),
        }
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
                enabled: guard.booleans.get(spec.key).copied().unwrap_or(spec.default_enabled),
            })
            .collect()
    }

    fn write_to_disk(&self) -> Result<()> {
        let serialized = {
            let guard = self.state.lock().expect("settings lock poisoned");
            let mut settings = guard
                .booleans
                .iter()
                .map(|(key, value)| (key.clone(), toml::Value::Boolean(*value)))
                .collect::<HashMap<_, _>>();
            if guard.tmux_hosting_overridden {
                settings.insert(TMUX_HOSTING_SETTING.to_owned(), guard.tmux_hosting.as_toml());
            }
            let shape = FileShape { settings };
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
    fn tmux_hosting_defaults_to_all_local_pools() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();

        assert!(store.tmux_hosting_enabled_for("review"));
        assert!(store.tmux_hosting_enabled_for("automation"));
        assert!(store.tmux_hosting_enabled_for("main"));
    }

    #[test]
    fn tmux_hosting_pool_set_round_trips_and_uses_interactive_for_main() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let pools = TmuxHostingPools(
            [TmuxHostingPool::Review, TmuxHostingPool::Interactive]
                .into_iter()
                .collect(),
        );
        store.set_tmux_hosting_pools(pools).unwrap();

        let restored = make_store(&tmp);
        restored.load().unwrap();
        assert!(restored.tmux_hosting_enabled_for("review"));
        assert!(restored.tmux_hosting_enabled_for("main"));
        assert!(!restored.tmux_hosting_enabled_for("automation"));
    }

    #[test]
    fn tmux_hosting_snapshot_defaults_to_enabled() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        let snap = store.tmux_hosting_snapshot();
        assert_eq!(snap.key, TMUX_HOSTING_SETTING);
        assert!(snap.default_enabled);
        assert!(snap.enabled);
    }

    #[test]
    fn set_tmux_hosting_enabled_true_covers_every_pool() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.load().unwrap();
        store.set_tmux_hosting_enabled(true).unwrap();

        assert!(store.tmux_hosting_snapshot().enabled);
        let pools = store.tmux_hosting_pool_snapshot();
        assert!(pools.review);
        assert!(pools.automation);
        assert!(pools.interactive);

        store.set_tmux_hosting_enabled(false).unwrap();
        assert!(!store.tmux_hosting_snapshot().enabled);
        let pools = store.tmux_hosting_pool_snapshot();
        assert!(!pools.review);
        assert!(!pools.automation);
        assert!(!pools.interactive);
    }

    #[test]
    fn tmux_hosting_snapshot_reads_off_for_a_partial_pool_set() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let partial = TmuxHostingPools([TmuxHostingPool::Review].into_iter().collect());
        store.set_tmux_hosting_pools(partial).unwrap();

        // The all-or-nothing UI switch reads off even though review alone
        // is enabled — a partial set only arises outside the switch (a
        // staged sweep, a hand-edited settings.toml).
        assert!(!store.tmux_hosting_snapshot().enabled);
        let pools = store.tmux_hosting_pool_snapshot();
        assert!(pools.review);
        assert!(!pools.automation);
        assert!(!pools.interactive);
    }

    #[test]
    fn set_tmux_hosting_enabled_round_trips_through_reload() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_tmux_hosting_enabled(true).unwrap();

        let restored = make_store(&tmp);
        restored.load().unwrap();
        assert!(restored.tmux_hosting_snapshot().enabled);
    }

    #[test]
    fn set_tmux_hosting_enabled_false_round_trips_through_reload() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_tmux_hosting_enabled(true).unwrap();
        store.set_tmux_hosting_enabled(false).unwrap();

        let restored = make_store(&tmp);
        restored.load().unwrap();
        assert!(!restored.tmux_hosting_snapshot().enabled);
        let pools = restored.tmux_hosting_pool_snapshot();
        assert!(!pools.review);
        assert!(!pools.automation);
        assert!(!pools.interactive);
    }

    #[test]
    fn tmux_hosting_rejects_unknown_pool_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "\"workers.tmux_hosting\" = [\"unsupported\"]\n").unwrap();

        let error = SettingsStore::new(path).load().unwrap_err();
        assert!(error.to_string().contains("unsupported"));
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
        // The obsolete workers.always_use_opus key must not cause an error;
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
}
