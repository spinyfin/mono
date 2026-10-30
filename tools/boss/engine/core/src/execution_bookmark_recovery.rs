//! Crash reporting reads execution references from the shared store. A failed
//! probe is recorded on the work item, never classified as empty.
use std::path::PathBuf;

use anyhow::Result;
use boss_engine_recovery::execution_bookmark::LocalJj;
use boss_engine_recovery::recovery_backup::{backup_execution_patch, default_recovery_dir};

use crate::work::{CreateAttentionItemInput, WorkDb, WorkExecution};

pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND as CREATE_FAILED;
pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_FAILED_ATTENTION_KIND as RECOVERY_FAILED;
pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND as RECOVERY_RETIRED;
pub(crate) use boss_engine_storage_values::attention_kinds::REVISION_CHAIN_RECOVERY_BLOCKED_ATTENTION_KIND as CHAIN_BLOCKED;

/// Only authoritative integrity failures park work; transport failures keep
/// the coordinator's pre-start backoff and rerun repair on the next attempt.
pub(crate) fn block_unrecoverable(db: &WorkDb, execution: &WorkExecution, err: &anyhow::Error) -> Result<bool> {
    use crate::work::ExecutionKind;
    let damaged = boss_engine_recovery::execution_bookmark::is_pointer_integrity_error(err);
    if damaged
        && matches!(
            execution.kind,
            ExecutionKind::ChoreImplementation
                | ExecutionKind::TaskImplementation
                | ExecutionKind::RevisionImplementation
        )
    {
        db.update_work_item_as_actor(
            &execution.work_item_id,
            boss_protocol::WorkItemPatch::builder()
                .status("blocked")
                .blocked_reason("execution_recovery_failed")
                .blocked_detail(format!("{err:#}"))
                .autostart(false)
                .build(),
            "engine",
        )?;
        report_chain_block(db, execution, &format!("{err:#}"))?;
    }
    Ok(damaged)
}

pub(crate) fn report_chain_block(db: &WorkDb, execution: &WorkExecution, reason: &str) -> Result<()> {
    if execution.kind != crate::work::ExecutionKind::RevisionImplementation {
        return Ok(());
    }
    let Some(root) = db.get_revision_chain_root_task_id(&execution.work_item_id) else {
        return Ok(());
    };
    db.upsert_refreshing_work_item_attention(
        &root,
        CHAIN_BLOCKED,
        "PR revision chain blocked on recovery",
        &format!(
            "Revision `{}` cannot recover its preserved work: {reason}\n\nLater revisions on this PR remain sequenced behind it. Repair the reported state and explicitly restart the blocked revision; do not discard its work or bypass sequencing. Execution: `{}`.",
            execution.work_item_id, execution.id
        ),
    )?;
    Ok(())
}

pub(crate) fn worker_instructions(execution: &WorkExecution) -> String {
    let bookmark = format!("boss-recovery/{}", execution.id);
    let publication = format!("boss/{}", execution.id);
    let text = format!(
        "## Execution recovery bookmark\n\nThe engine created and owns recovery bookmark `{bookmark}` and publication bookmark `{publication}` in the shared jj store for this execution, including revisions. Advance both existing bookmarks to `@` before editing a new change (`jj bookmark set {bookmark} {publication} -r @`). jj follows rewrites of that change automatically. After every `jj new`, `jj commit`, split, squash, rebase, or checkout, advance it again before editing; checkpoint file edits with `jj status` and keep the bookmark at the latest work before ending a turn or stopping. Never delete or push the recovery bookmark. Its separate namespace retains work even when cube cleans up a merged publication bookmark. A revision still publishes only to its existing PR branch; its execution bookmark is a local recovery pointer.\n\nThe engine has already positioned this lease. Inspect its current changes before any checkout; do not reset it to main or re-run PR positioning from generic setup instructions. If you produce nothing, leave the engine-created bookmark in place so an empty run remains distinguishable from a missing recovery pointer.\n\n"
    );
    text
}

/// `reconciliation_note` is the persisted [`Reconciliation::note`] when
/// dispatch had to re-anchor the preserved pointers before recovering them.
///
/// [`Reconciliation::note`]: boss_engine_recovery::execution_bookmark::Reconciliation::note
pub(crate) fn recovery_instructions(
    recovery: Option<&(String, bool)>,
    report: Option<&boss_engine_recovery::execution_bookmark::RestoreReport>,
    reconciliation_note: Option<&str>,
) -> String {
    let note = reconciliation_note.unwrap_or_default();
    if let Some(report) = report {
        return format!("{}{note}", report.instructions());
    }
    let Some((predecessor, has_work)) = recovery else {
        return note.to_owned();
    };
    let state = if *has_work {
        "Unpublished changes and their history were recovered into this lease. Stay at `@`; do not reset or repeat PR checkout instructions."
    } else {
        "The predecessor has no unpublished changes to recover. The engine positioned this lease normally."
    };
    format!(
        "## EXECUTION BOOKMARK RECOVERY\n\nThe engine resolved predecessor `{predecessor}` through its recorded bookmark in the shared jj store. {state} The old workspace was not used. Inspect `jj status`, `jj diff --stat`, and the inherited history. Re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n{note}"
    )
}

pub(crate) async fn backup_dead_execution(db: &WorkDb, execution: &WorkExecution) -> Option<PathBuf> {
    execution.started_at.as_ref()?;
    let result: Result<Option<PathBuf>> = async {
        let record = db.execution_bookmark(&execution.id)?;
        anyhow::ensure!(
            record.host_id == "local",
            "recovery store is on host {}; local crash backup cannot inspect it",
            record.host_id
        );
        let dir = default_recovery_dir().ok_or_else(|| anyhow::anyhow!("recovery directory unavailable"))?;
        backup_execution_patch(&dir, &LocalJj, &record).await
    }
    .await;
    match result {
        Ok(path) => path,
        Err(err) => {
            report_failure(db, execution, &format!("{err:#}"));
            None
        }
    }
}

pub(crate) fn report_failure(db: &WorkDb, execution: &WorkExecution, reason: &str) {
    tracing::error!(execution_id = %execution.id, reason, "execution bookmark recovery failed");
    match db.reraise_open_execution_attention(&execution.id, RECOVERY_FAILED) {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(err) => tracing::error!(error = %err, "could not check existing recovery attention"),
    }
    if let Err(err) = db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution.id.clone()),
        work_item_id: None,
        kind: RECOVERY_FAILED.to_owned(),
        status: None,
        title: "Execution bookmark recovery failed".to_owned(),
        body_markdown: format!(
            "Cannot inspect the engine-created recovery bookmark for execution `{}`: {reason}",
            execution.id
        ),
        resolved_at: None,
    }) {
        tracing::error!(execution_id = %execution.id, error = %err, "could not record recovery failure on work item");
    }
}
