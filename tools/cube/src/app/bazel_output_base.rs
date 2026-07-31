//! Bazel output-base discovery and removal, for both workspace teardown and
//! standalone orphan reclamation.
//!
//! ## The leak this closes
//!
//! Bazel keys a workspace's output base directory name on the MD5 hex digest
//! of the *absolute workspace path* (see [`hash_workspace_path`], verified
//! against a real `bazel info output_base` in `md5`'s tests), and stores it
//! under `<output_user_root>/_bazel_<user>/<hash>` — outside the workspace
//! directory entirely. Every cube workspace therefore has its own private
//! output tree (not a tree shared across workspaces, despite what an older
//! comment in `reclaim.rs` assumed for `bazel-out` symlinks *inside* a
//! workspace). Destroying the workspace directory leaves that tree
//! unreferenced forever unless something also removes it — that's this
//! module's job, called from both single-workspace teardown
//! ([`workspace.rs`](super::workspace)'s `Remove --expunge` handler and
//! `reclaim.rs`'s pool trim) and from the standalone
//! `cube workspace reclaim-bazel-bases` sweep for bases orphaned before this
//! fix existed (or by a crash / kill / interrupted teardown afterward).
//!
//! ## Why the root is discovered, not hardcoded
//!
//! `output_user_root` (the parent of `_bazel_<user>/`) is configurable per
//! bazelrc and differs across hosts/platforms/users — on this development
//! host it is `~/Library/Caches/bazel`, not bazel's own platform default of
//! `/var/tmp`. [`discover_bazel_user_root`] asks a real, live bazel
//! invocation (`bazel info output_base`), run in a repo's own canonical
//! source checkout — stable, always present, and never a workspace cube
//! itself tears down — then strips the hash component to get the shared
//! root. This deliberately avoids running bazel inside the workspace being
//! destroyed (which may already be mid-teardown) and avoids running it once
//! per workspace (`output_user_root` is a single host+user-wide value, so
//! one successful probe is reused for an entire teardown or sweep via
//! [`BazelUserRootCache`]).
//!
//! ## Why directories are chmod'd before removal
//!
//! Bazel marks its output-tree directories read-only to protect them from
//! accidental modification. A plain `fs::remove_dir_all` fails partway
//! through with `Permission denied` on a populated base. [`remove_output_base`]
//! walks the tree first, restoring the owner-write bit on every directory
//! (only directories need it — removing a file only requires write
//! permission on its *parent* directory, not on the file itself), then lets
//! `remove_dir_all` do the actual unlinking.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::metadata::WorkspaceRecord;
use crate::store::{Store, WorkspaceListFilter};

use crate::app::errors::{CubeError, Result};
use crate::app::jj::budgeted_timeout;

/// Wall-clock bound for the `bazel info output_base` probe used to discover
/// `output_user_root`. Generous relative to a warm bazel server (returns in
/// well under a second) because a cold server (none running yet for the
/// probed workspace) can take real time to start.
const DISCOVER_ROOT_TIMEOUT: Duration = Duration::from_secs(60);

/// How many candidate checkouts [`discover_bazel_user_root`] will try as a
/// probe before giving up. One is normally enough; more than one guards
/// against a candidate whose bazel setup is itself broken.
const MAX_PROBE_CANDIDATES: usize = 5;

/// MD5 hex digests are exactly 32 lowercase hex characters. Bazel's own
/// per-user root also holds non-hash entries (`install/`, `cache/` on this
/// host) that must never be treated as a candidate output base.
fn is_output_base_dir_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The directory name bazel would use for a workspace at `workspace_path`.
pub(super) fn hash_workspace_path(workspace_path: &Path) -> String {
    format!("{:x}", Md5::digest(workspace_path.to_string_lossy().as_bytes()))
}

/// The full output base path a workspace at `workspace_path` would have,
/// given the shared `_bazel_<user>` root.
pub(super) fn output_base_path(bazel_user_root: &Path, workspace_path: &Path) -> PathBuf {
    bazel_user_root.join(hash_workspace_path(workspace_path))
}

