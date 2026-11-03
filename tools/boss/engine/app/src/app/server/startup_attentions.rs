//! Startup and supervisor attention publication.

use super::*;

/// Record a failed coordinator-supervisor pass: increment the consecutive
/// failure counter, raise the ceiling attention the first time it trips,
/// and return the delay until the next attempt.
pub(super) async fn note_coordinator_restart_failure(
    restart_failures: &mut boss_engine_runtime::coordinator_tmux::CoordinatorRestartFailures,
    server_state: &Arc<ServerState>,
) -> Duration {
    let decision = restart_failures.record_failure();
    if decision.notify {
        raise_coordinator_restart_ceiling_attention(server_state).await;
    }
    if restart_failures.consecutive_failures()
        >= boss_engine_runtime::coordinator_tmux::CoordinatorRestartFailures::LIMIT
    {
        tracing::error!(
            restart_failures = restart_failures.consecutive_failures(),
            "coordinator restart limit reached; still retrying every 60s"
        );
    }
    decision.delay
}

/// Shared fan-out for the engine-health attentions below: list active
/// products/projects, reconcile one attention per project with the given
/// distinguishing strings, and publish each created attention. `log_context`
/// prefixes the tracing labels so failures in each caller's log lines stay
/// distinguishable.
pub(super) async fn raise_project_scoped_engine_health_attention(
    server_state: &Arc<ServerState>,
    group_key_prefix: &str,
    source_doc_path: &str,
    prompt_text: String,
    log_context: &str,
) {
    let products = match server_state.work_db.list_products() {
        Ok(products) => products,
        Err(err) => {
            tracing::warn!(?err, "{log_context}: failed to list products for attention");
            return;
        }
    };
    for product in products.into_iter().filter(|product| product.status == "active") {
        let projects = match server_state.work_db.list_projects(&product.id, None) {
            Ok(projects) => projects,
            Err(err) => {
                tracing::warn!(
                    product_id = %product.id,
                    ?err,
                    "{log_context}: failed to list projects for attention"
                );
                continue;
            }
        };
        for project in projects {
            let input = boss_protocol::CreateAttentionInput::builder()
                .kind("question")
                .group_key(format!("{group_key_prefix}|{}", project.id))
                .association_project_id(project.id.clone())
                .source_kind("manual")
                .source_doc_path(source_doc_path)
                .question_type("prompt")
                .prompt_text(prompt_text.clone())
                .build();
            match server_state.work_db.reconcile_attentions(vec![input]) {
                Ok(Some((group, attentions))) => {
                    for attention in attentions {
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &group.product_id,
                                boss_protocol::FrontendEvent::AttentionCreated {
                                    attention,
                                    group: group.clone(),
                                },
                            )
                            .await;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        project_id = %project.id,
                        ?err,
                        "{log_context}: failed to create attention"
                    )
                }
            }
        }
    }
}

/// Surface a latched coordinator restart ceiling in each active project's
/// attention feed. The supervisor keeps retrying at the flat `BACKOFF_CAP`
/// cadence after the ceiling; this makes that condition visible to the
/// operator rather than only in tracing.
pub(super) async fn raise_coordinator_restart_ceiling_attention(server_state: &Arc<ServerState>) {
    raise_project_scoped_engine_health_attention(
        server_state,
        "engine_health_coordinator_restart",
        "engine-health/coordinator-restart",
        "Coordinator tmux recovery has hit repeated restart failures. \
         The engine keeps retrying every 60s; check the coordinator model \
         and auth if the session keeps failing to come back."
            .to_owned(),
        "coordinator restart ceiling",
    )
    .await;
}

/// Surface a coordinator that is successfully recreated over and over — the
/// tmux session comes back up each pass, but the `claude` child inside it
/// keeps dying immediately (bad model, expired auth, `claude` missing from
/// `PATH`), so the supervisor churns kill-and-recreate cycles with no
/// restart-failure counted. Distinct `group_key`/prompt from
/// [`raise_coordinator_restart_ceiling_attention`] so the two conditions
/// read as different incidents in the attention feed.
pub(super) async fn raise_coordinator_restart_churn_attention(server_state: &Arc<ServerState>) {
    raise_project_scoped_engine_health_attention(
        server_state,
        "engine_health_coordinator_restart_churn",
        "engine-health/coordinator-restart-churn",
        "The coordinator keeps dying and being recreated: the tmux session comes back \
         each pass but the claude process inside it exits immediately. Check the \
         coordinator model and auth."
            .to_owned(),
        "coordinator restart churn",
    )
    .await;
}

/// Surface a failed required-runtime check in each active project's attention
/// feed. Attentions require a work-item association, so project-scoped items
/// are the durable global-notification representation available to the app.
pub(super) async fn raise_tmux_preflight_attention(server_state: &Arc<ServerState>, reason: &str) {
    raise_project_scoped_engine_health_attention(
        server_state,
        "engine_health_tmux",
        "engine-health/tmux",
        format!("Local dispatch is blocked because tmux is unavailable: {reason}"),
        "tmux preflight",
    )
    .await;
}

/// Surface each Planner run interrupted by the previous engine as an
/// actionable follow-up. Design-triggered runs reuse Populator's stable group
/// key so the restart outcome appears with the other outcomes for that design.
pub(super) async fn raise_recovered_planner_run_attentions(
    server_state: &Arc<ServerState>,
    recovered: &[boss_protocol::PlannerRun],
) {
    for run in recovered {
        let title = "Planner run interrupted by engine restart";
        let body = format!(
            "Planner run {} for project {} was interrupted by an engine restart. Investigate and re-run the populate.",
            run.id, run.project_id
        );
        let input = match &run.design_task_id {
            Some(design_task_id) => boss_protocol::CreateAttentionInput::builder()
                .kind("followup")
                .group_key(format!(
                    "{}|{design_task_id}",
                    boss_engine_runtime::populator::ATTENTION_GROUP_KEY_PREFIX
                ))
                .association_task_id(design_task_id)
                .source_kind("manual")
                .source_task_id(design_task_id)
                .proposed_name(title)
                .proposed_description(body)
                .build(),
            None => boss_protocol::CreateAttentionInput::builder()
                .kind("followup")
                .group_key(format!("followup|planner_restart|{}", run.id))
                .association_project_id(run.project_id.clone())
                .source_kind("manual")
                .proposed_name(title)
                .proposed_description(body)
                .build(),
        };
        match server_state.work_db.create_attention(input) {
            Ok((attention, group)) => {
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &run.product_id,
                        boss_protocol::FrontendEvent::AttentionCreated { attention, group },
                    )
                    .await;
            }
            Err(err) => tracing::warn!(
                project_id = %run.project_id,
                run_id = %run.id,
                ?err,
                "failed to raise attention for planner run recovered at startup",
            ),
        }
    }
}
