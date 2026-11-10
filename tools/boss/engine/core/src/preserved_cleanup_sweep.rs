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
//!   either carried by a later execution or abandoned; open unrecovered-work
//!   items keep the tip.
//!
//! Each pass forgets a bounded batch, one jj operation per execution. A host
//! that cannot be reached is retried after [`RETRY_AFTER`] rather than on every
//! pass, and each list is walked with a cursor that moves past every attempted
//! record and wraps only after the whole list has been traversed, so failing
//! records cannot starve the rest however many there are.
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
    superseded_cursor: Option<String>,
    closed_chain_cursor: Option<String>,
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
    run_one_pass_with(db, coordinator, backoff, Instant::now(), BATCH_SIZE).await
}

/// [`run_one_pass`] with an injectable clock and batch size.
pub(crate) async fn run_one_pass_with(
    db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    backoff: &mut Backoff,
    now: Instant,
    batch_size: usize,
) -> Result<CleanupReport> {
    let excluded: Vec<_> = backoff
        .failed_at
        .keys()
        .filter(|id| !backoff.due(id, now))
        .cloned()
        .collect();
    let superseded = db.list_superseded_preserved_after(batch_size, &excluded, backoff.superseded_cursor.as_deref())?;
    // A full batch means more may follow: resume after it. A short one reached
    // the end of the list, so the next pass starts over from the beginning.
    backoff.superseded_cursor = (superseded.len() >= batch_size)
        .then(|| superseded.last().map(|c| c.execution.id.clone()))
        .flatten();
    let closed = db.list_closed_chain_preserved_after(
        CLOSED_CHAIN_RETENTION_SECS,
        batch_size,
        &excluded,
        backoff.closed_chain_cursor.as_deref(),
    )?;
    backoff.closed_chain_cursor = (closed.len() >= batch_size)
        .then(|| closed.last().map(|c| c.execution.id.clone()))
        .flatten();
    let mut cleanups = superseded;
    cleanups.extend(closed);
    let mut report = CleanupReport::default();
    let mut seen = HashSet::new();
    report.deferred = excluded.len();
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
pub(crate) async fn forget_one(
    db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    cleanup: &PreservedCleanup,
) -> Result<usize> {
    let execution = &cleanup.execution;
    let record = db
        .preservation_source(&execution.id)?
        .ok_or_else(|| anyhow!("no store location recorded for execution {}", execution.id))?;
    // The scope was chosen when the list was read. A legacy store cleanup may
    // have created archives, or an inspection item been filed, since then.
    let scope = if cleanup.scope == PreservedScope::TipAndArchives && db.preserved_archives_protected(&execution.id)? {
        PreservedScope::Tip
    } else {
        cleanup.scope
    };
    // A repository that no longer exists on this machine holds nothing to forget.
    let forgotten = if record.host_id == "local" && !record.repo_path.exists() {
        Vec::new()
    } else {
        coordinator.forget_preserved_references(&record, scope).await?
    };
    let mut resolved = Vec::new();
    if matches!(cleanup.reason, PreservedCleanupReason::Superseded { .. }) {
        resolved.extend([
            ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND,
            PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
        ]);
    }
    if scope == PreservedScope::TipAndArchives {
        resolved.push(PRESERVED_WORK_ARCHIVED_ATTENTION_KIND);
    }
    db.finish_preserved_cleanup(&execution.id, scope, &resolved)?;
    tracing::info!(
        execution_id = %execution.id,
        reason = ?cleanup.reason,
        ?scope,
        forgotten = %forgotten.join(", "),
        "preserved references forgotten"
    );
    Ok(forgotten.len())
}
