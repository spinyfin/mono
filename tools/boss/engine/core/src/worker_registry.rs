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
        let progression = progression || inner.pid_to_run.get(&pid).is_some_and(|entry| {
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
        inner.pid_to_run.insert(pid, PidEntry { run_id, start, progression });
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
                    // Drop only the entry we judged stale; a concurrent
                    // re-registration at this pid must survive.
                    if inner.pid_to_run.get(&pid) == entry.as_ref() {
                        inner.pid_to_run.remove(&pid);
                    }
                }
                LinkVerdict::Unregistered => {}
            }

            pid = match snapshot {
                Ok(ProcessSnapshot { ppid: Some(parent), .. }) => parent,
                Ok(_) | Err(_) => break,
            };
        }
        AncestorResolution { chain, matched, progression }
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
        let candidates: Vec<(libc::pid_t, PidEntry)> = self
            .inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .iter()
            .map(|(pid, entry)| (*pid, entry.clone()))
            .collect();
        let mut live = Vec::new();
        for (pid, entry) in candidates {
            match (entry.start, self.processes.snapshot(pid)) {
                (Some(registered), Ok(snapshot)) if registered == snapshot.start => live.push(pid),
                _ => {
                    let mut inner = self.inner.lock().expect("registry poisoned");
                    if inner.pid_to_run.get(&pid) == Some(&entry) {
                        inner.pid_to_run.remove(&pid);
                        tracing::info!(pid, run_id = %entry.run_id, "worker registry: dropped a pid entry whose pid was reused");
                    }
                }
            }
        }
        live
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
mod tests {
    use super::*;

    #[test]
    fn register_after_register_progression_keeps_progression_tier() {
        let (reg, procs) = fake_registry();
        procs.spawn(77, 1, 7, "progression");
        procs.spawn(78, 1, 8, "worker");
        reg.inner.lock().unwrap().pid_to_run.insert(
            77,
            PidEntry {
                run_id: "run-p".to_owned(),
                start: Some(7),
                progression: true,
            },
        );
        reg.register(77, "run-p");
        assert_eq!(
            reg.lookup_classified_with_ancestor_walk(77),
            Some(("run-p".to_owned(), true))
        );
        reg.register_with_start(77, "run-p", Some(7), false);
        assert_eq!(
            reg.lookup_classified_with_ancestor_walk(77),
            Some(("run-p".to_owned(), true))
        );
        reg.register(78, "run-w");
        assert_eq!(
            reg.lookup_classified_with_ancestor_walk(78),
            Some(("run-w".to_owned(), false))
        );
    }

    #[test]
    fn register_and_lookup() {
        let reg = WorkerRegistry::new();
        reg.register(1234, "run-abc");
        assert_eq!(reg.lookup_unverified(1234).as_deref(), Some("run-abc"));
        assert_eq!(reg.lookup_unverified(9999), None);
    }

    #[test]
    fn unregister_removes_and_returns_old_value() {
        let reg = WorkerRegistry::new();
        reg.register(42, "run-1");
        assert_eq!(reg.unregister(42).as_deref(), Some("run-1"));
        assert_eq!(reg.lookup_unverified(42), None);
    }

    #[test]
    fn unregister_unknown_pid_returns_none() {
        let reg = WorkerRegistry::new();
        assert_eq!(reg.unregister(123), None);
    }

