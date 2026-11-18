//! `bossctl dispatch quarantine status|recheck|clear`: inspect and release
//! the hold on local workers the engine could not verify.
//!
//! The engine holds a non-terminal execution whose latest local run has no
//! complete tmux identity and whose process is not proven dead, and
//! dispatches nothing else for that execution's work item. It lifts the hold
//! itself when the execution goes terminal or the process is proven gone.
//! These verbs show what is held and why, and release a hold the engine
//! cannot lift, without editing the database by hand.
//!
//! All three read and write `state.db` directly, so they work while the
//! engine is down or wedged. Dispatch reads the hold on every decision, so a
//! release takes effect on the next one with no engine restart.

use std::path::PathBuf;

use anyhow::{Result, bail};
use boss_engine::local_worker_quarantine::{
    HeldLocalWorker, LocalWorkerHoldStatus, local_worker_hold_status, reevaluate_local_worker_holds,
    release_local_worker_holds,
};

use super::{open_state_db, open_state_db_read_only};

#[derive(clap::Subcommand, Debug)]
pub(crate) enum QuarantineAction {
    /// Show what is held: each execution, the work item it holds, the
    /// evidence keeping it held, and what releases it. Read-only.
    Status {
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Re-judge every hold now and release the ones whose cause is gone (the
    /// execution is terminal, its process has exited, or the run inventory is
    /// readable again). This is the check the engine already runs on every
    /// dispatch decision; run it by hand when the engine is down, or to see
    /// the result immediately.
    Recheck {
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Release holds by hand, for a hold the engine cannot lift on its own (a
    /// worker with no recorded pid can never be proven gone). Removes the hold
    /// only: the execution is left as it is, and one that is still
    /// non-terminal and unverifiable is held again at the next engine start.
    /// Cancel the execution to settle it for good. Refuses a hold whose
    /// recorded process is alive: stop that process, or cancel the execution.
    /// Without `--confirm` this prints what would be released and changes
    /// nothing.
    Clear {
        /// Release the hold on this execution. Repeatable.
        #[arg(long = "execution", value_name = "EXECUTION_ID", required_unless_present = "all")]
        executions: Vec<String>,
        /// Release every hold. When the run inventory is unreadable, re-read
        /// it and lift the pause only if that read succeeds; remaining
        /// classified holds stay.
        #[arg(long, conflicts_with = "executions")]
        all: bool,
        /// Actually release. Check first that no worker from before tmux
        /// hosting is still running for the work item: releasing the hold
        /// lets the engine dispatch a second worker for it.
        #[arg(long)]
        confirm: bool,
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

pub(crate) fn run(json: bool, action: QuarantineAction) -> Result<()> {
    match action {
        QuarantineAction::Status { state_root } => {
            let status = local_worker_hold_status(&open_state_db_read_only(state_root)?)?;
            print_status(json, &status)
        }
        QuarantineAction::Recheck { state_root } => {
            let db = open_state_db(state_root)?;
            let reevaluation = reevaluate_local_worker_holds(&db)?;
            let status = local_worker_hold_status(&db)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "released": released_json(&reevaluation.released),
                        "status": status_json(&status),
                    }))?
                );
                return Ok(());
            }
            print_released(&reevaluation.released);
            print_status(false, &status)
        }
        QuarantineAction::Clear {
            executions,
            all,
            confirm,
            state_root,
        } => clear(json, state_root, executions, all, confirm),
    }
}

fn clear(json: bool, state_root: Option<PathBuf>, executions: Vec<String>, all: bool, confirm: bool) -> Result<()> {
    let db = open_state_db(state_root)?;
    let before = local_worker_hold_status(&db)?;
    let targets: Vec<&HeldLocalWorker> = before
        .held
        .iter()
        .filter(|held| all || executions.contains(&held.execution_id))
        .collect();
    let not_held: Vec<&String> = executions
        .iter()
        .filter(|id| !before.held.iter().any(|held| &held.execution_id == *id))
        .collect();
    if !not_held.is_empty() {
        bail!(
            "not held, so there is nothing to release: {}. `bossctl dispatch quarantine status` lists what is held",
            not_held.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    // A hold whose recorded process exists is doing its job. Releasing it by
    // hand is how a second worker ends up on the same work item, so that is
    // refused outright; the two safe ways out are named instead.
    let alive: Vec<String> = targets
        .iter()
        .filter(|held| held.process.is_alive())
        .map(|held| format!("{} (pid {})", held.execution_id, held.shell_pid().unwrap_or_default()))
        .collect();
    if !alive.is_empty() {
        bail!(
            "refusing to release a hold whose worker process is alive: {}. Nothing was released. If the pid is \
             the old worker, stop it and run `bossctl dispatch quarantine recheck`. If the pid now belongs to \
             something else, cancel the execution with `bossctl work cancel <EXECUTION_ID>`; a terminal \
             execution holds nothing",
            alive.join(", ")
        );
    }
    let inventory_unreadable = all && before.scan_error.is_some();
    if targets.is_empty() && !inventory_unreadable {
        if json {
            println!("{}", serde_json::json!({ "released": [], "confirmed": confirm }));
        } else {
            println!("nothing is held; nothing to release");
        }
        return Ok(());
    }
    if !confirm {
        println!("would release (nothing has been changed):");
        for held in &targets {
            println!("  {}", held_line(held));
        }
        if inventory_unreadable {
            println!(
                "  the pause on all local dispatch left by an unreadable run inventory \
                 (re-read on --confirm; the pause lifts only if that read succeeds)"
            );
        }
        bail!(
            "not released: re-run with --confirm. When holds are listed above, check that no worker from before \
             tmux hosting is still running for those work items. When the run inventory is unreadable, \
             --confirm re-reads it and lifts the pause only if that read succeeds; remaining classified holds stay"
        );
    }
    if inventory_unreadable {
        let reevaluation = reevaluate_local_worker_holds(&db)?;
        let after = local_worker_hold_status(&db)?;
        if let Some(error) = after.scan_error.as_deref() {
            bail!(
                "refusing to lift the pause: the run inventory is still unreadable ({error}). Nothing was \
                 released. Retry once the inventory can be read, or inspect with `bossctl dispatch quarantine status`"
            );
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "released": released_json(&reevaluation.released),
                    "local_dispatch_pause_lifted": true,
                    "confirmed": true,
                }))?
            );
            return Ok(());
        }
        print_released(&reevaluation.released);
        println!("lifted the pause on all local dispatch");
        if !after.held.is_empty() {
            println!(
                "remaining classified holds stay until each execution is terminal, its process is proven gone, \
                 or a named clear releases it"
            );
        }
        return Ok(());
    }
    let released = if all {
        release_local_worker_holds(&db, None)?
    } else {
        release_local_worker_holds(&db, Some(&executions))?
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "released": released_json(&released),
                "local_dispatch_pause_lifted": false,
                "confirmed": true,
            }))?
        );
        return Ok(());
    }
    print_released(&released);
    println!(
        "the executions themselves are unchanged. The engine's liveness sweeps reap one whose worker is gone; \
         one that is still non-terminal and unverifiable is held again at the next engine start. Cancel it to \
         settle it for good."
    );
    Ok(())
}

