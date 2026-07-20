//! Auto-schedules real follow-up tasks for uncompleted work a
//! `design_postmortem` review surfaces (project-postmortem scope addition,
//! 2026-07-20).
//!
//! Distinct from [`crate::attentions_detector::reconcile_task_followups`]:
//! that path is best-effort and optional — an absent or malformed artifact
//! is a silent, expected no-op, and a hit creates a human-gated `followup`
//! attention item a person must promote into a task. This path is
//! mandatory and stronger. The operator's brief for this feature cited a
//! prior finding that free-text "filed as a follow-up" claims in worker PR
//! bodies had a 100% miss rate, because no engine write path ever actually
//! existed for them — the claim was always false. So here:
//!
//! - the postmortem worker's prompt (see
//!   `runner::compose_design_postmortem_directive`) requires writing a
//!   strict-schema JSON array to the engine-owned structured-output
//!   artifact, **even when empty** — omitting the file entirely is treated
//!   as an error, not as "no findings" (unlike the generic followups path,
//!   where an absent artifact is the normal case).
//! - a missing or malformed artifact is a real, operator-visible error
//!   (appended to the postmortem task's own description, the same
//!   mechanism `design_detector` uses for its own detector-miss notes) —
//!   never a silently dropped result.
//! - a hit creates real `project_task` rows directly in the same project,
//!   with normal (non-design-family) effort classification, rather than
//!   proposing them for a human to promote.
//!
//! Idempotency is a plain description marker on the postmortem task rather
//! than the create-time duplicate-name guard `CreateTaskInput` normally
//! relies on: a completion recheck / retry re-running this reconcile must
//! never double-file the same entries, and unlike the 60-second duplicate
//! window, a marker holds regardless of how much time passes between runs.

use std::path::Path;

use serde::Deserialize;

use crate::work::{CREATED_VIA_ENGINE_AUTO, CreateTaskInput, WorkDb, WorkItem};

/// Prefix marker appended to a postmortem task's `description` once this
/// reconcile has run for it (successfully or with an error) — the
/// idempotency guard against re-processing the same postmortem twice.
const PROCESSED_MARKER: &str = "[postmortem-followups]";

/// One entry of the postmortem-followups JSON array. All three fields are
/// required (no `#[serde(default)]`): a genuinely strict schema is the
/// point — see the module doc's "100% miss rate" note on why a lenient,
/// easy-to-get-wrong format is exactly what this replaces.
#[derive(Debug, Clone, Deserialize)]
struct PostmortemFollowupEntry {
    name: String,
    description: String,
    evidence: String,
}

/// Outcome of one reconcile call, for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PostmortemFollowupsOutcome {
    /// Number of follow-up tasks created this call.
    pub tasks_created: usize,
    /// `Some(reason)` when the artifact was missing or malformed. Always
    /// `None` when `tasks_created > 0` or when an explicit empty array was
    /// read successfully.
    pub error: Option<String>,
    /// `true` when a prior call already processed this postmortem and this
    /// call was skipped entirely (the idempotency guard fired).
    pub already_processed: bool,
}

