//! Worker pid → run-id registry.
//!
//! When the engine spawns a worker (claude in a leased cube workspace),
//! it knows the worker process's pid and the run id it was spawned for.
//! The events socket accepts hook deliveries from the `boss-event`
//! shim, which runs as a *descendant* of the worker process — so peer
//! pid is the shim's pid, not the worker's.
//!
//! This registry stores the (worker_pid, run_id) pairs the spawn flow
//! creates, and looks them up via an ancestor walk: given the shim's
//! peer pid, walk up the process tree until we hit a registered
//! ancestor. That's the run the hook event belongs to.
//!
//! # Identity, not just a pid
//!
//! A bare pid is not an identity: the kernel recycles pids, and this
//! machine wraps its counter within minutes under load. Every entry is
//! therefore keyed by the pid *and* the process start time captured at
//! registration. An ancestor-walk hit only counts when the live process at
//! that pid still has the registered start time; a mismatch means the
//! registered process is gone and the pid now belongs to something else, so
//! the entry is dropped and the walk keeps climbing. Entries are also removed
//! when their run is released ([`WorkerRegistry::take_worker_pane_for_run`] /
//! [`WorkerRegistry::forget_run_pids`]), so a finished run never resolves.
//!
//! Platform-specific process-tree walks: macOS uses `proc_pidinfo`; Linux
//! reads `/proc/<pid>/stat`. `getppid()` is per-process and would require
//! IPC for either implementation.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::sync::{Arc, Mutex};

/// The pane shell is registered, but the worker's tools run below the PTY
/// relay, its supervisor, the driver CLI (possibly a node wrapper plus a
/// native binary) and the tool's own shells, so the walk must outlast that
/// whole launcher chain with room for a few ordinary wrappers.
const ANCESTOR_WALK_DEPTH: usize = 16;

/// First slot id reserved for remote workers' virtual slots.
///
/// A remote worker holds no libghostty pane, so the spawn flow never
/// allocates a real pool slot for it. But the live-status surface is
/// slot-keyed, so the dispatcher assigns each live remote run a synthetic
/// slot from a high range that cannot collide with the local worker pool
/// (`1..=MAX_WORKER_POOL_SIZE`, currently 16 — Bridge Crew 1..=8, Lower Decks
/// 9..=16) or the automation pool (`MAX_WORKER_POOL_SIZE+1..`, currently
/// 17..=24). 200 leaves generous
/// headroom below for pool growth and 56 ids above for concurrent remote
/// runs — far more than any real deployment. See
/// [`WorkerRegistry::get_or_allocate_remote_slot`].
///
/// This re-exports `boss_protocol::REMOTE_SLOT_BASE` so the protocol
/// crate stays the single source of truth for the threshold.
pub const REMOTE_SLOT_BASE: u8 = boss_protocol::REMOTE_SLOT_BASE;

#[derive(Clone)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    /// Holds the machine-wide idle-sleep-prevention assertion for exactly
    /// as long as at least one *local* worker pane (slot id below
    /// [`REMOTE_SLOT_BASE`]) is registered. See
    /// [`crate::sleep_assertion::SleepAssertionController`] for why remote
    /// workers — which run on a different machine — must never count here.
    sleep_assertion: Arc<crate::sleep_assertion::SleepAssertionController>,
    /// Source of live process identity (parent pid, start time, command).
    /// Production reads the OS; tests inject a fake process tree.
    processes: Arc<dyn ProcessTable>,
}

/// One live process as the OS reports it right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSnapshot {
    /// Parent pid; `None` at the root of the tree (parent 0 / launchd).
    pub ppid: Option<libc::pid_t>,
    /// Opaque, platform-specific process start time. Equal values for the
    /// same pid mean the same process incarnation; a recycled pid differs.
    pub start: u64,
    /// Short process name, for diagnostics only.
    pub command: String,
}

/// Reads live process identity. A seam so the ancestor walk is testable
/// against a fabricated process tree, including pid reuse.
pub trait ProcessTable: Send + Sync {
    fn snapshot(&self, pid: libc::pid_t) -> io::Result<ProcessSnapshot>;
}

/// The real process table, backed by [`process_snapshot`].
struct SystemProcessTable;

