//! Hold on local workers the engine cannot verify.
//!
//! A non-terminal execution whose latest local run has no complete tmux
//! identity is one the engine cannot adopt, signal or tear down. What it does
//! about that depends on what the run's durable columns prove:
//!
//! - **The spawn never reached tmux.** Every local run is stamped
//!   `tmux_hosted` in the transaction that creates it, and the spawn records
//!   the session identity before tmux receives any command. A stamped run
//!   with no identity and no pid stopped between those two writes because
//!   the engine process spawning it went away (a restart, a crash, an app
//!   update). No worker process ever existed. Startup fails that execution
//!   with [`SPAWN_NEVER_REACHED_TMUX_REASON`] and the work item dispatches
//!   again; nothing is held. Startup can act on this because it runs under
//!   the engine's instance lock: any such run it finds was left by a process
//!   that no longer exists, so nothing can still be about to write its
//!   identity. While the engine is running the same shape is a spawn in
//!   flight, which is why only startup fails it.
//! - **Its process is proven dead.** The durable pid probe says so, or the
//!   run's last durable write predates the current kernel boot (by more than
//!   [`PRE_BOOT_MARGIN_SECS`]): no process survives a reboot. Orphan recovery
//!   proceeds; nothing is held.
//! - **Its process is alive or unknown.** It may be a worker from before tmux
//!   hosting that is still running where the engine cannot see it. The
//!   execution is held: it is not orphaned, re-driven or redispatched over,
//!   and nothing else is dispatched for *its work item*. Every other work
//!   item is unaffected.
//!
//! A hold is released by any of: the execution reaching a terminal status
//! (the storage readers check this on every dispatch decision, so a cancel
//! takes effect at once), the process being proven gone
//! ([`reevaluate_local_worker_holds`], also run on every dispatch decision),
//! or a manual `bossctl dispatch quarantine clear`. None needs an engine
//! restart.
//!
//! Only a run inventory that cannot be read at all pauses local dispatch as a
//! whole, because the engine then cannot name which work items are affected.
//! That read is retried on every dispatch decision too.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};

use crate::durable_liveness::{WorkerProcess, probe_execution_worker};
use crate::work::{LocalWorkerQuarantine, UnidentifiedLocalRun, WorkDb};

pub(crate) use boss_engine_storage_values::attention_kinds::LOCAL_WORKER_STARTUP_QUARANTINE_ATTENTION_KIND as ATTENTION_KIND;

/// How far before the kernel boot a run's last durable write must fall before
/// the boot alone proves its worker dead. The margin absorbs a wall clock that
/// was behind when the row was written and corrected after boot (the kernel
/// re-anchors its boot time on clock corrections, the row does not move).
/// The failure direction of a margin that is too small is a duplicate worker,
/// so it is generous; a row inside the margin simply stays held until the
/// pid probe or a later boot can prove it.
pub(crate) const PRE_BOOT_MARGIN_SECS: i64 = 3600;

/// Bounded retries when a concurrent writer changed the hold record between
/// the read and the conditional write.
const HOLD_RECORD_CAS_ATTEMPTS: usize = 8;

/// Recorded on an execution whose spawn stopped before it asked tmux for a
/// session. Names the cause so the failure is not mistaken for a worker that
/// ran and died.
pub const SPAWN_NEVER_REACHED_TMUX_REASON: &str = "engine startup: worker spawn never reached tmux. The engine \
     process that created this run stopped (restart, crash or app update) before it asked tmux for a session, \
     so no worker process ever started and no work was lost";

#[derive(Default)]
pub struct StartupQuarantineReport {
    pub protected_execution_ids: HashSet<String>,
    pub dead_execution_ids: HashSet<String>,
    /// Executions whose spawn never reached tmux. Startup fails each with
    /// [`SPAWN_NEVER_REACHED_TMUX_REASON`]; they are never held.
    pub never_started_execution_ids: HashSet<String>,
    pub scan_failed: bool,
}

/// What a re-evaluation of the existing hold changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HoldReevaluation {
    /// Executions (and their work items) the hold no longer covers.
    pub released: BTreeMap<String, String>,
    /// What still holds after the re-evaluation.
    pub held: LocalWorkerQuarantine,
}

/// One held execution and the evidence keeping it held, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLocalWorker {
    pub execution_id: String,
    pub work_item_id: String,
    /// The execution's current status; `None` when its row no longer exists.
    pub execution_status: Option<String>,
    /// What the durable pid probe says about its worker process right now.
    pub process: WorkerProcess,
}

