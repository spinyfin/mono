//! Recovery failures recorded on the work item, never classified as empty.
use crate::work::{CreateAttentionItemInput, WorkDb, WorkExecution};

#[cfg(test)]
pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND as CREATE_FAILED;
pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_FAILED_ATTENTION_KIND as RECOVERY_FAILED;
#[cfg(test)]
pub(crate) use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND as RECOVERY_RETIRED;
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