    #[test]
    fn len_and_is_empty_track_state() {
        let reg = WorkerRegistry::new();
        assert!(reg.is_empty());
        reg.register(1, "a");
        reg.register(2, "b");
        assert_eq!(reg.len(), 2);
        reg.unregister(1);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn run_for_slot_is_the_inverse_of_slot_for_run() {
        let reg = WorkerRegistry::new();
        reg.register_run_slot("run-x", 4);
        assert_eq!(reg.run_for_slot(4).as_deref(), Some("run-x"));
        assert_eq!(reg.run_for_slot(5), None);
        assert_eq!(reg.take_slot_for_run("run-x"), Some(4));
        assert_eq!(reg.run_for_slot(4), None);
    }

    #[test]
    fn take_slot_for_run_is_idempotent() {
        let reg = WorkerRegistry::new();
        reg.register_run_slot("run-x", 4);
        assert_eq!(reg.take_slot_for_run("run-x"), Some(4));
        // Second call returns None — that's the idempotency guard the
        // release path leans on.
        assert_eq!(reg.take_slot_for_run("run-x"), None);
        // Unregistered run is also None.
        assert_eq!(reg.take_slot_for_run("never-registered"), None);
    }

    #[test]
    fn tmux_registration_preserves_session_identity_until_cleanup() {
        let reg = WorkerRegistry::new();
        reg.register_tmux_run_slot("run-tmux", 4, "boss-4-worker");

        assert_eq!(reg.slot_for_run("run-tmux"), Some(4));
        assert_eq!(
            reg.take_worker_pane_for_run("run-tmux"),
            Some(RegisteredWorkerPane {
                slot_id: 4,
                tmux_session_name: Some("boss-4-worker".to_owned()),
            })
        );
        assert_eq!(reg.slot_for_run("run-tmux"), None);
    }

    #[test]
    fn registry_clones_share_state() {
        let reg = WorkerRegistry::new();
        let clone = reg.clone();
        reg.register(7, "shared");
        assert_eq!(clone.lookup_unverified(7).as_deref(), Some("shared"));
        clone.unregister(7);
        assert!(reg.is_empty());
    }

    #[test]
    fn remote_slot_allocation_is_idempotent_per_run() {
        let reg = WorkerRegistry::new();
        let (slot_a, fresh_a) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert!(fresh_a, "first allocation must report freshly_allocated");
        assert!(
            slot_a >= REMOTE_SLOT_BASE,
            "remote slot must come from the reserved range"
        );
        // Second call for the same run returns the same slot, not fresh.
        let (slot_again, fresh_again) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert_eq!(slot_again, slot_a);
        assert!(!fresh_again);
        // slot_for_run now resolves it like any other registered run.
        assert_eq!(reg.slot_for_run("exec-a"), Some(slot_a));
    }

    #[test]
    fn remote_slots_are_distinct_and_avoid_local_pool_range() {
        let reg = WorkerRegistry::new();
        // Occupy a couple of local-pool slots first.
        reg.register_run_slot("local-1", 1);
        reg.register_run_slot("local-9", 9);
        let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
        assert_ne!(a, b, "concurrent remote runs must get distinct slots");
        for s in [a, b] {
            assert!(s >= REMOTE_SLOT_BASE, "remote slot {s} collided with the pool range");
        }
    }

    #[test]
    fn sleep_assertion_counts_only_local_panes_and_releases_them() {
        let reg = WorkerRegistry::new();
        assert!(!reg.sleep_assertion.is_asserted());

        reg.get_or_allocate_remote_slot("remote").unwrap();
        assert!(
            !reg.sleep_assertion.is_asserted(),
            "remote panes must not take the local machine assertion"
        );

        reg.register_run_slot("local", 1);
        assert!(reg.sleep_assertion.is_asserted());
        reg.take_slot_for_run("local");
        assert!(!reg.sleep_assertion.is_asserted());
    }

    #[test]
    fn remote_slot_reuses_lowest_free_after_release() {
        let reg = WorkerRegistry::new();
        let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert_eq!(a, REMOTE_SLOT_BASE);
        let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
        assert_eq!(b, REMOTE_SLOT_BASE + 1);
        // Release the first run's slot; the next allocation reuses it.
        assert_eq!(reg.take_slot_for_run("exec-a"), Some(REMOTE_SLOT_BASE));
        let (c, fresh) = reg.get_or_allocate_remote_slot("exec-c").unwrap();
        assert_eq!(c, REMOTE_SLOT_BASE);
        assert!(fresh);
    }

    #[test]
    fn take_worker_pane_for_run_drops_the_runs_pids() {
        let (reg, procs) = fake_registry();
        procs.spawn(5001, 1, 5001, "zsh");
        reg.register(5001, "run-done");
        procs.spawn(5002, 1, 5002, "zsh");
        reg.register(5002, "run-live");
        reg.register_run_slot("run-done", 3);
        reg.register_run_slot("run-live", 4);

        assert_eq!(reg.lookup_with_ancestor_walk(5001).as_deref(), Some("run-done"));
        assert!(reg.take_worker_pane_for_run("run-done").is_some());

        assert_eq!(reg.lookup_with_ancestor_walk(5001), None, "a torn-down run's pid must no longer resolve");
        assert!(!reg.registered_pids().contains(&5001));
        assert_eq!(
            reg.lookup_with_ancestor_walk(5002).as_deref(),
            Some("run-live"),
            "other runs are untouched"
        );
    }

    #[test]
    fn take_worker_pane_for_run_drops_pids_even_without_a_mapped_pane() {
        let (reg, procs) = fake_registry();
        procs.spawn(5003, 1, 5003, "zsh");
        reg.register(5003, "run-no-slot");
        assert!(reg.take_worker_pane_for_run("run-no-slot").is_none());
        assert_eq!(reg.lookup_with_ancestor_walk(5003), None);
        assert!(reg.is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn registered_pid_reused_by_a_process_with_a_different_start_time_misses() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        let live = process_snapshot(self_pid).unwrap();
        // Simulate the pid having been registered by an earlier process that
        // has since exited and whose pid this test process now holds.
        let earlier = live.start.wrapping_sub(1);
        reg.register_with_start(self_pid, "finished-run", Some(earlier), false);
        reg.inner
            .lock()
            .unwrap()
            .pid_to_run
            .get_mut(&self_pid)
            .unwrap()
            .progression = true;

        assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
        assert_eq!(reg.resolve_ancestry(self_pid).matched, None);
        assert_eq!(reg.lookup_classified_with_ancestor_walk(self_pid), None);
        assert!(!reg.registered_pids().contains(&self_pid));
        reg.register(self_pid, "new-run");
        assert_eq!(
            reg.lookup_classified_with_ancestor_walk(self_pid),
            Some(("new-run".to_owned(), false))
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_walk_skips_a_reused_pid_and_finds_the_live_registered_ancestor() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        let Some(parent) = parent_pid(self_pid).unwrap() else {
            return;
        };
        let live = process_snapshot(self_pid).unwrap();
        let earlier = live.start.wrapping_sub(1);
        reg.register_with_start(self_pid, "stale-run", Some(earlier), false);
        reg.register(parent, "live-run");

        assert_eq!(
            reg.resolve_ancestry(self_pid).matched,
            Some((parent, "live-run".to_owned()))
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn register_records_start_time_so_the_live_process_still_matches() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        reg.register(self_pid, "self-run");
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("self-run"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn parent_pid_of_self_is_some() {
        let self_pid = std::process::id() as libc::pid_t;
        let parent = parent_pid(self_pid).unwrap();
        assert!(parent.is_some(), "expected a parent pid for the test process");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_walk_finds_self_pid_when_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        reg.register(self_pid, "self-run");
        // Walking up from self should hit self immediately.
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("self-run"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_walk_finds_parent_when_only_parent_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        let Some(parent) = parent_pid(self_pid).unwrap() else {
            // No parent? Skip.
            return;
        };
        reg.register(parent, "parent-run");
        // Walking up from self should reach parent.
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("parent-run"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_walk_returns_none_when_no_ancestor_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_walk_reaches_registered_shell_through_deep_launcher_chain() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("nest.sh");
        let marker = dir.path().join("deepest.pid");
        std::fs::write(
            &script,
            "if [ \"$1\" -gt 0 ]; then sh \"$0\" $(($1 - 1)) \"$2\"; true; else echo $$ > \"$2\"; sleep 5; fi\n",
        )
        .unwrap();
        // The registered shell sits 11 processes above the deepest caller.
        let mut outer = Command::new("sh").arg(&script).arg("11").arg(&marker).spawn().unwrap();
        let mut deepest = None;
        for _ in 0..200 {
            if let Ok(raw) = std::fs::read_to_string(&marker)
                && let Ok(pid) = raw.trim().parse::<libc::pid_t>()
            {
                deepest = Some(pid);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let deepest = deepest.expect("deepest process published its pid");
        let reg = WorkerRegistry::new();
        reg.register(outer.id() as libc::pid_t, "chain-run");
        let found = reg.lookup_with_ancestor_walk(deepest);
        let _ = outer.kill();
        let _ = outer.wait();
        assert_eq!(found.as_deref(), Some("chain-run"));
    }

    /// A fabricated process tree: pid -> (ppid, start, command).
    #[derive(Default)]
    struct FakeProcs(Mutex<HashMap<libc::pid_t, ProcessSnapshot>>);

    impl FakeProcs {
        fn spawn(&self, pid: libc::pid_t, ppid: libc::pid_t, start: u64, command: &str) {
            self.0.lock().unwrap().insert(
                pid,
                ProcessSnapshot {
                    ppid: (ppid != 0).then_some(ppid),
                    start,
                    command: command.to_owned(),
                },
            );
        }

        fn exit(&self, pid: libc::pid_t) {
            self.0.lock().unwrap().remove(&pid);
        }
    }

    impl ProcessTable for FakeProcs {
        fn snapshot(&self, pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
            self.0
                .lock()
                .unwrap()
                .get(&pid)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))
        }
    }

    fn fake_registry() -> (WorkerRegistry, Arc<FakeProcs>) {
        let procs = Arc::new(FakeProcs::default());
        let reg = WorkerRegistry::new().with_process_table(procs.clone());
        (reg, procs)
    }

    #[test]
    fn walk_resolves_a_live_worker_chain_and_reports_it() {
        let (reg, procs) = fake_registry();
        procs.spawn(1, 0, 10, "launchd");
        procs.spawn(6228, 1, 100, "zsh");
        procs.spawn(7133, 6228, 101, "codex");
        procs.spawn(7200, 7133, 102, "boss");
        reg.register(6228, "run-new");

        let resolution = reg.resolve_ancestry(7200);
        assert_eq!(resolution.matched, Some((6228, "run-new".to_owned())));
        assert_eq!(
            resolution.chain.iter().map(|link| link.pid).collect::<Vec<_>>(),
            vec![7200, 7133, 6228],
            "the walk stops at the match"
        );
        let described = resolution.describe_chain();
        assert!(described.contains("pid 7200 (boss)"), "{described}");
        assert!(
            described.contains("pid 6228 (zsh) [matched run run-new]"),
            "{described}"
        );
        assert_eq!(reg.lookup_with_ancestor_walk(7200).as_deref(), Some("run-new"));
    }

    #[test]
    fn released_run_pid_no_longer_resolves() {
        let (reg, procs) = fake_registry();
        procs.spawn(91148, 1, 100, "zsh");
        procs.spawn(91200, 91148, 101, "claude");
        reg.register_tmux_run_slot("run-old", 4, "boss-4");
        reg.register(91148, "run-old");
        assert_eq!(reg.lookup_with_ancestor_walk(91200).as_deref(), Some("run-old"));

        // The shell is still alive (a slow reap) but the run was released.
        assert!(reg.take_worker_pane_for_run("run-old").is_some());
        assert_eq!(reg.lookup_with_ancestor_walk(91200), None);
        assert_eq!(reg.lookup_unverified(91148), None);
        assert!(reg.is_empty());
    }

    #[test]
    fn release_drops_pids_even_when_no_pane_was_registered() {
        let (reg, procs) = fake_registry();
        procs.spawn(50, 1, 100, "zsh");
        reg.register(50, "run-x");
        assert_eq!(reg.take_worker_pane_for_run("run-x"), None);
        assert_eq!(reg.lookup_unverified(50), None);
    }

    #[test]
    fn forget_run_pids_leaves_other_runs_alone() {
        let (reg, procs) = fake_registry();
        procs.spawn(50, 1, 100, "zsh");
        procs.spawn(51, 1, 101, "zsh");
        reg.register(50, "run-a");
        reg.register(51, "run-b");
        assert_eq!(reg.forget_run_pids("run-a"), 1);
        assert_eq!(reg.lookup_unverified(50), None);
        assert_eq!(reg.lookup_unverified(51).as_deref(), Some("run-b"));
    }

    #[test]
    fn pid_reused_by_an_unrelated_process_does_not_resolve_and_is_dropped() {
        let (reg, procs) = fake_registry();
        procs.spawn(91148, 1, 100, "zsh");
        reg.register(91148, "run-old");
        // The old shell exits without any release; the kernel hands its pid to
        // an unrelated process that later spawns the caller.
        procs.exit(91148);
        procs.spawn(91148, 1, 999, "node");
        procs.spawn(91300, 91148, 1000, "boss");

        let resolution = reg.resolve_ancestry(91300);
        assert_eq!(resolution.matched, None);
        assert!(matches!(
            resolution.chain[1].verdict,
            LinkVerdict::PidReused {
                registered_start: 100,
                live_start: 999
            }
        ));
        assert!(
            resolution.describe_chain().contains("pid reused"),
            "{}",
            resolution.describe_chain()
        );
        assert_eq!(reg.lookup_unverified(91148), None, "the stale entry is dropped");
    }

    #[test]
    fn live_workers_chain_resolves_past_a_stale_entry_at_an_ancestor_pid() {
        let (reg, procs) = fake_registry();
        // An old run registered pid 7133; it ended and 7133 was recycled for
        // the new worker's agent process, which sits *below* the new shell.
        procs.spawn(7133, 1, 50, "old-shell");
        reg.register(7133, "run-old");
        procs.exit(7133);

        procs.spawn(6228, 1, 100, "zsh");
        reg.register(6228, "run-new");
        procs.spawn(7133, 6228, 101, "codex");
        procs.spawn(7200, 7133, 102, "boss");

        let resolution = reg.resolve_ancestry(7200);
        assert_eq!(resolution.matched, Some((6228, "run-new".to_owned())));
        assert!(matches!(resolution.chain[1].verdict, LinkVerdict::PidReused { .. }));
        assert_eq!(reg.lookup_unverified(7133), None);
        assert_eq!(reg.lookup_unverified(6228).as_deref(), Some("run-new"));
    }

    #[test]
    fn entry_without_a_captured_start_time_never_matches() {
        let (reg, procs) = fake_registry();
        // The process was already gone when it was registered.
        reg.register(77, "run-ghost");
        procs.spawn(77, 1, 5, "zsh");
        procs.spawn(78, 77, 6, "boss");
        let resolution = reg.resolve_ancestry(78);
        assert_eq!(resolution.matched, None);
        assert!(matches!(resolution.chain[1].verdict, LinkVerdict::Unverifiable));
        assert_eq!(reg.lookup_unverified(77), None);
    }

    #[test]
    fn reregistering_a_run_at_a_new_pid_replaces_the_old_entry() {
        let (reg, procs) = fake_registry();
        procs.spawn(10, 1, 1, "zsh");
        procs.spawn(11, 1, 2, "zsh");
        reg.register(10, "run-a");
        reg.register(11, "run-a");
        assert_eq!(reg.lookup_unverified(10), None);
        assert_eq!(reg.lookup_unverified(11).as_deref(), Some("run-a"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registering_a_pid_for_a_new_run_replaces_the_previous_runs_entry() {
        let (reg, procs) = fake_registry();
        procs.spawn(10, 1, 1, "zsh");
        reg.register(10, "run-a");
        procs.exit(10);
        procs.spawn(10, 1, 2, "zsh");
        reg.register(10, "run-b");
        procs.spawn(11, 10, 3, "boss");
        assert_eq!(reg.lookup_with_ancestor_walk(11).as_deref(), Some("run-b"));
    }

    #[test]
    fn registered_pids_excludes_and_drops_recycled_pids() {
        let (reg, procs) = fake_registry();
        procs.spawn(10, 1, 1, "zsh");
        procs.spawn(11, 1, 2, "zsh");
        reg.register(10, "run-a");
        reg.register(11, "run-b");
        procs.exit(10);
        procs.spawn(10, 1, 99, "unrelated");
        assert_eq!(reg.registered_pids(), vec![11]);
        assert_eq!(reg.lookup_unverified(10), None);
    }

    #[test]
    fn walk_depth_is_unchanged() {
        let (reg, procs) = fake_registry();
        // A registered ancestor beyond the walk depth is not found.
        procs.spawn(1000, 1, 1, "zsh");
        reg.register(1000, "run-deep");
        let mut parent = 1000;
        for pid in 1001..=1000 + ANCESTOR_WALK_DEPTH as libc::pid_t {
            procs.spawn(pid, parent, pid as u64, "p");
            parent = pid;
        }
        assert_eq!(reg.lookup_with_ancestor_walk(parent), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn real_process_table_verifies_the_live_process() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        let snapshot = process_snapshot(self_pid).unwrap();
        assert!(snapshot.start > 0);
        assert!(!snapshot.command.is_empty());
        reg.register(self_pid, "self-run");
        let resolution = reg.resolve_ancestry(self_pid);
        assert_eq!(resolution.matched, Some((self_pid, "self-run".to_owned())));
        // A different start time for the same pid is a different process.
        reg.inner.lock().unwrap().pid_to_run.get_mut(&self_pid).unwrap().start = Some(snapshot.start + 1);
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
    }
}
