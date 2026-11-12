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
    if !failures.is_empty() || capacity::sample(root, active)?.workers(0) == 0 {
        return Ok(Some(CommandResult {
            command_id: command.command_id.clone(),
            code: ResultCode::Rejected.into(),
            reason: "host resource budget or preflight unavailable".into(),
            value: None,
        }));
    }
    Ok(None)
}

/// Inventory is ordered ahead of capacity, so the hub must project the replay
/// and this inventory before using the budget. Unknown panes stay unknown.
pub fn report(journal: &mut Journal, root: &Path, observed: Stamp, inventory: Inventory, load: f64) -> Result<()> {
    let runs = executions(journal)?;
    let active = runs.iter().filter(|r| r.phase != Phase::Terminal).count() as u32;
    let resources = capacity::sample(root, u64::from(active))?;
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
