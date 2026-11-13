//! Hub-driven spoke upgrade rollout.
//!
//! Desired release is the hub's own stamped Boss version. Matching signed
//! spoke packages are listed in `spoke-packages.json` next to that version's
//! GitHub release assets (or a file pointed at by `BOSS_SPOKE_PACKAGES`).
//! Connected spokes whose running release differs are offered `Upgrade` one
//! host at a time; a failure or refusal of that release stops further offers.
mod rollout;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use boss_spoke_package::manifest::Platform;
use boss_spoke_stream::StreamManager;
use boss_work_db::WorkDb;
use serde::Deserialize;
use tokio::sync::Mutex;

pub use rollout::UpgradeController;

/// How the hub learns the manifest URL and digest for its own release.
#[async_trait]
pub trait CatalogSource: Send + Sync {
    async fn lookup(&self, release_id: &str) -> CatalogLookup;
}

/// Result of fetching the catalog for one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogLookup {
    Found(SpokePackageCatalog),
    Unpublished,
    Unavailable,
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
    async fn lookup(&self, release_id: &str) -> CatalogLookup {
        match load_catalog_file(&self.path).filter(|catalog| catalog.release_id == release_id) {
            Some(catalog) => CatalogLookup::Found(catalog),
            None => CatalogLookup::Unpublished,
        }
    }
}

/// In-memory catalog for isolated tests.
pub struct StaticCatalog(pub Option<SpokePackageCatalog>);

#[async_trait]
impl CatalogSource for StaticCatalog {
    async fn lookup(&self, release_id: &str) -> CatalogLookup {
        match self.0.clone().filter(|catalog| catalog.release_id == release_id) {
            Some(catalog) => CatalogLookup::Found(catalog),
            None => CatalogLookup::Unpublished,
        }
    }
}

/// GitHub release asset the hub fetches when `BOSS_SPOKE_PACKAGES` is unset.
pub struct GithubReleaseCatalog;

#[async_trait]
impl CatalogSource for GithubReleaseCatalog {
    async fn lookup(&self, release_id: &str) -> CatalogLookup {
        let url = github_catalog_url(release_id);
        let response = match boss_http_retry::http_client()
            .get(&url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return CatalogLookup::Unavailable,
        };
        let status = response.status();
        if status.as_u16() == 404 {
            return CatalogLookup::Unpublished;
        }
        if !status.is_success() {
            return CatalogLookup::Unavailable;
        }
        match response.text().await {
            Ok(body) => parse_catalog(&body)
                .filter(|catalog| catalog.release_id == release_id)
                .map(CatalogLookup::Found)
                .unwrap_or(CatalogLookup::Unpublished),
            Err(_) => CatalogLookup::Unavailable,
        }
    }
}

struct CacheEntry {
    lookup: CatalogLookup,
    next_retry: Option<Instant>,
    failures: u32,
}

