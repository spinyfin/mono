//! The dispatch table for `handle_frontend_connection`: one match arm per
//! [`FrontendRequest`] variant, routing to its handler module. Split out of
//! `app.rs` to keep that file under the repo's file-size limit.

use std::future::Future;
use std::pin::Pin;

use crate::protocol::FrontendRequest;

use super::{
    Dispatch, attachments, attentions, automations, boothby, ci_remediation, comments, conflict_resolution, context,
    coordinator_handoff, cost, decisions, dependencies, design_docs, effort, engine_meta, executions, external_tracker,
    github_auth, hosts, ideas, live_status, metrics, panes, planner_ops, pr_status, products, projects, proposals,
    review, review_guide, selected_product, sessions, subscriptions, trunk_auth, work_items,
};

// Box each arm's future individually so the outer match holds a single
// pointer rather than a generator sized to the sum of every arm. Boxing only
// the whole `match` as one `async move` block still materializes the summed
// state machine as a stack temporary before moving it to the heap; the stack
// requirement grows with every new `FrontendRequest` variant and previously
// overflowed Linux CI's sandbox thread once Ideas added more variants
// (`control_verbs_test` aborted with `has overflowed its stack`).
pub(super) async fn dispatch_request(ctx: Dispatch, request: FrontendRequest) {
    let dispatch_fut: Pin<Box<dyn Future<Output = ()> + Send>> = match request {
        r @ FrontendRequest::AbandonCiRemediation { .. } => {
            Box::pin(ci_remediation::handle_abandon_ci_remediation(ctx, r))
        }
        r @ FrontendRequest::AbandonConflictResolution { .. } => {
            Box::pin(conflict_resolution::handle_abandon_conflict_resolution(ctx, r))
        }
        r @ FrontendRequest::AcceptDeferredScopeAttention { .. } => {
            Box::pin(attentions::handle_accept_deferred_scope_attention(ctx, r))
        }
        r @ FrontendRequest::ActionAttentionGroup { .. } => Box::pin(attentions::handle_action_attention_group(ctx, r)),
        r @ FrontendRequest::AddDependency { .. } => Box::pin(dependencies::handle_add_dependency(ctx, r)),
        r @ FrontendRequest::AddHost { .. } => Box::pin(hosts::handle_add_host(ctx, r)),
        r @ FrontendRequest::AddHostTag { .. } => Box::pin(hosts::handle_add_host_tag(ctx, r)),
        r @ FrontendRequest::AnswerAttention { .. } => Box::pin(attentions::handle_answer_attention(ctx, r)),
        r @ FrontendRequest::AuditProductEffort { .. } => Box::pin(effort::handle_audit_product_effort(ctx, r)),
        r @ FrontendRequest::CancelExecution { .. } => Box::pin(executions::handle_cancel_execution(ctx, r)),
        r @ FrontendRequest::ClassifyCiRemediation { .. } => {
            Box::pin(ci_remediation::handle_classify_ci_remediation(ctx, r))
        }
        r @ FrontendRequest::CommentsBannerState { .. } => Box::pin(comments::handle_comments_banner_state(ctx, r)),
        r @ FrontendRequest::CommentsCreate { .. } => Box::pin(comments::handle_comments_create(ctx, r)),
        r @ FrontendRequest::CommentsDismiss { .. } => Box::pin(comments::handle_comments_dismiss(ctx, r)),
        r @ FrontendRequest::CommentsGet { .. } => Box::pin(comments::handle_comments_get(ctx, r)),
        r @ FrontendRequest::CommentsList { .. } => Box::pin(comments::handle_comments_list(ctx, r)),
        r @ FrontendRequest::CommentsPostAnswer { .. } => Box::pin(comments::handle_comments_post_answer(ctx, r)),
        r @ FrontendRequest::CommentsPostFollowup { .. } => Box::pin(comments::handle_comments_post_followup(ctx, r)),
        r @ FrontendRequest::CommentsResolve { .. } => Box::pin(comments::handle_comments_resolve(ctx, r)),
        r @ FrontendRequest::CommentsReviseDoc { .. } => Box::pin(comments::handle_comments_revise_doc(ctx, r)),
        r @ FrontendRequest::CommentsSetIntent { .. } => Box::pin(comments::handle_comments_set_intent(ctx, r)),
        r @ FrontendRequest::CommentsSetStatus { .. } => Box::pin(comments::handle_comments_set_status(ctx, r)),
        r @ FrontendRequest::CommentsUpdateAnchor { .. } => Box::pin(comments::handle_comments_update_anchor(ctx, r)),
        r @ FrontendRequest::CreateAttention { .. } => Box::pin(attentions::handle_create_attention(ctx, r)),
        r @ FrontendRequest::CreateAttentionItem { .. } => Box::pin(attentions::handle_create_attention_item(ctx, r)),
        r @ FrontendRequest::CreateAutomation { .. } => Box::pin(automations::handle_create_automation(ctx, r)),
        r @ FrontendRequest::CreateAutomationTask { .. } => {
            Box::pin(automations::handle_create_automation_task(ctx, r))
        }
        r @ FrontendRequest::CreateChore { .. } => Box::pin(work_items::handle_create_chore(ctx, r)),
        r @ FrontendRequest::CreateDecision { .. } => Box::pin(decisions::handle_create_decision(ctx, r)),
        r @ FrontendRequest::CreateExecution { .. } => Box::pin(executions::handle_create_execution(ctx, r)),
        r @ FrontendRequest::CreateIdea { .. } => Box::pin(ideas::handle_create_idea(ctx, r)),
        r @ FrontendRequest::CreateInvestigation { .. } => Box::pin(work_items::handle_create_investigation(ctx, r)),
        r @ FrontendRequest::CreateManyChores { .. } => Box::pin(work_items::handle_create_many_chores(ctx, r)),
        r @ FrontendRequest::CreateManyTasks { .. } => Box::pin(work_items::handle_create_many_tasks(ctx, r)),
        r @ FrontendRequest::CreateProduct { .. } => Box::pin(products::handle_create_product(ctx, r)),
        r @ FrontendRequest::CreateProject { .. } => Box::pin(projects::handle_create_project(ctx, r)),
        r @ FrontendRequest::CreateRevision { .. } => Box::pin(work_items::handle_create_revision(ctx, r)),
        r @ FrontendRequest::CreateRun { .. } => Box::pin(executions::handle_create_run(ctx, r)),
        r @ FrontendRequest::CreateTask { .. } => Box::pin(work_items::handle_create_task(ctx, r)),
        r @ FrontendRequest::CreateTaskFromDeferredScopeAttention { .. } => {
            Box::pin(attentions::handle_create_task_from_deferred_scope_attention(ctx, r))
        }
        r @ FrontendRequest::DebugLiveStatusPipeline => {
            Box::pin(live_status::handle_debug_live_status_pipeline(ctx, r))
        }
        r @ FrontendRequest::DeleteAutomation { .. } => Box::pin(automations::handle_delete_automation(ctx, r)),
        r @ FrontendRequest::DeleteIdea { .. } => Box::pin(ideas::handle_delete_idea(ctx, r)),
        r @ FrontendRequest::DeleteWorkItem { .. } => Box::pin(work_items::handle_delete_work_item(ctx, r)),
        r @ FrontendRequest::DisableAutomation { .. } => Box::pin(automations::handle_disable_automation(ctx, r)),
        r @ FrontendRequest::DismissAttention { .. } => Box::pin(attentions::handle_dismiss_attention(ctx, r)),
        r @ FrontendRequest::EnableAutomation { .. } => Box::pin(automations::handle_enable_automation(ctx, r)),
        r @ FrontendRequest::EngineResponse { .. } => Box::pin(sessions::handle_engine_response(ctx, r)),
        r @ FrontendRequest::ExecutionTranscript { .. } => Box::pin(executions::handle_execution_transcript(ctx, r)),
        r @ FrontendRequest::FindWorkItemsByPr { .. } => Box::pin(work_items::handle_find_work_items_by_pr(ctx, r)),
        r @ FrontendRequest::FocusWorkerPane { .. } => Box::pin(panes::handle_focus_worker_pane(ctx, r)),
        r @ FrontendRequest::GetAttentionGroup { .. } => Box::pin(attentions::handle_get_attention_group(ctx, r)),
        r @ FrontendRequest::GetAttentionItem { .. } => Box::pin(attentions::handle_get_attention_item(ctx, r)),
        r @ FrontendRequest::GetAutomation { .. } => Box::pin(automations::handle_get_automation(ctx, r)),
        r @ FrontendRequest::GetAutomationOpenTaskCount { .. } => {
            Box::pin(automations::handle_get_automation_open_task_count(ctx, r))
        }
        r @ FrontendRequest::GetAutomationState => Box::pin(engine_meta::handle_get_automation_state(ctx, r)),
        r @ FrontendRequest::GetBoothbyState => Box::pin(boothby::handle_get_boothby_state(ctx, r)),
        r @ FrontendRequest::GetCiBudget { .. } => Box::pin(ci_remediation::handle_get_ci_budget(ctx, r)),
        r @ FrontendRequest::GetDriverQuotaUsage { .. } => Box::pin(engine_meta::handle_get_driver_quota_usage(ctx, r)),
        r @ FrontendRequest::GetDriverTrafficSplit => Box::pin(engine_meta::handle_get_driver_traffic_split(ctx, r)),
        r @ FrontendRequest::GetCiRemediation { .. } => Box::pin(ci_remediation::handle_get_ci_remediation(ctx, r)),
        r @ FrontendRequest::GetConflictHotspots { .. } => {
            Box::pin(conflict_resolution::handle_get_conflict_hotspots(ctx, r))
        }
        r @ FrontendRequest::GetConflictResolution { .. } => {
            Box::pin(conflict_resolution::handle_get_conflict_resolution(ctx, r))
        }
        r @ FrontendRequest::GetCoordinatorHandoff => {
            Box::pin(coordinator_handoff::handle_get_coordinator_handoff(ctx, r))
        }
        r @ FrontendRequest::GetCostWindowReport { .. } => Box::pin(cost::handle_get_cost_window_report(ctx, r)),
        r @ FrontendRequest::GetDecision { .. } => Box::pin(decisions::handle_get_decision(ctx, r)),
        r @ FrontendRequest::GetDispatchConcurrency => Box::pin(engine_meta::handle_get_dispatch_concurrency(ctx, r)),
        r @ FrontendRequest::GetDispatchState => Box::pin(engine_meta::handle_get_dispatch_state(ctx, r)),
        r @ FrontendRequest::GetEngineHealth => Box::pin(engine_meta::handle_get_engine_health(ctx, r)),
        r @ FrontendRequest::GetEngineVersion => Box::pin(engine_meta::handle_get_engine_version(ctx, r)),
        r @ FrontendRequest::GetExecution { .. } => Box::pin(executions::handle_get_execution(ctx, r)),
        r @ FrontendRequest::GetHost { .. } => Box::pin(hosts::handle_get_host(ctx, r)),
        r @ FrontendRequest::GetIdea { .. } => Box::pin(ideas::handle_get_idea(ctx, r)),
        r @ FrontendRequest::GetPrBody { .. } => Box::pin(pr_status::handle_get_pr_body(ctx, r)),
        r @ FrontendRequest::GetProductDesignDoc { .. } => Box::pin(design_docs::handle_get_product_design_doc(ctx, r)),
        r @ FrontendRequest::GetPrStatus { .. } => Box::pin(pr_status::handle_get_pr_status(ctx, r)),
        r @ FrontendRequest::GetReviewGuideContent { .. } => {
            Box::pin(review_guide::handle_get_review_guide_content(ctx, r))
        }
        r @ FrontendRequest::GetReviewGuideSummary { .. } => {
            Box::pin(review_guide::handle_get_review_guide_summary(ctx, r))
        }
        r @ FrontendRequest::GetRun { .. } => Box::pin(executions::handle_get_run(ctx, r)),
        r @ FrontendRequest::GetSelectedProduct => Box::pin(selected_product::handle_get_selected_product(ctx, r)),
        r @ FrontendRequest::GetSettings => Box::pin(engine_meta::handle_get_settings(ctx, r)),
        r @ FrontendRequest::GetTaskRuntime { .. } => Box::pin(executions::handle_get_task_runtime(ctx, r)),
        r @ FrontendRequest::GetTopCostConsumers { .. } => Box::pin(cost::handle_get_top_cost_consumers(ctx, r)),
        r @ FrontendRequest::GetWorkerContext { .. } => Box::pin(context::handle_get_worker_context(ctx, r)),
        r @ FrontendRequest::GetWorkItem { .. } => Box::pin(work_items::handle_get_work_item(ctx, r)),
        r @ FrontendRequest::GetWorkItemByShortId { .. } => {
            Box::pin(work_items::handle_get_work_item_by_short_id(ctx, r))
        }
        r @ FrontendRequest::GetWorkItemCostReport { .. } => Box::pin(cost::handle_get_work_item_cost_report(ctx, r)),
        r @ FrontendRequest::GetWorkTree { .. } => Box::pin(work_items::handle_get_work_tree(ctx, r)),
        r @ FrontendRequest::GitHubAuthCancel => Box::pin(github_auth::handle_git_hub_auth_cancel(ctx, r)),
        r @ FrontendRequest::GitHubAuthDisconnect => Box::pin(github_auth::handle_git_hub_auth_disconnect(ctx, r)),
        r @ FrontendRequest::GitHubAuthStart => Box::pin(github_auth::handle_git_hub_auth_start(ctx, r)),
        r @ FrontendRequest::GitHubAuthStatus => Box::pin(github_auth::handle_git_hub_auth_status(ctx, r)),
        r @ FrontendRequest::GraduateIdea { .. } => Box::pin(ideas::handle_graduate_idea(ctx, r)),
        r @ FrontendRequest::HoldRun { .. } => Box::pin(executions::handle_hold_run(ctx, r)),
        r @ FrontendRequest::InterruptWorkerPane { .. } => Box::pin(panes::handle_interrupt_worker_pane(ctx, r)),
        r @ FrontendRequest::KickPrReconcilers => Box::pin(engine_meta::handle_kick_pr_reconcilers(ctx, r)),
        r @ FrontendRequest::LinkWorkItemExternalRef { .. } => {
            Box::pin(external_tracker::handle_link_work_item_external_ref(ctx, r))
        }
        r @ FrontendRequest::ListAnswerAgentRuns { .. } => Box::pin(comments::handle_list_answer_agent_runs(ctx, r)),
        r @ FrontendRequest::ListAttentionGroups { .. } => Box::pin(attentions::handle_list_attention_groups(ctx, r)),
        r @ FrontendRequest::ListAttentionItems { .. } => Box::pin(attentions::handle_list_attention_items(ctx, r)),
        r @ FrontendRequest::ListAttentionItemsForWorkItem { .. } => {
            Box::pin(attentions::handle_list_attention_items_for_work_item(ctx, r))
        }
        r @ FrontendRequest::ListAttentionMerges { .. } => Box::pin(attentions::handle_list_attention_merges(ctx, r)),
        r @ FrontendRequest::ListAutomationDedupSuppressions { .. } => {
            Box::pin(automations::handle_list_automation_dedup_suppressions(ctx, r))
        }
        r @ FrontendRequest::ListAutomationRuns { .. } => Box::pin(automations::handle_list_automation_runs(ctx, r)),
        r @ FrontendRequest::ListAutomations { .. } => Box::pin(automations::handle_list_automations(ctx, r)),
        r @ FrontendRequest::ListAutomationTasks { .. } => Box::pin(automations::handle_list_automation_tasks(ctx, r)),
        r @ FrontendRequest::ListBoothbyPasses { .. } => Box::pin(boothby::handle_list_boothby_passes(ctx, r)),
        r @ FrontendRequest::ListChores { .. } => Box::pin(work_items::handle_list_chores(ctx, r)),
        r @ FrontendRequest::ListCiRemediations { .. } => Box::pin(ci_remediation::handle_list_ci_remediations(ctx, r)),
        r @ FrontendRequest::ListConflictResolutions { .. } => {
            Box::pin(conflict_resolution::handle_list_conflict_resolutions(ctx, r))
        }
        r @ FrontendRequest::ListDecisions { .. } => Box::pin(decisions::handle_list_decisions(ctx, r)),
        r @ FrontendRequest::ListDeferredScopeAttentions { .. } => {
            Box::pin(attentions::handle_list_deferred_scope_attentions(ctx, r))
        }
        r @ FrontendRequest::ListDependencies { .. } => Box::pin(dependencies::handle_list_dependencies(ctx, r)),
        r @ FrontendRequest::ListDependenciesDetailed { .. } => {
            Box::pin(dependencies::handle_list_dependencies_detailed(ctx, r))
        }
        r @ FrontendRequest::ListEditorialActions { .. } => {
            Box::pin(automations::handle_list_editorial_actions(ctx, r))
        }
        r @ FrontendRequest::ListEngineAttempts { .. } => Box::pin(executions::handle_list_engine_attempts(ctx, r)),
        r @ FrontendRequest::ListExecutions { .. } => Box::pin(executions::handle_list_executions(ctx, r)),
        r @ FrontendRequest::ListFeatureFlags => Box::pin(engine_meta::handle_list_feature_flags(ctx, r)),
        r @ FrontendRequest::ListHosts => Box::pin(hosts::handle_list_hosts(ctx, r)),
        r @ FrontendRequest::ListHostedPaneStatuses => Box::pin(panes::handle_list_hosted_pane_statuses(ctx, r)),
        r @ FrontendRequest::ListIdeas { .. } => Box::pin(ideas::handle_list_ideas(ctx, r)),
        r @ FrontendRequest::ListLiveStatusDisabledSlots => {
            Box::pin(live_status::handle_list_live_status_disabled_slots(ctx, r))
        }
        r @ FrontendRequest::ListPlannerRuns { .. } => Box::pin(planner_ops::handle_list_planner_runs(ctx, r)),
        r @ FrontendRequest::ListProductDesignDocs { .. } => {
            Box::pin(design_docs::handle_list_product_design_docs(ctx, r))
        }
        r @ FrontendRequest::ListProducts => Box::pin(products::handle_list_products(ctx, r)),
        r @ FrontendRequest::ListProjects { .. } => Box::pin(projects::handle_list_projects(ctx, r)),
        r @ FrontendRequest::ListAttachments { .. } => Box::pin(attachments::handle_list_attachments(ctx, r)),
        r @ FrontendRequest::ListAttachmentsForWorkItem { .. } => {
            Box::pin(attachments::handle_list_attachments_for_work_item(ctx, r))
        }
        r @ FrontendRequest::ListProposals { .. } => Box::pin(proposals::handle_list_proposals(ctx, r)),
        r @ FrontendRequest::ListRuns { .. } => Box::pin(executions::handle_list_runs(ctx, r)),
        r @ FrontendRequest::ListTasks { .. } => Box::pin(work_items::handle_list_tasks(ctx, r)),
        r @ FrontendRequest::ListRevisions { .. } => Box::pin(work_items::handle_list_revisions(ctx, r)),
        r @ FrontendRequest::ListWorkerLiveStates => Box::pin(panes::handle_list_worker_live_states(ctx, r)),
        r @ FrontendRequest::ListTmuxWorkerStatuses => Box::pin(panes::handle_list_tmux_worker_statuses(ctx, r)),
        r @ FrontendRequest::MarkCiRemediationFailed { .. } => {
            Box::pin(ci_remediation::handle_mark_ci_remediation_failed(ctx, r))
        }
        r @ FrontendRequest::MarkCiRemediationNoop { .. } => {
            Box::pin(ci_remediation::handle_mark_ci_remediation_noop(ctx, r))
        }
        r @ FrontendRequest::MarkCiRemediationRetriggered { .. } => {
            Box::pin(ci_remediation::handle_mark_ci_remediation_retriggered(ctx, r))
        }
        r @ FrontendRequest::MarkCiRemediationSucceededViaRebase { .. } => {
            Box::pin(ci_remediation::handle_mark_ci_remediation_succeeded_via_rebase(ctx, r))
        }
        r @ FrontendRequest::MarkConflictResolutionFailed { .. } => {
            Box::pin(conflict_resolution::handle_mark_conflict_resolution_failed(ctx, r))
        }
        r @ FrontendRequest::MergeWhenReady { .. } => Box::pin(review::handle_merge_when_ready(ctx, r)),
        r @ FrontendRequest::MetricsListLive => Box::pin(metrics::handle_metrics_list_live(ctx, r)),
        r @ FrontendRequest::MetricsReset { .. } => Box::pin(metrics::handle_metrics_reset(ctx, r)),
        r @ FrontendRequest::MetricsShowLive { .. } => Box::pin(metrics::handle_metrics_show_live(ctx, r)),
        r @ FrontendRequest::OpenDocument { .. } => Box::pin(panes::handle_open_document(ctx, r)),
        r @ FrontendRequest::OpenLiveWorkspaceTerminal { .. } => {
            Box::pin(review::handle_open_live_workspace_terminal(ctx, r))
        }
        r @ FrontendRequest::OpenReviewTerminal { .. } => Box::pin(review::handle_open_review_terminal(ctx, r)),
        r @ FrontendRequest::PlanProject { .. } => Box::pin(planner_ops::handle_plan_project(ctx, r)),
        r @ FrontendRequest::ProbeRun { .. } => Box::pin(executions::handle_probe_run(ctx, r)),
        r @ FrontendRequest::ProbeStatus { .. } => Box::pin(executions::handle_probe_status(ctx, r)),
        r @ FrontendRequest::ReapRun { .. } => Box::pin(executions::handle_reap_run(ctx, r)),
        r @ FrontendRequest::RecordEffortEscalation { .. } => Box::pin(effort::handle_record_effort_escalation(ctx, r)),
        r @ FrontendRequest::RecordProducerSideConflict { .. } => {
            Box::pin(conflict_resolution::handle_record_producer_side_conflict(ctx, r))
        }
        r @ FrontendRequest::RecreateCoordinator { .. } => Box::pin(sessions::handle_recreate_coordinator(ctx, r)),
        r @ FrontendRequest::RegisterAppSession => Box::pin(sessions::handle_register_app_session(ctx, r)),
        r @ FrontendRequest::RegisterCapabilities { .. } => Box::pin(engine_meta::handle_register_capabilities(ctx, r)),
        r @ FrontendRequest::ReleaseHoldRun { .. } => Box::pin(executions::handle_release_hold_run(ctx, r)),
        r @ FrontendRequest::ReleaseProject { .. } => Box::pin(planner_ops::handle_release_project(ctx, r)),
        r @ FrontendRequest::ReleaseReviewTerminal { .. } => Box::pin(review::handle_release_review_terminal(ctx, r)),
        r @ FrontendRequest::RemoveDependency { .. } => Box::pin(dependencies::handle_remove_dependency(ctx, r)),
        r @ FrontendRequest::RemoveHost { .. } => Box::pin(hosts::handle_remove_host(ctx, r)),
        r @ FrontendRequest::RemoveHostTag { .. } => Box::pin(hosts::handle_remove_host_tag(ctx, r)),
        r @ FrontendRequest::ReorderProjectTasks { .. } => Box::pin(projects::handle_reorder_project_tasks(ctx, r)),
        r @ FrontendRequest::RequestExecution { .. } => Box::pin(executions::handle_request_execution(ctx, r)),
        r @ FrontendRequest::ResolveProjectDesignDoc { .. } => {
            Box::pin(projects::handle_resolve_project_design_doc(ctx, r))
        }
        r @ FrontendRequest::RestoreWorkItem { .. } => Box::pin(work_items::handle_restore_work_item(ctx, r)),
        r @ FrontendRequest::RetirePane { .. } => Box::pin(panes::handle_retire_pane(ctx, r)),
        r @ FrontendRequest::RetryCiRemediation { .. } => Box::pin(ci_remediation::handle_retry_ci_remediation(ctx, r)),
        r @ FrontendRequest::RetryConflictResolution { .. } => {
            Box::pin(conflict_resolution::handle_retry_conflict_resolution(ctx, r))
        }
        r @ FrontendRequest::RetryReviewGuide { .. } => Box::pin(review_guide::handle_retry_review_guide(ctx, r)),
        r @ FrontendRequest::RevealWorkItem { .. } => Box::pin(work_items::handle_reveal_work_item(ctx, r)),
        r @ FrontendRequest::RevokeDecision { .. } => Box::pin(decisions::handle_revoke_decision(ctx, r)),
        r @ FrontendRequest::RunAutomation { .. } => Box::pin(automations::handle_run_automation(ctx, r)),
        r @ FrontendRequest::RunBoothbyPass => Box::pin(boothby::handle_run_boothby_pass(ctx, r)),
        r @ FrontendRequest::SendInputToWorker { .. } => Box::pin(panes::handle_send_input_to_worker(ctx, r)),
        r @ FrontendRequest::SetAutomationPaused { .. } => Box::pin(engine_meta::handle_set_automation_paused(ctx, r)),
        r @ FrontendRequest::SetBoothbyMode { .. } => Box::pin(boothby::handle_set_boothby_mode(ctx, r)),
        r @ FrontendRequest::SetCiBudget { .. } => Box::pin(ci_remediation::handle_set_ci_budget(ctx, r)),
        r @ FrontendRequest::SetDriverTrafficSplit { .. } => {
            Box::pin(engine_meta::handle_set_driver_traffic_split(ctx, r))
        }
        r @ FrontendRequest::SetDispatchConcurrency { .. } => {
            Box::pin(engine_meta::handle_set_dispatch_concurrency(ctx, r))
        }
        r @ FrontendRequest::SetDispatchPaused { .. } => Box::pin(engine_meta::handle_set_dispatch_paused(ctx, r)),
        r @ FrontendRequest::SetFeatureFlag { .. } => Box::pin(engine_meta::handle_set_feature_flag(ctx, r)),
        r @ FrontendRequest::SetHostEnabled { .. } => Box::pin(hosts::handle_set_host_enabled(ctx, r)),
        r @ FrontendRequest::SetLiveStatusEnabled { .. } => {
            Box::pin(live_status::handle_set_live_status_enabled(ctx, r))
        }
        r @ FrontendRequest::SetProductDefaultModel { .. } => {
            Box::pin(products::handle_set_product_default_model(ctx, r))
        }
        r @ FrontendRequest::SetProductDefaultDriver { .. } => {
            Box::pin(products::handle_set_product_default_driver(ctx, r))
        }
        r @ FrontendRequest::SetProductMergeMechanism { .. } => {
            Box::pin(products::handle_set_product_merge_mechanism(ctx, r))
        }
        r @ FrontendRequest::SetProductEditorialRules { .. } => {
            Box::pin(products::handle_set_product_editorial_rules(ctx, r))
        }
        r @ FrontendRequest::EvaluateEditorialRules { .. } => {
            Box::pin(products::handle_evaluate_editorial_rules(ctx, r))
        }
        r @ FrontendRequest::EvaluateDispatchAdmission { .. } => {
            Box::pin(executions::handle_evaluate_dispatch_admission(ctx, r))
        }
        r @ FrontendRequest::SetProductExternalTracker { .. } => {
            Box::pin(external_tracker::handle_set_product_external_tracker(ctx, r))
        }
        r @ FrontendRequest::SetCoordinatorHandoff { .. } => {
            Box::pin(coordinator_handoff::handle_set_coordinator_handoff(ctx, r))
        }
        r @ FrontendRequest::SetProjectDesignDoc { .. } => Box::pin(projects::handle_set_project_design_doc(ctx, r)),
        r @ FrontendRequest::SetSetting { .. } => Box::pin(engine_meta::handle_set_setting(ctx, r)),
        r @ FrontendRequest::SetTaskDocPointer { .. } => Box::pin(work_items::handle_set_task_doc_pointer(ctx, r)),
        r @ FrontendRequest::Shutdown { .. } => Box::pin(sessions::handle_shutdown(ctx, r)),
        r @ FrontendRequest::SpawnCapabilityRestored => Box::pin(sessions::handle_spawn_capability_restored(ctx, r)),
        r @ FrontendRequest::StopRun { .. } => Box::pin(executions::handle_stop_run(ctx, r)),
        r @ FrontendRequest::SubmitAttachment { .. } => Box::pin(attachments::handle_submit_attachment(ctx, r)),
        r @ FrontendRequest::SubmitProposal { .. } => Box::pin(proposals::handle_submit_proposal(ctx, r)),
        r @ FrontendRequest::Subscribe { .. } => Box::pin(subscriptions::handle_subscribe(ctx, r)),
        r @ FrontendRequest::SupersedeDecision { .. } => Box::pin(decisions::handle_supersede_decision(ctx, r)),
        r @ FrontendRequest::SyncProductExternalTracker { .. } => {
            Box::pin(external_tracker::handle_sync_product_external_tracker(ctx, r))
        }
        r @ FrontendRequest::TailRunTranscript { .. } => Box::pin(executions::handle_tail_run_transcript(ctx, r)),
        r @ FrontendRequest::TriggerPrReview { .. } => Box::pin(review::handle_trigger_pr_review(ctx, r)),
        r @ FrontendRequest::TrunkSetToken { .. } => Box::pin(trunk_auth::handle_trunk_set_token(ctx, r)),
        r @ FrontendRequest::TrunkStatus => Box::pin(trunk_auth::handle_trunk_status(ctx, r)),
        r @ FrontendRequest::UnlinkWorkItemExternalRef { .. } => {
            Box::pin(external_tracker::handle_unlink_work_item_external_ref(ctx, r))
        }
        r @ FrontendRequest::UnpopulateProject { .. } => Box::pin(planner_ops::handle_unpopulate_project(ctx, r)),
        r @ FrontendRequest::Unsubscribe { .. } => Box::pin(subscriptions::handle_unsubscribe(ctx, r)),
        r @ FrontendRequest::UpdateAutomation { .. } => Box::pin(automations::handle_update_automation(ctx, r)),
        r @ FrontendRequest::UpdateIdea { .. } => Box::pin(ideas::handle_update_idea(ctx, r)),
        r @ FrontendRequest::MoveWorkItemOnBoard { .. } => Box::pin(work_items::handle_move_work_item_on_board(ctx, r)),
        r @ FrontendRequest::UpdateWorkItem { .. } => Box::pin(work_items::handle_update_work_item(ctx, r)),
        r @ FrontendRequest::ReportSelectedProduct { .. } => {
            Box::pin(selected_product::handle_report_selected_product(ctx, r))
        }
        r @ FrontendRequest::ReportWorkerSpawnFailed { .. } => {
            Box::pin(sessions::handle_report_worker_spawn_failed(ctx, r))
        }
        r @ FrontendRequest::UpdateWorkerShellPid { .. } => Box::pin(sessions::handle_update_worker_shell_pid(ctx, r)),
        r @ FrontendRequest::WorkerPaneDied { .. } => Box::pin(sessions::handle_worker_pane_died(ctx, r)),
        r @ FrontendRequest::WorkerPoolSummary => Box::pin(engine_meta::handle_worker_pool_summary(ctx, r)),
        r @ FrontendRequest::WorkspacePoolSummary => Box::pin(engine_meta::handle_workspace_pool_summary(ctx, r)),
    };
    dispatch_fut.await;
}
