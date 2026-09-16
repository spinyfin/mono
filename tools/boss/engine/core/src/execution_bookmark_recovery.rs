//! Crash reporting reads execution references from the shared store. A failed
//! probe is recorded on the work item, never classified as empty.
use std::path::PathBuf;

use anyhow::Result;
use boss_engine_recovery::execution_bookmark::LocalJj;
use boss_engine_recovery::recovery_backup::{backup_execution_patch, default_recovery_dir};

use crate::work::{CreateAttentionItemInput, WorkDb, WorkExecution};

pub(crate) const RECOVERY_FAILED: &str = "execution_bookmark_recovery_failed";
pub(crate) const CREATE_FAILED: &str = "execution_bookmark_create_failed";

pub(crate) fn worker_instructions(execution: &WorkExecution) -> String {
    let bookmark = format!("boss-recovery/{}", execution.id);
    let publication = format!("boss/{}", execution.id);
    let text = format!(
        "## Execution recovery bookmark\n\nThe engine created and owns recovery bookmark `{bookmark}` and publication bookmark `{publication}` in the shared jj store for this execution, including revisions. Advance both existing bookmarks to `@` before editing a new change (`jj bookmark set {bookmark} {publication} -r @`). jj follows rewrites of that change automatically. After every `jj new`, `jj commit`, split, squash, rebase, or checkout, advance it again before editing; checkpoint file edits with `jj status` and keep the bookmark at the latest work before ending a turn or stopping. Never delete or push the recovery bookmark. Its separate namespace retains work even when cube cleans up a merged publication bookmark. A revision still publishes only to its existing PR branch; its execution bookmark is a local recovery pointer.\n\nThe engine has already positioned this lease. Inspect its current changes before any checkout; do not reset it to main or re-run PR positioning from generic setup instructions. If you produce nothing, leave the engine-created bookmark in place so an empty run remains distinguishable from a missing recovery pointer.\n\n"
    );
    text
}

pub(crate) fn recovery_instructions(recovery: Option<&(String, bool)>) -> String {
    let Some((predecessor, has_work)) = recovery else {
        return String::new();
    };
    let state = if *has_work {
        "Unpublished changes and their history were recovered into this lease. Stay at `@`; do not reset or repeat PR checkout instructions."
    } else {
        "The predecessor has no unpublished changes to recover. The engine positioned this lease normally."
    };
    format!(
        "## EXECUTION BOOKMARK RECOVERY\n\nThe engine resolved predecessor `{predecessor}` through its recorded bookmark in the shared jj store. {state} The old workspace was not used. Inspect `jj status`, `jj diff --stat`, and the inherited history. Re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n"
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
