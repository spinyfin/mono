//! On-disk LRU cache of design-doc bodies, keyed on `(owner, repo, path, ref)`.
//!
//! This is a revalidating cache, not a mirror: GitHub remains the source of
//! truth. Entries are served immediately on open and must be proven current
//! (HTTP conditional request, or skipped entirely for an immutable commit
//! SHA) before being trusted indefinitely. Nothing here is written back to
//! GitHub.
//!
//! Eviction: at most [`MAX_ENTRIES`] documents and [`MAX_BYTES`] of body
//! bytes, whichever binds first. LRU by last-access. Docs are small; this
//! bound exists so the cache cannot grow without limit across a long-lived
//! engine.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard cap on the number of cached documents.
pub const MAX_ENTRIES: usize = 200;

/// Hard cap on total cached body bytes (32 MiB).
pub const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Directory name under the engine state root.
pub const DIR_NAME: &str = "design-doc-cache";

/// One cache key: the same `(repo, path, ref)` triple the rest of Boss
/// uses to address a document.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    pub owner: String,
    pub repo: String,
    pub path: String,
    pub git_ref: String,
}

impl CacheKey {
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        path: impl Into<String>,
        git_ref: impl Into<String>,
    ) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            path: path.into(),
            git_ref: git_ref.into(),
        }
    }
}

/// A cache hit: the body plus the ETag to send on the next revalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBody {
    pub markdown: String,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    #[serde(flatten)]
    key: CacheKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    blob: String,
    size: u64,
    last_access: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    entries: Vec<IndexEntry>,
}

struct MemEntry {
    markdown: String,
    etag: Option<String>,
    size: u64,
    last_access: u64,
    blob_hex: String,
}

struct Inner {
    entries: HashMap<CacheKey, MemEntry>,
    /// Monotonic LRU clock. Wall time is too coarse (many puts in one
    /// second would all look equally old); this tick is what eviction
    /// actually orders on.
    tick: u64,
}

/// LRU body cache. `dir` empty means in-memory only (unit tests that
/// don't want a tempfile).
pub struct BodyCache {
    dir: PathBuf,
    inner: Mutex<Inner>,
    /// Serializes on-disk writes so two `put`s cannot clobber
    /// `index.json.tmp`. Distinct from `inner` so `get` is not blocked
    /// behind filesystem work.
    persist_lock: Mutex<()>,
}

impl BodyCache {
    /// Load (or create) a cache rooted at `dir`.
    pub fn open(dir: PathBuf) -> Self {
        let inner = load_from_disk(&dir);
        let cache = Self {
            dir,
            inner: Mutex::new(inner),
            persist_lock: Mutex::new(()),
        };
        // Drop blobs (and leftover `*.tmp`) that the index no longer
        // references. Run on open, not on the per-put hot path.
        let _ = cache.gc_orphan_blobs();
        cache
    }

    /// In-memory cache that never touches the filesystem.
    pub fn in_memory() -> Self {
        Self {
            dir: PathBuf::new(),
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                tick: 0,
            }),
            persist_lock: Mutex::new(()),
        }
    }

    fn persist_enabled(&self) -> bool {
        !self.dir.as_os_str().is_empty()
    }

    /// Look up `(owner, repo, path, ref)`. Bumps last-access on hit.
    pub fn get(&self, key: &CacheKey) -> Option<CachedBody> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.tick += 1;
        let last_access = inner.tick;
        let entry = inner.entries.get_mut(key)?;
        entry.last_access = last_access;
        Some(CachedBody {
            markdown: entry.markdown.clone(),
            etag: entry.etag.clone(),
        })
    }

    /// Store (or replace) a body. Evicts LRU entries until both caps
    /// hold. Empty markdown is still stored — a legitimately empty file
    /// is not the same as a missing cache entry.
    pub fn put(&self, key: CacheKey, markdown: String, etag: Option<String>) {
        let blob_hex = blob_hex(&markdown);
        let size = markdown.len() as u64;
        let mut needs_gc = false;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.tick += 1;
            let last_access = inner.tick;
            let old = inner.entries.insert(
                key,
                MemEntry {
                    markdown,
                    etag,
                    size,
                    last_access,
                    blob_hex: blob_hex.clone(),
                },
            );
            if old.is_some_and(|prev| prev.blob_hex != blob_hex) {
                needs_gc = true;
            }
            let before = inner.entries.len();
            evict_until_within_caps(&mut inner.entries);
            if inner.entries.len() < before {
                needs_gc = true;
            }
        }
        if self.persist_enabled() {
            let _ = self.persist();
            if needs_gc {
                let _ = self.gc_orphan_blobs();
            }
        }
    }

    /// Update only the ETag of an existing entry (304 Not Modified).
    /// Bumps last-access so a frequently-revalidated SHA/branch is not
    /// the first to evict. Does **not** persist: last-access drift
    /// across a restart is harmless, and a 304 does not change the
    /// stored ETag.
    pub fn touch_etag(&self, key: &CacheKey, etag: Option<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.entries.contains_key(key) {
            inner.tick += 1;
            let last_access = inner.tick;
            if let Some(entry) = inner.entries.get_mut(key) {
                if let Some(etag) = etag {
                    entry.etag = Some(etag);
                }
                entry.last_access = last_access;
            }
        }
    }

    /// Number of entries currently held. Test/metrics helper.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn persist(&self) -> io::Result<()> {
        let snapshot = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            snapshot_entries(&inner)
        };
        let _persist = self.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
        persist_snapshot(&self.dir, &snapshot)
    }

    fn gc_orphan_blobs(&self) -> io::Result<()> {
        if !self.persist_enabled() {
            return Ok(());
        }
        let live = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner
                .entries
                .values()
                .map(|e| e.blob_hex.clone())
                .collect::<std::collections::HashSet<_>>()
        };
        gc_orphan_blobs(&self.dir, &live)
    }
}