impl HeldLocalWorker {
    /// Stable evidence identifier (`process_alive`, `process_unknown`, ...).
    pub fn evidence(&self) -> &'static str {
        self.process.reason()
    }

    /// The pid that evidence is about, when one was recorded.
    pub fn shell_pid(&self) -> Option<i32> {
        self.process.shell_pid()
    }
}

/// The hold as it needs to be shown.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LocalWorkerHoldStatus {
    /// Set when the run inventory could not be read, which pauses all local dispatch.
    pub scan_error: Option<String>,
    pub held: Vec<HeldLocalWorker>,
}

/// Which unidentified runs a pass may judge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// Engine startup. Every unidentified run was left by an earlier engine
    /// process, so all of them are judged.
    Startup,
    /// While the engine is dispatching. Only executions already held are
    /// re-judged: an unidentified run that is not held is a spawn this
    /// process has in flight, which the spawn itself completes or fails.
    Running,
}

/// Why an unidentified run is, or is not, held.
enum Verdict {
    NeverStarted,
    Dead(&'static str),
    Held(&'static str),
}

/// Establish the hold at engine startup, before any recovery runs.
pub fn quarantine_historical_local_workers(db: &WorkDb) -> Result<StartupQuarantineReport> {
    let boot = kernel_boot_epoch_secs();
    if boot.is_none() {
        tracing::warn!("kernel boot time is unavailable; pre-boot historical runs cannot be proven dead this startup");
    }
    quarantine_with_probe(db, |id| probe_execution_worker(db, id), boot)
}

/// Re-judge the existing hold and release whatever no longer needs it.
///
/// Dispatch calls this before every dispatch decision (admission, host
/// selection, and the active-dispatch rescan), so a hold never outlives its
/// cause by more than one decision. It never adds a new execution to a hold
/// that a successful scan established, and it does nothing at all, beyond
/// one metadata read, while nothing is held.
pub fn reevaluate_local_worker_holds(db: &WorkDb) -> Result<HoldReevaluation> {
    reevaluate_with_probe(db, |id| probe_execution_worker(db, id), kernel_boot_epoch_secs())
}

fn reevaluate_with_probe(
    db: &WorkDb,
    probe: impl Fn(&str) -> WorkerProcess,
    boot_epoch_secs: Option<i64>,
) -> Result<HoldReevaluation> {
    let previous = db.read_local_worker_quarantine()?;
    if previous.is_empty() {
        return Ok(HoldReevaluation::default());
    }
    evaluate(db, &previous, probe, boot_epoch_secs, Scope::Running)?;
    let held = db.read_local_worker_quarantine()?;
    let released = previous
        .executions
        .into_iter()
        .filter(|(execution_id, _)| !held.executions.contains_key(execution_id))
        .collect();
    Ok(HoldReevaluation { released, held })
}

/// The current hold with the evidence behind each entry. Read-only.
pub fn local_worker_hold_status(db: &WorkDb) -> Result<LocalWorkerHoldStatus> {
    let quarantine = db.held_local_workers()?;
    let held = quarantine
        .executions
        .into_iter()
        .map(|(execution_id, work_item_id)| HeldLocalWorker {
            execution_status: db
                .get_execution(&execution_id)
                .ok()
                .map(|execution| execution.status.as_str().to_owned()),
            process: probe_execution_worker(db, &execution_id),
            execution_id,
            work_item_id,
        })
        .collect();
    Ok(LocalWorkerHoldStatus {
        scan_error: quarantine.scan_error,
        held,
    })
}

/// Release the selected holds explicitly. `execution_ids` of `None` releases
/// every held execution and leaves an unreadable-inventory pause in place:
/// that pause lifts only after a successful inventory read.
///
/// This removes the hold only. The executions themselves are left as they
/// are: the engine's liveness sweeps reap one whose worker is gone, and an
/// execution that is still non-terminal and unverifiable at the next engine
/// start is held again. Cancel the execution to settle it for good.
pub fn release_local_worker_holds(db: &WorkDb, execution_ids: Option<&[String]>) -> Result<BTreeMap<String, String>> {
    for _ in 0..HOLD_RECORD_CAS_ATTEMPTS {
        let previous = db.read_local_worker_quarantine()?;
        let mut remaining = previous.clone();
        match execution_ids {
            Some(ids) => remaining
                .executions
                .retain(|execution_id, _| !ids.contains(execution_id)),
            None => remaining.executions.clear(),
        }
        if remaining == previous {
            return Ok(BTreeMap::new());
        }
        if !db.replace_local_worker_quarantine(&previous, &remaining)? {
            continue;
        }
        let released: BTreeMap<String, String> = previous
            .executions
            .iter()
            .filter(|(execution_id, _)| !remaining.executions.contains_key(*execution_id))
            .map(|(execution_id, work_item_id)| (execution_id.clone(), work_item_id.clone()))
            .collect();
        for (execution_id, work_item_id) in &released {
            tracing::warn!(
                execution_id,
                work_item_id,
                "released the hold on an unverified local worker"
            );
        }
        resolve_released_attention(db, &previous, &remaining);
        return Ok(released);
    }
    bail!("could not release the hold; the record changed concurrently. Retry the clear")
}

/// Epoch seconds at which the running kernel booted, or `None` when the
/// platform cannot say. `None` is never treated as evidence.
pub(crate) fn kernel_boot_epoch_secs() -> Option<i64> {
    kernel_boot_epoch_secs_impl()
}

#[cfg(target_os = "macos")]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    let mut boottime = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>();
    let name = c"kern.boottime";
    // SAFETY: `boottime` is a valid, writable timeval of exactly `len` bytes,
    // and sysctl writes at most `len` bytes into it.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boottime as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != std::mem::size_of::<libc::timeval>() || boottime.tv_sec <= 0 {
        return None;
    }
    Some(boottime.tv_sec)
}

