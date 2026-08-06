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
//! Process groups were initially used as a name-free discriminator. Live
//! Codex and Claude trees disproved that premise: driver helpers and ordinary
//! tool subprocesses may create their own process groups just like delegated
//! work. A process table therefore cannot honestly distinguish those two
//! kinds of child. The production probe consequently fails open until a
//! driver/harness-owned delegated-work signal is available. The retained
//! interface remains useful to that future signal and to deterministic tests.
//! A worker with delegated descendants is WAITING, not stalled — the same
//! time-bounded suppression pattern build-wait uses
//! ([`crate::build_wait_tracker::BuildWaitTracker`]) bounds how long that
//! trust lasts, so a descendant that never exits (a genuinely wedged
//! subagent) still eventually surfaces to the normal nudge/park flow
//! rather than being trusted forever.

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

/// Count live descendants of `shell_pid` that do not belong to the shell's
/// foreground process group — children, grandchildren, … down to
/// [`DESCENDANT_WALK_DEPTH`] levels. The foreground group is the driver
/// runtime; other process groups are background jobs, including delegated
/// subagents.
///
/// Any failure to identify the foreground group or classify a descendant is
/// returned to the caller. Suppression is fail-open: an indeterminate probe
/// must be logged and the nudge allowed to proceed, never silently converted
/// into evidence that background work is present.
pub fn count_live_delegated_descendants(shell_pid: libc::pid_t) -> Result<usize, String> {
    imp::count_live_delegated_descendants(shell_pid)
}

#[cfg(any(target_os = "macos", test))]
trait ProcessTable {
    /// Direct (one-level) live children of `pid`. A `pid` that has already
    /// exited must report an empty list, not an error — that is the normal
    /// outcome of walking a live process tree, not an indeterminate probe.
    fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String>;
    /// `pid`'s process group, or `Ok(None)` if `pid` exited between being
    /// listed by its parent and being probed here — a transient race that
    /// must be skipped, not treated as evidence one way or the other.
    fn process_group(&self, pid: libc::pid_t) -> Result<Option<libc::pid_t>, String>;
    fn foreground_process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String>;
}

#[cfg(any(target_os = "macos", test))]
fn count_with_process_table(table: &dyn ProcessTable, shell_pid: libc::pid_t) -> Result<usize, String> {
    let foreground_pgid = table.foreground_process_group(shell_pid)?;
    if foreground_pgid <= 0 {
        return Err(format!(
            "shell pid {shell_pid} has no foreground process group (tpgid={foreground_pgid})"
        ));
    }

    let mut frontier = vec![shell_pid];
    let mut visited = 0usize;
    let mut delegated = 0usize;
    for _ in 0..DESCENDANT_WALK_DEPTH {
        if frontier.is_empty() || visited >= MAX_VISITED_PIDS {
            break;
        }
        let mut next = Vec::new();
        for parent in frontier {
            let children = table.child_pids(parent)?;
            for child in children {
                if visited >= MAX_VISITED_PIDS {
                    break;
                }
                visited += 1;
                // A child that vanished between being listed and probed has
                // no live descendants of its own to walk further, and must
                // not be counted either way — skip it rather than aborting
                // the whole probe or erroring the caller into a nudge.
                let Some(pgid) = table.process_group(child)? else {
                    continue;
                };
                if pgid != foreground_pgid {
                    delegated += 1;
                }
                next.push(child);
            }
        }
        frontier = next;
    }
    if visited >= MAX_VISITED_PIDS {
        return Err(format!(
            "process tree below shell pid {shell_pid} exceeded the {MAX_VISITED_PIDS}-pid probe cap"
        ));
    }
    if frontier.iter().try_fold(false, |has_more, pid| {
        table.child_pids(*pid).map(|children| has_more || !children.is_empty())
    })? {
        return Err(format!(
            "process tree below shell pid {shell_pid} exceeded the {DESCENDANT_WALK_DEPTH}-level probe depth"
        ));
    }
    Ok(delegated)
}

/// Bound on how many process-tree levels the probe walks below the shell.
#[cfg(any(target_os = "macos", test))]
const DESCENDANT_WALK_DEPTH: usize = 8;

/// Hard cap on how many pids a single probe visits across the whole walk.
#[cfg(any(target_os = "macos", test))]
const MAX_VISITED_PIDS: usize = 512;

#[cfg(target_os = "macos")]
mod imp {
    use super::{ProcessTable, count_with_process_table};
    use std::os::raw::c_void;

    unsafe extern "C" {
        fn proc_listchildpids(ppid: libc::pid_t, buffer: *mut c_void, buffersize: libc::c_int) -> libc::c_int;
    }

    /// `true` iff `err` is `ESRCH` — the pid this call targeted has already
    /// exited. That is the ordinary outcome of walking a live process tree,
    /// not an indeterminate probe.
    fn is_vanished(err: &std::io::Error) -> bool {
        err.raw_os_error() == Some(libc::ESRCH)
    }