/// Ask a real bazel invocation, run inside `probe_workspace`, for its output
/// base, then strip the hash component to get `_bazel_<user>/`.
///
/// Returns `Err` with a human-readable reason on any failure — a missing
/// `bazel` binary, a `bazel info` failure, or unparseable output. Callers
/// treat this as "try the next candidate", not as fatal.
fn probe_bazel_user_root(
    runner: &dyn CommandRunner,
    probe_workspace: &Path,
    deadline: Option<Instant>,
) -> std::result::Result<PathBuf, String> {
    let timeout = budgeted_timeout(deadline, DISCOVER_ROOT_TIMEOUT)
        .ok_or_else(|| "time budget exhausted before the bazel probe could start".to_string())?;
    let output = runner
        .run_with_timeout(
            &RealCommandRunner::invocation(probe_workspace, "bazel", &["info", "output_base"]),
            timeout,
        )
        .map_err(|e| format!("`bazel info output_base` in {} failed: {e}", probe_workspace.display()))?;
    let output_base = output
        .lines()
        .next_back()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .ok_or_else(|| {
            format!(
                "`bazel info output_base` in {} produced no output",
                probe_workspace.display()
            )
        })?;
    let output_base = Path::new(output_base);
    let root = output_base.parent().ok_or_else(|| {
        format!(
            "bazel output base `{}` has no parent directory; cannot derive the shared root",
            output_base.display()
        )
    })?;
    if !is_output_base_dir_name(&output_base.file_name().unwrap_or_default().to_string_lossy()) {
        return Err(format!(
            "bazel output base `{}` does not look like an MD5-hash directory; refusing to derive a root from it",
            output_base.display()
        ));
    }
    Ok(root.to_path_buf())
}

/// Try each of `candidates` in turn until one yields a root. Reports every
/// failed attempt to stderr (visibility for "why didn't cleanup run") and
/// returns `None` only once every candidate has failed.
pub(super) fn discover_bazel_user_root(
    runner: &dyn CommandRunner,
    candidates: &[PathBuf],
    deadline: Option<Instant>,
) -> Option<PathBuf> {
    if candidates.is_empty() {
        eprintln!("cube: bazel output base: no candidate checkout available to probe bazel from; skipping cleanup");
        return None;
    }
    for candidate in candidates.iter().take(MAX_PROBE_CANDIDATES) {
        match probe_bazel_user_root(runner, candidate, deadline) {
            Ok(root) => return Some(root),
            Err(e) => eprintln!(
                "cube: bazel output base: probe candidate {} failed: {e}",
                candidate.display()
            ),
        }
    }
    eprintln!(
        "cube: bazel output base: could not discover the shared bazel root from any of {} candidate \
         workspace(s); skipping cleanup",
        candidates.len().min(MAX_PROBE_CANDIDATES),
    );
    None
}

/// Lazily discovers and memoizes the shared `_bazel_<user>` root for the
/// lifetime of one cube invocation, so a batch operation (pool trim, the
/// orphan sweep) pays for at most one `bazel info` subprocess no matter how
/// many workspaces it touches — and pays nothing at all if it ends up
/// touching none.
pub(super) struct BazelUserRootCache<'a> {
    runner: &'a dyn CommandRunner,
    candidates: Vec<PathBuf>,
    resolved: std::cell::RefCell<Option<Option<PathBuf>>>,
}

impl<'a> BazelUserRootCache<'a> {
    /// `candidates` are tried in order until one yields a root — normally
    /// just the repo's own canonical source checkout (stable, always
    /// present, never itself a workspace cube tears down), never the
    /// workspace currently being destroyed.
    pub(super) fn new(runner: &'a dyn CommandRunner, candidates: Vec<PathBuf>) -> Self {
        Self {
            runner,
            candidates,
            resolved: std::cell::RefCell::new(None),
        }
    }

    pub(super) fn get(&self) -> Option<PathBuf> {
        if let Some(cached) = self.resolved.borrow().as_ref() {
            return cached.clone();
        }
        let discovered = discover_bazel_user_root(self.runner, &self.candidates, None);
        *self.resolved.borrow_mut() = Some(discovered.clone());
        discovered
    }
}

