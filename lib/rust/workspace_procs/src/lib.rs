//! Find and kill the processes a finished run left behind in a workspace.
//!
//! # The leak this closes
//!
//! A worker run ends, its terminal pane is torn down, and yet processes the
//! run started keep going: a `( while :; do …; done ) &` subshell from a tool
//! command that was cut short before its `kill`, a test binary stranded in a
//! Bazel sandbox, a daemon. They re-parent to launchd (pid 1), so nothing
//! that walks *down* from the pane's shell finds them, and killing the pane
//! shell's process group misses them too: an agent's tool shells are spawned
//! detached into their own groups, and a backgrounded subshell inherits the
//! tool shell's group, not the pane's. Observed live: a pane shell in group
//! 30509, the agent in group 31286, its tool shell in group 35428, and a
//! `( sleep 30 ) &` from that shell in 35428 — three groups, one run. The
//! workspace is then released and even removed underneath the survivors,
//! which is how four fork loops ran for four days with their working
//! directory in a `.removing-*` directory.
//!
//! # Ownership
//!
//! A process belongs to a run when either holds:
//!
//! - **Descendant**: its parent chain reaches one of the run's root pids
//!   (the pane shell). Catches everything still attached, whatever its
//!   process group.
//! - **Working directory**: its current directory is inside one of the run's
//!   paths — the workspace (under whatever name it currently has, so a
//!   staged `.removing-<id>` directory counts) or the workspace's Bazel output
//!   base, where Bazel runs test actions. The working directory is read from
//!   the kernel, so it survives re-parenting and names the directory's
//!   *current* path after a rename.
//!
//! Three kinds of process are deliberately **spared** and reported instead
//! of killed:
//!
//! - the workspace's Bazel server ([`bazel::is_bazel_server`]): long-lived
//!   on purpose, exits on its own idle timeout; its `--output_base=` is what
//!   extends the sweep to the output base in the first place;
//! - live descendants of that server (actions it is still running; it
//!   cancels them itself when its client goes away);
//! - a process with a controlling terminal that is owned only by working
//!   directory: a human's interactive shell that happens to be `cd`'d into
//!   the workspace. Nothing a worker's detached tool runner spawns has one.
//!
//! The caller's own process and its ancestors are never candidates, so a
//! `cube workspace release` typed from a shell inside the workspace cannot
//! kill the shell it was typed into.
//!
//! # Killing
//!
//! [`reap`] sends `SIGTERM` to every owned process, waits up to a grace
//! period for them to exit, rescans (a fork loop keeps producing children
//! until its loop process is gone), sends `SIGKILL` to whatever is left,
//! rescans once more and reports the survivors. Every signal is checked
//! against the process start time recorded at the scan, so a pid recycled
//! in between is never signalled.

pub mod bazel;
pub mod process_table;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use process_table::{LiveProcess, ProcessTable, Signal, StaticProcessTable, SystemProcessTable, fake_process};

/// Bound on an ancestry walk; the launcher chain between a pane shell and a
/// tool's own subprocesses is a dozen deep at most.
const ANCESTRY_WALK_DEPTH: usize = 64;

/// How long [`reap`] waits for a `SIGKILL`ed process to disappear before
/// reporting it as a survivor.
const KILL_SETTLE: Duration = Duration::from_secs(2);

/// How often [`reap`] re-checks whether signalled processes are gone.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What a sweep is looking for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    /// Pids whose live descendants belong to the run (the pane shell).
    pub roots: Vec<i32>,
    /// Directories whose occupants belong to the run: the workspace path (or
    /// its staged removal path) and anything else the caller knows about.
    pub paths: Vec<PathBuf>,
    /// Also own processes inside the workspace's Bazel output base, found
    /// via the workspace's `bazel-out` symlink and via the command line of
    /// a live Bazel server whose `--workspace_directory=` is one of `paths`.
    pub include_bazel_output_bases: bool,
}

impl Scope {
    /// Everything inside `path`, plus its Bazel output base.
    pub fn for_workspace(path: impl Into<PathBuf>) -> Self {
        Self {
            roots: Vec::new(),
            paths: vec![path.into()],
            include_bazel_output_bases: true,
        }
    }

    pub fn with_root(mut self, pid: i32) -> Self {
        if pid > 1 {
            self.roots.push(pid);
        }
        self
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }
}

/// Why a process belongs to the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// Its parent chain reaches `root`.
    Descendant { root: i32 },
    /// Its working directory is inside `inside`.
    WorkingDirectory { inside: PathBuf },
}