/// Fired from `completion::finalize_pr_transition` when a
/// `design_postmortem` task's PR merges. See the module doc for the full
/// rationale.
pub async fn reconcile_postmortem_followups(
    work_db: &WorkDb,
    postmortem_task_id: &str,
    product_id: &str,
    project_id: &str,
    execution_id: &str,
    structured_output_dir: Option<&Path>,
) -> PostmortemFollowupsOutcome {
    let mut outcome = PostmortemFollowupsOutcome::default();

    let current_description = match work_db.get_work_item(postmortem_task_id) {
        Ok(WorkItem::Task(t)) | Ok(WorkItem::Chore(t)) => t.description,
        Ok(other) => {
            tracing::warn!(
                postmortem_task_id,
                work_item = ?other,
                "postmortem-followups: work item is not a task; skipping"
            );
            return outcome;
        }
        Err(err) => {
            tracing::warn!(
                postmortem_task_id,
                ?err,
                "postmortem-followups: failed to look up task; skipping"
            );
            return outcome;
        }
    };
    if current_description.contains(PROCESSED_MARKER) {
        outcome.already_processed = true;
        return outcome;
    }

    let entries = match read_artifact(structured_output_dir, execution_id) {
        Ok(entries) => entries,
        Err(reason) => {
            tracing::error!(
                postmortem_task_id,
                execution_id,
                reason,
                "postmortem-followups: failed to read/parse the required structured-output artifact",
            );
            let note = format!(
                "\n{PROCESSED_MARKER} ERROR: {reason} — uncompleted-work follow-up tasks were NOT \
                 auto-created for this postmortem. A human should re-check the review and file them \
                 manually.",
            );
            if let Err(err) = crate::reconcile_audit::append_description_line(work_db, postmortem_task_id, &note) {
                tracing::warn!(
                    postmortem_task_id,
                    ?err,
                    "postmortem-followups: failed to append error note"
                );
            }
            outcome.error = Some(reason);
            return outcome;
        }
    };

    let mut created_labels = Vec::new();
    for entry in &entries {
        let name = entry.name.trim();
        if name.is_empty() {
            tracing::warn!(
                postmortem_task_id,
                "postmortem-followups: skipping entry with empty name"
            );
            continue;
        }
        let description = format!(
            "{}\n\nEvidence: {}\n\nSurfaced by a design postmortem review; this task was auto-created by the \
             engine, not filed by the reviewing worker directly.",
            entry.description.trim(),
            entry.evidence.trim(),
        );
        match work_db.create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(name)
                .description(description)
                .created_via(CREATED_VIA_ENGINE_AUTO)
                .build(),
        ) {
            Ok(task) => {
                outcome.tasks_created += 1;
                created_labels.push(task.short_label());
            }
            Err(err) => {
                tracing::warn!(
                    postmortem_task_id,
                    name,
                    ?err,
                    "postmortem-followups: failed to create follow-up task"
                );
            }
        }
    }

    let summary = if entries.is_empty() {
        format!("\n{PROCESSED_MARKER} review surfaced no uncompleted work.")
    } else {
        format!(
            "\n{PROCESSED_MARKER} created {} follow-up task(s): {}",
            outcome.tasks_created,
            created_labels.join(", "),
        )
    };
    if let Err(err) = crate::reconcile_audit::append_description_line(work_db, postmortem_task_id, &summary) {
        tracing::warn!(
            postmortem_task_id,
            ?err,
            "postmortem-followups: failed to append summary note"
        );
    }

    tracing::info!(
        postmortem_task_id,
        execution_id,
        tasks_created = outcome.tasks_created,
        "postmortem-followups: reconcile complete",
    );
    outcome
}

