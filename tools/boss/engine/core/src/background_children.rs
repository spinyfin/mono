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
    fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String>;
    fn process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String>;
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
                if table.process_group(child)? != foreground_pgid {
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

    const PROC_PIDTBSDINFO: libc::c_int = 3;

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdHeader {
        flags: u32,
        status: u32,
        xstatus: u32,
        pid: u32,
        ppid: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdCredentials {
        uid: u32,
        gid: u32,
        ruid: u32,
        rgid: u32,
        svuid: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdNames {
        svgid: u32,
        reserved: u32,
        command: [u8; 16],
        name: [u8; 32],
        open_files: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdJobControl {
        process_group: u32,
        job_count: u32,
        terminal_device: u32,
        terminal_foreground_group: u32,
        nice: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdStartTime {
        seconds: u64,
        microseconds: u64,
    }

    /// Rust layout mirror of Darwin's `proc_bsdinfo`. Grouping adjacent C
    /// fields into `repr(C)` components preserves the ABI while keeping the
    /// process-table API focused on the job-control fields this probe reads.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct ProcBsdInfo {
        header: ProcBsdHeader,
        credentials: ProcBsdCredentials,
        names: ProcBsdNames,
        job_control: ProcBsdJobControl,
        start_time: ProcBsdStartTime,
    }

    unsafe extern "C" {
        fn proc_listchildpids(ppid: libc::pid_t, buffer: *mut c_void, buffersize: libc::c_int) -> libc::c_int;
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    /// Direct (one-level) live children of `pid`, via `libproc`'s
    /// `proc_listchildpids`. Errors are propagated because an unclassifiable
    /// tree must never suppress a nudge.
    fn list_child_pids(pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
        const MAX_CHILDREN: usize = 256;
        let mut buf: Vec<libc::pid_t> = vec![0; MAX_CHILDREN];
        let buffersize = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `buf` is a valid, appropriately-sized buffer for
        // `buffersize` bytes; `proc_listchildpids` only ever writes up to
        // that many bytes and returns the count of pids it wrote.
        let n = unsafe { proc_listchildpids(pid, buf.as_mut_ptr() as *mut c_void, buffersize) };
        if n < 0 {
            return Err(format!(
                "proc_listchildpids failed for pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if n == 0 {
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

    fn process_info(pid: libc::pid_t) -> Result<ProcBsdInfo, String> {
        let mut info = ProcBsdInfo::default();
        let info_size = std::mem::size_of::<ProcBsdInfo>() as libc::c_int;
        // SAFETY: `info` is a valid buffer of exactly `info_size` bytes.
        let n = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, info_size) };
        if n <= 0 {
            return Err(format!(
                "proc_pidinfo failed for pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if n != info_size {
            return Err(format!(
                "proc_pidinfo returned {n} bytes for pid {pid}; expected {info_size}"
            ));
        }
        Ok(info)
    }

    struct MacProcessTable;

    impl ProcessTable for MacProcessTable {
        fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
            list_child_pids(pid)
        }

        fn process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String> {
            Ok(process_info(pid)?.job_control.process_group as libc::pid_t)
        }

        fn foreground_process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String> {
            Ok(process_info(pid)?.job_control.terminal_foreground_group as libc::pid_t)
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

    #[cfg(target_os = "macos")]
    #[test]
    fn proc_bsd_info_layout_matches_darwin_abi() {
        assert_eq!(std::mem::size_of::<imp::ProcBsdInfo>(), 136);
    }

    struct FakeProcessTable {
        foreground_pgid: libc::pid_t,
        children: HashMap<libc::pid_t, Vec<libc::pid_t>>,
        process_groups: HashMap<libc::pid_t, libc::pid_t>,
    }

    impl ProcessTable for FakeProcessTable {
        fn child_pids(&self, pid: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }

        fn process_group(&self, pid: libc::pid_t) -> Result<libc::pid_t, String> {
            self.process_groups
                .get(&pid)
                .copied()
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
        };
        assert_eq!(count_with_process_table(&table, 10), Ok(0));
    }

    #[test]
    fn separate_background_process_group_is_counted_as_delegated() {
        let table = FakeProcessTable {
            foreground_pgid: 100,
            children: HashMap::from([(10, vec![20, 30]), (20, vec![21]), (30, vec![31])]),
            process_groups: HashMap::from([(20, 100), (21, 100), (30, 300), (31, 300)]),
        };
        assert_eq!(count_with_process_table(&table, 10), Ok(2));
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