fn print_released(released: &std::collections::BTreeMap<String, String>) {
    if released.is_empty() {
        println!("released: nothing");
        return;
    }
    println!("released:");
    for (execution_id, work_item_id) in released {
        println!("  {execution_id} (work item {work_item_id})");
    }
}

fn released_json(released: &std::collections::BTreeMap<String, String>) -> serde_json::Value {
    released
        .iter()
        .map(|(execution_id, work_item_id)| serde_json::json!({ "execution_id": execution_id, "work_item_id": work_item_id }))
        .collect()
}

fn print_status(json: bool, status: &LocalWorkerHoldStatus) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&status_json(status))?);
    } else {
        for line in status_lines(status) {
            println!("{line}");
        }
    }
    Ok(())
}

fn status_json(status: &LocalWorkerHoldStatus) -> serde_json::Value {
    serde_json::json!({
        "local_dispatch_paused": status.scan_error.is_some(),
        "scan_error": status.scan_error,
        "held": status
            .held
            .iter()
            .map(|held| serde_json::json!({
                "execution_id": held.execution_id,
                "work_item_id": held.work_item_id,
                "execution_status": held.execution_status,
                "evidence": held.evidence(),
                "shell_pid": held.shell_pid(),
                "releases_when": releases_when(held),
            }))
            .collect::<Vec<_>>(),
    })
}