#[cfg(target_os = "linux")]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    None
}

/// Whether a run whose last durable write was at `last_write_epoch` is proven
/// dead by a kernel that booted at `boot_epoch`.
fn run_predates_boot(last_write_epoch: Option<i64>, boot_epoch: Option<i64>) -> bool {
    match (last_write_epoch, boot_epoch) {
        (Some(last_write), Some(boot)) => last_write.saturating_add(PRE_BOOT_MARGIN_SECS) < boot,
        _ => false,
    }
}

fn quarantine_with_probe(
    db: &WorkDb,
    probe: impl Fn(&str) -> WorkerProcess,
    boot_epoch_secs: Option<i64>,
) -> Result<StartupQuarantineReport> {
    let previous = db.read_local_worker_quarantine()?;
    evaluate(db, &previous, probe, boot_epoch_secs, Scope::Startup)
}

fn classify(
    db: &WorkDb,
    run: &UnidentifiedLocalRun,
    probe: &impl Fn(&str) -> WorkerProcess,
    boot_epoch_secs: Option<i64>,
) -> Verdict {
    if run.never_reached_tmux {
        return Verdict::NeverStarted;
    }
    // A run written entirely before the kernel booted has no process left to
    // probe: whatever pid it recorded (or did not) belongs to a previous
    // boot. This outranks the pid probe, which after a reboot can only
    // report pid reuse.
    let last_write = match db.latest_local_run_last_write_epoch(&run.execution_id) {
        Ok(last_write) => last_write,
        Err(err) => {
            tracing::warn!(
                execution_id = %run.execution_id,
                error = %format!("{err:#}"),
                "failed to read the run's last write time; treating boot evidence as unknown"
            );
            None
        }
    };
    if run_predates_boot(last_write, boot_epoch_secs) {
        return Verdict::Dead("run_predates_kernel_boot");
    }
    let process = probe(&run.execution_id);
    if matches!(process, WorkerProcess::Gone { .. }) {
        Verdict::Dead(process.reason())
    } else {
        Verdict::Held(process.reason())
    }
}