impl Ownership {
    pub fn describe(&self) -> String {
        match self {
            Self::Descendant { root } => format!("descendant of pid {root}"),
            Self::WorkingDirectory { inside } => format!("working directory inside {}", inside.display()),
        }
    }
}

/// Why an owned process was left alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpareReason {
    BazelServer,
    BazelServerDescendant { server: i32 },
    Interactive,
}

impl SpareReason {
    pub fn describe(&self) -> String {
        match self {
            Self::BazelServer => "bazel server (left to its idle timeout)".to_owned(),
            Self::BazelServerDescendant { server } => format!("child of live bazel server pid {server}"),
            Self::Interactive => "has a controlling terminal; owned only by working directory".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedProcess {
    pub process: LiveProcess,
    pub ownership: Ownership,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SparedProcess {
    pub process: LiveProcess,
    pub ownership: Ownership,
    pub reason: SpareReason,
}

/// One pass over the process table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub owned: Vec<OwnedProcess>,
    pub spared: Vec<SparedProcess>,
    /// The directories the working-directory rule matched against after
    /// canonicalisation and Bazel output-base discovery.
    pub paths: Vec<PathBuf>,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.owned.is_empty()
    }
}

/// A signal actually delivered during [`reap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalledProcess {
    pub pid: i32,
    pub command: String,
    pub argv: String,
    pub age_secs: u64,
    pub ownership: Ownership,
    pub signal: Signal,
}

impl SignalledProcess {
    /// A single log-line description.
    pub fn describe(&self) -> String {
        format!(
            "pid {} ({}) age {}s {} [{}]: {}",
            self.pid,
            self.command,
            self.age_secs,
            self.signal.as_str(),
            self.ownership.describe(),
            self.argv,
        )
    }
}

/// What [`reap`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Every signal sent, in order (`SIGTERM`s, then `SIGKILL`s).
    pub signalled: Vec<SignalledProcess>,
    /// Owned processes still alive after the ladder.
    pub survivors: Vec<OwnedProcess>,
    /// Owned-but-spared processes, from the final scan.
    pub spared: Vec<SparedProcess>,
    pub passes: usize,
}

impl ReapReport {
    /// Distinct pids that received a signal.
    pub fn killed_pids(&self) -> Vec<i32> {
        let mut seen = HashSet::new();
        self.signalled
            .iter()
            .filter(|s| seen.insert(s.pid))
            .map(|s| s.pid)
            .collect()
    }

    pub fn is_clean(&self) -> bool {
        self.survivors.is_empty()
    }