/// Read + strictly validate the structured-output artifact as a
/// `Vec<PostmortemFollowupEntry>`. Every failure mode returns `Err` with an
/// operator-readable reason — there is no "absent means no findings"
/// fallback here, unlike the generic followups artifact reader.
fn read_artifact(dir: Option<&Path>, execution_id: &str) -> Result<Vec<PostmortemFollowupEntry>, String> {
    let dir = dir.ok_or_else(|| "no structured-output directory configured on this engine".to_owned())?;
    let raw = crate::structured_output::read(dir, execution_id).ok_or_else(|| {
        "no structured-output artifact was written — the postmortem worker must always write one, \
         an empty array `[]` if it found no uncompleted work"
            .to_owned()
    })?;
    serde_json::from_str::<Vec<PostmortemFollowupEntry>>(&raw).map_err(|e| {
        format!("structured-output artifact is not a valid JSON array of {{name, description, evidence}} objects: {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_product_named, open_db};
    use crate::work::CreateProjectInput;

    fn setup_postmortem(db: &WorkDb) -> (String, String, String) {
        let product = create_test_product_named(db, "Boss");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        let postmortem = db
            .create_design_postmortem(&product.id, &project.id, &project.name, "review these PRs".to_owned())
            .unwrap();
        (product.id, project.id, postmortem.id)
    }

    #[tokio::test]
    async fn missing_artifact_is_a_real_error_not_a_silent_noop() {
        let (_dir, db) = open_db();
        let (product_id, project_id, postmortem_id) = setup_postmortem(&db);

        let outcome =
            reconcile_postmortem_followups(&db, &postmortem_id, &product_id, &project_id, "exec_missing", None).await;

        assert_eq!(outcome.tasks_created, 0);
        assert!(
            outcome.error.is_some(),
            "an absent artifact must be reported as an error"
        );

        let task = match db.get_work_item(&postmortem_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
            other => panic!("expected a task, got {other:?}"),
        };
        assert!(
            task.description.contains("ERROR"),
            "the error must be visible on the postmortem task's own description"
        );
    }

    #[tokio::test]
    async fn malformed_artifact_is_a_real_error() {
        let (dir, db) = open_db();
        let (product_id, project_id, postmortem_id) = setup_postmortem(&db);
        let out_dir = dir.path().join("structured-output");
        let path = crate::structured_output::prepare(&out_dir, "exec_bad").unwrap();
        std::fs::write(&path, "not json").unwrap();

        let outcome = reconcile_postmortem_followups(
            &db,
            &postmortem_id,
            &product_id,
            &project_id,
            "exec_bad",
            Some(&out_dir),
        )
        .await;

        assert_eq!(outcome.tasks_created, 0);
        assert!(outcome.error.is_some());
    }

    #[tokio::test]
    async fn explicit_empty_array_is_not_an_error_and_creates_nothing() {
        let (dir, db) = open_db();
        let (product_id, project_id, postmortem_id) = setup_postmortem(&db);
        let out_dir = dir.path().join("structured-output");
        let path = crate::structured_output::prepare(&out_dir, "exec_empty").unwrap();
        std::fs::write(&path, "[]").unwrap();

        let outcome = reconcile_postmortem_followups(
            &db,
            &postmortem_id,
            &product_id,
            &project_id,
            "exec_empty",
            Some(&out_dir),
        )
        .await;

        assert_eq!(outcome.tasks_created, 0);
        assert!(
            outcome.error.is_none(),
            "an explicit empty array is a valid, error-free result"
        );
    }

    #[tokio::test]
    async fn valid_entries_create_project_tasks_in_the_same_project() {
        let (dir, db) = open_db();
        let (product_id, project_id, postmortem_id) = setup_postmortem(&db);
        let out_dir = dir.path().join("structured-output");
        let path = crate::structured_output::prepare(&out_dir, "exec_hit").unwrap();
        std::fs::write(
            &path,
            r#"[{"name":"Wire the frontend field","description":"Backend shipped the field but no UI consumes it.","evidence":"PR #42 added the field; grep shows zero frontend references."}]"#,
        )
        .unwrap();

        let outcome = reconcile_postmortem_followups(
            &db,
            &postmortem_id,
            &product_id,
            &project_id,
            "exec_hit",
            Some(&out_dir),
        )
        .await;

        assert_eq!(outcome.tasks_created, 1);
        assert!(outcome.error.is_none());

        let tasks = db.list_tasks(&product_id, Some(&project_id), None, false).unwrap();
        let created = tasks
            .iter()
            .find(|t| t.name == "Wire the frontend field")
            .expect("follow-up task must be created in the project");
        assert_eq!(created.kind, crate::work::TaskKind::ProjectTask);
        assert!(
            created.description.contains("PR #42"),
            "evidence must be preserved in the task description"
        );
        assert!(
            created.effort_level.is_none(),
            "normal effort classification: no design-family floor forced"
        );
    }

    #[tokio::test]
    async fn reprocessing_the_same_postmortem_is_a_noop() {
        let (dir, db) = open_db();
        let (product_id, project_id, postmortem_id) = setup_postmortem(&db);
        let out_dir = dir.path().join("structured-output");
        let path = crate::structured_output::prepare(&out_dir, "exec_hit").unwrap();
        std::fs::write(
            &path,
            r#"[{"name":"Follow up A","description":"desc","evidence":"evidence"}]"#,
        )
        .unwrap();

        let first = reconcile_postmortem_followups(
            &db,
            &postmortem_id,
            &product_id,
            &project_id,
            "exec_hit",
            Some(&out_dir),
        )
        .await;
        assert_eq!(first.tasks_created, 1);

        // Simulate a completion recheck re-running the same reconcile —
        // even with the artifact still present, it must not double-file.
        let second = reconcile_postmortem_followups(
            &db,
            &postmortem_id,
            &product_id,
            &project_id,
            "exec_hit",
            Some(&out_dir),
        )
        .await;
        assert_eq!(second.tasks_created, 0);
        assert!(second.already_processed);

        let tasks = db.list_tasks(&product_id, Some(&project_id), None, false).unwrap();
        assert_eq!(
            tasks.iter().filter(|t| t.name == "Follow up A").count(),
            1,
            "must not double-file the same follow-up on a re-run"
        );
    }
}