fn evaluate(
    db: &WorkDb,
    previous: &LocalWorkerQuarantine,
    probe: impl Fn(&str) -> WorkerProcess,
    boot_epoch_secs: Option<i64>,
    scope: Scope,
) -> Result<StartupQuarantineReport> {
    let mut quarantine = LocalWorkerQuarantine::default();
    let mut report = StartupQuarantineReport::default();
    // After an unreadable inventory nothing was judged, so a later pass has
    // to look at every run, exactly as startup would have.
    let judge_all = scope == Scope::Startup || previous.scan_error.is_some();
    match db.local_workers_without_tmux_identity() {
        Ok(runs) => {
            // The hold is recomputed from what is true now. An execution that
            // was held and is no longer listed here has gone terminal or
            // gained an identity, and either way has nothing left to protect.
            for run in runs {
                let was_held = previous.executions.contains_key(&run.execution_id);
                if !judge_all && !was_held {
                    continue;
                }
                match classify(db, &run, &probe, boot_epoch_secs) {
                    Verdict::NeverStarted => {
                        tracing::warn!(
                            execution_id = %run.execution_id,
                            work_item_id = %run.work_item_id,
                            was_held,
                            "local worker spawn never reached tmux; no process exists, so the execution is not held"
                        );
                        report.never_started_execution_ids.insert(run.execution_id);
                    }
                    Verdict::Dead(evidence) => {
                        tracing::info!(
                            execution_id = %run.execution_id,
                            evidence,
                            "local worker has no tmux identity and its process is proven dead; permitting orphan recovery"
                        );
                        report.dead_execution_ids.insert(run.execution_id);
                    }
                    Verdict::Held(evidence) => {
                        if was_held {
                            tracing::debug!(
                                execution_id = %run.execution_id,
                                evidence,
                                "unverified local worker is still held"
                            );
                        } else {
                            tracing::error!(
                                execution_id = %run.execution_id,
                                work_item_id = %run.work_item_id,
                                evidence,
                                "local worker has no tmux identity and its process is not proven dead; holding \
                                 this execution and its work item (other work is unaffected)"
                            );
                        }
                        report.protected_execution_ids.insert(run.execution_id.clone());
                        quarantine.executions.insert(run.execution_id, run.work_item_id);
                    }
                }
            }
        }
        Err(err) if scope == Scope::Running => {
            // A failed read while dispatching changes nothing: the record a
            // scan already established stands, and the next dispatch decision
            // reads again. Only startup, which has nothing to fall back on,
            // turns an unreadable inventory into a pause.
            tracing::warn!(
                error = %format!("{err:#}"),
                "could not re-read the local run inventory; leaving the existing hold as it is"
            );
            report.scan_failed = true;
            return Ok(report);
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "local run inventory is unreadable; pausing local dispatch until it can be read"
            );
            quarantine.scan_error = Some(format!("{err:#}"));
            quarantine.executions = previous.executions.clone();
            report.scan_failed = true;
        }
    }
    if quarantine != *previous {
        match db.replace_local_worker_quarantine(previous, &quarantine) {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    "local worker hold record changed concurrently; leaving it for the next dispatch decision"
                );
                return Ok(report);
            }
            Err(err) => return Err(err),
        }
    }
    for (execution_id, work_item_id) in &previous.executions {
        if !quarantine.executions.contains_key(execution_id) {
            tracing::info!(
                execution_id,
                work_item_id,
                "released the hold on a local worker: it is terminal, identified, proven dead or never started"
            );
        }
    }
    // The hold above is what dispatch admission reads; the attention items
    // below are the display explanation. Filing them is best-effort:
    // a held execution can belong to a work item that was soft-deleted after
    // the worker spawned, and `upsert_external_tracker_attention` refuses a
    // tombstoned task. That refusal must not take the engine down at startup
    // (the hold is already durable), so it is logged and the pass continues.
    resolve_released_attention(db, previous, &quarantine);
    for (execution_id, work_item_id) in &quarantine.executions {
        // A re-evaluation runs on every dispatch decision; refile only what
        // this pass newly held.
        if scope == Scope::Running && previous.executions.contains_key(execution_id) {
            continue;
        }
        if let Err(err) = db.upsert_external_tracker_attention(
            work_item_id,
            ATTENTION_KIND,
            "Dispatch held for an unverified local worker",
            &format!(
                "Execution `{execution_id}` has no complete tmux identity and its process is alive or unknown. \
                 It may be a worker from before tmux hosting that is still running, so nothing else is \
                 dispatched for this work item; other work is unaffected. The hold lifts when the execution is \
                 cancelled or reaches any other terminal state, or when its process is proven gone. \
                 `bossctl dispatch quarantine status` shows the evidence, and `bossctl dispatch quarantine \
                 clear --execution {execution_id} --confirm` releases it by hand once you have checked that \
                 no such worker is running. A missing workspace lease does not prove death."
            ),
        ) {
            tracing::error!(
                execution_id,
                work_item_id,
                error = %format!("{err:#}"),
                "failed to file the unverified local worker attention item; the hold itself is already durable"
            );
        }
    }
    Ok(report)
}