/// Caches a parsed catalog per release. Transport errors keep the last known value.
pub struct CachedCatalog {
    inner: Arc<dyn CatalogSource>,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl CachedCatalog {
    pub fn new(inner: Arc<dyn CatalogSource>) -> Self {
        Self {
            inner,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl CatalogSource for CachedCatalog {
    async fn lookup(&self, release_id: &str) -> CatalogLookup {
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(release_id) {
                match &entry.lookup {
                    CatalogLookup::Found(catalog) => return CatalogLookup::Found(catalog.clone()),
                    CatalogLookup::Unpublished => {
                        if entry.next_retry.is_some_and(|deadline| Instant::now() < deadline) {
                            return CatalogLookup::Unpublished;
                        }
                    }
                    CatalogLookup::Unavailable => {
                        if entry.next_retry.is_some_and(|deadline| Instant::now() < deadline) {
                            return CatalogLookup::Unavailable;
                        }
                    }
                }
            }
        }
        let fetched = self.inner.lookup(release_id).await;
        let mut cache = self.cache.lock().await;
        match &fetched {
            CatalogLookup::Found(_) => {
                cache.insert(
                    release_id.to_owned(),
                    CacheEntry {
                        lookup: fetched.clone(),
                        next_retry: None,
                        failures: 0,
                    },
                );
                fetched
            }
            CatalogLookup::Unpublished => {
                let failures = cache
                    .get(release_id)
                    .map(|entry| entry.failures.saturating_add(1))
                    .unwrap_or(1);
                let delay = boss_http_retry::jitter(boss_http_retry::backoff_delay(
                    &boss_http_retry::RetryPolicy::new(8, Duration::from_secs(5), Duration::from_secs(300)),
                    failures,
                ));
                cache.insert(
                    release_id.to_owned(),
                    CacheEntry {
                        lookup: CatalogLookup::Unpublished,
                        next_retry: Some(Instant::now() + delay),
                        failures,
                    },
                );
                CatalogLookup::Unpublished
            }
            CatalogLookup::Unavailable => {
                if let Some(CatalogLookup::Found(catalog)) = cache.get(release_id).map(|entry| entry.lookup.clone()) {
                    return CatalogLookup::Found(catalog);
                }
                let failures = cache
                    .get(release_id)
                    .map(|entry| entry.failures.saturating_add(1))
                    .unwrap_or(1);
                let delay = boss_http_retry::jitter(boss_http_retry::backoff_delay(
                    &boss_http_retry::RetryPolicy::new(8, Duration::from_secs(5), Duration::from_secs(300)),
                    failures,
                ));
                let kept = cache
                    .get(release_id)
                    .map(|entry| entry.lookup.clone())
                    .unwrap_or(CatalogLookup::Unavailable);
                cache.insert(
                    release_id.to_owned(),
                    CacheEntry {
                        lookup: kept.clone(),
                        next_retry: Some(Instant::now() + delay),
                        failures,
                    },
                );
                kept
            }
        }
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

pub fn production_catalog() -> Arc<dyn CatalogSource> {
    env_catalog().unwrap_or_else(|| Arc::new(CachedCatalog::new(Arc::new(GithubReleaseCatalog))))
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
    Platform::from_os_arch(os, arch).map(Platform::as_str)
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
    boss_work_db::upgrade_command_id(host_id, release_id)
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

    #[test]
    fn hub_catalog_url_uses_the_boss_version_as_the_spoke_tag() {
        assert_eq!(
            github_catalog_url("1.0.42"),
            "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.42/spoke-packages.json"
        );
        assert_eq!(upgrade_command_id("edge", "1.0.42"), "upgrade-edge-1.0.42");
        assert_eq!(package_platform("linux", "x86_64"), Some("linux-amd64"));
        assert_eq!(package_platform("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(
            package_platform("linux", "aarch64"),
            Platform::from_os_arch("linux", "aarch64").map(Platform::as_str)
        );
    }

    struct CountingCatalog {
        inner: std::sync::Mutex<CatalogLookup>,
        hits: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl CatalogSource for CountingCatalog {
        async fn lookup(&self, _: &str) -> CatalogLookup {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.lock().expect("catalog").clone()
        }
    }

    #[tokio::test]
    async fn cached_catalog_keeps_a_hit_and_does_not_flip_on_transport_errors() {
        let digest = "ab".repeat(32);
        let catalog = SpokePackageCatalog {
            release_id: "1.0.9".into(),
            packages: BTreeMap::from([(
                "linux-amd64".into(),
                SpokePackage {
                    manifest_url: "https://example.invalid/m".into(),
                    manifest_sha256: digest,
                },
            )]),
        };
        let inner = Arc::new(CountingCatalog {
            inner: std::sync::Mutex::new(CatalogLookup::Found(catalog.clone())),
            hits: std::sync::atomic::AtomicU32::new(0),
        });
        let cached = CachedCatalog::new(inner.clone());
        assert!(matches!(cached.lookup("1.0.9").await, CatalogLookup::Found(_)));
        assert!(matches!(cached.lookup("1.0.9").await, CatalogLookup::Found(_)));
        assert_eq!(inner.hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let failing = Arc::new(CountingCatalog {
            inner: std::sync::Mutex::new(CatalogLookup::Found(catalog)),
            hits: std::sync::atomic::AtomicU32::new(0),
        });
        let cached = CachedCatalog::new(failing.clone());
        assert!(matches!(cached.lookup("1.0.9").await, CatalogLookup::Found(_)));
        *failing.inner.lock().expect("catalog") = CatalogLookup::Unavailable;
        assert!(
            matches!(cached.lookup("1.0.9").await, CatalogLookup::Found(_)),
            "a later transport error must keep the cached catalog"
        );
        assert_eq!(failing.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
