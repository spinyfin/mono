//! Fabricated process tables for cleanup tests.
use std::{io, path::PathBuf, sync::Mutex};
use workspace_procs::{LiveProcess, Signal, WorkspaceProcessTable};

/// An in-memory process table for tests: a fixed set of processes, plus a
/// log of every signal sent. A `Kill` removes the process from the table so
/// a following scan sees it gone; a `Term` does too unless the pid is in
/// `ignores_term`, which models a process that shrugs off SIGTERM. An
/// `immortal` pid survives both, modelling one the caller cannot kill.
pub struct StaticProcessTable {
    inner: Mutex<StaticInner>,
    own_pid: i32,
    now_micros: u64,
}

struct StaticInner {
    processes: Vec<LiveProcess>,
    ignores_term: Vec<i32>,
    immortal: Vec<i32>,
    signals: Vec<(i32, Signal)>,
}

impl StaticProcessTable {
    pub fn new(processes: Vec<LiveProcess>, own_pid: i32) -> Self {
        Self {
            inner: Mutex::new(StaticInner {
                processes,
                ignores_term: Vec::new(),
                immortal: Vec::new(),
                signals: Vec::new(),
            }),
            own_pid,
            now_micros: 1_000_000_000_000_000,
        }
    }

    /// Make `pid` survive SIGTERM (it only disappears on SIGKILL).
    pub fn ignore_term(&self, pid: i32) {
        self.inner.lock().unwrap().ignores_term.push(pid);
    }

    /// Make `pid` survive every signal: the signal is recorded as delivered
    /// but the process stays.
    pub fn immortal(&self, pid: i32) {
        self.inner.lock().unwrap().immortal.push(pid);
    }

    /// Every `(pid, signal)` delivered so far, in order.
    pub fn signals(&self) -> Vec<(i32, Signal)> {
        self.inner.lock().unwrap().signals.clone()
    }

    pub fn replace(&self, process: LiveProcess) {
        let mut inner = self.inner.lock().unwrap();
        inner.processes.retain(|p| p.pid != process.pid);
        inner.processes.push(process);
    }

    pub fn is_alive(&self, pid: i32) -> bool {
        self.inner.lock().unwrap().processes.iter().any(|p| p.pid == pid)
    }
}

impl WorkspaceProcessTable for StaticProcessTable {
    fn list(&self) -> io::Result<Vec<LiveProcess>> {
        Ok(self.inner.lock().unwrap().processes.clone())
    }

    fn snapshot(&self, pid: i32) -> io::Result<LiveProcess> {
        self.inner
            .lock()
            .unwrap()
            .processes
            .iter()
            .find(|p| p.pid == pid)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))
    }

    fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.processes.iter().any(|p| p.pid == pid) {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")));
        }
        inner.signals.push((pid, signal));
        let dies = !inner.immortal.contains(&pid)
            && match signal {
                Signal::Kill => true,
                Signal::Term => !inner.ignores_term.contains(&pid),
            };
        if dies {
            inner.processes.retain(|p| p.pid != pid);
            for child in &mut inner.processes {
                if child.ppid == pid {
                    child.ppid = 1;
                }
            }
        }
        Ok(())
    }

    fn own_pid(&self) -> i32 {
        self.own_pid
    }

    fn now_micros(&self) -> u64 {
        self.now_micros
    }
}

/// A convenient fabricated process for tests.
pub fn fake_process(pid: i32, ppid: i32, command: &str, cwd: Option<&str>) -> LiveProcess {
    LiveProcess {
        pid,
        ppid,
        pgid: pid,
        start_micros: 999_000_000_000_000,
        command: command.to_owned(),
        argv: vec![command.to_owned()],
        cwd: cwd.map(PathBuf::from),
        has_controlling_terminal: false,
    }
}
