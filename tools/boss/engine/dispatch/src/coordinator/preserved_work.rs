use super::blocked_workspace::Recovered;
use super::fresh_start::{FreshStart, position_workspace};
use super::*;
use boss_engine_recovery::execution_bookmark::{is_pointer_integrity_error, pointer_integrity_error, preserved_name};
use boss_engine_storage_values::attention_kinds::{
    PRESERVED_WORK_ARCHIVED_ATTENTION_KIND, PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
};

impl ExecutionCoordinator {
    pub(super) async fn recover_preserved_work(
        &self,
        execution: &WorkExecution,
        prior: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        prs: (Option<u64>, Option<u64>),
    ) -> Result<Option<Recovered>> {
        let pr = match prs {
            (Some(bound), _) => Some(bound),
            (None, Some(task_pr)) if adapter.recovery_pr_is_open(repo_id, task_pr).await? => Some(task_pr),
            _ => None,
        };
        // Cube positions the PR itself: it refuses merged or closed PRs, fetches
        // fork heads and sets the branch bookmark. Restaging reads that
        // positioned commit rather than a poller-recorded head, which can lag
        // the real head.
        let positioning = FreshStart {
            repo_id,
            immutable_target: None,
            bound_pr: pr,
            task_pr: None,
        };
        position_workspace(adapter.as_ref(), &lease.workspace_path, &positioning).await?;
        let restored = async {
            let record = self
                .work_db
                .preservation_source(&prior.id)?
                .ok_or_else(|| pointer_integrity_error("preserved work store location is unavailable"))?;
            let recorded = self.work_db.preserved_tip(&prior.id)?;
            adapter
                .restore_preserved_work(&record, &lease.workspace_path, recorded.as_ref())
                .await
        }
        .await;
        match restored {
            Ok(Some(report)) => {
                let has_work = !report.commits.trim().is_empty();
                self.work_db.record_execution_restore_report(&execution.id, &report)?;
                if !report.unrestaged_unpublished.trim().is_empty()
                    && let Err(note_error) = self.archived_variant_attention(prior, &report.unrestaged_unpublished)
                {
                    tracing::error!(?note_error, "could not raise archived-variant attention");
                }
                if let Err(err) = self
                    .work_db
                    .resolve_attention_kind_for_execution(&prior.id, PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND)
                {
                    tracing::error!(?err, "could not resolve preserved-work attention");
                }
                Ok(Some(Recovered {
                    predecessor: prior.id.clone(),
                    has_work,
                }))
            }
            Ok(None) => {
                let name = preserved_name(&prior.id);
                let err = pointer_integrity_error(format!("{name} is missing and the recorded commit is not visible"));
                tracing::warn!(execution_id = %execution.id, bookmark = name, error = %err, "preserved work not restaged; starting fresh");
                if let Err(note_error) = self.unrestaged_attention(prior, &name, &err) {
                    tracing::error!(?note_error, "could not raise preserved-work attention");
                }
                self.work_db.delete_execution_restore_report(&execution.id)?;
                position_workspace(adapter.as_ref(), &lease.workspace_path, &positioning).await?;
                Ok(None)
            }
            Err(err) if is_pointer_integrity_error(&err) => {
                let name = preserved_name(&prior.id);
                tracing::warn!(execution_id = %execution.id, bookmark = name, error = %err, "preserved work not restaged; starting fresh");
                if let Err(note_error) = self.unrestaged_attention(prior, &name, &err) {
                    tracing::error!(?note_error, "could not raise preserved-work attention");
                }
                self.work_db.delete_execution_restore_report(&execution.id)?;
                position_workspace(adapter.as_ref(), &lease.workspace_path, &positioning).await?;
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }
    fn archived_variant_attention(&self, prior: &WorkExecution, archives: &str) -> Result<()> {
        let kind = PRESERVED_WORK_ARCHIVED_ATTENTION_KIND;
        let title = "Preserved variants were archived";
        let lines: Vec<String> = archives
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| format!("- `{line}`"))
            .collect();
        let open = self
            .work_db
            .list_attention_items(&prior.id)?
            .into_iter()
            .find(|item| item.kind == kind && item.status == "open");
        let body = match open {
            Some(existing) => {
                let mut body = existing.body_markdown;
                for line in lines {
                    if !body.contains(&line) {
                        body.push('\n');
                        body.push_str(&line);
                    }
                }
                body
            }
            None => {
                let listed = lines.join("\n");
                format!(
                    "The selected preserved tip was restaged. \
                     Unpublished unselected variants remain under archive names:\n{listed}\n\
                     Those archive bookmarks stay named for inspection. \
                     A later restage of the selected tip does not recover them."
                )
            }
        };
        self.work_db.upsert_execution_attention(&prior.id, kind, title, &body)?;
        Ok(())
    }
    fn unrestaged_attention(&self, prior: &WorkExecution, name: &str, err: &anyhow::Error) -> Result<()> {
        self.work_db.upsert_execution_attention(
            &prior.id,
            PRESERVED_WORK_NOT_RESTAGED_ATTENTION_KIND,
            "Preserved work could not be restaged",
            &format!("`{name}` remains available for inspection. {err:#}. The next worker starts from the current PR head or trunk."),
        )?;
        Ok(())
    }
}
