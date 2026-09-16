//! The local half of abandoned-branch detection, including revision runs.
use crate::coordinator::ExecutionCoordinator;
use crate::work::{CreateAttentionItemInput, WorkDb};

pub(crate) const RECOVERABLE_WORK: &str = "abandoned_execution_bookmark";

pub(crate) async fn run_one_pass(db: &WorkDb, coordinator: &ExecutionCoordinator) -> anyhow::Result<usize> {
    let executions = db.terminal_bookmark_executions(
        crate::abandoned_branch_pr_sweep::TERMINATION_GRACE_SECS,
        crate::abandoned_branch_pr_sweep::MAX_LOOKBACK_SECS,
    )?;
    let mut reported = 0;
    let mut failures = Vec::new();
    for execution in executions {
        let inspection = async {
            let record = db.execution_bookmark(&execution.id)?;
            let patch = coordinator.inspect_execution_bookmark(&record).await?;
            db.resolve_attention_kind_for_execution(
                &execution.id,
                crate::execution_bookmark_recovery::RECOVERY_FAILED,
            )?;
            Ok::<_, anyhow::Error>((record, patch))
        }
        .await;
        match inspection {
            Ok((record, patch)) if !patch.trim().is_empty() => {
                if db
                    .reraise_open_execution_attention(&execution.id, RECOVERABLE_WORK)?
                    .is_some()
                {
                    continue;
                }
                db.create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: RECOVERABLE_WORK.to_owned(),
                    status: None,
                    title: "Unpublished execution work is recoverable".to_owned(),
                    body_markdown: format!("Execution `{}` left unpublished work at engine-created bookmark `{}` in the shared jj store on host `{}`. Recovery can resume this work in a fresh lease; the originating workspace is not needed. The engine has not pushed this unvalidated work.", execution.id, record.head(), record.host_id),
                    resolved_at: None,
                })?;
                reported += 1;
            }
            Ok(_) => {
                db.resolve_attention_kind_for_execution(&execution.id, RECOVERABLE_WORK)?;
            }
            Err(err) => {
                crate::execution_bookmark_recovery::report_failure(db, &execution, &format!("{err:#}"));
                failures.push(format!("{}: {err:#}", execution.id));
            }
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "execution bookmark inspection failed: {}",
        failures.join("; ")
    );
    Ok(reported)
}
