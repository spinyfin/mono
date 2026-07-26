//! Live-descendant-process probe for a worker's Stop boundary.
//!
//! Background: observed live 2026-07-17 — a worker's Claude session had
//! spawned one or more BACKGROUND SUBAGENTS via the harness Agent tool
//! (`subagent_type: "fork"` and friends) — separate `claude` processes
//! that re-invoke the worker with a task-notification once they finish.
//! Between the worker's own Stop (its *turn* genuinely ended — it is
//! waiting on the subagent) and that notification, the engine sees pure
//! hook silence: exactly the signature [`crate::nudge_breaker`] and
//! [`crate::completion::WorkerCompletionHandler::nudge_or_park`] treat as
//! a stall, so a worker legitimately waiting on delegated work could be
//! nudged into unproductive replies and eventually parked/abandoned.
//!
//! Same misclassification family as the build-wait false positives
//! ([`crate::build_wait`]) — a worker that ended its turn
//! for a legitimate reason looks identical, over hooks alone, to one that
//! is truly stuck — but a distinct case: there the worker's own
//! foreground process is busy (mid-tool-call); here the worker's turn has
//! genuinely ended and its process tree simply still contains live
//! descendant processes doing delegated work.
//!
//! The fix is a cheap, sweep-time process-tree scan: before nudging or
//! parking a worker whose Stop looks idle, check whether its shell pid
//! still has any live descendant processes. A worker with live
//! descendants is WAITING, not stalled — the same time-bounded
//! suppression pattern build-wait uses
//! ([`crate::build_wait_tracker::BuildWaitTracker`]) bounds how long that
//! trust lasts, so a descendant that never exits (a genuinely wedged
//! subagent) still eventually surfaces to the normal nudge/park flow
//! rather than being trusted forever.

/// Bound on how many process-tree levels [`count_live_descendants`] walks
/// below the probed pid. Mirrors [`crate::worker_registry::ANCESTOR_WALK_DEPTH`]'s
/// choice of a small, generous-enough constant rather than an unbounded
/// walk — a worker's own descendant tree (shell → claude → subagent(s)) is
/// only ever a couple of levels deep in practice; this just guards against
/// a pathological tree turning a "cheap" sweep-time check into a long scan.
const DESCENDANT_WALK_DEPTH: usize = 8;

/// Hard cap on how many pids a single probe visits across the whole walk.
/// A real worker's descendant count is in the single digits; this bound
/// only ever protects against a runaway process tree (fork bomb, buggy
/// tool loop) making the probe itself expensive.
const MAX_VISITED_PIDS: usize = 512;

/// Default horizon a continuously-reported live-descendant sighting is
/// trusted for, measured from the first reported sighting, before
/// [`crate::completion::WorkerCompletionHandler::nudge_or_park`] stops
/// suppressing and falls back to the normal nudge/park flow. Reuses
/// [`crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS`]'s value
/// (45 minutes) — the same reasoning applies: comfortably longer than
/// [`crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS`] while still
/// bounding an indefinite suppression should a subagent process genuinely
/// wedge instead of exiting.
pub const DEFAULT_BACKGROUND_CHILDREN_HORIZON_SECS: i64 = crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS;

