//! Hub-driven spoke upgrade rollout.
//!
//! Desired release is the hub's own stamped Boss version. Matching signed
//! spoke packages are listed in `spoke-packages.json` next to that version's
//! GitHub release assets (or a file pointed at by `BOSS_SPOKE_PACKAGES`).
//! Connected spokes whose running release differs are offered `Upgrade` one
//! host at a time; a failure or refusal of that release stops further offers.
mod rollout;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boss_spoke_stream::StreamManager;
use boss_work_db::WorkDb;
use serde::Deserialize;

pub use rollout::UpgradeController;

/// How the hub learns the manifest URL and digest for its own release.
#[async_trait]
pub trait CatalogSource: Send + Sync {
    async fn catalog(&self, release_id: &str) -> Option<SpokePackageCatalog>;
}

/// Signed spoke packages published for one Boss release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SpokePackageCatalog {
    pub release_id: String,
    pub packages: BTreeMap<String, SpokePackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SpokePackage {
    pub manifest_url: String,
    pub manifest_sha256: String,
}

/// Catalog loaded from a JSON file. `BOSS_SPOKE_PACKAGES` points here in tests.
pub struct FileCatalog {
    path: PathBuf,
}

impl FileCatalog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var_os("BOSS_SPOKE_PACKAGES").map(Self::new)
    }
}

#[async_trait]
impl CatalogSource for FileCatalog {
    async fn catalog(&self, release_id: &str) -> Option<SpokePackageCatalog> {
        load_catalog_file(&self.path).filter(|catalog| catalog.release_id == release_id)
    }
}

/// In-memory catalog for isolated tests.
pub struct StaticCatalog(pub Option<SpokePackageCatalog>);

#[async_trait]
impl CatalogSource for StaticCatalog {
    async fn catalog(&self, release_id: &str) -> Option<SpokePackageCatalog> {
        self.0.clone().filter(|catalog| catalog.release_id == release_id)
    }
}

/// GitHub release asset the hub fetches when `BOSS_SPOKE_PACKAGES` is unset.
pub fn github_catalog_url(release_id: &str) -> String {
    format!("https://github.com/spinyfin/mono/releases/download/boss-spoke-v{release_id}/spoke-packages.json")
}

/// File override when set; callers that need the network catalog supply it.
pub fn env_catalog() -> Option<Arc<dyn CatalogSource>> {
    FileCatalog::from_env().map(|file| Arc::new(file) as Arc<dyn CatalogSource>)
}

pub fn parse_catalog(json: &str) -> Option<SpokePackageCatalog> {
    serde_json::from_str(json).ok().filter(|catalog: &SpokePackageCatalog| {
        !catalog.release_id.is_empty()
            && catalog.packages.values().all(|package| {
                package.manifest_url.starts_with("https://") && decode_sha256(&package.manifest_sha256).is_some()
            })
    })
}

fn load_catalog_file(path: &Path) -> Option<SpokePackageCatalog> {
    let bytes = std::fs::read(path).ok()?;
    parse_catalog(&String::from_utf8_lossy(&bytes))
}

/// Dev and unstamped hubs have no published spoke packages.
pub fn release_has_published_packages(release_id: &str) -> bool {
    !release_id.is_empty() && release_id != "unknown" && !release_id.contains("-dev-")
}

pub fn package_platform(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("macos-arm64"),
        ("linux", "x86_64") => Some("linux-amd64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

pub fn decode_sha256(hex: &str) -> Option<Vec<u8>> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(32);
    for chunk in hex.as_bytes().chunks(2) {
        let text = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(text, 16).ok()?);
    }
    (bytes.len() == 32).then_some(bytes)
}

pub fn upgrade_command_id(host_id: &str, release_id: &str) -> String {
    format!("upgrade-{host_id}-{release_id}")
}

/// Run the sequential rollout until the caller drops the task.
pub async fn run(controller: UpgradeController) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(error) = controller.tick().await {
            tracing::warn!(error = %error, "spoke upgrade rollout tick failed");
        }
    }
}

pub fn spawn(db: WorkDb, streams: StreamManager, desired_release: impl Into<String>, catalog: Arc<dyn CatalogSource>) {
    let controller = UpgradeController::new(db, streams, desired_release, catalog);
    tokio::spawn(run(controller));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_rejects_non_https_and_short_digests() {
        assert!(parse_catalog(r#"{"release_id":"1.0.1","packages":{}}"#).is_some());
        assert!(
            parse_catalog(r#"{"release_id":"1.0.1","packages":{"linux-amd64":{"manifest_url":"http://x","manifest_sha256":"aa"}}}"#)
                .is_none()
        );
        let digest = "ab".repeat(32);
        let json = format!(
            r#"{{"release_id":"1.0.9","packages":{{"macos-arm64":{{"manifest_url":"https://example.invalid/m","manifest_sha256":"{digest}"}}}}}}"#
        );
        let catalog = parse_catalog(&json).unwrap();
        assert_eq!(catalog.release_id, "1.0.9");
        assert_eq!(
            catalog.packages["macos-arm64"].manifest_url,
            "https://example.invalid/m"
        );
    }

    #[test]
    fn unstamped_and_dev_releases_have_no_packages() {
        assert!(!release_has_published_packages(""));
        assert!(!release_has_published_packages("unknown"));
        assert!(!release_has_published_packages("1.0.9-dev-abc"));
        assert!(release_has_published_packages("1.0.9"));
    }
}