impl ProcessTable for SystemProcessTable {
    fn snapshot(&self, pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
        process_snapshot(pid)
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::with_sleep_assertion(Arc::new(crate::sleep_assertion::SleepAssertionController::no_op()))
    }
}

#[derive(Default)]
struct RegistryInner {
    pid_to_run: HashMap<libc::pid_t, PidEntry>,
    run_to_slot: HashMap<String, RegisteredWorkerPane>,
}

/// A registered worker shell: the run it belongs to and the start time of
/// the process that was at this pid when it was registered.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PidEntry {
    run_id: String,
    /// `None` when the process could not be read at registration (it had
    /// already exited). Such an entry can never be verified, so it never
    /// matches.
    start: Option<u64>,
    progression: bool,
}

/// How one pid on an ancestor walk related to the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkVerdict {
    /// No registry entry for this pid.
    Unregistered,
    /// A registry entry whose recorded start time equals the live process's:
    /// the walk resolved here.
    Matched,
    /// A registry entry exists but the live process at this pid started at a
    /// different time: the pid was recycled. The entry is dropped.
    PidReused { registered_start: u64, live_start: u64 },
    /// A registry entry exists but its identity cannot be verified (it was
    /// registered without a start time, or the live process cannot be read).
    Unverifiable,
}

/// One level of an ancestor walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AncestorLink {
    pub pid: libc::pid_t,
    /// `None` when the process could not be read.
    pub command: Option<String>,
    /// The run the registry holds for this pid, matched or not.
    pub registered_run: Option<String>,
    pub verdict: LinkVerdict,
}

/// Outcome of [`WorkerRegistry::resolve_ancestry`]: the full walked chain and,
/// when an identity-verified entry was found, which pid it was and its run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AncestorResolution {
    pub chain: Vec<AncestorLink>,
    pub matched: Option<(libc::pid_t, String)>,
    progression: bool,
}

impl AncestorResolution {
    /// The resolved run id, if any.
    pub fn run_id(&self) -> Option<&str> {
        self.matched.as_ref().map(|(_, run_id)| run_id.as_str())
    }

    /// Single-line rendering of the walked chain for logs, innermost first,
    /// e.g. `pid 7133 (codex) > pid 6228 (zsh) [matched run exec_1]`.
    pub fn describe_chain(&self) -> String {
        let mut out = String::new();
        for (index, link) in self.chain.iter().enumerate() {
            if index > 0 {
                out.push_str(" > ");
            }
            let command = link.command.as_deref().unwrap_or("?");
            let _ = write!(out, "pid {} ({command})", link.pid);
            match (&link.verdict, link.registered_run.as_deref()) {
                (LinkVerdict::Unregistered, _) => {}
                (LinkVerdict::Matched, run) => {
                    let _ = write!(out, " [matched run {}]", run.unwrap_or("?"));
                }
                (
                    LinkVerdict::PidReused {
                        registered_start,
                        live_start,
                    },
                    run,
                ) => {
                    let _ = write!(
                        out,
                        " [stale entry for run {}: pid reused, registered start {registered_start} != live start {live_start}; dropped]",
                        run.unwrap_or("?"),
                    );
                }
                (LinkVerdict::Unverifiable, run) => {
                    let _ = write!(out, " [unverifiable entry for run {}; dropped]", run.unwrap_or("?"));
                }
            }
        }
        if out.is_empty() {
            out.push_str("(empty)");
        }
        out
    }
}