/// Count every live descendant process of `pid` — children, grandchildren,
/// … down to [`DESCENDANT_WALK_DEPTH`] levels — NOT including `pid`
/// itself. Best-effort: a pid that no longer exists, or any per-pid probe
/// failure, simply contributes zero children rather than aborting the
/// whole walk, since a transient race on one branch of the tree must never
/// hide live descendants found on another branch.
///
/// Returns `0` on non-macOS targets — this repo's worker panes are
/// macOS-only (libghostty), so the process-tree scan has nothing to do
/// there; callers treat `0` the same as "no evidence of pending work",
/// which is the safe direction (falls through to the normal nudge/park
/// flow rather than suppressing it).
pub fn count_live_descendants(pid: libc::pid_t) -> usize {
    imp::count_live_descendants(pid)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{DESCENDANT_WALK_DEPTH, MAX_VISITED_PIDS};
    use std::os::raw::c_void;

    unsafe extern "C" {
        fn proc_listchildpids(ppid: libc::pid_t, buffer: *mut c_void, buffersize: libc::c_int) -> libc::c_int;
    }

    /// Direct (one-level) live children of `pid`, via `libproc`'s
    /// `proc_listchildpids`. Returns an empty vec for a pid with no
    /// children, an already-dead pid, or any probe error — this is a
    /// best-effort liveness signal, not a source of truth that must be
    /// propagated as an error (see [`super::count_live_descendants`]'s
    /// doc comment on why per-pid failures must not abort the walk).
    fn list_child_pids(pid: libc::pid_t) -> Vec<libc::pid_t> {
        const MAX_CHILDREN: usize = 256;
        let mut buf: Vec<libc::pid_t> = vec![0; MAX_CHILDREN];
        let buffersize = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `buf` is a valid, appropriately-sized buffer for
        // `buffersize` bytes; `proc_listchildpids` only ever writes up to
        // that many bytes and returns the count of pids it wrote.
        let n = unsafe { proc_listchildpids(pid, buf.as_mut_ptr() as *mut c_void, buffersize) };
        if n <= 0 {
            return Vec::new();
        }
        buf.truncate((n as usize).min(MAX_CHILDREN));
        buf
    }

    pub(super) fn count_live_descendants(pid: libc::pid_t) -> usize {
        let mut frontier = vec![pid];
        let mut total = 0usize;
        for _ in 0..DESCENDANT_WALK_DEPTH {
            if frontier.is_empty() || total >= MAX_VISITED_PIDS {
                break;
            }
            let mut next = Vec::new();
            for p in frontier {
                let children = list_child_pids(p);
                total += children.len();
                next.extend(children);
                if total >= MAX_VISITED_PIDS {
                    break;
                }
            }
            frontier = next;
        }
        total
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub(super) fn count_live_descendants(_pid: libc::pid_t) -> usize {
        0
    }
}

/// Reports whether an execution's worker process tree still has live
/// descendant processes at Stop boundary — the signal
/// [`crate::completion::WorkerCompletionHandler::nudge_or_park`] uses to
/// distinguish "waiting on delegated work" from "genuinely idle". See the
/// module doc comment for the incident this exists to fix.
pub trait BackgroundActivityProbe: Send + Sync {
    /// Number of live descendant processes for the worker backing
    /// `execution_id`, or `0` if there are none, the worker's shell pid
    /// cannot be resolved, or the execution is not a live worker at all.
    fn live_descendant_count(&self, execution_id: &str) -> usize;
}

/// Default probe that always reports zero descendants. Used as the
/// [`crate::completion::WorkerCompletionHandler`] default so test sites
/// that don't wire in a real probe get the historical behaviour
/// (background-children suppression never fires).
pub struct NoopBackgroundActivityProbe;

impl BackgroundActivityProbe for NoopBackgroundActivityProbe {
    fn live_descendant_count(&self, _execution_id: &str) -> usize {
        0
    }
}

/// Production probe: resolves `execution_id` to its live worker's shell
/// pid via [`crate::live_worker_state::LiveWorkerStateRegistry`], then
/// scans that pid's process tree.
pub struct RegistryBackgroundActivityProbe {
    live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
}

impl RegistryBackgroundActivityProbe {
    pub fn new(live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>) -> Self {
        Self { live_worker_states }
    }
}

impl BackgroundActivityProbe for RegistryBackgroundActivityProbe {
    fn live_descendant_count(&self, execution_id: &str) -> usize {
        let Some(shell_pid) = self.live_worker_states.shell_pid_for_run(execution_id) else {
            return 0;
        };
        if shell_pid <= 0 {
            return 0;
        }
        count_live_descendants(shell_pid as libc::pid_t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn no_children_counts_zero() {
        // A freshly-spawned test process (this test itself) has no
        // children unless something else in the process forked one.
        let self_pid = std::process::id() as libc::pid_t;
        // Not asserting exactly zero (the test harness/runtime may hold
        // its own worker threads/processes) — just that a pid with no
        // deliberately-spawned children doesn't explode or hang.
        let _ = count_live_descendants(self_pid);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn direct_child_is_counted() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("failed to spawn sleep");
        // Give the OS a moment to register the child in the process table.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let self_pid = std::process::id() as libc::pid_t;
        let count = count_live_descendants(self_pid);
        child.kill().ok();
        child.wait().ok();
        assert!(count >= 1, "expected at least the spawned `sleep` child, got {count}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dead_pid_counts_zero() {
        // A pid vanishingly unlikely to be alive.
        assert_eq!(count_live_descendants(999_999), 0);
    }

    struct FixedProbe(usize);
    impl BackgroundActivityProbe for FixedProbe {
        fn live_descendant_count(&self, _execution_id: &str) -> usize {
            self.0
        }
    }

    #[test]
    fn noop_probe_always_reports_zero() {
        let probe = NoopBackgroundActivityProbe;
        assert_eq!(probe.live_descendant_count("exec_a"), 0);
    }

    #[test]
    fn fixed_probe_reports_configured_count() {
        let probe = FixedProbe(3);
        assert_eq!(probe.live_descendant_count("exec_a"), 3);
    }

    #[test]
    fn registry_probe_returns_zero_for_unknown_execution() {
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert_eq!(probe.live_descendant_count("exec_unknown"), 0);
    }
}
