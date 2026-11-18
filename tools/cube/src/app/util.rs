//! Small shared helpers: lease-holder identity, lock paths, and the clock.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;
use crate::store::{Store, WorkspaceListFilter};

use crate::app::errors::{CubeError, Result};

pub(super) fn holder_identity() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("{user}@{host}:{}", std::process::id())
}

pub(super) fn resolve_release_lease(
    store: &mut Store,
    workspace: Option<String>,
    lease: Option<String>,
    repo: Option<String>,
) -> Result<String> {
    if let Some(lease) = lease {
        return Ok(lease);
    }
    let workspace_id = workspace.ok_or_else(|| {
        CubeError::InvalidArgument("release requires a workspace id positional or --lease".to_string())
    })?;
    let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo.as_deref(),
        workspace_id: Some(&workspace_id),
        ..Default::default()
    })?;
    match matches.as_slice() {
        [] => Err(CubeError::WorkspaceNotFound(workspace_id)),
        [single] => single.lease_id.clone().ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "workspace `{}/{}` is not currently leased",
                single.repo, single.workspace_id
            ))
        }),
        many => {
            let repos = many.iter().map(|r| r.repo.as_str()).collect::<Vec<_>>().join(", ");
            Err(CubeError::InvalidArgument(format!(
                "workspace id `{workspace_id}` matches multiple repos ({repos}); disambiguate with --repo"
            )))
        }
    }
}

pub(super) fn repo_lock_path(repo: &str, database_path: Option<&Path>) -> Result<PathBuf> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(paths::repo_lock_path_in(parent, repo)),
        None => paths::repo_lock_path(repo),
    }
}

pub(super) fn cleanup_lock_path(database_path: Option<&Path>) -> Result<PathBuf> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(paths::cleanup_lock_path_in(parent)),
        None => paths::cleanup_lock_path(),
    }
}

thread_local! {
    /// A test-pinned "now" for this thread. `None` in production, always.
    static PINNED_EPOCH_S: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Wall-clock seconds since the Unix epoch — the one clock every command
/// handler in `app` reads.
///
/// This is the seam tests use to make time deterministic: a test that
/// holds a [`PinnedClock`] (see [`pin_clock`]) gets the pinned instant from
/// every read on its thread, so a retention age or expiry it asserts on is
/// exact rather than "whatever the wall clock said between two reads".
/// Nothing in production ever pins the clock.
pub(super) fn current_epoch_s() -> Result<i64> {
    if let Some(pinned) = PINNED_EPOCH_S.with(Cell::get) {
        return Ok(pinned);
    }
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CubeError::Io(io::Error::other(e)))?
        .as_secs() as i64)
}

/// Pin [`current_epoch_s`] to `epoch_s` on the calling thread until the
/// returned guard is dropped. Each test runs on its own thread, so a pin
/// never leaks into a concurrently running test.
#[cfg(test)]
pub(super) fn pin_clock(epoch_s: i64) -> PinnedClock {
    let previous = PINNED_EPOCH_S.with(|pinned| pinned.replace(Some(epoch_s)));
    PinnedClock { previous }
}

/// Guard returned by [`pin_clock`]; restores the prior pin state on drop.
#[cfg(test)]
#[must_use = "the clock is only pinned while this guard is alive"]
pub(super) struct PinnedClock {
    previous: Option<i64>,
}

#[cfg(test)]
impl Drop for PinnedClock {
    fn drop(&mut self) {
        PINNED_EPOCH_S.with(|pinned| pinned.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::{current_epoch_s, pin_clock};

    #[test]
    fn pinned_clock_is_read_back_exactly_and_released_on_drop() {
        let before = current_epoch_s().expect("clock");
        {
            let _pin = pin_clock(1_700_000_000);
            assert_eq!(current_epoch_s().expect("pinned clock"), 1_700_000_000);
            assert_eq!(
                current_epoch_s().expect("pinned clock"),
                1_700_000_000,
                "every read sees the same instant"
            );
        }
        let after = current_epoch_s().expect("clock");
        assert!(
            after >= before,
            "the real clock is back once the pin is dropped: {before} -> {after}"
        );
        assert!(
            !(1_700_000_000..=1_700_000_060).contains(&after),
            "the pinned value must not leak past the guard"
        );
    }

    #[test]
    fn nested_pins_restore_the_outer_pin() {
        let _outer = pin_clock(100);
        {
            let _inner = pin_clock(200);
            assert_eq!(current_epoch_s().unwrap(), 200);
        }
        assert_eq!(current_epoch_s().unwrap(), 100);
    }
}
