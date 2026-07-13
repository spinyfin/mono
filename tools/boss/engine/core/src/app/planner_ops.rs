//! `FrontendRequest` handlers for the auto-populate operator surface
//! (design P783 §2 "Reusability" #2, task 9 of
//! `auto-populate-project-tasks-on-design-pr-merge.md`).
//!
//! These exercise the reusable Planner/Materializer ([`crate::populator`])
//! from outside the design-PR-merge trigger: `boss project plan` (with
//! `--force` / `--dry-run`), `boss project release`, `boss project
//! unpopulate`, and `boss project plan-runs`.

use boss_protocol::{PLANNER_OUTCOME_APPLIED, PLANNER_OUTCOME_STAGED, UnpopulatePreservedTask, WorkItemPatch};

use crate::populator::{DEFAULT_MAX_TASKS, LivePopulatorSteps, PopulateOutcome, Populator, PreviewOutcome};
use crate::work::PlannerRunPatch;

use super::*;

pub(super) async fn handle_list_planner_runs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListPlannerRuns { project_id } = req else {
        unreachable!()
    };
    match work_db.list_planner_runs_for_project(&project_id) {
        Ok(runs) => send_response(&sink, &request_id, FrontendEvent::PlannerRunsList { project_id, runs }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_plan_project(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::PlanProject {
        project_id,
        force,
        dry_run,
    } = req
    else {
        unreachable!()
    };

    let steps = LivePopulatorSteps {
        api_key: server_state.anthropic_api_key.clone(),
    };

    if dry_run {
        match Populator::preview(&work_db, &steps, &project_id, DEFAULT_MAX_TASKS, force).await {
            Ok(preview) => send_response(&sink, &request_id, preview_to_event(project_id, preview)),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
        return;
    }

    match Populator::run_operator(
        &work_db,
        &steps,
        &project_id,
        DEFAULT_MAX_TASKS,
        force,
        server_state.publisher.as_ref(),
    )
    .await
    {
        Ok(outcome) => {
            let (created, edges, skipped) = match &outcome {
                PopulateOutcome::Staged {
                    created,
                    edges,
                    skipped,
                    ..
                } => (*created, *edges, *skipped),
                _ => (0, 0, 0),
            };
            let staged = matches!(outcome, PopulateOutcome::Staged { .. });
            let run_id = if staged {
                work_db
                    .live_planner_run_for_project(&project_id)
                    .ok()
                    .flatten()
                    .map(|run| run.id)
            } else {
                None
            };
            let (tag, message) = describe_populate_outcome(&outcome);
            let event = FrontendEvent::PlanProjectResult {
                project_id: project_id.clone(),
                outcome: tag,
                message,
                created,
                edges,
                skipped,
                run_id,
                proposal: None,
            };
            if staged {
                let product_id = work_db.get_project(&project_id).ok().map(|project| project.product_id);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    product_id
                        .as_deref()
                        .map(|product_id| vec![work_product_topic(product_id)])
                        .unwrap_or_default(),
                    "project_planned",
                    product_id,
                    vec![project_id],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, event);
            } else {
                send_response(&sink, &request_id, event);
            }
        }
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_release_project(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ReleaseProject { project_id } = req else {
        unreachable!()
    };

    let run = match work_db.live_planner_run_for_project(&project_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: "no staged planner run for this project — nothing to release".to_owned(),
                },
            );
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    if run.outcome != PLANNER_OUTCOME_STAGED {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!(
                    "planner run {} for this project is not staged (outcome: {}); nothing to release",
                    run.id, run.outcome
                ),
            },
        );
        return;
    }

    let task_ids = match work_db.list_task_ids_for_planner_run(&run.id) {
        Ok(ids) => ids,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    let actor = resolve_status_actor(&server_state, peer_pid);
    let mut released = 0usize;
    for task_id in &task_ids {
        let patch = WorkItemPatch {
            autostart: Some(true),
            ..WorkItemPatch::default()
        };
        match work_db.update_work_item_as_actor(task_id, patch, actor) {
            Ok(_) => released += 1,
            Err(err) => {
                tracing::warn!(task_id, ?err, "release_project: failed to flip autostart");
            }
        }
    }

    if let Err(err) = work_db.update_planner_run(
        &run.id,
        PlannerRunPatch::builder().outcome(PLANNER_OUTCOME_APPLIED).build(),
    ) {
        tracing::warn!(run_id = %run.id, ?err, "release_project: failed to mark planner run applied");
    }

    let revision = publish_work_invalidation(
        &server_state,
        &session_id,
        &request_id,
        vec![work_product_topic(&run.product_id)],
        "project_released",
        Some(run.product_id.clone()),
        task_ids,
    )
    .await;

    send_response_with_revision(
        &sink,
        &request_id,
        revision,
        FrontendEvent::ReleaseProjectResult {
            project_id,
            run_id: run.id,
            released,
        },
    );
}

pub(super) async fn handle_unpopulate_project(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::UnpopulateProject { project_id, run_id } = req else {
        unreachable!()
    };

    let run = match work_db.get_planner_run(&run_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("unknown planner run: {run_id}"),
                },
            );
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    if run.project_id != project_id {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("planner run {run_id} does not belong to project {project_id}"),
            },
        );
        return;
    }

    let task_ids = match work_db.list_task_ids_for_planner_run(&run_id) {
        Ok(ids) => ids,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    let mut deleted = Vec::new();
    let mut preserved = Vec::new();
    for task_id in &task_ids {
        // Only a task with no execution yet was never released and
        // dispatched — safe to delete. Preserve (never destroy) anything we
        // can't positively confirm is execution-free.
        let has_executions = match work_db.list_executions(Some(task_id)) {
            Ok(executions) => !executions.is_empty(),
            Err(err) => {
                tracing::warn!(
                    task_id,
                    ?err,
                    "unpopulate_project: failed to check executions; preserving task"
                );
                true
            }
        };
        if has_executions {
            let name = task_name_for_id(&work_db, task_id);
            preserved.push(UnpopulatePreservedTask {
                id: task_id.clone(),
                name,
            });
            continue;
        }
        match work_db.delete_work_item(task_id) {
            Ok(()) => deleted.push(task_id.clone()),
            Err(err) => {
                let name = task_name_for_id(&work_db, task_id);
                tracing::warn!(task_id, ?err, "unpopulate_project: failed to delete task");
                preserved.push(UnpopulatePreservedTask {
                    id: task_id.clone(),
                    name,
                });
            }
        }
    }

    if let Err(err) = work_db.delete_planner_run(&run_id) {
        tracing::warn!(run_id, ?err, "unpopulate_project: failed to delete planner_runs row");
    }

    let revision = publish_work_invalidation(
        &server_state,
        &session_id,
        &request_id,
        vec![work_product_topic(&run.product_id)],
        "project_unpopulated",
        Some(run.product_id.clone()),
        deleted.clone(),
    )
    .await;

    send_response_with_revision(
        &sink,
        &request_id,
        revision,
        FrontendEvent::UnpopulateProjectResult {
            project_id,
            run_id,
            deleted,
            preserved,
        },
    );
}

