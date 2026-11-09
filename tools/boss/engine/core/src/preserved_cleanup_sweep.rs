//! Forget the preserved references an execution no longer needs.
//!
//! Teardown keeps a worker's unpublished tip alive under one
//! `preserved/<execution>` bookmark, and jj writes every bookmark into the view
//! of every operation in the shared store, so a reference kept past its use
//! makes every later `jj new`, snapshot and `jj bookmark set` larger. Two facts
//! the engine already records say a reference is finished with, and neither
//! needs a proof about commits ([`boss_engine_work_db::PreservedCleanup`]):
//!
//! - a later execution that restaged the tip has published, so the tip was
//!   carried by a push;
//! - the item's PR chain has been closed for a fixed age, so the work was
//!   either carried by a later execution or abandoned, and an attention item
//!   already told the operator which.
//!
//! Each pass forgets a bounded batch, one jj operation per execution. A host
//! that cannot be reached is retried after [`RETRY_AFTER`] rather than on every
//! pass, so a few unreachable records cannot starve the rest.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use boss_engine_recovery::execution_bookmark::PreservedScope;
use boss_engine_storage_values::attention_kinds::{
    ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND, PRESERVED_WORK_ARCHIVED_ATTENTION_KIND,
    PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
};

use crate::coordinator::ExecutionCoordinator;
use crate::work::{CLOSED_CHAIN_RETENTION_SECS, PreservedCleanup, PreservedCleanupReason, WorkDb};

/// Executions examined per list per pass.
pub(crate) const BATCH_SIZE: usize = 50;
/// How soon a record whose references could not be forgotten is tried again.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Which records failed recently, kept across passes by the sweep loop.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    failed_at: HashMap<String, Instant>,
}

impl Backoff {
    fn due(&self, execution_id: &str, now: Instant) -> bool {
        self.failed_at
            .get(execution_id)
            .is_none_or(|failed| now.duration_since(*failed) >= RETRY_AFTER)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CleanupReport {
    pub superseded: usize,
    pub closed_chain: usize,
    pub references_forgotten: usize,
    pub deferred: usize,
    pub failed: usize,
}

impl CleanupReport {
    pub(crate) fn has_activity(&self) -> bool {
        self.superseded + self.closed_chain + self.failed > 0
    }
}

/// One pass over the executions whose preserved references are finished with.
/// A failure on one record is logged and backed off; it never stops the rest.
pub(crate) async fn run_one_pass(
    db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    backoff: &mut Backoff,
) -> Result<CleanupReport> {
    let mut cleanups = db.list_superseded_preserved(BATCH_SIZE)?;
    cleanups.extend(db.list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, BATCH_SIZE)?);
    let mut report = CleanupReport::default();
    let mut seen = HashSet::new();
    let now = Instant::now();
    for cleanup in cleanups {
        let execution_id = cleanup.execution.id.clone();
        if !seen.insert(execution_id.clone()) {
            continue;
        }
        if !backoff.due(&execution_id, now) {
            report.deferred += 1;
            continue;
        }
        match forget_one(db, coordinator, &cleanup).await {
            Ok(forgotten) => {
                backoff.failed_at.remove(&execution_id);
                report.references_forgotten += forgotten;
                match cleanup.reason {
                    PreservedCleanupReason::Superseded { .. } => report.superseded += 1,
                    PreservedCleanupReason::ClosedChain { .. } => report.closed_chain += 1,
                }
            }
            Err(err) => {
                report.failed += 1;
                backoff.failed_at.insert(execution_id.clone(), now);
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %format!("{err:#}"),
                    "preserved reference cleanup failed; keeping the references and retrying later"
                );
            }
        }
    }
    backoff
        .failed_at
        .retain(|_, failed| now.duration_since(*failed) < RETRY_AFTER);
    Ok(report)
}

/// Forget one execution's references and record that they are gone. Returns how
/// many references were forgotten.
async fn forget_one(db: &WorkDb, coordinator: &ExecutionCoordinator, cleanup: &PreservedCleanup) -> Result<usize> {
    let execution = &cleanup.execution;
    let record = db
        .preservation_source(&execution.id)?
        .ok_or_else(|| anyhow!("no store location recorded for execution {}", execution.id))?;
    let scope = cleanup.scope;
    // A repository that no longer exists on this machine holds nothing to forget.
    let forgotten = if record.host_id == "local" && !record.repo_path.exists() {
        Vec::new()
    } else {
        coordinator.forget_preserved_references(&record, scope).await?
    };
    let mut resolved = vec![
        ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND,
        PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
    ];
    if scope == PreservedScope::TipAndArchives {
        resolved.push(PRESERVED_WORK_ARCHIVED_ATTENTION_KIND);
    }
    db.finish_preserved_cleanup(&execution.id, &resolved)?;
    tracing::info!(
        execution_id = %execution.id,
        reason = ?cleanup.reason,
        ?scope,
        forgotten = %forgotten.join(", "),
        "preserved references forgotten"
    );
    Ok(forgotten.len())
}
