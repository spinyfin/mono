//! The dispatch table for `handle_frontend_connection`: one match arm per
//! [`FrontendRequest`] variant, routing to its handler module. Split out of
//! `app.rs` to keep that file under the repo's file-size limit.

use crate::protocol::FrontendRequest;

use super::{
    Dispatch, attachments, attentions, automations, boothby, ci_remediation, comments, conflict_resolution, context,
    coordinator_handoff, cost, decisions, dependencies, design_docs, effort, engine_meta, executions, external_tracker,
    github_auth, hosts, ideas, live_status, metrics, panes, planner_ops, pr_status, products, projects, proposals,
    review, selected_product, sessions, subscriptions, trunk_auth, work_items,
};

pub(super) async fn dispatch_request(ctx: Dispatch, request: FrontendRequest) {
    match request {
        r @ FrontendRequest::AbandonCiRemediation { .. } => ci_remediation::handle_abandon_ci_remediation(ctx, r).await,
        r @ FrontendRequest::AbandonConflictResolution { .. } => {
            conflict_resolution::handle_abandon_conflict_resolution(ctx, r).await
        }
        r @ FrontendRequest::AcceptDeferredScopeAttention { .. } => {
            attentions::handle_accept_deferred_scope_attention(ctx, r).await
        }
        r @ FrontendRequest::ActionAttentionGroup { .. } => attentions::handle_action_attention_group(ctx, r).await,
        r @ FrontendRequest::AddDependency { .. } => dependencies::handle_add_dependency(ctx, r).await,
        r @ FrontendRequest::AddHost { .. } => hosts::handle_add_host(ctx, r).await,
        r @ FrontendRequest::AddHostTag { .. } => hosts::handle_add_host_tag(ctx, r).await,
        r @ FrontendRequest::AnswerAttention { .. } => attentions::handle_answer_attention(ctx, r).await,
        r @ FrontendRequest::AuditProductEffort { .. } => effort::handle_audit_product_effort(ctx, r).await,
        r @ FrontendRequest::CancelExecution { .. } => executions::handle_cancel_execution(ctx, r).await,
        r @ FrontendRequest::ClassifyCiRemediation { .. } => {
            ci_remediation::handle_classify_ci_remediation(ctx, r).await
        }
        r @ FrontendRequest::CommentsBannerState { .. } => comments::handle_comments_banner_state(ctx, r).await,
        r @ FrontendRequest::CommentsCreate { .. } => comments::handle_comments_create(ctx, r).await,
        r @ FrontendRequest::CommentsDismiss { .. } => comments::handle_comments_dismiss(ctx, r).await,
        r @ FrontendRequest::CommentsGet { .. } => comments::handle_comments_get(ctx, r).await,
        r @ FrontendRequest::CommentsList { .. } => comments::handle_comments_list(ctx, r).await,
        r @ FrontendRequest::CommentsPostAnswer { .. } => comments::handle_comments_post_answer(ctx, r).await,
        r @ FrontendRequest::CommentsPostFollowup { .. } => comments::handle_comments_post_followup(ctx, r).await,
        r @ FrontendRequest::CommentsResolve { .. } => comments::handle_comments_resolve(ctx, r).await,
        r @ FrontendRequest::CommentsReviseDoc { .. } => comments::handle_comments_revise_doc(ctx, r).await,
        r @ FrontendRequest::CommentsSetIntent { .. } => comments::handle_comments_set_intent(ctx, r).await,
        r @ FrontendRequest::CommentsSetStatus { .. } => comments::handle_comments_set_status(ctx, r).await,
        r @ FrontendRequest::CommentsUpdateAnchor { .. } => comments::handle_comments_update_anchor(ctx, r).await,
        r @ FrontendRequest::CreateAttention { .. } => attentions::handle_create_attention(ctx, r).await,
        r @ FrontendRequest::CreateAttentionItem { .. } => attentions::handle_create_attention_item(ctx, r).await,
        r @ FrontendRequest::CreateAutomation { .. } => automations::handle_create_automation(ctx, r).await,
        r @ FrontendRequest::CreateAutomationTask { .. } => automations::handle_create_automation_task(ctx, r).await,
        r @ FrontendRequest::CreateChore { .. } => work_items::handle_create_chore(ctx, r).await,
        r @ FrontendRequest::CreateDecision { .. } => decisions::handle_create_decision(ctx, r).await,
        r @ FrontendRequest::CreateExecution { .. } => executions::handle_create_execution(ctx, r).await,
        r @ FrontendRequest::CreateIdea { .. } => ideas::handle_create_idea(ctx, r).await,
        r @ FrontendRequest::CreateInvestigation { .. } => work_items::handle_create_investigation(ctx, r).await,
        r @ FrontendRequest::CreateManyChores { .. } => work_items::handle_create_many_chores(ctx, r).await,
        r @ FrontendRequest::CreateManyTasks { .. } => work_items::handle_create_many_tasks(ctx, r).await,
        r @ FrontendRequest::CreateProduct { .. } => products::handle_create_product(ctx, r).await,
        r @ FrontendRequest::CreateProject { .. } => projects::handle_create_project(ctx, r).await,
        r @ FrontendRequest::CreateRevision { .. } => work_items::handle_create_revision(ctx, r).await,
        r @ FrontendRequest::CreateRun { .. } => executions::handle_create_run(ctx, r).await,
        r @ FrontendRequest::CreateTask { .. } => work_items::handle_create_task(ctx, r).await,
        r @ FrontendRequest::CreateTaskFromDeferredScopeAttention { .. } => {
            attentions::handle_create_task_from_deferred_scope_attention(ctx, r).await
        }
        r @ FrontendRequest::DebugLiveStatusPipeline => live_status::handle_debug_live_status_pipeline(ctx, r).await,
        r @ FrontendRequest::DeleteAutomation { .. } => automations::handle_delete_automation(ctx, r).await,
        r @ FrontendRequest::DeleteIdea { .. } => ideas::handle_delete_idea(ctx, r).await,
        r @ FrontendRequest::DeleteWorkItem { .. } => work_items::handle_delete_work_item(ctx, r).await,
        r @ FrontendRequest::DisableAutomation { .. } => automations::handle_disable_automation(ctx, r).await,
        r @ FrontendRequest::DismissAttention { .. } => attentions::handle_dismiss_attention(ctx, r).await,
        r @ FrontendRequest::EnableAutomation { .. } => automations::handle_enable_automation(ctx, r).await,
        r @ FrontendRequest::EngineResponse { .. } => sessions::handle_engine_response(ctx, r).await,
        r @ FrontendRequest::ExecutionTranscript { .. } => executions::handle_execution_transcript(ctx, r).await,
        r @ FrontendRequest::FindWorkItemsByPr { .. } => work_items::handle_find_work_items_by_pr(ctx, r).await,
        r @ FrontendRequest::FocusWorkerPane { .. } => panes::handle_focus_worker_pane(ctx, r).await,
        r @ FrontendRequest::GetAttentionGroup { .. } => attentions::handle_get_attention_group(ctx, r).await,
        r @ FrontendRequest::GetAttentionItem { .. } => attentions::handle_get_attention_item(ctx, r).await,
        r @ FrontendRequest::GetAutomation { .. } => automations::handle_get_automation(ctx, r).await,
        r @ FrontendRequest::GetAutomationOpenTaskCount { .. } => {
            automations::handle_get_automation_open_task_count(ctx, r).await
        }
        r @ FrontendRequest::GetAutomationState => engine_meta::handle_get_automation_state(ctx, r).await,
        r @ FrontendRequest::GetBoothbyState => boothby::handle_get_boothby_state(ctx, r).await,
        r @ FrontendRequest::GetCiBudget { .. } => ci_remediation::handle_get_ci_budget(ctx, r).await,
        r @ FrontendRequest::GetDriverQuotaUsage { .. } => engine_meta::handle_get_driver_quota_usage(ctx, r).await,
        r @ FrontendRequest::GetDriverTrafficSplit => engine_meta::handle_get_driver_traffic_split(ctx, r).await,
        r @ FrontendRequest::GetCiRemediation { .. } => ci_remediation::handle_get_ci_remediation(ctx, r).await,
        r @ FrontendRequest::GetConflictHotspots { .. } => {
            conflict_resolution::handle_get_conflict_hotspots(ctx, r).await
        }
        r @ FrontendRequest::GetConflictResolution { .. } => {
            conflict_resolution::handle_get_conflict_resolution(ctx, r).await
        }
        r @ FrontendRequest::GetCoordinatorHandoff => coordinator_handoff::handle_get_coordinator_handoff(ctx, r).await,
        r @ FrontendRequest::GetCostWindowReport { .. } => cost::handle_get_cost_window_report(ctx, r).await,
        r @ FrontendRequest::GetDecision { .. } => decisions::handle_get_decision(ctx, r).await,
        r @ FrontendRequest::GetDispatchConcurrency => engine_meta::handle_get_dispatch_concurrency(ctx, r).await,
        r @ FrontendRequest::GetDispatchState => engine_meta::handle_get_dispatch_state(ctx, r).await,
        r @ FrontendRequest::GetEngineHealth => engine_meta::handle_get_engine_health(ctx, r).await,
        r @ FrontendRequest::GetEngineVersion => engine_meta::handle_get_engine_version(ctx, r).await,
        r @ FrontendRequest::GetExecution { .. } => executions::handle_get_execution(ctx, r).await,
        r @ FrontendRequest::GetHost { .. } => hosts::handle_get_host(ctx, r).await,
        r @ FrontendRequest::GetIdea { .. } => ideas::handle_get_idea(ctx, r).await,
        r @ FrontendRequest::GetPrBody { .. } => pr_status::handle_get_pr_body(ctx, r).await,
        r @ FrontendRequest::GetProductDesignDoc { .. } => design_docs::handle_get_product_design_doc(ctx, r).await,
        r @ FrontendRequest::GetPrStatus { .. } => pr_status::handle_get_pr_status(ctx, r).await,
        r @ FrontendRequest::GetRun { .. } => executions::handle_get_run(ctx, r).await,
        r @ FrontendRequest::GetSelectedProduct => selected_product::handle_get_selected_product(ctx, r).await,
        r @ FrontendRequest::GetSettings => engine_meta::handle_get_settings(ctx, r).await,
        r @ FrontendRequest::GetTaskRuntime { .. } => executions::handle_get_task_runtime(ctx, r).await,
        r @ FrontendRequest::GetTopCostConsumers { .. } => cost::handle_get_top_cost_consumers(ctx, r).await,
        r @ FrontendRequest::GetWorkerContext { .. } => context::handle_get_worker_context(ctx, r).await,
        r @ FrontendRequest::GetWorkItem { .. } => work_items::handle_get_work_item(ctx, r).await,
        r @ FrontendRequest::GetWorkItemByShortId { .. } => work_items::handle_get_work_item_by_short_id(ctx, r).await,
        r @ FrontendRequest::GetWorkItemCostReport { .. } => cost::handle_get_work_item_cost_report(ctx, r).await,
        r @ FrontendRequest::GetWorkTree { .. } => work_items::handle_get_work_tree(ctx, r).await,
        r @ FrontendRequest::GitHubAuthCancel => github_auth::handle_git_hub_auth_cancel(ctx, r).await,
        r @ FrontendRequest::GitHubAuthDisconnect => github_auth::handle_git_hub_auth_disconnect(ctx, r).await,
        r @ FrontendRequest::GitHubAuthStart => github_auth::handle_git_hub_auth_start(ctx, r).await,
        r @ FrontendRequest::GitHubAuthStatus => github_auth::handle_git_hub_auth_status(ctx, r).await,
        r @ FrontendRequest::GraduateIdea { .. } => ideas::handle_graduate_idea(ctx, r).await,
        r @ FrontendRequest::HoldRun { .. } => executions::handle_hold_run(ctx, r).await,
        r @ FrontendRequest::InterruptWorkerPane { .. } => panes::handle_interrupt_worker_pane(ctx, r).await,
        r @ FrontendRequest::KickPrReconcilers => engine_meta::handle_kick_pr_reconcilers(ctx, r).await,
        r @ FrontendRequest::LinkWorkItemExternalRef { .. } => {
            external_tracker::handle_link_work_item_external_ref(ctx, r).await
        }
        r @ FrontendRequest::ListAnswerAgentRuns { .. } => comments::handle_list_answer_agent_runs(ctx, r).await,
        r @ FrontendRequest::ListAttentionGroups { .. } => attentions::handle_list_attention_groups(ctx, r).await,
        r @ FrontendRequest::ListAttentionItems { .. } => attentions::handle_list_attention_items(ctx, r).await,
        r @ FrontendRequest::ListAttentionItemsForWorkItem { .. } => {
            attentions::handle_list_attention_items_for_work_item(ctx, r).await
        }
        r @ FrontendRequest::ListAttentionMerges { .. } => attentions::handle_list_attention_merges(ctx, r).await,
        r @ FrontendRequest::ListAutomationDedupSuppressions { .. } => {
            automations::handle_list_automation_dedup_suppressions(ctx, r).await
        }
        r @ FrontendRequest::ListAutomationRuns { .. } => automations::handle_list_automation_runs(ctx, r).await,
        r @ FrontendRequest::ListAutomations { .. } => automations::handle_list_automations(ctx, r).await,
        r @ FrontendRequest::ListAutomationTasks { .. } => automations::handle_list_automation_tasks(ctx, r).await,
        r @ FrontendRequest::ListBoothbyPasses { .. } => boothby::handle_list_boothby_passes(ctx, r).await,
        r @ FrontendRequest::ListChores { .. } => work_items::handle_list_chores(ctx, r).await,
        r @ FrontendRequest::ListCiRemediations { .. } => ci_remediation::handle_list_ci_remediations(ctx, r).await,
        r @ FrontendRequest::ListConflictResolutions { .. } => {
            conflict_resolution::handle_list_conflict_resolutions(ctx, r).await
        }
        r @ FrontendRequest::ListDecisions { .. } => decisions::handle_list_decisions(ctx, r).await,
        r @ FrontendRequest::ListDeferredScopeAttentions { .. } => {
            attentions::handle_list_deferred_scope_attentions(ctx, r).await
        }
        r @ FrontendRequest::ListDependencies { .. } => dependencies::handle_list_dependencies(ctx, r).await,
        r @ FrontendRequest::ListDependenciesDetailed { .. } => {
            dependencies::handle_list_dependencies_detailed(ctx, r).await
        }
        r @ FrontendRequest::ListEditorialActions { .. } => automations::handle_list_editorial_actions(ctx, r).await,
        r @ FrontendRequest::ListEngineAttempts { .. } => executions::handle_list_engine_attempts(ctx, r).await,
        r @ FrontendRequest::ListExecutions { .. } => executions::handle_list_executions(ctx, r).await,
        r @ FrontendRequest::ListFeatureFlags => engine_meta::handle_list_feature_flags(ctx, r).await,
        r @ FrontendRequest::ListHosts => hosts::handle_list_hosts(ctx, r).await,
        r @ FrontendRequest::ListHostedPaneStatuses => panes::handle_list_hosted_pane_statuses(ctx, r).await,
        r @ FrontendRequest::ListIdeas { .. } => ideas::handle_list_ideas(ctx, r).await,
        r @ FrontendRequest::ListLiveStatusDisabledSlots => {
            live_status::handle_list_live_status_disabled_slots(ctx, r).await
        }
        r @ FrontendRequest::ListPlannerRuns { .. } => planner_ops::handle_list_planner_runs(ctx, r).await,
        r @ FrontendRequest::ListProductDesignDocs { .. } => design_docs::handle_list_product_design_docs(ctx, r).await,
        r @ FrontendRequest::ListProducts => products::handle_list_products(ctx, r).await,
        r @ FrontendRequest::ListProjects { .. } => projects::handle_list_projects(ctx, r).await,
        r @ FrontendRequest::ListAttachments { .. } => attachments::handle_list_attachments(ctx, r).await,
        r @ FrontendRequest::ListAttachmentsForWorkItem { .. } => {
            attachments::handle_list_attachments_for_work_item(ctx, r).await
        }
        r @ FrontendRequest::ListProposals { .. } => proposals::handle_list_proposals(ctx, r).await,
        r @ FrontendRequest::ListRuns { .. } => executions::handle_list_runs(ctx, r).await,
        r @ FrontendRequest::ListTasks { .. } => work_items::handle_list_tasks(ctx, r).await,
        r @ FrontendRequest::ListRevisions { .. } => work_items::handle_list_revisions(ctx, r).await,
        r @ FrontendRequest::ListWorkerLiveStates => panes::handle_list_worker_live_states(ctx, r).await,
        r @ FrontendRequest::ListTmuxWorkerStatuses => panes::handle_list_tmux_worker_statuses(ctx, r).await,
        r @ FrontendRequest::MarkCiRemediationFailed { .. } => {
            ci_remediation::handle_mark_ci_remediation_failed(ctx, r).await
        }
        r @ FrontendRequest::MarkCiRemediationNoop { .. } => {
            ci_remediation::handle_mark_ci_remediation_noop(ctx, r).await
        }
        r @ FrontendRequest::MarkCiRemediationRetriggered { .. } => {
            ci_remediation::handle_mark_ci_remediation_retriggered(ctx, r).await
        }
        r @ FrontendRequest::MarkCiRemediationSucceededViaRebase { .. } => {
            ci_remediation::handle_mark_ci_remediation_succeeded_via_rebase(ctx, r).await
        }
        r @ FrontendRequest::MarkConflictResolutionFailed { .. } => {
            conflict_resolution::handle_mark_conflict_resolution_failed(ctx, r).await
        }
        r @ FrontendRequest::MergeWhenReady { .. } => review::handle_merge_when_ready(ctx, r).await,
        r @ FrontendRequest::MetricsListLive => metrics::handle_metrics_list_live(ctx, r).await,
        r @ FrontendRequest::MetricsReset { .. } => metrics::handle_metrics_reset(ctx, r).await,
        r @ FrontendRequest::MetricsShowLive { .. } => metrics::handle_metrics_show_live(ctx, r).await,
        r @ FrontendRequest::OpenDocument { .. } => panes::handle_open_document(ctx, r).await,
        r @ FrontendRequest::OpenLiveWorkspaceTerminal { .. } => {
            review::handle_open_live_workspace_terminal(ctx, r).await
        }
        r @ FrontendRequest::OpenReviewTerminal { .. } => review::handle_open_review_terminal(ctx, r).await,
        r @ FrontendRequest::PlanProject { .. } => planner_ops::handle_plan_project(ctx, r).await,
        r @ FrontendRequest::ProbeRun { .. } => executions::handle_probe_run(ctx, r).await,
        r @ FrontendRequest::ProbeStatus { .. } => executions::handle_probe_status(ctx, r).await,
        r @ FrontendRequest::ReapRun { .. } => executions::handle_reap_run(ctx, r).await,
        r @ FrontendRequest::RecordEffortEscalation { .. } => effort::handle_record_effort_escalation(ctx, r).await,
        r @ FrontendRequest::RecordProducerSideConflict { .. } => {
            conflict_resolution::handle_record_producer_side_conflict(ctx, r).await
        }
        r @ FrontendRequest::RecreateCoordinator { .. } => sessions::handle_recreate_coordinator(ctx, r).await,
        r @ FrontendRequest::RegisterAppSession => sessions::handle_register_app_session(ctx, r).await,
        r @ FrontendRequest::RegisterCapabilities { .. } => engine_meta::handle_register_capabilities(ctx, r).await,
        r @ FrontendRequest::ReleaseHoldRun { .. } => executions::handle_release_hold_run(ctx, r).await,
        r @ FrontendRequest::ReleaseProject { .. } => planner_ops::handle_release_project(ctx, r).await,
        r @ FrontendRequest::ReleaseReviewTerminal { .. } => review::handle_release_review_terminal(ctx, r).await,
        r @ FrontendRequest::RemoveDependency { .. } => dependencies::handle_remove_dependency(ctx, r).await,
        r @ FrontendRequest::RemoveHost { .. } => hosts::handle_remove_host(ctx, r).await,
        r @ FrontendRequest::RemoveHostTag { .. } => hosts::handle_remove_host_tag(ctx, r).await,
        r @ FrontendRequest::ReorderProjectTasks { .. } => projects::handle_reorder_project_tasks(ctx, r).await,
        r @ FrontendRequest::RequestExecution { .. } => executions::handle_request_execution(ctx, r).await,
        r @ FrontendRequest::ResolveProjectDesignDoc { .. } => {
            projects::handle_resolve_project_design_doc(ctx, r).await
        }
        r @ FrontendRequest::RestoreWorkItem { .. } => work_items::handle_restore_work_item(ctx, r).await,
        r @ FrontendRequest::RetirePane { .. } => panes::handle_retire_pane(ctx, r).await,
        r @ FrontendRequest::RetryCiRemediation { .. } => ci_remediation::handle_retry_ci_remediation(ctx, r).await,
        r @ FrontendRequest::RetryConflictResolution { .. } => {
            conflict_resolution::handle_retry_conflict_resolution(ctx, r).await
        }
        r @ FrontendRequest::RevealWorkItem { .. } => work_items::handle_reveal_work_item(ctx, r).await,
        r @ FrontendRequest::RevokeDecision { .. } => decisions::handle_revoke_decision(ctx, r).await,
        r @ FrontendRequest::RunAutomation { .. } => automations::handle_run_automation(ctx, r).await,
        r @ FrontendRequest::RunBoothbyPass => boothby::handle_run_boothby_pass(ctx, r).await,
        r @ FrontendRequest::SendInputToWorker { .. } => panes::handle_send_input_to_worker(ctx, r).await,
        r @ FrontendRequest::SetAutomationPaused { .. } => engine_meta::handle_set_automation_paused(ctx, r).await,
        r @ FrontendRequest::SetBoothbyMode { .. } => boothby::handle_set_boothby_mode(ctx, r).await,
        r @ FrontendRequest::SetCiBudget { .. } => ci_remediation::handle_set_ci_budget(ctx, r).await,
        r @ FrontendRequest::SetDriverTrafficSplit { .. } => engine_meta::handle_set_driver_traffic_split(ctx, r).await,
        r @ FrontendRequest::SetDispatchConcurrency { .. } => {
            engine_meta::handle_set_dispatch_concurrency(ctx, r).await
        }
        r @ FrontendRequest::SetDispatchPaused { .. } => engine_meta::handle_set_dispatch_paused(ctx, r).await,
        r @ FrontendRequest::SetFeatureFlag { .. } => engine_meta::handle_set_feature_flag(ctx, r).await,
        r @ FrontendRequest::SetHostEnabled { .. } => hosts::handle_set_host_enabled(ctx, r).await,
        r @ FrontendRequest::SetLiveStatusEnabled { .. } => live_status::handle_set_live_status_enabled(ctx, r).await,
        r @ FrontendRequest::SetProductDefaultModel { .. } => products::handle_set_product_default_model(ctx, r).await,
        r @ FrontendRequest::SetProductDefaultDriver { .. } => {
            products::handle_set_product_default_driver(ctx, r).await
        }
        r @ FrontendRequest::SetProductMergeMechanism { .. } => {
            products::handle_set_product_merge_mechanism(ctx, r).await
        }
        r @ FrontendRequest::SetProductEditorialRules { .. } => {
            products::handle_set_product_editorial_rules(ctx, r).await
        }
        r @ FrontendRequest::EvaluateEditorialRules { .. } => products::handle_evaluate_editorial_rules(ctx, r).await,
        r @ FrontendRequest::EvaluateDispatchAdmission { .. } => {
            executions::handle_evaluate_dispatch_admission(ctx, r).await
        }
        r @ FrontendRequest::SetProductExternalTracker { .. } => {
            external_tracker::handle_set_product_external_tracker(ctx, r).await
        }
        r @ FrontendRequest::SetCoordinatorHandoff { .. } => {
            coordinator_handoff::handle_set_coordinator_handoff(ctx, r).await
        }
        r @ FrontendRequest::SetProjectDesignDoc { .. } => projects::handle_set_project_design_doc(ctx, r).await,
        r @ FrontendRequest::SetSetting { .. } => engine_meta::handle_set_setting(ctx, r).await,
        r @ FrontendRequest::SetTaskDocPointer { .. } => work_items::handle_set_task_doc_pointer(ctx, r).await,
        r @ FrontendRequest::Shutdown { .. } => sessions::handle_shutdown(ctx, r).await,
        r @ FrontendRequest::SpawnCapabilityRestored => sessions::handle_spawn_capability_restored(ctx, r).await,
        r @ FrontendRequest::StopRun { .. } => executions::handle_stop_run(ctx, r).await,
        r @ FrontendRequest::SubmitAttachment { .. } => attachments::handle_submit_attachment(ctx, r).await,
        r @ FrontendRequest::SubmitProposal { .. } => proposals::handle_submit_proposal(ctx, r).await,
        r @ FrontendRequest::Subscribe { .. } => subscriptions::handle_subscribe(ctx, r).await,
        r @ FrontendRequest::SupersedeDecision { .. } => decisions::handle_supersede_decision(ctx, r).await,
        r @ FrontendRequest::SyncProductExternalTracker { .. } => {
            external_tracker::handle_sync_product_external_tracker(ctx, r).await
        }
        r @ FrontendRequest::TailRunTranscript { .. } => executions::handle_tail_run_transcript(ctx, r).await,
        r @ FrontendRequest::TriggerPrReview { .. } => review::handle_trigger_pr_review(ctx, r).await,
        r @ FrontendRequest::TrunkSetToken { .. } => trunk_auth::handle_trunk_set_token(ctx, r).await,
        r @ FrontendRequest::TrunkStatus => trunk_auth::handle_trunk_status(ctx, r).await,
        r @ FrontendRequest::UnlinkWorkItemExternalRef { .. } => {
            external_tracker::handle_unlink_work_item_external_ref(ctx, r).await
        }
        r @ FrontendRequest::UnpopulateProject { .. } => planner_ops::handle_unpopulate_project(ctx, r).await,
        r @ FrontendRequest::Unsubscribe { .. } => subscriptions::handle_unsubscribe(ctx, r).await,
        r @ FrontendRequest::UpdateAutomation { .. } => automations::handle_update_automation(ctx, r).await,
        r @ FrontendRequest::UpdateIdea { .. } => ideas::handle_update_idea(ctx, r).await,
        r @ FrontendRequest::MoveWorkItemOnBoard { .. } => work_items::handle_move_work_item_on_board(ctx, r).await,
        r @ FrontendRequest::UpdateWorkItem { .. } => work_items::handle_update_work_item(ctx, r).await,
        r @ FrontendRequest::ReportSelectedProduct { .. } => {
            selected_product::handle_report_selected_product(ctx, r).await
        }
        r @ FrontendRequest::ReportWorkerSpawnFailed { .. } => {
            sessions::handle_report_worker_spawn_failed(ctx, r).await
        }
        r @ FrontendRequest::UpdateWorkerShellPid { .. } => sessions::handle_update_worker_shell_pid(ctx, r).await,
        r @ FrontendRequest::WorkerPaneDied { .. } => sessions::handle_worker_pane_died(ctx, r).await,
        r @ FrontendRequest::WorkerPoolSummary => engine_meta::handle_worker_pool_summary(ctx, r).await,
        r @ FrontendRequest::WorkspacePoolSummary => engine_meta::handle_workspace_pool_summary(ctx, r).await,
    }
}
