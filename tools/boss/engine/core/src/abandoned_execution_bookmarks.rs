//! The local half of abandoned-branch detection, including revision runs.
use crate::coordinator::ExecutionCoordinator;
use crate::work::{CreateAttentionItemInput, WorkDb};

pub(crate) use boss_engine_storage_values::attention_kinds::ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND as RECOVERABLE_WORK;

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
            let (pointer, has_work) = if let Some(tip) = db.preserved_tip(&execution.id)? {
                (
                    boss_engine_recovery::execution_bookmark::preserved_name(&execution.id),
                    tip.count > 0,
                )
            } else {
                let patch = coordinator.inspect_execution_bookmark(&record).await?;
                (record.head(), !patch.trim().is_empty())
            };
            db.resolve_attention_kind_for_execution(
                &execution.id,
                crate::execution_bookmark_recovery::RECOVERY_FAILED,
            )?;
            Ok::<_, anyhow::Error>((record, pointer, has_work))
        }
        .await;
        match inspection {
            Ok((record, pointer, true)) => {
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
                    body_markdown: format!("Execution `{}` has recorded unpublished work at bookmark `{}` in the shared jj store on host `{}`. Recovery can resume this work in a fresh lease; the originating workspace is not needed. The engine has not pushed this unvalidated work.", execution.id, pointer, record.host_id),
                    resolved_at: None,
                })?;
                reported += 1;
            }
            Ok(_) => {
                db.resolve_attention_kind_for_execution(&execution.id, RECOVERABLE_WORK)?;
            }
            Err(err) if boss_engine_recovery::execution_bookmark::is_missing_pointer_error(&err) => {
                // Absence is normal for new executions. Teardown owns workspace
                // preservation and reports any failure through its own attention.
                tracing::debug!(execution_id = %execution.id, "no legacy execution bookmark to inspect");
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