    /// `pid (command)` for each survivor, for an error message.
    pub fn survivor_summary(&self) -> String {
        self.survivors
            .iter()
            .map(|s| format!("{} ({})", s.process.pid, s.process.command))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Classify every live process against `scope`. Read-only.
pub fn scan(table: &dyn ProcessTable, scope: &Scope) -> io::Result<Scan> {
    let processes = table.list()?;
    let by_pid: HashMap<i32, &LiveProcess> = processes.iter().map(|p| (p.pid, p)).collect();
    let protected = ancestry(table.own_pid(), &by_pid);
    let roots: HashSet<i32> = scope.roots.iter().copied().filter(|p| *p > 1).collect();

    let mut paths: Vec<PathBuf> = scope.paths.iter().map(|p| canonical(p)).collect();
    let servers: Vec<&LiveProcess> = processes.iter().filter(|p| bazel::is_bazel_server(p)).collect();
    if scope.include_bazel_output_bases {
        for path in scope.paths.iter() {
            if let Some(base) = bazel::output_base_from_symlink(path) {
                paths.push(canonical(&base));
            }
        }
        for server in &servers {
            if let Some(ws) = bazel::server_workspace_directory(server)
                && paths.iter().any(|p| canonical(&ws).starts_with(p))
                && let Some(base) = bazel::server_output_base(server)
            {
                paths.push(canonical(&base));
            }
        }
    }
    paths.sort();
    paths.dedup();
    let server_pids: HashSet<i32> = servers.iter().map(|s| s.pid).collect();

    let mut owned = Vec::new();
    let mut spared = Vec::new();
    for process in &processes {
        if process.pid <= 1 || protected.contains(&process.pid) {
            continue;
        }
        let chain = ancestry(process.pid, &by_pid);
        let descendant_root = chain.iter().find(|p| roots.contains(p)).copied();
        let ownership = if let Some(root) = descendant_root {
            Ownership::Descendant { root }
        } else if let Some(inside) = process
            .cwd
            .as_deref()
            .and_then(|cwd| paths.iter().find(|p| cwd.starts_with(p)))
        {
            Ownership::WorkingDirectory { inside: inside.clone() }
        } else {
            continue;
        };
        // A server's live descendants are its own to cancel — unless one of
        // the run's roots sits between them and the server, in which case
        // the server is merely an ancestor of the run (the case under
        // `bazel test`, where every test process descends from one) and the
        // process is the run's.
        let server_ancestor = chain
            .iter()
            .take_while(|p| !roots.contains(p))
            .skip(1)
            .find(|p| server_pids.contains(p));
        let reason = if server_pids.contains(&process.pid) {
            Some(SpareReason::BazelServer)
        } else if let Some(server) = server_ancestor {
            Some(SpareReason::BazelServerDescendant { server: *server })
        } else if process.has_controlling_terminal && descendant_root.is_none() {
            Some(SpareReason::Interactive)
        } else {
            None
        };
        match reason {
            Some(reason) => spared.push(SparedProcess {
                process: process.clone(),
                ownership,
                reason,
            }),
            None => owned.push(OwnedProcess {
                process: process.clone(),
                ownership,
            }),
        }
    }
    owned.sort_by_key(|o| o.process.pid);
    spared.sort_by_key(|s| s.process.pid);
    Ok(Scan { owned, spared, paths })
}

/// Deliver `signal` to each of `owned`, skipping any whose pid now belongs to
/// a different process (start time changed) or has already exited. Returns
/// the signals actually sent.
pub fn signal_owned(table: &dyn ProcessTable, owned: &[OwnedProcess], signal: Signal) -> Vec<SignalledProcess> {
    let now = table.now_micros();
    let mut sent = Vec::new();
    for target in owned {
        let Ok(live) = table.snapshot(target.process.pid) else {
            continue;
        };
        if live.start_micros != target.process.start_micros {
            continue;
        }
        if table.signal(target.process.pid, signal).is_err() {
            continue;
        }
        sent.push(SignalledProcess {
            pid: target.process.pid,
            command: target.process.command.clone(),
            argv: target.process.argv_summary(),
            age_secs: now.saturating_sub(target.process.start_micros) / 1_000_000,
            ownership: target.ownership.clone(),
            signal,
        });
    }
    sent
}

/// Whether any of `targets` is still the same live process.
pub fn any_alive(table: &dyn ProcessTable, targets: &[OwnedProcess]) -> bool {
    targets
        .iter()
        .any(|t| matches!(table.snapshot(t.process.pid), Ok(live) if live.start_micros == t.process.start_micros))
}

/// Wait until none of `targets` is alive or `limit` has elapsed.
pub fn wait_for_exit(table: &dyn ProcessTable, targets: &[OwnedProcess], limit: Duration) {
    let deadline = Instant::now() + limit;
    while any_alive(table, targets) && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The blocking `SIGTERM` → grace → rescan → `SIGKILL` → rescan ladder. See
/// the crate docs. Never returns early on a signalling failure; an owned
/// process that cannot be signalled shows up as a survivor.
pub fn reap(table: &dyn ProcessTable, scope: &Scope, grace: Duration) -> io::Result<ReapReport> {
    let first = scan(table, scope)?;
    let mut report = ReapReport {
        passes: 1,
        ..ReapReport::default()
    };
    if first.owned.is_empty() {
        report.spared = first.spared;
        return Ok(report);
    }
    report.signalled.extend(signal_owned(table, &first.owned, Signal::Term));
    wait_for_exit(table, &first.owned, grace);

    let second = scan(table, scope)?;
    report.passes = 2;
    if !second.owned.is_empty() {
        report
            .signalled
            .extend(signal_owned(table, &second.owned, Signal::Kill));
        wait_for_exit(table, &second.owned, KILL_SETTLE);
    }

    let third = scan(table, scope)?;
    report.passes = 3;
    report.survivors = third.owned;
    report.spared = third.spared;
    Ok(report)
}

/// `pid` and its ancestors, nearest first, as far as the table can see.
fn ancestry(pid: i32, by_pid: &HashMap<i32, &LiveProcess>) -> Vec<i32> {
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..ANCESTRY_WALK_DEPTH {
        if current <= 0 || chain.contains(&current) {
            break;
        }
        chain.push(current);
        match by_pid.get(&current) {
            Some(process) => current = process.ppid,
            None => break,
        }
    }
    chain
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests;