fn blob_hex(markdown: &str) -> String {
    let digest = Sha256::digest(markdown.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

fn index_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

fn load_from_disk(dir: &Path) -> Inner {
    let mut entries = HashMap::new();
    let empty = Inner {
        entries: HashMap::new(),
        tick: 0,
    };
    let Ok(bytes) = fs::read(index_path(dir)) else {
        return empty;
    };
    let Ok(file) = serde_json::from_slice::<IndexFile>(&bytes) else {
        tracing::warn!(path = %index_path(dir).display(), "design-doc cache: index unreadable, starting empty");
        return empty;
    };
    let mut tick = 0;
    for meta in file.entries {
        let blob_path = blobs_dir(dir).join(&meta.blob);
        let Ok(markdown) = fs::read_to_string(&blob_path) else {
            continue;
        };
        tick = tick.max(meta.last_access);
        entries.insert(
            meta.key,
            MemEntry {
                markdown,
                etag: meta.etag,
                size: meta.size,
                last_access: meta.last_access,
                blob_hex: meta.blob,
            },
        );
    }
    Inner { entries, tick }
}

fn snapshot_entries(inner: &Inner) -> Vec<(IndexEntry, String)> {
    inner
        .entries
        .iter()
        .map(|(key, entry)| {
            (
                IndexEntry {
                    key: key.clone(),
                    etag: entry.etag.clone(),
                    blob: entry.blob_hex.clone(),
                    size: entry.size,
                    last_access: entry.last_access,
                },
                entry.markdown.clone(),
            )
        })
        .collect()
}

fn persist_snapshot(dir: &Path, entries: &[(IndexEntry, String)]) -> io::Result<()> {
    fs::create_dir_all(blobs_dir(dir))?;
    let mut index = IndexFile { entries: Vec::new() };
    for (meta, markdown) in entries {
        let blob_path = blobs_dir(dir).join(&meta.blob);
        if !blob_path.exists() {
            write_blob_atomic(&blob_path, markdown.as_bytes())?;
        }
        index.entries.push(meta.clone());
    }
    let tmp = dir.join("index.json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&index).unwrap_or_else(|_| b"{}".to_vec()),
    )?;
    fs::rename(&tmp, index_path(dir))?;
    Ok(())
}

/// Write `bytes` to `final_path` via a sibling `*.tmp` then rename, so a
/// crash mid-write cannot leave a truncated file at the content-addressed
/// path (the `exists()` shortcut would then treat it as complete forever).
fn write_blob_atomic(final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_name = final_path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        let _ = file.sync_all();
    }
    fs::rename(&tmp_path, final_path)?;
    Ok(())
}

fn gc_orphan_blobs(dir: &Path, live: &std::collections::HashSet<String>) -> io::Result<()> {
    let Ok(rd) = fs::read_dir(blobs_dir(dir)) else {
        return Ok(());
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        // Leftover atomic-write temps are never live, regardless of hex.
        if name.ends_with(".tmp") || !live.contains(name) {
            let _ = fs::remove_file(ent.path());
        }
    }
    Ok(())
}

fn evict_until_within_caps(entries: &mut HashMap<CacheKey, MemEntry>) {
    loop {
        let total_bytes: u64 = entries.values().map(|e| e.size).sum();
        if entries.len() <= MAX_ENTRIES && total_bytes <= MAX_BYTES {
            return;
        }
        let victim = entries
            .iter()
            .min_by_key(|(_, e)| e.last_access)
            .map(|(k, _)| k.clone());
        let Some(victim) = victim else { return };
        entries.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> CacheKey {
        CacheKey::new("o", "r", format!("d{n}.md"), "main")
    }

    #[test]
    fn get_miss_is_none() {
        let cache = BodyCache::in_memory();
        assert!(cache.get(&key(1)).is_none());
    }

    #[test]
    fn put_then_get_returns_body_and_etag() {
        let cache = BodyCache::in_memory();
        cache.put(key(1), "# hi".into(), Some("W/\"abc\"".into()));
        let hit = cache.get(&key(1)).expect("hit");
        assert_eq!(hit.markdown, "# hi");
        assert_eq!(hit.etag.as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn distinct_refs_are_distinct_entries() {
        let cache = BodyCache::in_memory();
        let sha = CacheKey::new("o", "r", "d.md", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let branch = CacheKey::new("o", "r", "d.md", "main");
        cache.put(sha.clone(), "at sha".into(), None);
        cache.put(branch.clone(), "at main".into(), None);
        assert_eq!(cache.get(&sha).unwrap().markdown, "at sha");
        assert_eq!(cache.get(&branch).unwrap().markdown, "at main");
    }

    #[test]
    fn eviction_drops_the_least_recently_used() {
        let cache = BodyCache::in_memory();
        // Fill past the cap. last_access is "now" for each put, so the
        // first-inserted is the oldest once we bump later ones via get.
        for i in 0..=MAX_ENTRIES as u32 {
            cache.put(key(i), format!("doc {i}"), None);
        }
        assert_eq!(cache.len(), MAX_ENTRIES, "must not grow past the entry cap");
        // key(0) was the first put and never re-read, so it is the LRU.
        assert!(cache.get(&key(0)).is_none(), "oldest entry must be evicted");
        assert!(
            cache.get(&key(MAX_ENTRIES as u32)).is_some(),
            "newest entry must survive"
        );
    }

    #[test]
    fn disk_round_trip_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        {
            let cache = BodyCache::open(path.clone());
            cache.put(key(7), "# persisted".into(), Some("etag-7".into()));
        }
        let reopened = BodyCache::open(path);
        let hit = reopened.get(&key(7)).expect("survived reopen");
        assert_eq!(hit.markdown, "# persisted");
        assert_eq!(hit.etag.as_deref(), Some("etag-7"));
    }

    #[test]
    fn touch_etag_does_not_rewrite_the_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let cache = BodyCache::open(path.clone());
        cache.put(key(1), "# hi".into(), Some("etag-1".into()));
        let before = fs::read(index_path(&path)).expect("index written by put");
        cache.touch_etag(&key(1), Some("etag-1-new".into()));
        let after = fs::read(index_path(&path)).expect("index still there");
        assert_eq!(before, after, "304 must not rewrite index.json");
        // Memory still sees the touch.
        assert_eq!(cache.get(&key(1)).unwrap().etag.as_deref(), Some("etag-1-new"));
    }

    #[test]
    fn open_gcs_orphan_blobs_and_tmp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        {
            let cache = BodyCache::open(path.clone());
            cache.put(key(1), "# keep".into(), None);
        }
        let blobs = blobs_dir(&path);
        fs::write(blobs.join("deadbeef"), "orphan").expect("orphan blob");
        fs::write(blobs.join("abc.tmp"), "partial").expect("tmp leftover");
        let reopened = BodyCache::open(path);
        assert!(reopened.get(&key(1)).is_some());
        let names: Vec<String> = fs::read_dir(&blobs)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert!(
            !names.iter().any(|n| n == "deadbeef" || n.ends_with(".tmp")),
            "open must drop orphans and leftover temps, got {names:?}"
        );
    }

    #[test]
    fn blob_write_uses_temp_then_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let cache = BodyCache::open(path.clone());
        cache.put(key(1), "# atomic".into(), None);
        let hex = blob_hex("# atomic");
        let blob = blobs_dir(&path).join(&hex);
        assert!(blob.exists(), "final blob path must exist after put");
        assert!(
            !blobs_dir(&path).join(format!("{hex}.tmp")).exists(),
            "tmp sibling must not remain after a successful rename"
        );
        assert_eq!(fs::read_to_string(&blob).unwrap(), "# atomic");
    }
}