/// A worker's presentation slot and optional local tmux identity.
/// Remote virtual slots have no local tmux session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredWorkerPane {
    pub slot_id: u8,
    /// Missing identity is valid for remote slots; local operations fail closed.
    pub tmux_session_name: Option<String>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry with the supplied power-assertion controller. The
    /// engine supplies the production `caffeinate` controller; the default
    /// registry uses a no-op controller for hermetic tests.
    pub(crate) fn with_sleep_assertion(sleep_assertion: Arc<crate::sleep_assertion::SleepAssertionController>) -> Self {
        Self {
            inner: Arc::default(),
            sleep_assertion,
            processes: Arc::new(SystemProcessTable),
        }
    }

    /// Replace the process table. Tests use this to fabricate a process tree
    /// and simulate pid reuse.
    #[cfg(test)]
    pub(crate) fn with_process_table(mut self, processes: Arc<dyn ProcessTable>) -> Self {
        self.processes = processes;
        self
    }

    /// Record that pid is the worker process for run `run_id`. Called by
    /// the spawn flow once it has both the run id and the spawned pid.
    ///
    /// The process's start time is captured now, so a later hit on this pid
    /// only counts while the same process incarnation is still there. A run
    /// owns one shell pid: registering a different pid for a run that already
    /// has one (restart re-adoption, a respawn) replaces the earlier entry.
    pub fn register(&self, pid: libc::pid_t, run_id: impl Into<String>) {
        self.register_identity(pid, run_id.into(), false);
    }

    /// Capture durable authority before exposing the progression identity.
    pub fn register_progression(&self, pid: libc::pid_t, run_id: &str, db: &crate::work::WorkDb) -> anyhow::Result<()> {
        db.capture_progression_neighbourhood(run_id)?;
        self.register_identity(pid, run_id.to_owned(), true);
        Ok(())
    }

    fn register_identity(&self, pid: libc::pid_t, run_id: String, progression: bool) {
        let start = match self.processes.snapshot(pid) {
            Ok(snapshot) => Some(snapshot.start),
            Err(error) => {
                tracing::warn!(
                    pid,
                    run_id = %run_id,
                    %error,
                    "worker registry: could not read the process start time; this entry cannot \
                     be identity-verified and will never match an ancestor walk",
                );
                None
            }
        };
        self.register_with_start(pid, run_id, start, progression);
    }

    fn register_with_start(&self, pid: libc::pid_t, run_id: impl Into<String>, start: Option<u64>, progression: bool) {
        let run_id = run_id.into();
        let mut inner = self.inner.lock().expect("registry poisoned");
        // Re-registration must not downgrade a confined process. Preserve
        // its tier unless a different process incarnation is proven.
        let progression = progression
            || inner.pid_to_run.get(&pid).is_some_and(|entry| {
                entry.progression && !matches!((entry.start, start), (Some(old), Some(new)) if old != new)
            });
        inner.pid_to_run.retain(|existing_pid, entry| {
            let superseded = entry.run_id == run_id && *existing_pid != pid;
            if superseded {
                tracing::info!(
                    pid = *existing_pid,
                    run_id = %run_id,
                    replaced_by = pid,
                    "worker registry: replacing the run's earlier pid entry",
                );
            }
            !superseded
        });
        inner.pid_to_run.insert(
            pid,
            PidEntry {
                run_id,
                start,
                progression,
            },
        );
    }

    /// Record the viewer slot id for `run_id` without a tmux session identity.
    /// Local pane writes and interrupts fail closed for such a run; tmux-hosted
    /// local workers must use [`Self::register_tmux_run_slot`].
    pub fn register_run_slot(&self, run_id: impl Into<String>, slot_id: u8) {
        {
            let mut inner = self.inner.lock().expect("registry poisoned");
            inner.run_to_slot.insert(
                run_id.into(),
                RegisteredWorkerPane {
                    slot_id,
                    tmux_session_name: None,
                },
            );
            self.sleep_assertion
                .set_live_worker_panes(local_live_pane_count(&inner));
        }
    }

    /// Record a tmux-hosted worker whose app surface must detach without
    /// signalling the independently-owned tmux session.
    pub fn register_tmux_run_slot(&self, run_id: impl Into<String>, slot_id: u8, session_name: impl Into<String>) {
        {
            let mut inner = self.inner.lock().expect("registry poisoned");
            inner.run_to_slot.insert(
                run_id.into(),
                RegisteredWorkerPane {
                    slot_id,
                    tmux_session_name: Some(session_name.into()),
                },
            );
            self.sleep_assertion
                .set_live_worker_panes(local_live_pane_count(&inner));
        }
    }

    /// Look up the slot id for `run_id`. Returns `None` if the run
    /// has not been registered (or has been released).
    pub fn slot_for_run(&self, run_id: &str) -> Option<u8> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .get(run_id)
            .map(|pane| pane.slot_id)
    }

    /// Inverse of [`Self::slot_for_run`]: the run currently registered in
    /// `slot_id`, if any. A slot maps to at most one run; if two registrations
    /// somehow share a slot, the first match in iteration order wins.
    pub fn run_for_slot(&self, slot_id: u8) -> Option<String> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .iter()
            .find(|(_, pane)| pane.slot_id == slot_id)
            .map(|(run_id, _)| run_id.clone())
    }

    /// Look up the complete pane registration for `run_id`. Input and
    /// interrupt delivery need the tmux session name, while focus still only
    /// needs the app-facing slot id.
    pub fn pane_for_run(&self, run_id: &str) -> Option<RegisteredWorkerPane> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .get(run_id)
            .cloned()
    }

    /// Get the slot mapped to `run_id`, allocating a virtual remote slot
    /// if none exists.
    ///
    /// Remote workers carry no libghostty pane, so the spawn flow never
    /// calls [`Self::register_run_slot`] for them and [`Self::slot_for_run`]
    /// returns `None` — which makes `dispatch_live_worker_state` drop their
    /// hook fan-out and leaves the live-status surface blank for remote
    /// runs. The dispatcher calls this for a slotless run it has identified
    /// as a live remote worker: it returns the existing mapping when one is
    /// present, otherwise allocates the lowest free slot in
    /// `[REMOTE_SLOT_BASE, u8::MAX]` and records it under the same
    /// `run_to_slot` map the local path uses, so every downstream lookup
    /// (`slot_for_run`, `take_slot_for_run`) works identically.
    ///
    /// Returns `(slot_id, freshly_allocated)`. `freshly_allocated` is
    /// `true` only on the first call for a run, so the caller stamps the
    /// initial `LiveWorkerState` exactly once. Returns `None` only when
    /// every slot in the remote range is already mapped — the caller then
    /// drops the event as it would have before.
    pub fn get_or_allocate_remote_slot(&self, run_id: &str) -> Option<(u8, bool)> {
        let mut inner = self.inner.lock().expect("registry poisoned");
        if let Some(pane) = inner.run_to_slot.get(run_id) {
            return Some((pane.slot_id, false));
        }
        let slot = {
            let used = &inner.run_to_slot;
            (REMOTE_SLOT_BASE..=u8::MAX).find(|s| !used.values().any(|pane| pane.slot_id == *s))
        }?;
        inner.run_to_slot.insert(
            run_id.to_owned(),
            RegisteredWorkerPane {
                slot_id: slot,
                tmux_session_name: None,
            },
        );
        Some((slot, true))
    }

    /// Atomically remove and return the slot id for `run_id`. The
    /// release-pane flow uses this for idempotency — duplicate release
    /// attempts (e.g. completion-detection plus a manual stop firing
    /// for the same run) see `None` on the second call and skip.
    pub fn take_slot_for_run(&self, run_id: &str) -> Option<u8> {
        self.take_worker_pane_for_run(run_id).map(|pane| pane.slot_id)
    }

    /// Atomically remove and return the pane registration for `run_id`.
    /// Cleanup detaches the app viewer; the worker itself is torn down by
    /// token-verified tmux teardown.
    ///
    /// Every release path funnels through here, so it also drops the run's pid
    /// entries — even when no pane was registered — and a finished run's shell
    /// pid can never resolve to it again.
    pub fn take_worker_pane_for_run(&self, run_id: &str) -> Option<RegisteredWorkerPane> {
        {
            let mut inner = self.inner.lock().expect("registry poisoned");
            drop_run_pids(&mut inner, run_id);
            let pane = inner.run_to_slot.remove(run_id);
            self.sleep_assertion
                .set_live_worker_panes(local_live_pane_count(&inner));
            pane
        }
    }

    /// Drop every pid entry for `run_id` without touching its slot mapping.
    /// For teardown that reaps the worker process but does not go through
    /// pane release (an aborted spawn). Returns how many entries were removed.
    pub fn forget_run_pids(&self, run_id: &str) -> usize {
        let mut inner = self.inner.lock().expect("registry poisoned");
        drop_run_pids(&mut inner, run_id)
    }

    /// Drop the entry for `pid`. Called when a run terminates and the
    /// worker process exits.
    pub fn unregister(&self, pid: libc::pid_t) -> Option<String> {
        {
            let mut inner = self.inner.lock().expect("registry poisoned");
            let run_id = inner.pid_to_run.remove(&pid).map(|entry| entry.run_id);
            if let Some(rid) = run_id.as_deref() {
                inner.run_to_slot.remove(rid);
            }
            self.sleep_assertion
                .set_live_worker_panes(local_live_pane_count(&inner));
            run_id
        }
    }

    /// Raw pid lookup: no ancestor walk and **no identity check**, so a
    /// recycled pid still answers. Never use this to attribute a caller; it
    /// exists to inspect registrations. Attribution goes through
    /// [`Self::resolve_ancestry`] / [`Self::lookup_with_ancestor_walk`].
    #[cfg(test)]
    pub(crate) fn lookup_unverified(&self, pid: libc::pid_t) -> Option<String> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .get(&pid)
            .map(|entry| entry.run_id.clone())
    }

    /// Walk up the process tree from `pid` until we find an identity-verified
    /// registered ancestor or hit the depth limit. The shim runs as a
    /// descendant of the worker (claude → shell? → boss-event), so this is
    /// how hook events are correlated to runs. See [`Self::resolve_ancestry`]
    /// for the semantics and the walked chain.
    pub fn lookup_with_ancestor_walk(&self, pid: libc::pid_t) -> Option<String> {
        self.resolve_ancestry(pid).matched.map(|(_, run_id)| run_id)
    }

    /// Resolve identity and confinement tier from the same verified entry.
    pub(crate) fn lookup_classified_with_ancestor_walk(&self, pid: libc::pid_t) -> Option<(String, bool)> {
        let resolution = self.resolve_ancestry(pid);
        resolution.matched.map(|(_, run)| (run, resolution.progression))
    }

    /// Walk up from `pid`, returning every level visited and the first
    /// registered ancestor that is *the same process* that was registered.
    ///
    /// A registry entry whose pid is now held by a process with a different
    /// start time is not a match: it is dropped from the registry and the
    /// walk continues to the next ancestor, so a stale entry can neither
    /// attribute the caller to the wrong run nor mask the caller's real
    /// worker further up. An entry that cannot be verified is skipped too.
    pub fn resolve_ancestry(&self, mut pid: libc::pid_t) -> AncestorResolution {
        let mut chain = Vec::new();
        let mut matched = None;
        let mut progression = false;
        for _ in 0..ANCESTOR_WALK_DEPTH {
            let mut inner = self.inner.lock().expect("registry poisoned");
            let snapshot = self.processes.snapshot(pid);
            let entry = inner.pid_to_run.get(&pid).cloned();

            let verdict = match (&entry, &snapshot) {
                (None, _) => LinkVerdict::Unregistered,
                (
                    Some(PidEntry {
                        start: Some(registered),
                        ..
                    }),
                    Ok(live),
                ) if *registered == live.start => LinkVerdict::Matched,
                (
                    Some(PidEntry {
                        start: Some(registered),
                        ..
                    }),
                    Ok(live),
                ) => LinkVerdict::PidReused {
                    registered_start: *registered,
                    live_start: live.start,
                },
                (Some(_), _) => LinkVerdict::Unverifiable,
            };
            chain.push(AncestorLink {
                pid,
                command: snapshot.as_ref().ok().map(|live| live.command.clone()),
                registered_run: entry.as_ref().map(|entry| entry.run_id.clone()),
                verdict: verdict.clone(),
            });

            match verdict {
                LinkVerdict::Matched => {
                    progression = entry.as_ref().is_some_and(|entry| entry.progression);
                    matched = entry.map(|entry| (pid, entry.run_id));
                    break;
                }
                LinkVerdict::PidReused { .. } | LinkVerdict::Unverifiable => {
                    inner.pid_to_run.remove(&pid);
                }
                LinkVerdict::Unregistered => {}
            }

            pid = match snapshot {
                Ok(ProcessSnapshot { ppid: Some(parent), .. }) => parent,
                Ok(_) | Err(_) => break,
            };
        }
        AncestorResolution {
            chain,
            matched,
            progression,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("registry poisoned").pid_to_run.len()
    }

    /// Snapshot of every registered worker shell pid whose process is still
    /// the one that was registered. Used by the engine's RPC auth to decide
    /// "is this peer a worker?" — peers whose process tree descends from one
    /// of these pids are workers, not the Boss session. Entries whose pid has
    /// been recycled are dropped (and so excluded) rather than letting an
    /// unrelated process's descendants be treated as a worker's.
    pub fn registered_pids(&self) -> Vec<libc::pid_t> {
        let mut inner = self.inner.lock().expect("registry poisoned");
        inner.pid_to_run.retain(|pid, entry| {
            let current = matches!((entry.start, self.processes.snapshot(*pid)),
                (Some(start), Ok(snapshot)) if start == snapshot.start);
            if !current {
                tracing::info!(pid, run_id = %entry.run_id, "worker registry: dropped an unverifiable or reused pid entry");
            }
            current
        });
        inner.pid_to_run.keys().copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Remove every pid entry belonging to `run_id`, returning how many there were.
fn drop_run_pids(inner: &mut RegistryInner, run_id: &str) -> usize {
    let before = inner.pid_to_run.len();
    inner.pid_to_run.retain(|_, entry| entry.run_id != run_id);
    let removed = before - inner.pid_to_run.len();
    if removed > 0 {
        tracing::debug!(
            run_id,
            removed,
            "worker registry: dropped the released run's pid entries"
        );
    }
    removed
}

/// Count of registered panes hosted on *this* machine — every slot below
/// [`REMOTE_SLOT_BASE`]. A remote worker (see
/// [`WorkerRegistry::get_or_allocate_remote_slot`]) runs on a different
/// host entirely, so it must never contribute to the local idle-sleep
/// assertion: this machine keeping itself awake does nothing for a worker
/// that isn't running on it.
fn local_live_pane_count(inner: &RegistryInner) -> u32 {
    inner
        .run_to_slot
        .values()
        .filter(|pane| pane.slot_id < REMOTE_SLOT_BASE)
        .count() as u32
}

/// Read `pid`'s parent pid, start time and command via macOS `proc_pidinfo`.
/// `ppid` is `None` when `pid` is the root of the process tree (parent 0 /
/// launchd).
#[cfg(target_os = "macos")]
pub fn process_snapshot(pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    let info = crate::libproc::proc_bsd_info(pid)?;
    Ok(ProcessSnapshot {
        ppid: if info.ppid == 0 { None } else { Some(info.ppid) },
        start: info.start_micros,
        command: info.command,
    })
}

/// Read `pid`'s parent pid, start time and command from Linux's
/// `/proc/<pid>/stat`.
///
/// The process name field is parenthesized and may itself contain spaces or
/// closing parentheses, so split after its final closing parenthesis before
/// reading the fields that follow it. After the name, field 0 is the state,
/// field 1 the parent pid and field 19 the start time (clock ticks since boot).
#[cfg(target_os = "linux")]
pub fn process_snapshot(pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let invalid = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    let (head, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| invalid(format!("/proc/{pid}/stat is missing its process-name terminator")))?;
    let command = head
        .split_once('(')
        .map(|(_, name)| name)
        .unwrap_or_default()
        .to_owned();
    let mut fields = fields.split_whitespace();
    let parent = fields
        .nth(1)
        .ok_or_else(|| invalid(format!("/proc/{pid}/stat is missing ppid")))?
        .parse::<libc::pid_t>()
        .map_err(|err| invalid(format!("invalid ppid in /proc/{pid}/stat: {err}")))?;
    let start = fields
        .nth(17)
        .ok_or_else(|| invalid(format!("/proc/{pid}/stat is missing starttime")))?
        .parse::<u64>()
        .map_err(|err| invalid(format!("invalid starttime in /proc/{pid}/stat: {err}")))?;
    Ok(ProcessSnapshot {
        ppid: if parent <= 0 { None } else { Some(parent) },
        start,
        command,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_snapshot(_pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process_snapshot is only implemented on macOS and Linux",
    ))
}

/// Look up the parent pid of `pid`. Returns `Ok(None)` when `pid` is the root
/// of the process tree (parent is 0 / launchd).
pub fn parent_pid(pid: libc::pid_t) -> io::Result<Option<libc::pid_t>> {
    process_snapshot(pid).map(|snapshot| snapshot.ppid)
}

#[cfg(test)]
mod tests;
