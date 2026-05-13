use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::RepobinError;

// Version 2: removed build_witnesses (previously only tracked BUILD.bazel,
// missing source file changes) and binary_mtime_ns. Correctness is now
// delegated to bazel's own incrementality — callers always invoke `bazel build`
// before consulting this cache; the cache only skips the subsequent cquery.
const CACHE_VERSION: u32 = 2;
const CACHE_SUBDIR: &str = "dispatch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    repo_root: String,
    target: String,
    executable_path: String,
}

/// Returns the cached executable path if it is still valid.
///
/// Call this *after* a successful `bazel build` completes. The cache only
/// stores the executable path so that a repeated `bazel cquery` can be
/// skipped. Build correctness is handled by bazel itself.
pub fn lookup_in(cache_root: &Path, repo_root: &Path, target: &str) -> Option<PathBuf> {
    let entry_path = entry_path(cache_root, repo_root, target);
    let raw = fs::read(&entry_path).ok()?;
    let cache: CacheFile = serde_json::from_slice(&raw).ok()?;

    if cache.version != CACHE_VERSION {
        return None;
    }
    if cache.repo_root != repo_root.to_string_lossy() {
        return None;
    }
    if cache.target != target {
        return None;
    }

    let executable = PathBuf::from(&cache.executable_path);
    if !executable.exists() {
        return None;
    }

    Some(executable)
}

pub fn record_in(
    cache_root: &Path,
    repo_root: &Path,
    target: &str,
    executable_path: &Path,
) -> Result<(), RepobinError> {
    if !executable_path.exists() {
        return Ok(());
    }

    let cache = CacheFile {
        version: CACHE_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        target: target.to_string(),
        executable_path: executable_path.to_string_lossy().into_owned(),
    };

    let entry_path = entry_path(cache_root, repo_root, target);
    if let Some(parent) = entry_path.parent() {
        fs::create_dir_all(parent).map_err(|source| RepobinError::CreateCacheDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let serialized = serde_json::to_vec(&cache).map_err(io::Error::other).map_err(
        |source| RepobinError::WriteCacheMetadata {
            path: entry_path.clone(),
            source,
        },
    )?;

    write_atomic(&entry_path, &serialized)
}

fn entry_path(cache_root: &Path, repo_root: &Path, target: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    cache_root.join(CACHE_SUBDIR).join(format!("{hex}.json"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), RepobinError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".dispatch-cache-")
        .suffix(".json.tmp")
        .tempfile_in(parent)
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    use std::io::Write;
    tmp.as_file_mut()
        .write_all(bytes)
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.as_file_mut()
        .sync_data()
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.persist(path)
        .map_err(|err| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source: err.error,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{CACHE_VERSION, CacheFile, entry_path, lookup_in, record_in};

    fn touch(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn cold_lookup_returns_none() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let result = lookup_in(&cache_root, &repo_root, "//tools/boss/cli:boss");
        assert!(result.is_none());
    }

    #[test]
    fn warm_hit_returns_recorded_path() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");

        touch(&exe, "binary");
        record_in(&cache_root, &repo_root, target, &exe).expect("record");

        let hit = lookup_in(&cache_root, &repo_root, target).expect("hit");
        assert_eq!(hit, exe);
    }

    #[test]
    fn missing_binary_is_a_miss() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");

        touch(&exe, "binary");
        record_in(&cache_root, &repo_root, target, &exe).expect("record");

        fs::remove_file(&exe).unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn corrupt_cache_falls_through() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        fs::create_dir_all(&repo_root).unwrap();
        let entry = entry_path(&cache_root, &repo_root, target);
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&entry, b"not json").unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn version_mismatch_is_a_miss() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        fs::create_dir_all(&repo_root).unwrap();
        let entry = entry_path(&cache_root, &repo_root, target);
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let bad = CacheFile {
            version: CACHE_VERSION + 1,
            repo_root: repo_root.to_string_lossy().into_owned(),
            target: target.to_string(),
            executable_path: "/nope".into(),
        };
        fs::write(&entry, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn different_repo_roots_are_distinct_keys() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let target = "//tools/boss/cli:boss";

        let repo_a = temp.path().join("repo-a");
        let exe_a = repo_a.join("bazel-bin/tools/boss/cli/boss");
        touch(&exe_a, "a");
        record_in(&cache_root, &repo_a, target, &exe_a).expect("record a");

        let repo_b = temp.path().join("repo-b");
        let exe_b = repo_b.join("bazel-bin/tools/boss/cli/boss");
        touch(&exe_b, "b");
        record_in(&cache_root, &repo_b, target, &exe_b).expect("record b");

        assert_eq!(lookup_in(&cache_root, &repo_a, target).unwrap(), exe_a);
        assert_eq!(lookup_in(&cache_root, &repo_b, target).unwrap(), exe_b);
    }
}
