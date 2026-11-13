//! Resource reports and serialized spawn admission for the host command loop.
use anyhow::Result;
use boss_host_execution::{
    capacity,
    worker::{ExecutionRecord, Phase},
};
use boss_protocol_spoke::v1::{
    Capacity, Command, CommandResult, Inventory, InventorySnapshot, Record, ResultCode, RunObservation, Stamp, command,
    record,
};
use boss_spoke_journal::{Admission, Journal};
use std::path::Path;

fn executions(journal: &Journal) -> Result<Vec<ExecutionRecord>> {
    journal
        .list_executions()?
        .iter()
        .map(|raw| Ok(serde_json::from_slice(raw)?))
        .collect()
}

/// Called on the single serialized command executor, before Spawn persists its
/// intent. The next Spawn cannot enter until this one's durable result exists.
/// Recovery of an existing token remains available even under resource pressure.
pub fn admission(
    journal: &Journal,
    root: &Path,
    command: &Command,
    failures: &[String],
) -> Result<Option<CommandResult>> {
    if !matches!(command.body, Some(command::Body::Spawn(_))) {
        return Ok(None);
    }
    if let Some(run) = &command.run
        && journal.load_execution(&run.run_id, &run.spawn_token)?.is_some()
    {
        return Ok(None);
    }
    let active = executions(journal)?
        .iter()
        .filter(|r| r.phase != Phase::Terminal)
        .count() as u64;
    // A failed probe is no budget: reject durably and keep the daemon alive.
    let reason = if !failures.is_empty() {
        Some("host resource budget or preflight unavailable".to_owned())
    } else {
        match capacity::sample(root, active) {
            Ok(resources) if resources.workers(0) > 0 => None,
            Ok(_) => Some("host resource budget or preflight unavailable".to_owned()),
            Err(error) => {
                tracing::error!(%error, "host resource sampling failed; rejecting spawn");
                Some(format!("host resource sampling failed: {error:#}"))
            }
        }
    };
    Ok(reason.map(|reason| CommandResult {
        command_id: command.command_id.clone(),
        code: ResultCode::Rejected.into(),
        reason,
        value: None,
    }))
}

/// Inventory is ordered ahead of capacity, so the hub must project the replay
/// and this inventory before using the budget. Unknown panes stay unknown.
pub fn report(journal: &mut Journal, root: &Path, observed: Stamp, inventory: Inventory, load: f64) -> Result<()> {
    let runs = executions(journal)?;
    let active = runs.iter().filter(|r| r.phase != Phase::Terminal).count() as u32;
    // A failed probe publishes zero capacity, which admits nothing, and the
    // next report retries.
    let resources = capacity::sample(root, u64::from(active)).unwrap_or_else(|error| {
        tracing::error!(%error, "host resource sampling failed; publishing zero capacity");
        Default::default()
    });
    let capacity = Capacity {
        generation: journal.record_bounds()?.1 + 1,
        total_slots: std::thread::available_parallelism()?.get() as u32,
        active_slots: active,
        available_cpu_millis: resources.cpu_millis.try_into()?,
        available_memory_bytes: resources.memory_bytes,
        free_disk_bytes: resources.disk_bytes,
        load_one_minute: load,
        journal_bytes: journal.bytes()?,
        applied_command_through: journal.applied_commands()?,
        inventory: Some(Inventory {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            ..inventory
        }),
        ..Default::default()
    };
    journal.append(
        Record {
            observed: Some(observed),
            body: Some(record::Body::Capacity(capacity)),
            ..Default::default()
        },
        Admission::Capacity,
    )?;
    Ok(())
}

/// Append bounded pages only after observing the real host sessions.
pub fn inventory(journal: &mut Journal, observed: Stamp, observations: Vec<RunObservation>) -> Result<()> {
    let through = journal.record_bounds()?.1;
    let pages = observations.len().div_ceil(64).max(1);
    for page in 0..pages {
        journal.append(
            Record {
                observed: Some(observed.clone()),
                body: Some(record::Body::Inventory(InventorySnapshot {
                    runs: observations.iter().skip(page * 64).take(64).cloned().collect(),
                    through_record: through,
                    page: page as u32,
                    last_page: page + 1 == pages,
                })),
                ..Default::default()
            },
            Admission::Capacity,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol_spoke::v1::{Run, Spawn};
    use boss_spoke_journal::Limits;

    fn journal(dir: &Path) -> Journal {
        Journal::open(
            &dir.join("state"),
            "host-a",
            Limits {
                spool_bytes: 1 << 20,
                lifecycle_reserve: 1 << 10,
            },
        )
        .unwrap()
    }

    fn spawn() -> Command {
        Command {
            command_id: "c1".into(),
            run: Some(Run {
                run_id: "r1".into(),
                spawn_token: "t1".into(),
                ..Default::default()
            }),
            body: Some(command::Body::Spawn(Spawn::default())),
            ..Default::default()
        }
    }

    #[test]
    fn failed_preflights_reject_a_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let result = admission(&journal, dir.path(), &spawn(), &["tmux unavailable".into()])
            .unwrap()
            .expect("rejected");
        assert_eq!(result.code, i32::from(ResultCode::Rejected));
        assert_eq!(result.command_id, "c1");
    }

    #[test]
    fn a_sampling_failure_rejects_a_spawn_without_failing_the_executor() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let missing = dir.path().join("does-not-exist");
        let result = admission(&journal, &missing, &spawn(), &[]).unwrap().expect("rejected");
        assert_eq!(result.code, i32::from(ResultCode::Rejected));
        assert!(result.reason.contains("sampling failed"), "{}", result.reason);
    }

    #[test]
    fn an_existing_spawn_token_recovers_under_resource_pressure() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        journal.save_execution("r1", "t1", b"{}").unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(
            admission(&journal, &missing, &spawn(), &["tmux unavailable".into()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn commands_other_than_spawn_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let stop = Command {
            command_id: "c2".into(),
            body: Some(command::Body::Stop(Default::default())),
            ..Default::default()
        };
        let missing = dir.path().join("does-not-exist");
        assert!(
            admission(&journal, &missing, &stop, &["tmux unavailable".into()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_sampling_failure_publishes_zero_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let missing = dir.path().join("does-not-exist");
        report(
            &mut journal,
            &missing,
            Stamp {
                boot_id: "boot".into(),
                wall_unix_ms: 1,
                ..Default::default()
            },
            Inventory::default(),
            0.0,
        )
        .unwrap();
    }
}