/// Best-effort task name lookup for the `preserved` report — falls back to
/// the id itself if the row can't be read (should not happen for a task we
/// just resolved via `list_task_ids_for_planner_run`).
fn task_name_for_id(work_db: &WorkDb, task_id: &str) -> String {
    match work_db.get_work_item(task_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => task.name,
        _ => task_id.to_owned(),
    }
}

/// Human-readable `(outcome_tag, message)` for a completed
/// [`PopulateOutcome`] — used by [`handle_plan_project`]'s real (non-dry-run)
/// path. Mirrors the attention-item text [`crate::populator::Populator`]
/// raises, condensed for a CLI response.
fn describe_populate_outcome(outcome: &PopulateOutcome) -> (String, String) {
    match outcome {
        PopulateOutcome::SkippedAlreadyPopulated => (
            outcome.tag().to_owned(),
            "A planner run is already in flight or already completed for this project.".to_owned(),
        ),
        PopulateOutcome::SkippedPreSeeded { existing } => (
            outcome.tag().to_owned(),
            format!(
                "Skipped: project already has {existing} implementation task(s). Pass --force to add \
                 the planner's tasks anyway (existing tasks are preserved by name dedup)."
            ),
        ),
        PopulateOutcome::NoBreakdown => (
            outcome.tag().to_owned(),
            "The design doc has no implementation task-breakdown section. No tasks were created.".to_owned(),
        ),
        PopulateOutcome::EmptyBreakdown => (
            outcome.tag().to_owned(),
            "A breakdown section was found but no tasks were extracted from it.".to_owned(),
        ),
        PopulateOutcome::RejectedTooMany { count, max } => (
            outcome.tag().to_owned(),
            format!("The planner proposed {count} tasks, over the cap of {max}. The whole proposal was rejected."),
        ),
        PopulateOutcome::RejectedBadGraph => (
            outcome.tag().to_owned(),
            "The planner's proposed task graph was malformed (cycle, duplicate, or unknown handle). Rejected whole."
                .to_owned(),
        ),
        PopulateOutcome::DocMissing => (
            outcome.tag().to_owned(),
            "The design doc could not be found (no pointer set, no repo resolvable, or 404 at the merged ref)."
                .to_owned(),
        ),
        PopulateOutcome::FetchFailed => (
            outcome.tag().to_owned(),
            "Could not fetch the design doc from GitHub after retries.".to_owned(),
        ),
        PopulateOutcome::PlannerFailed => (
            outcome.tag().to_owned(),
            "The planner call failed (no API key, an API error, or invalid structured output).".to_owned(),
        ),
        PopulateOutcome::Staged {
            created,
            edges,
            skipped,
            low_confidence,
        } => (
            outcome.tag().to_owned(),
            format!(
                "Staged {created} task(s) and {edges} edge(s) ({skipped} deduped).{} Run `boss project release \
                 <project>` to begin dispatch.",
                if *low_confidence {
                    " Planner flagged LOW CONFIDENCE in this plan — review before releasing."
                } else {
                    ""
                }
            ),
        ),
        PopulateOutcome::Errored => (
            outcome.tag().to_owned(),
            "An internal error prevented the run from completing.".to_owned(),
        ),
    }
}

