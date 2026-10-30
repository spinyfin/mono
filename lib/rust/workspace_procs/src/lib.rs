//! Reap descendants and workspace occupants, including re-parented tools.
//! Escalation follows captured identities; only Bazel servers are spared.

pub mod bazel;
#[cfg(target_os = "macos")]
pub mod darwin;
pub mod process_args;
pub mod process_table;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use process_table::{LiveProcess, Signal, SystemWorkspaceProcessTable, WorkspaceProcessTable};

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
    pub roots: Vec<(i32, u64)>,
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

    pub fn with_root(mut self, process: &LiveProcess) -> Self {
        if process.pid > 1 {
            self.roots.push((process.pid, process.start_micros));
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
}

impl SpareReason {
    pub fn describe(&self) -> String {
        match self {
            Self::BazelServer => "bazel server (left to its idle timeout)".to_owned(),
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
    /// Inconclusive verification, retaining the partial signal audit.
    pub error: Option<String>,
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
        self.survivors.is_empty() && self.error.is_none()
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
pub fn scan(table: &dyn WorkspaceProcessTable, scope: &Scope) -> io::Result<Scan> {
    let processes = table.list()?;
    let by_pid: HashMap<i32, &LiveProcess> = processes.iter().map(|p| (p.pid, p)).collect();
    let protected = ancestry(table.own_pid(), &by_pid);
    let roots: HashSet<i32> = scope
        .roots
        .iter()
        .filter_map(|(pid, start)| by_pid.get(pid).filter(|p| p.start_micros == *start).map(|_| *pid))
        .collect();

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
    for path in &paths {
        validate_path(path)?;
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
        let reason = server_pids.contains(&process.pid).then_some(SpareReason::BazelServer);
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
pub fn signal_owned(
    table: &dyn WorkspaceProcessTable,
    owned: &[OwnedProcess],
    signal: Signal,
) -> Vec<SignalledProcess> {
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
pub fn any_alive(table: &dyn WorkspaceProcessTable, targets: &[OwnedProcess]) -> bool {
    targets
        .iter()
        .any(|t| matches!(table.snapshot(t.process.pid), Ok(live) if live.start_micros == t.process.start_micros))
}

/// Wait until none of `targets` is alive or `limit` has elapsed.
pub fn wait_for_exit(table: &dyn WorkspaceProcessTable, targets: &[OwnedProcess], limit: Duration) {
    let deadline = Instant::now() + limit;
    while any_alive(table, targets) && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The blocking `SIGTERM` → grace → rescan → `SIGKILL` → rescan ladder. See
/// the crate docs. Never returns early on a signalling failure; an owned
/// process that cannot be signalled shows up as a survivor.
pub fn reap(table: &dyn WorkspaceProcessTable, scope: &Scope, grace: Duration) -> io::Result<ReapReport> {
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

    let second = match escalation_scan(table, &first.owned) {
        Ok(scan) => scan,
        Err(err) => {
            report.error = Some(err.to_string());
            return Ok(report);
        }
    };
    report.passes = 2;
    if !second.owned.is_empty() {
        report
            .signalled
            .extend(signal_owned(table, &second.owned, Signal::Kill));
        wait_for_exit(table, &second.owned, KILL_SETTLE);
    }

    // Verify the directory as well as retained identities. A newly orphaned
    // occupant must block removal even when it was not an escalation target.
    let mut verification_scope = scope.clone();
    verification_scope.roots.extend(
        first
            .owned
            .iter()
            .chain(&second.owned)
            .map(|target| (target.process.pid, target.process.start_micros)),
    );
    let third = match scan(table, &verification_scope) {
        Ok(scan) => scan,
        Err(err) => {
            report.error = Some(err.to_string());
            return Ok(report);
        }
    };
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

/// Follow captured identities and their descendants, never new path occupants.
pub fn escalation_scan(table: &dyn WorkspaceProcessTable, targets: &[OwnedProcess]) -> io::Result<Scan> {
    scan(
        table,
        &Scope {
            roots: targets
                .iter()
                .map(|t| (t.process.pid, t.process.start_micros))
                .collect(),
            ..Scope::default()
        },
    )
}

fn validate_path(path: &Path) -> io::Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if !path.is_absolute()
        || path.parent().is_none()
        || home.is_some_and(|home| {
            canonical(&home).starts_with(path)
                || canonical(&home.join(".local/share/cube/workspaces")).starts_with(path)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing broad process ownership path {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
extern crate self as workspace_procs;
#[cfg(test)]
mod testing;