/// Resolve the attention item of every work item `previous` held and `now`
/// does not.
fn resolve_released_attention(db: &WorkDb, previous: &LocalWorkerQuarantine, now: &LocalWorkerQuarantine) {
    for work_item_id in previous.executions.values().collect::<HashSet<_>>() {
        let still_held = now.scan_error.is_some() || now.executions.values().any(|id| id == work_item_id);
        if still_held {
            continue;
        }
        if let Err(err) = db.resolve_external_tracker_attention(work_item_id, ATTENTION_KIND) {
            tracing::error!(
                work_item_id,
                error = %format!("{err:#}"),
                "failed to resolve the unverified local worker attention item; the hold itself is already lifted"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_old_execution, create_product};

    /// A running local execution whose run carries no tmux hosting stamp: the
    /// shape a worker from before tmux hosting left behind.
    fn seed(db: &WorkDb, name: &str) -> (String, String) {
        let product = create_product(db);
        let item = create_active_chore(db, &product, name);
        let execution = create_old_execution(db, &item);
        db.start_execution_run(&execution, "worker-1", "repo", "lease", "workspace", "/tmp/workspace")
            .unwrap();
        (execution, item)
    }

    /// A running local execution exactly as dispatch records it before the
    /// spawn reaches tmux: stamped for tmux hosting, no identity, no pid.
    fn seed_spawn_window(db: &WorkDb, name: &str) -> (String, String) {
        let product = create_product(db);
        let item = create_active_chore(db, &product, name);
        let execution = create_old_execution(db, &item);
        db.start_execution_run_on_host_with_tmux_hosting(
            &execution,
            "worker-1",
            "repo",
            "lease",
            "workspace",
            "/tmp/workspace",
            "local",
            true,
        )
        .unwrap();
        (execution, item)
    }

    fn open_attention(db: &WorkDb, item: &str) -> bool {
        db.list_attention_items_for_work_item(item)
            .unwrap()
            .iter()
            .any(|attention| attention.kind == ATTENTION_KIND && attention.status == "open")
    }

    #[test]
    fn only_proven_death_permits_orphaning_and_redispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let db = WorkDb::open(path.clone()).unwrap();
        let (live, live_item) = seed(&db, "live");
        let (unknown, unknown_item) = seed(&db, "unknown");
        let (dead, dead_item) = seed(&db, "dead");
        let report = quarantine_with_probe(
            &db,
            |id| {
                if id == live {
                    WorkerProcess::Alive { shell_pid: 42 }
                } else if id == dead {
                    WorkerProcess::Gone { shell_pid: 43 }
                } else {
                    WorkerProcess::Unknown
                }
            },
            None,
        )
        .unwrap();
        assert_eq!(
            report.protected_execution_ids,
            HashSet::from([live.clone(), unknown.clone()])
        );
        assert_eq!(report.dead_execution_ids, HashSet::from([dead.clone()]));
        // Held workers hold their own work items; local dispatch as a whole
        // keeps running.
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        for (execution, item) in [(&live, &live_item), (&unknown, &unknown_item)] {
            let attentions = db.list_attention_items_for_work_item(item).unwrap();
            assert!(
                attentions
                    .iter()
                    .any(|attention| attention.kind == ATTENTION_KIND && attention.status == "open")
            );
            assert!(db.mark_execution_orphaned(execution, "lease expired").is_err());
            assert!(db.ensure_work_item_not_quarantined(item).is_err());
            assert!(!db.get_execution(execution).unwrap().status.is_terminal());
        }
        db.mark_execution_orphaned(&dead, "process proven gone").unwrap();
        db.ensure_work_item_not_quarantined(&dead_item).unwrap();
        // An engine restart cannot erase the hold before it re-probes.
        let reopened = WorkDb::open(path).unwrap();
        assert!(reopened.is_execution_quarantined(&live).unwrap());
        let drained = quarantine_with_probe(&reopened, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
        assert!(drained.protected_execution_ids.is_empty());
        assert!(reopened.read_local_worker_quarantine().unwrap().is_empty());
        assert!(
            reopened
                .list_attention_items_for_work_item(&live_item)
                .unwrap()
                .iter()
                .all(|attention| attention.kind != ATTENTION_KIND || attention.status == "resolved")
        );
    }

    #[test]
    fn missing_pid_is_unknown_even_when_no_tmux_worker_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "no pid");
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(report.dead_execution_ids.is_empty());
    }

    #[test]
    fn cancelling_a_held_execution_releases_its_work_item_without_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "still live");
        let (other, other_item) = seed(&db, "also held");
        quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, None).unwrap();
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
        assert!(open_attention(&db, &item));

        db.cancel_execution(&execution).unwrap();

        // No scan, no restart: the next read already sees the release.
        db.ensure_work_item_not_quarantined(&item).unwrap();
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        db.request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(&item)
                .build(),
        )
        .expect("a cancelled execution must not keep its work item from dispatching");
        // The other held worker is untouched.
        assert!(db.is_execution_quarantined(&other).unwrap());
        assert!(db.ensure_work_item_not_quarantined(&other_item).is_err());

        // The next dispatch decision's re-evaluation drops the entry from the
        // record and resolves its attention item, even though the probe still
        // cannot prove anything about the process.
        let reevaluation = reevaluate_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert_eq!(
            reevaluation.released,
            BTreeMap::from([(execution.clone(), item.clone())])
        );
        assert_eq!(
            db.read_local_worker_quarantine().unwrap().executions,
            BTreeMap::from([(other, other_item.clone())])
        );
        assert!(!open_attention(&db, &item));
        assert!(open_attention(&db, &other_item));
    }

    #[test]
    fn a_startup_scan_never_holds_an_execution_that_is_already_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "failed while held");
        quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, None).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                [&execution],
            )
            .unwrap();
        // An engine restart: the hold an earlier process recorded names an
        // execution that has since failed. It must not be re-established.
        let report = quarantine_with_probe(&db, |_| panic!("a terminal execution must not be probed"), None).unwrap();
        assert!(report.protected_execution_ids.is_empty());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        assert!(!open_attention(&db, &item));
    }

    #[test]
    fn a_spawn_that_never_reached_tmux_is_failed_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed_spawn_window(&db, "restart mid-spawn");
        let unrelated_item = create_active_chore(&db, &create_product(&db), "unrelated");

        let report = quarantine_with_probe(
            &db,
            |_| panic!("a spawn that never reached tmux has no process to probe"),
            None,
        )
        .unwrap();

        assert_eq!(report.never_started_execution_ids, HashSet::from([execution.clone()]));
        assert!(report.protected_execution_ids.is_empty());
        assert!(report.dead_execution_ids.is_empty());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        db.ensure_work_item_not_quarantined(&unrelated_item).unwrap();
        assert!(!open_attention(&db, &item));
        // Nothing stands between the execution and its loud failure.
        let failed = db
            .mark_execution_orphaned(&execution, SPAWN_NEVER_REACHED_TMUX_REASON)
            .unwrap();
        assert!(failed.status.is_terminal());
        assert_eq!(failed.last_error.as_deref(), Some(SPAWN_NEVER_REACHED_TMUX_REASON));
    }

    #[test]
    fn a_hold_an_earlier_build_recorded_for_a_never_started_spawn_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed_spawn_window(&db, "held by the previous build");
        // The record and attention item exactly as a build without the
        // never-started verdict left them.
        db.write_local_worker_quarantine(&LocalWorkerQuarantine {
            executions: BTreeMap::from([(execution.clone(), item.clone())]),
            scan_error: None,
        })
        .unwrap();
        db.upsert_external_tracker_attention(&item, ATTENTION_KIND, "Local dispatch paused", "old text")
            .unwrap();
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());

        let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();

        assert_eq!(report.never_started_execution_ids, HashSet::from([execution]));
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        assert!(!open_attention(&db, &item));
    }

    #[test]
    fn reevaluation_releases_a_hold_once_its_process_is_proven_gone() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "live then killed");
        quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, None).unwrap();

        let still_alive = reevaluate_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, None).unwrap();
        assert!(still_alive.released.is_empty());
        assert!(still_alive.held.executions.contains_key(&execution));
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());

        // The old worker exits; no engine restart follows.
        let gone = reevaluate_with_probe(&db, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
        assert_eq!(gone.released, BTreeMap::from([(execution.clone(), item.clone())]));
        assert!(gone.held.is_empty());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        assert!(!open_attention(&db, &item));
        db.mark_execution_orphaned(&execution, "process proven gone").unwrap();
    }

    #[test]
    fn reevaluation_never_adds_a_hold_and_costs_nothing_when_none_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        // With nothing held, an unverifiable run is left alone entirely.
        let (unheld, unheld_item) = seed(&db, "appeared after startup");
        let nothing = reevaluate_with_probe(&db, |_| panic!("nothing is held, so nothing is probed"), None).unwrap();
        assert_eq!(nothing, HoldReevaluation::default());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());

        // With something held, only that entry is re-judged.
        let (held, held_item) = seed(&db, "held at startup");
        db.write_local_worker_quarantine(&LocalWorkerQuarantine {
            executions: BTreeMap::from([(held.clone(), held_item.clone())]),
            scan_error: None,
        })
        .unwrap();
        let reevaluation = reevaluate_with_probe(
            &db,
            |id| {
                assert_eq!(id, held, "only the held execution is re-judged");
                WorkerProcess::Unknown
            },
            None,
        )
        .unwrap();
        assert!(reevaluation.released.is_empty());
        assert_eq!(reevaluation.held.executions, BTreeMap::from([(held, held_item)]));
        assert!(!db.is_execution_quarantined(&unheld).unwrap());
        db.ensure_work_item_not_quarantined(&unheld_item).unwrap();
    }

    #[test]
    fn an_unreadable_inventory_is_retried_and_the_pause_clears_itself() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (historical, historical_item) = seed(&db, "unseen by the failed scan");
        let (in_flight, in_flight_item) = seed_spawn_window(&db, "spawning right now");
        let rename = |from: &str, to: &str| {
            db.connect()
                .unwrap()
                .execute(&format!("ALTER TABLE work_runs RENAME COLUMN {from} TO {to}"), [])
                .unwrap();
        };
        rename("tmux_session_name", "unavailable_identity");
        assert!(quarantine_historical_local_workers(&db).unwrap().scan_failed);
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        // Still unreadable: the pause stands.
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        reevaluate_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());

        rename("unavailable_identity", "tmux_session_name");
        let reevaluation = reevaluate_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();

        // Readable again: the pause is gone without a restart, the worker the
        // failed scan never saw is now held by name, and the spawn in flight
        // is neither held nor touched.
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        assert_eq!(
            reevaluation.held.executions,
            BTreeMap::from([(historical, historical_item.clone())])
        );
        assert!(db.ensure_work_item_not_quarantined(&historical_item).is_err());
        db.ensure_work_item_not_quarantined(&in_flight_item).unwrap();
        assert!(!db.get_execution(&in_flight).unwrap().status.is_terminal());
    }

    #[test]
    fn a_failed_reread_while_dispatching_leaves_the_hold_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "held");
        quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        let recorded = db.read_local_worker_quarantine().unwrap();
        db.connect()
            .unwrap()
            .execute(
                "ALTER TABLE work_runs RENAME COLUMN tmux_session_name TO unavailable_identity",
                [],
            )
            .unwrap();

        let reevaluation = reevaluate_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();

        // A read that fails mid-flight must not turn one held work item into
        // a pause on all local dispatch.
        assert!(reevaluation.released.is_empty());
        assert_eq!(db.read_local_worker_quarantine().unwrap(), recorded);
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        assert!(db.is_execution_quarantined(&execution).unwrap());
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
    }

    #[test]
    fn a_manual_release_lifts_the_named_hold_and_reports_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (first, first_item) = seed(&db, "first");
        let (second, second_item) = seed(&db, "second");
        quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();

        let status = local_worker_hold_status(&db).unwrap();
        assert_eq!(status.scan_error, None);
        assert_eq!(
            status
                .held
                .iter()
                .map(|held| (
                    held.execution_id.as_str(),
                    held.evidence(),
                    held.execution_status.as_deref()
                ))
                .collect::<HashSet<_>>(),
            HashSet::from([
                (first.as_str(), "process_unknown", Some("running")),
                (second.as_str(), "process_unknown", Some("running")),
            ])
        );

        let released = release_local_worker_holds(&db, Some(std::slice::from_ref(&first))).unwrap();
        assert_eq!(released, BTreeMap::from([(first.clone(), first_item.clone())]));
        db.ensure_work_item_not_quarantined(&first_item).unwrap();
        assert!(!open_attention(&db, &first_item));
        assert!(db.is_execution_quarantined(&second).unwrap());
        assert!(open_attention(&db, &second_item));
        // Releasing a hold does not decide the execution's fate.
        assert!(!db.get_execution(&first).unwrap().status.is_terminal());

        // Naming something that is not held releases nothing.
        assert!(
            release_local_worker_holds(&db, Some(&["exec_not_held".to_owned()]))
                .unwrap()
                .is_empty()
        );
        let released = release_local_worker_holds(&db, None).unwrap();
        assert_eq!(released, BTreeMap::from([(second, second_item.clone())]));
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
        assert!(!open_attention(&db, &second_item));
    }

    #[test]
    fn releasing_every_execution_leaves_an_unreadable_inventory_pause() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        db.write_local_worker_quarantine(&LocalWorkerQuarantine {
            executions: BTreeMap::from([("exec-1".to_owned(), "item-1".to_owned())]),
            scan_error: Some("database disk image is malformed".to_owned()),
        })
        .unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        release_local_worker_holds(&db, Some(&["exec_any".to_owned()])).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        let released = release_local_worker_holds(&db, None).unwrap();
        assert_eq!(released, BTreeMap::from([("exec-1".to_owned(), "item-1".to_owned())]));
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        assert!(db.read_local_worker_quarantine().unwrap().executions.is_empty());
    }

    #[test]
    fn live_pid_with_partial_identity_stays_quarantined_even_with_tmux_hosted_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "live partial identity");
        db.set_run_shell_pid_for_execution(&execution, i64::from(std::process::id()))
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET tmux_hosted = 1, tmux_session_name = 'historical' WHERE execution_id = ?1",
                [&execution],
            )
            .unwrap();
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.reconcile_active_dispatch(|_| false).unwrap().is_empty());
        assert!(!db.get_execution(&execution).unwrap().status.is_terminal());
    }

    #[test]
    fn tmux_remote_and_terminal_rows_do_not_enter_historical_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (tmux, _) = seed(&db, "tmux");
        db.record_tmux_spawn_intent_for_execution(&tmux, "boss", "boss-worker", "token")
            .unwrap();
        let (terminal, _) = seed(&db, "terminal");
        db.mark_execution_orphaned(&terminal, "already dead").unwrap();
        let (remote, _) = seed(&db, "remote");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO hosts (id, pool_size, created_at) VALUES ('remote', 1, '0')",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE work_runs SET host_id = 'remote' WHERE execution_id = ?1",
                [&remote],
            )
            .unwrap();
        }
        let report = quarantine_with_probe(&db, |_| panic!("excluded rows must never be probed"), None).unwrap();
        assert!(report.protected_execution_ids.is_empty());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
    }

    #[test]
    fn unreadable_historical_inventory_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "ALTER TABLE work_runs RENAME COLUMN tmux_session_name TO unavailable_identity",
                [],
            )
            .unwrap();
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.scan_failed);
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        assert!(db.is_execution_quarantined("unknown-execution").unwrap());
        assert!(db.ensure_work_item_not_quarantined("unknown-item").is_err());
    }

    #[test]
    fn tombstoned_work_item_keeps_its_hold_without_aborting_startup() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "deleted after spawn");
        db.delete_work_item(&item).unwrap();
        // Filing attention against a tombstoned task is refused; the hold
        // must still land and the sweep must still return `Ok`.
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.is_execution_quarantined(&execution).unwrap());
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
        assert_eq!(
            db.read_local_worker_quarantine().unwrap().executions,
            BTreeMap::from([(execution.clone(), item.clone())])
        );
        // The hold is the deleted item's alone; local dispatch is not paused.
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        // ...and proven death still lifts it, again without an attention error
        // surfacing as a startup failure.
        let drained = quarantine_with_probe(&db, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
        assert!(drained.dead_execution_ids.contains(&execution));
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
    }

    #[test]
    fn run_written_before_the_kernel_booted_is_proven_dead() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "pre-boot");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        // Establish the hold first, as a build that lacked boot evidence
        // would have, so this also covers lifting a persisted hold.
        quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(db.is_execution_quarantined(&execution).unwrap());
        // A kernel that booted well after the row's last write outranks a
        // probe that claims the (recycled) pid is alive.
        let boot = now + PRE_BOOT_MARGIN_SECS * 2;
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, Some(boot)).unwrap();
        assert_eq!(report.dead_execution_ids, HashSet::from([execution.clone()]));
        assert!(report.protected_execution_ids.is_empty());
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        assert!(db.read_local_worker_quarantine().unwrap().is_empty());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        db.mark_execution_orphaned(&execution, "run predates kernel boot")
            .unwrap();
    }

    #[test]
    fn run_inside_the_pre_boot_margin_or_without_boot_evidence_stays_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "recent");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        for boot in [None, Some(now + PRE_BOOT_MARGIN_SECS / 2), Some(now - 1)] {
            let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, boot).unwrap();
            assert!(report.protected_execution_ids.contains(&execution), "boot={boot:?}");
            assert!(report.dead_execution_ids.is_empty(), "boot={boot:?}");
            assert!(db.is_execution_quarantined(&execution).unwrap(), "boot={boot:?}");
        }
    }

    #[test]
    fn kernel_boot_time_is_in_the_past_on_supported_platforms() {
        let boot = kernel_boot_epoch_secs();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let boot = boot.expect("boot time readable");
            assert!(boot > 0 && boot <= boss_engine_utils::epoch_time::now_epoch_secs());
        } else {
            assert!(boot.is_none());
        }
    }
}