/// Convert a [`PreviewOutcome`] (`boss project plan --dry-run`) into the
/// response event. Nothing was written; `outcome` tags are prefixed with
/// `preview_` so they are never confused with a real run's terminal
/// `planner_runs.outcome` value.
fn preview_to_event(project_id: String, preview: PreviewOutcome) -> FrontendEvent {
    match preview {
        PreviewOutcome::AlreadyPopulated { outcome } => FrontendEvent::PlanProjectResult {
            project_id,
            outcome: format!("preview_already_populated_as_{outcome}"),
            message: format!(
                "A live planner run already exists for this project (outcome: {outcome}). A real run would skip."
            ),
            created: 0,
            edges: 0,
            skipped: 0,
            run_id: None,
            proposal: None,
        },
        PreviewOutcome::PreSeeded { existing } => FrontendEvent::PlanProjectResult {
            project_id,
            outcome: "preview_pre_seeded".to_owned(),
            message: format!(
                "Project already has {existing} implementation task(s); a real run would refuse without --force."
            ),
            created: 0,
            edges: 0,
            skipped: 0,
            run_id: None,
            proposal: None,
        },
        PreviewOutcome::Terminal { outcome, message } => FrontendEvent::PlanProjectResult {
            project_id,
            outcome: format!("preview_{}", outcome.tag()),
            message,
            created: 0,
            edges: 0,
            skipped: 0,
            run_id: None,
            proposal: None,
        },
        PreviewOutcome::Valid { output, low_confidence } => {
            let created = output.tasks.len();
            let edges = output.edges.len();
            let message = format!(
                "Would stage {created} task(s) and {edges} edge(s).{}",
                if low_confidence {
                    " Planner flagged LOW CONFIDENCE in this plan."
                } else {
                    ""
                }
            );
            FrontendEvent::PlanProjectResult {
                project_id,
                outcome: "preview_valid".to_owned(),
                message,
                created,
                edges,
                skipped: 0,
                run_id: None,
                proposal: Some(output),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use boss_protocol::{Confidence, EffortLevel, PlannerOutput, ProposedEdge, ProposedTask, TaskKind};

    use super::*;

    /// Low-confidence warning fragment appended to the `Staged` message.
    const STAGED_LOW_CONFIDENCE: &str = "LOW CONFIDENCE";

    fn ptask(handle: &str) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: format!("task {handle}"),
            description: "do the thing".to_owned(),
            kind: TaskKind::ProjectTask,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn pedge(dependent: &str, prerequisite: &str) -> ProposedEdge {
        ProposedEdge {
            dependent: dependent.to_owned(),
            prerequisite: prerequisite.to_owned(),
        }
    }

    fn planner_output(tasks: Vec<ProposedTask>, edges: Vec<ProposedEdge>) -> PlannerOutput {
        PlannerOutput {
            tasks,
            edges,
            merge_order_hints: vec![],
            confidence: Confidence::High,
            breakdown_found: true,
            notes: String::new(),
            effort_audit: vec![],
        }
    }

    /// Destructure the `PlanProjectResult` event into its fields for
    /// field-level assertions. Panics if the event is any other variant.
    fn unwrap_plan_result(
        event: FrontendEvent,
    ) -> (
        String,
        String,
        String,
        usize,
        usize,
        usize,
        Option<String>,
        Option<PlannerOutput>,
    ) {
        match event {
            FrontendEvent::PlanProjectResult {
                project_id,
                outcome,
                message,
                created,
                edges,
                skipped,
                run_id,
                proposal,
            } => (project_id, outcome, message, created, edges, skipped, run_id, proposal),
            other => panic!("expected PlanProjectResult, got {other:?}"),
        }
    }

    // --- describe_populate_outcome: tag contract --------------------------

    /// Every variant's returned tag must equal `outcome.tag()` — the value
    /// persisted to `planner_runs.outcome` — so the CLI response and the
    /// audit row never diverge.
    #[test]
    fn describe_tag_matches_outcome_tag_for_every_variant() {
        let variants = [
            PopulateOutcome::SkippedAlreadyPopulated,
            PopulateOutcome::SkippedPreSeeded { existing: 3 },
            PopulateOutcome::NoBreakdown,
            PopulateOutcome::EmptyBreakdown,
            PopulateOutcome::RejectedTooMany { count: 40, max: 25 },
            PopulateOutcome::RejectedBadGraph,
            PopulateOutcome::DocMissing,
            PopulateOutcome::FetchFailed,
            PopulateOutcome::PlannerFailed,
            PopulateOutcome::Staged {
                created: 1,
                edges: 0,
                skipped: 0,
                low_confidence: false,
            },
            PopulateOutcome::Errored,
        ];
        for outcome in &variants {
            let (tag, message) = describe_populate_outcome(outcome);
            assert_eq!(tag, outcome.tag(), "tag mismatch for {outcome:?}");
            assert!(!message.is_empty(), "empty message for {outcome:?}");
        }
    }

    // --- describe_populate_outcome: interpolated values -------------------

    #[test]
    fn describe_pre_seeded_reports_existing_count() {
        let (tag, message) = describe_populate_outcome(&PopulateOutcome::SkippedPreSeeded { existing: 7 });
        assert_eq!(tag, "skipped_pre_seeded");
        assert!(message.contains('7'), "message missing existing count: {message}");
    }

    #[test]
    fn describe_rejected_too_many_reports_count_and_cap() {
        let (tag, message) = describe_populate_outcome(&PopulateOutcome::RejectedTooMany { count: 42, max: 25 });
        assert_eq!(tag, "rejected_too_many");
        assert!(message.contains("42"), "message missing proposed count: {message}");
        assert!(message.contains("25"), "message missing cap: {message}");
    }

    #[test]
    fn describe_staged_reports_created_edges_skipped_counts() {
        let (tag, message) = describe_populate_outcome(&PopulateOutcome::Staged {
            created: 5,
            edges: 4,
            skipped: 2,
            low_confidence: false,
        });
        assert_eq!(tag, "staged");
        assert!(message.contains('5'), "message missing created count: {message}");
        assert!(message.contains('4'), "message missing edges count: {message}");
        assert!(message.contains('2'), "message missing skipped count: {message}");
    }

    #[test]
    fn describe_staged_appends_low_confidence_warning_only_when_flagged() {
        let (_, high) = describe_populate_outcome(&PopulateOutcome::Staged {
            created: 3,
            edges: 1,
            skipped: 0,
            low_confidence: false,
        });
        assert!(
            !high.contains(STAGED_LOW_CONFIDENCE),
            "high-confidence message must not warn: {high}"
        );

        let (_, low) = describe_populate_outcome(&PopulateOutcome::Staged {
            created: 3,
            edges: 1,
            skipped: 0,
            low_confidence: true,
        });
        assert!(
            low.contains(STAGED_LOW_CONFIDENCE),
            "low-confidence message must warn: {low}"
        );
    }

    // --- preview_to_event: preview_ prefix + zeroed fields ----------------

    /// Preview tags must always carry the `preview_` prefix so they can never
    /// be confused with a real run's terminal `planner_runs.outcome` value.
    #[test]
    fn preview_tags_are_always_prefixed_preview() {
        let cases = vec![
            PreviewOutcome::AlreadyPopulated {
                outcome: "staged".to_owned(),
            },
            PreviewOutcome::PreSeeded { existing: 2 },
            PreviewOutcome::Terminal {
                outcome: PopulateOutcome::NoBreakdown,
                message: "no breakdown".to_owned(),
            },
            PreviewOutcome::Valid {
                output: planner_output(vec![ptask("a")], vec![]),
                low_confidence: false,
            },
        ];
        for preview in cases {
            let (_, outcome, ..) = unwrap_plan_result(preview_to_event("proj-1".to_owned(), preview));
            assert!(outcome.starts_with("preview_"), "tag not prefixed: {outcome}");
        }
    }

    #[test]
    fn preview_already_populated_embeds_live_outcome_and_zeroes_fields() {
        let event = preview_to_event(
            "proj-1".to_owned(),
            PreviewOutcome::AlreadyPopulated {
                outcome: "running".to_owned(),
            },
        );
        let (project_id, outcome, message, created, edges, skipped, run_id, proposal) = unwrap_plan_result(event);
        assert_eq!(project_id, "proj-1");
        assert_eq!(outcome, "preview_already_populated_as_running");
        assert!(message.contains("running"), "message missing live outcome: {message}");
        assert_eq!((created, edges, skipped), (0, 0, 0));
        assert_eq!(run_id, None);
        assert!(proposal.is_none());
    }

    #[test]
    fn preview_pre_seeded_reports_existing_and_zeroes_fields() {
        let event = preview_to_event("proj-1".to_owned(), PreviewOutcome::PreSeeded { existing: 9 });
        let (_, outcome, message, created, edges, skipped, run_id, proposal) = unwrap_plan_result(event);
        assert_eq!(outcome, "preview_pre_seeded");
        assert!(message.contains('9'), "message missing existing count: {message}");
        assert_eq!((created, edges, skipped), (0, 0, 0));
        assert_eq!(run_id, None);
        assert!(proposal.is_none());
    }

    #[test]
    fn preview_terminal_prefixes_inner_tag_and_passes_message_through() {
        let event = preview_to_event(
            "proj-1".to_owned(),
            PreviewOutcome::Terminal {
                outcome: PopulateOutcome::FetchFailed,
                message: "could not fetch the doc".to_owned(),
            },
        );
        let (_, outcome, message, created, edges, skipped, run_id, proposal) = unwrap_plan_result(event);
        assert_eq!(outcome, "preview_fetch_failed");
        assert_eq!(message, "could not fetch the doc");
        assert_eq!((created, edges, skipped), (0, 0, 0));
        assert_eq!(run_id, None);
        assert!(proposal.is_none());
    }

    #[test]
    fn preview_valid_carries_counts_and_proposal() {
        let output = planner_output(
            vec![ptask("a"), ptask("b"), ptask("c")],
            vec![pedge("b", "a"), pedge("c", "a")],
        );
        let event = preview_to_event(
            "proj-1".to_owned(),
            PreviewOutcome::Valid {
                output: output.clone(),
                low_confidence: false,
            },
        );
        let (_, outcome, message, created, edges, skipped, run_id, proposal) = unwrap_plan_result(event);
        assert_eq!(outcome, "preview_valid");
        // created/edges track the proposal's own vectors; skipped/run_id stay
        // at the documented zero/None (nothing was written for a preview).
        assert_eq!(created, output.tasks.len());
        assert_eq!(edges, output.edges.len());
        assert_eq!(skipped, 0);
        assert_eq!(run_id, None);
        let proposal = proposal.expect("valid preview must carry the proposal");
        assert_eq!(proposal.tasks.len(), output.tasks.len());
        assert_eq!(proposal.edges.len(), output.edges.len());
        assert!(
            !message.contains(STAGED_LOW_CONFIDENCE),
            "unexpected warning: {message}"
        );
    }

    #[test]
    fn preview_valid_appends_low_confidence_warning_only_when_flagged() {
        let output = planner_output(vec![ptask("a")], vec![]);

        let (_, _, high, ..) = unwrap_plan_result(preview_to_event(
            "proj-1".to_owned(),
            PreviewOutcome::Valid {
                output: output.clone(),
                low_confidence: false,
            },
        ));
        assert!(!high.contains(STAGED_LOW_CONFIDENCE), "must not warn: {high}");

        let (_, _, low, ..) = unwrap_plan_result(preview_to_event(
            "proj-1".to_owned(),
            PreviewOutcome::Valid {
                output,
                low_confidence: true,
            },
        ));
        assert!(low.contains(STAGED_LOW_CONFIDENCE), "must warn: {low}");
    }
}