/// Recursively restore the owner-write bit on every directory under (and
/// including) `root`, without following symlinks. Bazel marks output-tree
/// directories read-only; `fs::remove_dir_all` cannot unlink entries inside a
/// directory it cannot write to, and fails part-way through with `Permission
/// denied` on a populated output base (verified against a real one — see
/// `remove_output_base`'s doc comment).
fn make_dirs_writable(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(root, perms)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_dirs_writable(&entry.path())?;
        }
    }
    Ok(())
}

/// Remove one bazel output base, handling bazel's read-only directory
/// permissions. Returns `Ok(true)` if something was removed, `Ok(false)` if
/// nothing was there to begin with (not an error — the common case for a
/// workspace whose output base was never populated).
///
/// Verified against a real populated output base (not just an empty
/// directory): a fresh `bazel build //tools/cube:cube_lib` output tree
/// contains directories such as
/// `execroot/_main/bazel-out/darwin_arm64-opt/bin/external/rules_rust++crate+*`
/// mode `0500` (read+execute, no write) — a plain `remove_dir_all` on that
/// tree fails with `Permission denied`; this function's `make_dirs_writable`
/// pass clears it first, and the same tree then removes cleanly.
pub(super) fn remove_output_base(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(CubeError::BazelOutputBaseRemove {
                path: path.to_path_buf(),
                source: e,
            });
        }
    }
    make_dirs_writable(path).map_err(|source| CubeError::BazelOutputBaseRemove {
        path: path.to_path_buf(),
        source,
    })?;
    fs::remove_dir_all(path).map_err(|source| CubeError::BazelOutputBaseRemove {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(true)
}

/// Outcome of attempting to clean up one already-destroyed workspace's bazel
/// output base, for the caller to report and audit.
#[derive(Debug)]
pub(super) enum CleanupOutcome {
    /// The output base existed and was removed.
    Removed(PathBuf),
    /// No output base existed for this workspace (bazel was never run
    /// there) — not a failure.
    NothingToRemove,
    /// The shared bazel root could not be discovered (already reported to
    /// stderr by the cache); cleanup was skipped, not attempted.
    RootUnknown,
    /// The output base was found but could not be removed. Always surfaced
    /// by the caller — silently swallowing this is exactly how the leak this
    /// module exists to fix went unnoticed.
    Failed(CubeError),
}

/// Clean up the bazel output base for a workspace whose directory has
/// already been removed (or renamed out of the pool) — called from both
/// teardown paths right after the workspace directory itself is gone.
pub(super) fn cleanup_output_base_for_workspace(
    cache: &BazelUserRootCache<'_>,
    workspace_path: &Path,
) -> CleanupOutcome {
    let Some(root) = cache.get() else {
        return CleanupOutcome::RootUnknown;
    };
    let base = output_base_path(&root, workspace_path);
    match remove_output_base(&base) {
        Ok(true) => CleanupOutcome::Removed(base),
        Ok(false) => CleanupOutcome::NothingToRemove,
        Err(e) => CleanupOutcome::Failed(e),
    }
}

/// The set of output-base hashes that belong to a currently-registered
/// workspace, as of right now. Recomputed fresh by the caller before each
/// deletion decision in the orphan sweep — never trusted from an earlier
/// snapshot, because a workspace can be provisioned mid-sweep.
pub(super) fn live_output_base_hashes(store: &Store) -> std::collections::HashSet<String> {
    store
        .list_workspaces_filtered(&WorkspaceListFilter::default())
        .unwrap_or_default()
        .iter()
        .map(|r: &WorkspaceRecord| hash_workspace_path(&r.workspace_path))
        .collect()
}

/// One base the sweep found orphaned and either removed or failed to remove.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct OrphanOutcome {
    pub(super) path: PathBuf,
    pub(super) removed: bool,
    pub(super) error: Option<String>,
}

/// Result of a full `cube workspace reclaim-bazel-bases` pass.
#[derive(Debug, Default, serde::Serialize)]
pub(super) struct OrphanSweepReport {
    pub(super) bazel_user_root: Option<PathBuf>,
    pub(super) candidates_examined: usize,
    pub(super) removed: Vec<PathBuf>,
    pub(super) failed: Vec<OrphanOutcome>,
    pub(super) skipped_now_live: usize,
}