fn status_lines(status: &LocalWorkerHoldStatus) -> Vec<String> {
    let mut lines = Vec::new();
    match &status.scan_error {
        Some(error) => {
            lines.push(format!(
                "local dispatch: PAUSED. The engine could not read its local run inventory: {error}"
            ));
            lines.push(
                "  releases when: a read succeeds. The engine retries on every dispatch decision; \
                 `bossctl dispatch quarantine recheck` retries now."
                    .to_owned(),
            );
        }
        None => lines.push("local dispatch: not paused".to_owned()),
    }
    if status.held.is_empty() {
        lines.push("held executions: none".to_owned());
        return lines;
    }
    lines.push(format!(
        "held executions: {} (each holds only its own work item; other work dispatches normally)",
        status.held.len()
    ));
    for held in &status.held {
        lines.push(format!("  {}", held_line(held)));
        lines.push(format!("    releases when: {}", releases_when(held)));
    }
    lines.push(
        "release by hand, after checking no such worker is running: \
         `bossctl dispatch quarantine clear --execution <EXECUTION_ID> --confirm`"
            .to_owned(),
    );
    lines
}

fn held_line(held: &HeldLocalWorker) -> String {
    let pid = held.shell_pid().map(|pid| format!(" pid {pid}")).unwrap_or_default();
    format!(
        "{}  work item {}  status {}  evidence {}{pid}",
        held.execution_id,
        held.work_item_id,
        held.execution_status.as_deref().unwrap_or("(row missing)"),
        held.evidence(),
    )
}

