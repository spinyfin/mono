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
            let tip = db.preserved_tip(&execution.id)?;
            let has_work = match &tip {
                // The teardown count is a snapshot; a successor may have
                // restored and published the work since.
                Some(tip) if tip.count > 0 => {
                    let record = db
                        .preservation_source(&execution.id)?
                        .ok_or_else(|| anyhow::anyhow!("no store location recorded for execution {}", execution.id))?;
                    coordinator.preserved_work_unpublished(&record, tip).await?
                }
                _ => false,
            };
            db.resolve_attention_kind_for_execution(
                &execution.id,
                crate::execution_bookmark_recovery::RECOVERY_FAILED,
            )?;
            Ok::<_, anyhow::Error>(has_work)
        }
        .await;
        match inspection {
            Ok(true) => {
                if db
                    .reraise_open_execution_attention(&execution.id, RECOVERABLE_WORK)?
                    .is_some()
                {
                    continue;
                }
                let host = db
                    .preservation_source(&execution.id)?
                    .map_or_else(|| "unknown".to_owned(), |record| record.host_id);
                db.create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: RECOVERABLE_WORK.to_owned(),
                    status: None,
                    title: "Unpublished execution work is recoverable".to_owned(),
                    body_markdown: format!("Execution `{}` has recorded unpublished work at bookmark `{}` in the shared jj store on host `{host}`. Recovery can resume this work in a fresh lease; the originating workspace is not needed. The engine has not pushed this unvalidated work.", execution.id, boss_engine_recovery::execution_bookmark::preserved_name(&execution.id)),
                    resolved_at: None,
                })?;
                reported += 1;
            }
            Ok(false) => {
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