/// Sweep `bazel_user_root` for output-base directories with no corresponding
/// live workspace and remove them (or, with `dry_run`, just report them).
///
/// Never deletes a base belonging to a live workspace: membership is
/// re-checked against [`live_output_base_hashes`] immediately before each
/// individual removal, not just once at the start of the sweep, so a
/// workspace minted while the sweep is running is never touched.
pub(super) fn sweep_orphaned_output_bases(store: &Store, bazel_user_root: &Path, dry_run: bool) -> OrphanSweepReport {
    let mut report = OrphanSweepReport {
        bazel_user_root: Some(bazel_user_root.to_path_buf()),
        ..Default::default()
    };

    let entries = match fs::read_dir(bazel_user_root) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "cube: bazel output base sweep: failed to read {}: {e}",
                bazel_user_root.display()
            );
            return report;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_output_base_dir_name(name) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        report.candidates_examined += 1;
        let path = entry.path();

        // Recomputed on every iteration, not hoisted above the loop: a
        // workspace can be provisioned for the duration of a sweep over
        // hundreds of orphans, each removal taking 20-30s.
        let live = live_output_base_hashes(store);
        if live.contains(name) {
            report.skipped_now_live += 1;
            continue;
        }

        if dry_run {
            report.removed.push(path);
            continue;
        }

        match remove_output_base(&path) {
            Ok(true) => report.removed.push(path),
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "cube: bazel output base sweep: failed to remove {}: {e}",
                    path.display()
                );
                report.failed.push(OrphanOutcome {
                    path,
                    removed: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn hash_workspace_path_matches_observed_bazel_digest() {
        assert_eq!(
            hash_workspace_path(Path::new(
                "/Users/brianduff/.local/share/cube/workspaces/mono-agent-119"
            )),
            "1eb7f20eb66e1385f89492d4801636bf"
        );
    }

    #[test]
    fn output_base_dir_name_accepts_only_32_lowercase_hex() {
        assert!(is_output_base_dir_name("1eb7f20eb66e1385f89492d4801636bf"));
        assert!(!is_output_base_dir_name("install"));
        assert!(!is_output_base_dir_name("cache"));
        assert!(!is_output_base_dir_name("1EB7F20EB66E1385F89492D4801636B")); // uppercase
        assert!(!is_output_base_dir_name("1eb7f20eb66e1385f89492d4801636b")); // 31 chars
    }

    /// Reproduces the exact failure mode described in the task: a populated
    /// tree with directories bazel has marked read-only (mode 0500), the way
    /// `execroot/.../external/rules_rust++crate+*/...` looks in a real
    /// output base. A plain `remove_dir_all` must fail here; ours must not.
    #[test]
    fn remove_output_base_handles_read_only_directories() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let base = tempdir.path().join("1eb7f20eb66e1385f89492d4801636bf");
        let nested = base.join("execroot/_main/bazel-out/darwin_arm64-opt/bin/external/rules_rust/crate");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        std::fs::write(nested.join("_bs.cargo_runfiles"), b"stub").expect("file");

        // Mark every directory in the tree read-only, mirroring bazel's own
        // output-tree protection.
        let mut dir = nested.as_path();
        loop {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).expect("chmod ro");
            if dir == base {
                break;
            }
            dir = dir.parent().expect("parent");
        }

        // Sanity: confirm the setup actually reproduces bazel's failure mode.
        assert!(
            std::fs::remove_dir_all(&base).is_err(),
            "test setup did not reproduce a permission-denied removal"
        );
        // The failed remove_dir_all may have partially unlinked files inside
        // now-writable-looking dirs on some platforms; recreate to be sure.
        if !nested.exists() {
            std::fs::create_dir_all(&nested).expect("recreate nested dirs");
            std::fs::write(nested.join("_bs.cargo_runfiles"), b"stub").expect("recreate file");
            let mut dir = nested.as_path();
            loop {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).expect("chmod ro");
                if dir == base {
                    break;
                }
                dir = dir.parent().expect("parent");
            }
        }

        assert!(remove_output_base(&base).expect("remove_output_base should succeed"));
        assert!(!base.exists());
    }

    #[test]
    fn remove_output_base_reports_nothing_to_remove_when_absent() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let missing = tempdir.path().join("does-not-exist");
        assert!(!remove_output_base(&missing).expect("should not error"));
    }
}