    /// Direct (one-level) live children of `pid`, via `libproc`'s
    /// `proc_listchildpids`. A parent that has already exited (ESRCH) is
    /// reported as having no children, since it is gone; any other failure
    /// is propagated because an unclassifiable tree must never suppress a
    /// nudge.
    fn list_child_pids(pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
        const MAX_CHILDREN: usize = 256;
        let mut buf: Vec<libc::pid_t> = vec![0; MAX_CHILDREN];
        let buffersize = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // Clear errno first so a `n == 0` return can be told apart from a
        // real failure that also happens to report zero pids — some
        // libproc revisions signal failure via errno on a zero return
        // rather than a negative one.
        unsafe { *libc::__error() = 0 };
        // SAFETY: `buf` is a valid, appropriately-sized buffer for
        // `buffersize` bytes; `proc_listchildpids` only ever writes up to
        // that many bytes and returns the count of pids it wrote.
        let n = unsafe { proc_listchildpids(pid, buf.as_mut_ptr() as *mut c_void, buffersize) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if is_vanished(&err) {
                return Ok(Vec::new());
            }
            return Err(format!("proc_listchildpids failed for pid {pid}: {err}"));
        }
        if n == 0 {
            let err = std::io::Error::last_os_error();
            if let Some(errno) = err.raw_os_error()
                && errno != 0
                && !is_vanished(&err)
            {
                return Err(format!(
                    "proc_listchildpids returned 0 for pid {pid} with errno {errno} set: {err}"
                ));
            }
            return Ok(Vec::new());
        }
        if n as usize >= MAX_CHILDREN {
            return Err(format!(
                "pid {pid} has at least {MAX_CHILDREN} direct children; process-tree probe is incomplete"
            ));
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    struct MacProcessTable;

    impl ProcessTable for MacProcessTable {
        fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
            list_child_pids(pid)
        }

        fn process_group(&self, pid: libc::pid_t) -> Result<Option<libc::pid_t>, String> {
            match crate::libproc::proc_bsd_info(pid) {
                Ok(info) => Ok(Some(info.pgid)),
                Err(err) if is_vanished(&err) => Ok(None),
                Err(err) => Err(format!("proc_pidinfo failed for pid {pid}: {err}")),
            }
        }

        fn foreground_process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String> {
            crate::libproc::proc_bsd_info(pid)
                .map(|info| info.foreground_pgid)
                .map_err(|err| format!("proc_pidinfo failed for pid {pid}: {err}"))
        }
    }

    pub(super) fn count_live_delegated_descendants(shell_pid: libc::pid_t) -> Result<usize, String> {
        count_with_process_table(&MacProcessTable, shell_pid)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub(super) fn count_live_delegated_descendants(_pid: libc::pid_t) -> Result<usize, String> {
        Err("background process-group probing is only supported on macOS".to_owned())
    }
}

/// Reports whether an execution's worker process tree still has live
/// descendant processes at Stop boundary — the signal
/// [`crate::completion::WorkerCompletionHandler::nudge_or_park`] uses to
/// distinguish "waiting on delegated work" from "genuinely idle". See the
/// module doc comment for the incident this exists to fix.
pub trait BackgroundActivityProbe: Send + Sync {
    /// Number of live delegated descendants for the worker backing
    /// `execution_id`. An unresolved worker or indeterminate process-tree
    /// classification is an error so the caller can fail loudly and nudge.
    fn live_delegated_descendant_count(&self, execution_id: &str) -> Result<usize, String>;

    /// Opaque hook-activity watermark for avoiding a recurring probe in the
    /// middle of a worker's resumed turn. Implementations without hook state
    /// return `None`.
    fn activity_watermark(&self, _execution_id: &str) -> Option<String> {
        None
    }
}

/// Default probe that always reports zero descendants. Used as the
/// [`crate::completion::WorkerCompletionHandler`] default so test sites
/// that don't wire in a real probe get the historical behaviour
/// (background-children suppression never fires).
pub struct NoopBackgroundActivityProbe;

impl BackgroundActivityProbe for NoopBackgroundActivityProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> Result<usize, String> {
        Ok(0)
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
    fn live_delegated_descendant_count(&self, execution_id: &str) -> Result<usize, String> {
        let Some(shell_pid) = self.live_worker_states.shell_pid_for_run(execution_id) else {
            return Err(format!("no live shell pid registered for execution {execution_id}"));
        };
        if shell_pid <= 0 {
            return Err(format!("invalid shell pid {shell_pid} for execution {execution_id}"));
        }
        // Do not classify descendants by pgid: Codex's code-mode host and
        // Claude Bash tools both legitimately run in their own group, which
        // is indistinguishable from a delegated child.
        let _ = shell_pid;
        Err("process-table descendants cannot distinguish driver helpers from delegated work".to_owned())
    }

    fn activity_watermark(&self, execution_id: &str) -> Option<String> {
        self.live_worker_states.activity_watermark_for_run(execution_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;

    struct FakeProcessTable {
        foreground_pgid: libc::pid_t,
        children: HashMap<libc::pid_t, Vec<libc::pid_t>>,
        process_groups: HashMap<libc::pid_t, libc::pid_t>,
        /// Pids that exited between being listed by their parent and being
        /// probed for their process group — `process_group` reports these
        /// as `Ok(None)` rather than an error, exactly like a real ESRCH.
        vanished: HashSet<libc::pid_t>,
    }

    impl ProcessTable for FakeProcessTable {
        fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }

        fn process_group(&self, pid: libc::pid_t) -> Result<Option<libc::pid_t>, String> {
            if self.vanished.contains(&pid) {
                return Ok(None);
            }
            self.process_groups
                .get(&pid)
                .copied()
                .map(Some)
                .ok_or_else(|| format!("missing process group for pid {pid}"))
        }

        fn foreground_process_group(&self, _pid: libc::pid_t) -> Result<libc::pid_t, String> {
            Ok(self.foreground_pgid)
        }
    }

    #[test]
    fn probe_returns_zero_for_driver_runtime_and_helpers() {
        // shell(10) -> driver(20) -> runtime helper(21). Both descendants
        // belong to the terminal foreground process group, so the probe
        // excludes them without consulting executable names.
        let table = FakeProcessTable {
            foreground_pgid: 100,
            children: HashMap::from([(10, vec![20]), (20, vec![21])]),
            process_groups: HashMap::from([(20, 100), (21, 100)]),
            vanished: HashSet::new(),
        };
        assert_eq!(count_with_process_table(&table, 10), Ok(0));
    }

    #[test]
    fn separate_background_process_group_is_counted_as_delegated() {
        let table = FakeProcessTable {
            foreground_pgid: 100,
            children: HashMap::from([(10, vec![20, 30]), (20, vec![21]), (30, vec![31])]),
            process_groups: HashMap::from([(20, 100), (21, 100), (30, 300), (31, 300)]),
            vanished: HashSet::new(),
        };
        assert_eq!(count_with_process_table(&table, 10), Ok(2));
    }

    #[test]
    fn descendant_that_vanishes_mid_walk_is_skipped_not_errored() {
        // shell(10) -> [dying(20), alive(30)]. `dying` exits between being
        // listed by `child_pids(10)` and having its process group probed —
        // an ordinary race, not an indeterminate probe. It must be skipped,
        // and the live delegated sibling `alive` must still be counted.
        let table = FakeProcessTable {
            foreground_pgid: 100,
            children: HashMap::from([(10, vec![20, 30])]),
            process_groups: HashMap::from([(30, 300)]),
            vanished: HashSet::from([20]),
        };
        assert_eq!(count_with_process_table(&table, 10), Ok(1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_process_group_discriminator_classifies_delegated_child() {
        use std::os::unix::process::CommandExt;

        let self_pid = std::process::id() as libc::pid_t;
        // Establishes whether this environment has a resolvable foreground
        // process group at all (a sandboxed test runner may have no
        // controlling terminal, in which case the probe is legitimately
        // indeterminate and there is nothing real to assert against).
        let baseline = match count_live_delegated_descendants(self_pid) {
            Ok(count) => count,
            Err(reason) => {
                eprintln!("skipping: process-group probe indeterminate in this environment: {reason}");
                return;
            }
        };

        let mut plain_child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("failed to spawn plain child");
        let mut delegated_child = std::process::Command::new("sleep")
            .arg("5")
            .process_group(0)
            .spawn()
            .expect("failed to spawn backgrounded child");
        std::thread::sleep(std::time::Duration::from_millis(200));

        let count = count_live_delegated_descendants(self_pid);

        plain_child.kill().ok();
        plain_child.wait().ok();
        delegated_child.kill().ok();
        delegated_child.wait().ok();

        assert_eq!(
            count,
            Ok(baseline + 1),
            "expected exactly the backgrounded child to be classified as delegated"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dead_pid_reports_indeterminate_not_zero() {
        // A pid that never existed has no resolvable foreground process
        // group; the probe must fail loudly (fail-open — the caller
        // proceeds to nudge) rather than silently report zero descendants.
        assert!(count_live_delegated_descendants(999_999).is_err());
    }

    struct FixedProbe(usize);
    impl BackgroundActivityProbe for FixedProbe {
        fn live_delegated_descendant_count(&self, _execution_id: &str) -> Result<usize, String> {
            Ok(self.0)
        }
    }

    #[test]
    fn noop_probe_always_reports_zero() {
        let probe = NoopBackgroundActivityProbe;
        assert_eq!(probe.live_delegated_descendant_count("exec_a"), Ok(0));
    }

    #[test]
    fn fixed_probe_reports_configured_count() {
        let probe = FixedProbe(3);
        assert_eq!(probe.live_delegated_descendant_count("exec_a"), Ok(3));
    }

    #[test]
    fn registry_probe_fails_for_unknown_execution() {
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert!(probe.live_delegated_descendant_count("exec_unknown").is_err());
    }
}