/// What lifts this hold without a manual clear. Depends on the evidence:
/// a recorded pid can be proven gone, a missing one never can.
fn releases_when(held: &HeldLocalWorker) -> String {
    let terminal = "the execution is cancelled or reaches any other terminal state";
    match held.process.alive_pid() {
        Some(pid) => format!(
            "pid {pid} exits (checked on every dispatch decision), or {terminal}. If pid {pid} is not a Boss \
             worker, the number was recycled and only cancelling the execution lifts the hold"
        ),
        None => format!(
            "{terminal}. No live pid is recorded for it, so its process cannot be proven gone short of a reboot"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use boss_engine::durable_liveness::WorkerProcess;
    use boss_engine::local_worker_quarantine::quarantine_historical_local_workers;
    use boss_engine::work::{CreateChoreInput, CreateProductInput, WorkDb};
    use boss_protocol::RequestExecutionInput;
    use clap::Parser;

    use super::*;
    use crate::{Cli, Command, DispatchAction};

    /// This crate has no `tempfile` dev-dependency; see `proposals.rs`.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("bossctl-quarantine-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn state_root(&self) -> Option<PathBuf> {
            Some(self.0.clone())
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A running local execution with no tmux hosting stamp, no identity and
    /// no pid: a worker the engine can never verify. Returns
    /// `(execution_id, work_item_id)`.
    fn seed_unverifiable_worker(db: &WorkDb, name: &str) -> (String, String) {
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name(format!("Quarantine {name}"))
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput::builder().product_id(product.id).name(name).build())
            .unwrap();
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "repo",
            "lease",
            "workspace",
            "/tmp/workspace",
        )
        .unwrap();
        (execution.id, chore.id)
    }

    fn held_db(scratch: &ScratchDir) -> (WorkDb, Vec<(String, String)>) {
        let db = WorkDb::open(scratch.0.join("state.db")).unwrap();
        let held = vec![
            seed_unverifiable_worker(&db, "first"),
            seed_unverifiable_worker(&db, "second"),
        ];
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert_eq!(report.protected_execution_ids.len(), 2);
        (db, held)
    }

    fn clear_action(executions: &[&str], all: bool, confirm: bool, scratch: &ScratchDir) -> QuarantineAction {
        QuarantineAction::Clear {
            executions: executions.iter().map(|id| (*id).to_owned()).collect(),
            all,
            confirm,
            state_root: scratch.state_root(),
        }
    }

    #[test]
    fn the_verbs_parse() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).map(|cli| cli.command) {
            Ok(Command::Dispatch {
                action: DispatchAction::Quarantine { action },
            }) => Ok(action),
            Ok(other) => panic!("parsed as {other:?}"),
            Err(err) => Err(err.to_string()),
        };
        assert!(matches!(
            parse(&["bossctl", "dispatch", "quarantine", "status"]),
            Ok(QuarantineAction::Status { state_root: None })
        ));
        assert!(matches!(
            parse(&["bossctl", "dispatch", "quarantine", "recheck"]),
            Ok(QuarantineAction::Recheck { state_root: None })
        ));
        match parse(&[
            "bossctl",
            "dispatch",
            "quarantine",
            "clear",
            "--execution",
            "exec_a",
            "--execution",
            "exec_b",
            "--confirm",
        ]) {
            Ok(QuarantineAction::Clear {
                executions,
                all: false,
                confirm: true,
                ..
            }) => assert_eq!(executions, ["exec_a", "exec_b"]),
            other => panic!("unexpected parse: {other:?}"),
        }
        assert!(matches!(
            parse(&["bossctl", "dispatch", "quarantine", "clear", "--all"]),
            Ok(QuarantineAction::Clear {
                all: true,
                confirm: false,
                ..
            })
        ));
        // A clear has to say what it clears, and "everything" excludes a list.
        assert!(parse(&["bossctl", "dispatch", "quarantine", "clear", "--confirm"]).is_err());
        assert!(
            parse(&[
                "bossctl",
                "dispatch",
                "quarantine",
                "clear",
                "--all",
                "--execution",
                "exec_a"
            ])
            .is_err()
        );
    }

    #[test]
    fn status_names_each_held_execution_and_what_releases_it() {
        let scratch = ScratchDir::new("status");
        let (db, held) = held_db(&scratch);

        let status = local_worker_hold_status(&db).unwrap();
        let text = status_lines(&status).join("\n");
        assert!(text.contains("local dispatch: not paused"), "{text}");
        assert!(text.contains("each holds only its own work item"), "{text}");
        for (execution_id, work_item_id) in &held {
            assert!(text.contains(execution_id), "{text}");
            assert!(text.contains(work_item_id), "{text}");
        }
        assert!(text.contains("evidence process_unknown"), "{text}");
        assert!(text.contains("releases when: the execution is cancelled"), "{text}");
        assert!(text.contains("clear --execution"), "{text}");

        let json = status_json(&status);
        assert_eq!(json["local_dispatch_paused"], false);
        assert_eq!(json["held"].as_array().unwrap().len(), 2);
        assert_eq!(json["held"][0]["evidence"], "process_unknown");
        assert_eq!(json["held"][0]["execution_status"], "running");

        // The read-only verb runs against the same file.
        run(
            false,
            QuarantineAction::Status {
                state_root: scratch.state_root(),
            },
        )
        .unwrap();
    }

    #[test]
    fn a_live_pid_is_reported_with_the_exit_that_releases_it() {
        let held = HeldLocalWorker {
            execution_id: "exec_live".to_owned(),
            work_item_id: "task_live".to_owned(),
            execution_status: Some("running".to_owned()),
            process: WorkerProcess::Alive { shell_pid: 4242 },
        };
        assert!(held_line(&held).contains("evidence process_alive pid 4242"));
        assert!(releases_when(&held).starts_with("pid 4242 exits"));
    }

    #[test]
    fn an_unreadable_inventory_is_reported_as_the_one_thing_pausing_local_dispatch() {
        let status = LocalWorkerHoldStatus {
            scan_error: Some("no such column: r.tmux_session_name".to_owned()),
            held: Vec::new(),
        };
        let text = status_lines(&status).join("\n");
        assert!(text.contains("local dispatch: PAUSED"), "{text}");
        assert!(text.contains("no such column: r.tmux_session_name"), "{text}");
        assert!(text.contains("held executions: none"), "{text}");
        assert_eq!(status_json(&status)["local_dispatch_paused"], true);
    }

    #[test]
    fn clear_changes_nothing_until_it_is_confirmed() {
        let scratch = ScratchDir::new("clear-unconfirmed");
        let (db, held) = held_db(&scratch);
        let (first, first_item) = &held[0];

        let refusal = run(false, clear_action(&[first], false, false, &scratch))
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("--confirm"), "{refusal}");
        assert!(db.is_execution_quarantined(first).unwrap());
        assert!(db.ensure_work_item_not_quarantined(first_item).is_err());
        assert_eq!(db.read_local_worker_quarantine().unwrap().executions.len(), 2);
    }

    #[test]
    fn a_confirmed_clear_releases_only_the_named_execution() {
        let scratch = ScratchDir::new("clear-confirmed");
        let (db, held) = held_db(&scratch);
        let (first, first_item) = &held[0];
        let (second, second_item) = &held[1];

        run(false, clear_action(&[first], false, true, &scratch)).unwrap();

        // Dispatch reads the record on every decision: the release is visible
        // through the engine's own handle with no restart and no rescan.
        db.ensure_work_item_not_quarantined(first_item).unwrap();
        assert!(!db.is_execution_quarantined(first).unwrap());
        assert!(db.is_execution_quarantined(second).unwrap());
        assert_eq!(
            db.read_local_worker_quarantine().unwrap().executions,
            BTreeMap::from([(second.clone(), second_item.clone())])
        );
    }

    #[test]
    fn clear_refuses_to_release_a_hold_whose_process_is_alive() {
        let scratch = ScratchDir::new("clear-live");
        let (db, held) = held_db(&scratch);
        let (live, live_item) = &held[0];
        let (unknown, _) = &held[1];
        // This test process stands in for a worker that is demonstrably running.
        let live_pid = i64::from(std::process::id());
        db.set_run_shell_pid_for_execution(live, live_pid).unwrap();

        for action in [
            clear_action(&[live], false, true, &scratch),
            clear_action(&[], true, true, &scratch),
        ] {
            let refusal = run(false, action).unwrap_err().to_string();
            assert!(refusal.contains("worker process is alive"), "{refusal}");
            assert!(refusal.contains(live), "{refusal}");
            assert!(refusal.contains(&format!("pid {live_pid}")), "{refusal}");
            assert!(refusal.contains("bossctl work cancel"), "{refusal}");
            // Nothing was released, the other hold included.
            assert_eq!(db.read_local_worker_quarantine().unwrap().executions.len(), 2);
        }
        assert!(db.ensure_work_item_not_quarantined(live_item).is_err());

        // The hold next to it can still be released by name, and cancelling
        // the live worker's execution releases its work item with no clear.
        run(false, clear_action(&[unknown], false, true, &scratch)).unwrap();
        assert!(db.is_execution_quarantined(live).unwrap());
        db.cancel_execution(live).unwrap();
        db.ensure_work_item_not_quarantined(live_item).unwrap();
    }

    #[test]
    fn clear_refuses_an_execution_that_is_not_held() {
        let scratch = ScratchDir::new("clear-not-held");
        let (db, _held) = held_db(&scratch);
        let refusal = run(false, clear_action(&["exec_not_held"], false, true, &scratch))
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("exec_not_held"), "{refusal}");
        assert_eq!(db.read_local_worker_quarantine().unwrap().executions.len(), 2);
    }

    #[test]
    fn clear_all_refuses_to_lift_an_unreadable_inventory_until_a_read_succeeds() {
        let scratch = ScratchDir::new("clear-all-unreadable");
        let (db, _held) = held_db(&scratch);
        let mut recorded = db.read_local_worker_quarantine().unwrap();
        recorded.scan_error = Some("database is locked".to_owned());
        db.write_local_worker_quarantine(&recorded).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "ALTER TABLE work_runs RENAME COLUMN tmux_session_name TO unavailable_identity",
                [],
            )
            .unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());

        let dry_run = run(false, clear_action(&[], true, false, &scratch))
            .unwrap_err()
            .to_string();
        assert!(dry_run.contains("re-reads it"), "{dry_run}");
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());

        let refusal = run(false, clear_action(&[], true, true, &scratch))
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("still unreadable"), "{refusal}");
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        assert!(!db.read_local_worker_quarantine().unwrap().executions.is_empty());
    }

    #[test]
    fn clear_all_lifts_an_unreadable_inventory_pause_only_after_a_successful_recheck() {
        let scratch = ScratchDir::new("clear-all-recheck");
        let (db, held) = held_db(&scratch);
        let mut recorded = db.read_local_worker_quarantine().unwrap();
        recorded.scan_error = Some("database is locked".to_owned());
        db.write_local_worker_quarantine(&recorded).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());

        run(true, clear_action(&[], true, true, &scratch)).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        // A successful re-read classifies the same unverifiable workers again;
        // those holds stay. The pause is what lifted.
        assert_eq!(db.read_local_worker_quarantine().unwrap().executions.len(), held.len());
        for (_, work_item_id) in &held {
            assert!(db.ensure_work_item_not_quarantined(work_item_id).is_err());
        }
    }

    #[test]
    fn recheck_releases_a_cancelled_execution_and_reports_it() {
        let scratch = ScratchDir::new("recheck");
        let (db, held) = held_db(&scratch);
        let (first, first_item) = &held[0];
        db.cancel_execution(first).unwrap();

        run(
            true,
            QuarantineAction::Recheck {
                state_root: scratch.state_root(),
            },
        )
        .unwrap();

        let recorded = db.read_local_worker_quarantine().unwrap();
        assert!(!recorded.executions.contains_key(first));
        assert_eq!(recorded.executions.len(), 1);
        db.ensure_work_item_not_quarantined(first_item).unwrap();
    }
}
