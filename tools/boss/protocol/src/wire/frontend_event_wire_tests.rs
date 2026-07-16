//! Wire-contract tests for [`FrontendEvent`] — the JSON contract between
//! the engine and its frontends (macOS app, CLI, `bossctl`).
//!
//! `FrontendEvent` is `#[serde(tag = "type", rename_all = "snake_case")]`,
//! so every variant serializes to an object carrying a `"type"`
//! discriminator plus its named fields. The frontends hand-decode that
//! shape, so a renamed variant or a renamed field is a silent
//! wire-break with no compile-time signal on the Rust side. These tests
//! pin the observable contract — the exact snake_case tag and the
//! serde round-trip — for **every** `FrontendEvent` variant, plus the
//! documented field names for a handful of representative shapes, rather
//! than the enum's internals.
//!
//! Kept table-driven (`tag_cases`) so pinning a newly-added variant's
//! tag + round-trip is a one-line addition. A compile error here when a
//! variant is added (via the exhaustiveness check in
//! `every_variant_is_pinned`) is the signal to add that line.

use super::*;
// Enum discriminants and inputs used only by these fixtures are not part of
// the `wire` module's import set — bring them in explicitly from the crate root.
use crate::{
    AutomationTrigger, BoothbyPass, DesignDocEntry, DesignDocTree, EffortLevel, ExecutionKind, ExecutionStatus,
    ListHostedPanesInput, ProjectDesignDocState, ProposalFieldError, TaskKind, TaskStatus, WorkerTierDenialReason,
};

/// One representative event paired with the exact `"type"` tag it must
/// serialize under. The label is only for failure messages.
struct TagCase {
    label: &'static str,
    event: FrontendEvent,
    expected_tag: &'static str,
}

// --- Representative fixtures for the payload-carrying variants ------------
//
// Every variant that embeds a domain struct gets a minimal, valid instance
// here so `tag_cases` stays one-line-per-variant. Values are only as rich as
// the wire test needs — a real id string, a real enum discriminant — never a
// full realistic row.

fn product() -> Product {
    Product::builder()
        .id("prod_1")
        .created_at("1747000000")
        .description("A product")
        .name("Widgets")
        .slug("widgets")
        .status("active")
        .updated_at("1747000000")
        .build()
}

fn task() -> Task {
    Task::builder()
        .id("task_1")
        .product_id("prod_1")
        .created_at("1747000000")
        .description("Do a thing")
        .kind(TaskKind::Task)
        .name("A task")
        .status(TaskStatus::Todo)
        .updated_at("1747000000")
        .build()
}

fn worker_proposal() -> WorkerProposal {
    WorkerProposal::builder()
        .id("prp_1")
        .execution_id("exec_1")
        .created_at("1747000000")
        .idempotency_key("auto:blocked:abc")
        .kind(ProposalKind::Blocked)
        .payload_json(r#"{"reason":"stuck"}"#)
        .work_item_id("task_1")
        .build()
}

fn worker_context_bundle() -> WorkerContextBundle {
    WorkerContextBundle {
        task: task(),
        project: None,
        product: product(),
        sibling_tasks: vec![],
        own_dependencies: WorkItemDependencyDetail {
            work_item_id: "task_1".into(),
            dependents: vec![],
            prerequisites: vec![],
        },
        attention_groups: vec![],
        proposals: vec![worker_proposal()],
    }
}

fn work_execution() -> WorkExecution {
    WorkExecution::builder()
        .id("exec_1")
        .work_item_id("task_1")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@example.com:foo.git")
        .created_at("1747000000")
        .build()
}

fn work_run() -> WorkRun {
    WorkRun::builder()
        .id("run_1")
        .agent_id("agent_1")
        .execution_id("exec_1")
        .created_at("1747000000")
        .status("running")
        .build()
}

fn task_runtime() -> TaskRuntime {
    TaskRuntime::builder().work_item_id("task_1").build()
}

fn attention_item() -> WorkAttentionItem {
    WorkAttentionItem::builder()
        .id("wai_1")
        .body_markdown("Something needs attention")
        .created_at("1747000000")
        .kind("failure")
        .status("open")
        .title("A failure")
        .build()
}

fn attention() -> Attention {
    Attention::builder()
        .id("att_1")
        .group_id("atg_1")
        .ordinal(1)
        .created_at("1747000000")
        .build()
}

fn attention_group() -> AttentionGroup {
    AttentionGroup::builder()
        .id("atg_1")
        .product_id("prod_1")
        .created_at("1747000000")
        .grouping_key("question|proj_1|doc:foo.md")
        .kind("question")
        .source_kind("design_doc")
        .build()
}

fn dependency() -> WorkItemDependency {
    WorkItemDependency {
        dependent_id: "task_2".into(),
        prerequisite_id: "task_1".into(),
        created_at: "1747000000".into(),
        relation: "blocks".into(),
    }
}

fn conflict_resolution() -> ConflictResolution {
    ConflictResolution::builder()
        .id("crz_1")
        .product_id("prod_1")
        .work_item_id("task_1")
        .base_branch("main")
        .created_at("1747000000")
        .head_branch("boss/exec_1")
        .pr_number(42)
        .pr_url("https://example.test/pr/42")
        .status("pending")
        .build()
}

fn ci_remediation() -> CiRemediation {
    CiRemediation::builder()
        .id("cir_1")
        .product_id("prod_1")
        .work_item_id("task_1")
        .attempt_kind("fix")
        .consumes_budget(1)
        .created_at("1747000000")
        .failed_checks("[]")
        .head_branch("boss/exec_1")
        .head_sha_at_trigger("abc123")
        .pr_number(42)
        .pr_url("https://example.test/pr/42")
        .status("pending")
        .build()
}

fn ci_budget() -> CiBudgetSnapshot {
    CiBudgetSnapshot::builder()
        .work_item_id("task_1")
        .effective(3)
        .product_default(3)
        .used(0)
        .build()
}

fn conflict_hotspot_report() -> ConflictHotspotReport {
    ConflictHotspotReport {
        product_id: "prod_1".into(),
        total_events: 0,
        file_frequency: vec![],
        file_pair_frequency: vec![],
        class_counts: vec![],
    }
}

fn host_snapshot() -> HostSnapshot {
    HostSnapshot::builder()
        .id("local")
        .pool_size(4)
        .enabled(true)
        .consecutive_failures(0)
        .created_at("1747000000")
        .capabilities(vec![])
        .build()
}

fn work_comment() -> WorkComment {
    WorkComment::builder()
        .id("cmt_1")
        .artifact_id("task_1")
        .anchor(CommentAnchor::default())
        .artifact_kind("work_item")
        .author("user:test@example.com")
        .body("A comment")
        .created_at("1747000000")
        .doc_version("v1")
        .updated_at("1747000000")
        .build()
}

fn automation() -> Automation {
    Automation::builder()
        .id("aut_1")
        .product_id("prod_1")
        .created_at("1747000000")
        .name("Nightly triage")
        .standing_instruction("Look for stale PRs")
        .trigger(AutomationTrigger::Schedule {
            cron: "0 9 * * *".into(),
            timezone: "America/Los_Angeles".into(),
        })
        .updated_at("1747000000")
        .build()
}

fn automation_run() -> AutomationRun {
    AutomationRun::builder()
        .id("aur_1")
        .automation_id("aut_1")
        .outcome("produced_task")
        .scheduled_for("1747000000")
        .started_at("1747000000")
        .build()
}

fn live_status_debug_report() -> crate::LiveStatusDebugReport {
    crate::LiveStatusDebugReport {
        engine_build_sha: "abc123".into(),
        engine_build_dirty: false,
        engine_build_time: "2026-01-01T00:00:00Z".into(),
        engine_binary_fingerprint: "def456".into(),
        engine_process_started_at: "2026-01-01T00:00:00Z".into(),
        anthropic_api_key_present: true,
        tracked_slot_count: 0,
        disabled_slot_count: 0,
        dispatcher_stats: crate::DispatcherStatsReport::default(),
        slots: vec![],
    }
}

fn effort_audit_report() -> crate::EffortAuditReport {
    crate::EffortAuditReport::builder()
        .product_id("prod_1")
        .generated_at("1747000000")
        .product_slug("widgets")
        .rows(vec![])
        .total_chores(0)
        .total_escalations(0)
        .under_class_threshold(0.5)
        .build()
}

fn effort_escalation() -> crate::EffortEscalation {
    crate::EffortEscalation::builder()
        .id("esc_1")
        .product_id("prod_1")
        .work_item_id("task_1")
        .created_at("1747000000")
        .markers(vec!["rename".to_string()])
        .new_level(EffortLevel::Large)
        .original_level(EffortLevel::Small)
        .build()
}

/// Every `FrontendEvent` variant paired with the exact `"type"` tag it
/// must serialize under. Ordered to match the declaration order in
/// [`events.rs`](super::events) so a reviewer can diff the two side by
/// side. The compile-time exhaustiveness guard `every_variant_is_pinned`
/// forces a new variant to be represented here.
fn tag_cases() -> Vec<TagCase> {
    vec![
        // --- Connection lifecycle ---
        TagCase {
            label: "Hello",
            event: FrontendEvent::Hello {
                session_id: "sess_1".into(),
            },
            expected_tag: "hello",
        },
        TagCase {
            label: "Subscribed",
            event: FrontendEvent::Subscribed {
                topics: vec!["work.products".into()],
                current_revision: 7,
            },
            expected_tag: "subscribed",
        },
        TagCase {
            label: "Unsubscribed",
            event: FrontendEvent::Unsubscribed {
                topics: vec!["work.products".into()],
            },
            expected_tag: "unsubscribed",
        },
        TagCase {
            label: "TopicEvent",
            event: FrontendEvent::TopicEvent {
                topic: "work.product.prod_1".into(),
                revision: 3,
                origin_session_id: "sess_2".into(),
                origin_request_id: Some("req_9".into()),
                event: TopicEventPayload::WorkInvalidated {
                    reason: "created".into(),
                    product_id: Some("prod_1".into()),
                    item_ids: vec!["task_1".into()],
                },
            },
            expected_tag: "topic_event",
        },
        // --- Work-item / product / project lists and CRUD ---
        TagCase {
            label: "ProductsList",
            event: FrontendEvent::ProductsList { products: vec![] },
            expected_tag: "products_list",
        },
        TagCase {
            label: "ProjectsList",
            event: FrontendEvent::ProjectsList {
                product_id: "prod_1".into(),
                projects: vec![],
            },
            expected_tag: "projects_list",
        },
        TagCase {
            label: "TasksList",
            event: FrontendEvent::TasksList {
                product_id: "prod_1".into(),
                project_id: Some("proj_1".into()),
                tasks: vec![],
            },
            expected_tag: "tasks_list",
        },
        TagCase {
            label: "ChoresList",
            event: FrontendEvent::ChoresList {
                product_id: "prod_1".into(),
                chores: vec![],
            },
            expected_tag: "chores_list",
        },
        TagCase {
            label: "RevisionsList",
            event: FrontendEvent::RevisionsList {
                product_id: "prod_1".into(),
                revisions: vec![],
            },
            expected_tag: "revisions_list",
        },
        TagCase {
            label: "WorkTree",
            event: FrontendEvent::WorkTree {
                product: product(),
                projects: vec![],
                tasks: vec![],
                chores: vec![],
                task_runtimes: vec![],
                dependencies: vec![],
            },
            expected_tag: "work_tree",
        },
        TagCase {
            label: "WorkItemResult",
            event: FrontendEvent::WorkItemResult {
                item: WorkItem::Task(task()),
            },
            expected_tag: "work_item_result",
        },
        TagCase {
            label: "WorkItemsByPrResult",
            event: FrontendEvent::WorkItemsByPrResult {
                pr_number: 42,
                matches: vec![],
            },
            expected_tag: "work_items_by_pr_result",
        },
        TagCase {
            label: "WorkItemCreated",
            event: FrontendEvent::WorkItemCreated {
                item: WorkItem::Task(task()),
            },
            expected_tag: "work_item_created",
        },
        TagCase {
            label: "WorkItemsCreated",
            event: FrontendEvent::WorkItemsCreated { items: vec![] },
            expected_tag: "work_items_created",
        },
        TagCase {
            label: "WorkItemUpdated",
            event: FrontendEvent::WorkItemUpdated {
                item: WorkItem::Task(task()),
            },
            expected_tag: "work_item_updated",
        },
        TagCase {
            label: "ProjectTasksReordered",
            event: FrontendEvent::ProjectTasksReordered {
                project_id: "proj_1".into(),
                task_ids: vec!["task_1".into()],
            },
            expected_tag: "project_tasks_reordered",
        },
        // --- Executions & runs ---
        TagCase {
            label: "ExecutionsList",
            event: FrontendEvent::ExecutionsList {
                work_item_id: Some("task_1".into()),
                executions: vec![],
            },
            expected_tag: "executions_list",
        },
        TagCase {
            label: "TaskRuntimeResult",
            event: FrontendEvent::TaskRuntimeResult {
                runtime: task_runtime(),
            },
            expected_tag: "task_runtime_result",
        },
        TagCase {
            label: "ExecutionResult",
            event: FrontendEvent::ExecutionResult {
                execution: work_execution(),
            },
            expected_tag: "execution_result",
        },
        TagCase {
            label: "ExecutionCreated",
            event: FrontendEvent::ExecutionCreated {
                execution: work_execution(),
            },
            expected_tag: "execution_created",
        },
        TagCase {
            label: "ExecutionRequested",
            event: FrontendEvent::ExecutionRequested {
                execution: work_execution(),
            },
            expected_tag: "execution_requested",
        },
        TagCase {
            label: "PrReviewTriggered",
            event: FrontendEvent::PrReviewTriggered {
                execution: work_execution(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/42".into(),
            },
            expected_tag: "pr_review_triggered",
        },
        TagCase {
            label: "RunsList",
            event: FrontendEvent::RunsList {
                execution_id: "exec_1".into(),
                runs: vec![],
            },
            expected_tag: "runs_list",
        },
        TagCase {
            label: "RunResult",
            event: FrontendEvent::RunResult { run: work_run() },
            expected_tag: "run_result",
        },
        TagCase {
            label: "RunCreated",
            event: FrontendEvent::RunCreated { run: work_run() },
            expected_tag: "run_created",
        },
        // --- Attention items ---
        TagCase {
            label: "AttentionItemsList",
            event: FrontendEvent::AttentionItemsList {
                execution_id: "exec_1".into(),
                items: vec![],
            },
            expected_tag: "attention_items_list",
        },
        TagCase {
            label: "AttentionItemResult",
            event: FrontendEvent::AttentionItemResult { item: attention_item() },
            expected_tag: "attention_item_result",
        },
        TagCase {
            label: "AttentionItemCreated",
            event: FrontendEvent::AttentionItemCreated { item: attention_item() },
            expected_tag: "attention_item_created",
        },
        TagCase {
            label: "AttentionItemsForWorkItemList",
            event: FrontendEvent::AttentionItemsForWorkItemList {
                work_item_id: "task_1".into(),
                items: vec![],
            },
            expected_tag: "attention_items_for_work_item_list",
        },
        TagCase {
            label: "AttentionItemUpdated",
            event: FrontendEvent::AttentionItemUpdated { item: attention_item() },
            expected_tag: "attention_item_updated",
        },
        TagCase {
            label: "AttentionItemConverted",
            event: FrontendEvent::AttentionItemConverted {
                item: attention_item(),
                task: Box::new(task()),
            },
            expected_tag: "attention_item_converted",
        },
        TagCase {
            label: "DeferredScopeAttentionsList",
            event: FrontendEvent::DeferredScopeAttentionsList {
                product_id: "prod_1".into(),
                items: vec![],
            },
            expected_tag: "deferred_scope_attentions_list",
        },
        // --- Attention groups ---
        TagCase {
            label: "AttentionGroupsList",
            event: FrontendEvent::AttentionGroupsList {
                product_id: "prod_1".into(),
                groups: vec![],
                members: vec![],
            },
            expected_tag: "attention_groups_list",
        },
        TagCase {
            label: "AttentionGroupResult",
            event: FrontendEvent::AttentionGroupResult {
                group: attention_group(),
                members: vec![],
            },
            expected_tag: "attention_group_result",
        },
        TagCase {
            label: "AttentionCreated",
            event: FrontendEvent::AttentionCreated {
                attention: attention(),
                group: attention_group(),
            },
            expected_tag: "attention_created",
        },
        TagCase {
            label: "AttentionGroupUpdated",
            event: FrontendEvent::AttentionGroupUpdated {
                group: attention_group(),
                members: vec![],
            },
            expected_tag: "attention_group_updated",
        },
        TagCase {
            label: "AttentionGroupActioned",
            event: FrontendEvent::AttentionGroupActioned {
                group: attention_group(),
                members: vec![],
            },
            expected_tag: "attention_group_actioned",
        },
        TagCase {
            label: "AttentionMergesList",
            event: FrontendEvent::AttentionMergesList {
                attention_id: "att_1".into(),
                merges: vec![],
            },
            expected_tag: "attention_merges_list",
        },
        // --- Work-item deletion / errors ---
        TagCase {
            label: "WorkItemDeleted",
            event: FrontendEvent::WorkItemDeleted { id: "task_1".into() },
            expected_tag: "work_item_deleted",
        },
        TagCase {
            label: "WorkItemRestored",
            event: FrontendEvent::WorkItemRestored {
                item: WorkItem::Task(task()),
            },
            expected_tag: "work_item_restored",
        },
        TagCase {
            label: "WorkError",
            event: FrontendEvent::WorkError { message: "boom".into() },
            expected_tag: "work_error",
        },
        TagCase {
            label: "WorkItemDuplicateBlocked",
            event: FrontendEvent::WorkItemDuplicateBlocked {
                existing_id: "task_1".into(),
                existing_short_id: 439,
                name: "A task".into(),
                age_secs: 12,
            },
            expected_tag: "work_item_duplicate_blocked",
        },
        TagCase {
            label: "Error",
            event: FrontendEvent::Error { message: "boom".into() },
            expected_tag: "error",
        },
        // --- Session registration & engine config ---
        TagCase {
            label: "AppSessionRegistered",
            event: FrontendEvent::AppSessionRegistered,
            expected_tag: "app_session_registered",
        },
        TagCase {
            label: "EnginePoolConfig",
            event: FrontendEvent::EnginePoolConfig {
                worker_slots: 6,
                automation_slots: 2,
                review_slots: 2,
                coordinator_model: "claude-opus-4-8".into(),
            },
            expected_tag: "engine_pool_config",
        },
        TagCase {
            label: "BossSessionRegistered",
            event: FrontendEvent::BossSessionRegistered,
            expected_tag: "boss_session_registered",
        },
        // --- Probes ---
        TagCase {
            label: "ProbeQueued",
            event: FrontendEvent::ProbeQueued {
                run_id: "run_1".into(),
                probe_id: "probe_1".into(),
                urgent: true,
            },
            expected_tag: "probe_queued",
        },
        TagCase {
            label: "ProbeReplied",
            event: FrontendEvent::ProbeReplied {
                run_id: "run_1".into(),
                probe_id: "probe_1".into(),
                text: "on it".into(),
            },
            expected_tag: "probe_replied",
        },
        TagCase {
            label: "ProbeDeliveryEscalated",
            event: FrontendEvent::ProbeDeliveryEscalated {
                run_id: "run_1".into(),
                probe_id: "probe_1".into(),
                reason: "unconfirmed".into(),
            },
            expected_tag: "probe_delivery_escalated",
        },
        // --- Worker pane control ---
        TagCase {
            label: "RunStopped",
            event: FrontendEvent::RunStopped { run_id: "run_1".into() },
            expected_tag: "run_stopped",
        },
        TagCase {
            label: "WorkerPaneFocused",
            event: FrontendEvent::WorkerPaneFocused {
                run_id: "run_1".into(),
                slot_id: 3,
            },
            expected_tag: "worker_pane_focused",
        },
        TagCase {
            label: "WorkerInputSent",
            event: FrontendEvent::WorkerInputSent {
                run_id: "run_1".into(),
                slot_id: 3,
            },
            expected_tag: "worker_input_sent",
        },
        TagCase {
            label: "WorkerPaneInterrupted",
            event: FrontendEvent::WorkerPaneInterrupted {
                run_id: "run_1".into(),
                slot_id: 3,
            },
            expected_tag: "worker_pane_interrupted",
        },
        TagCase {
            label: "EngineRequest",
            event: FrontendEvent::EngineRequest {
                request_id: "req_1".into(),
                request: EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {}),
            },
            expected_tag: "engine_request",
        },
        TagCase {
            label: "WorkerLiveStatesList",
            event: FrontendEvent::WorkerLiveStatesList { states: vec![] },
            expected_tag: "worker_live_states_list",
        },
        TagCase {
            label: "ExecutionCancelled",
            event: FrontendEvent::ExecutionCancelled {
                execution: work_execution(),
            },
            expected_tag: "execution_cancelled",
        },
        TagCase {
            label: "RunReaped",
            event: FrontendEvent::RunReaped {
                run_id: "run_1".into(),
                execution: work_execution(),
            },
            expected_tag: "run_reaped",
        },
        TagCase {
            label: "PaneRetired",
            event: FrontendEvent::PaneRetired { slot_id: 3 },
            expected_tag: "pane_retired",
        },
        TagCase {
            label: "HuskPanesList",
            event: FrontendEvent::HuskPanesList { panes: vec![] },
            expected_tag: "husk_panes_list",
        },
        // --- Transcripts ---
        TagCase {
            label: "RunTranscriptTail",
            event: FrontendEvent::RunTranscriptTail {
                run_id: "run_1".into(),
                transcript_path: "/tmp/t.jsonl".into(),
                lines: vec![],
                truncated: false,
            },
            expected_tag: "run_transcript_tail",
        },
        TagCase {
            label: "ExecutionTranscriptResult",
            event: FrontendEvent::ExecutionTranscriptResult {
                execution_id: "exec_1".into(),
                segments: vec![],
                is_live: true,
                complete: false,
            },
            expected_tag: "execution_transcript_result",
        },
        TagCase {
            label: "ExecutionTranscriptUnavailable",
            event: FrontendEvent::ExecutionTranscriptUnavailable {
                execution_id: "exec_1".into(),
                reason: "no transcript".into(),
            },
            expected_tag: "execution_transcript_unavailable",
        },
        // --- Pool summaries ---
        TagCase {
            label: "WorkspacePoolSummaryResult",
            event: FrontendEvent::WorkspacePoolSummaryResult { workspaces: vec![] },
            expected_tag: "workspace_pool_summary_result",
        },
        TagCase {
            label: "WorkerPoolSummaryResult",
            event: FrontendEvent::WorkerPoolSummaryResult { pools: vec![] },
            expected_tag: "worker_pool_summary_result",
        },
        // --- Dependencies ---
        TagCase {
            label: "DependencyAdded",
            event: FrontendEvent::DependencyAdded { edge: dependency() },
            expected_tag: "dependency_added",
        },
        TagCase {
            label: "DependencyRemoved",
            event: FrontendEvent::DependencyRemoved {
                dependent_id: "task_2".into(),
                prerequisite_id: "task_1".into(),
                relation: "blocks".into(),
                removed: true,
            },
            expected_tag: "dependency_removed",
        },
        TagCase {
            label: "DependencyList",
            event: FrontendEvent::DependencyList {
                view: WorkItemDependencyView {
                    work_item_id: "task_1".into(),
                    dependents: vec![],
                    prerequisites: vec![],
                },
            },
            expected_tag: "dependency_list",
        },
        TagCase {
            label: "DependencyDetail",
            event: FrontendEvent::DependencyDetail {
                detail: WorkItemDependencyDetail {
                    work_item_id: "task_1".into(),
                    dependents: vec![],
                    prerequisites: vec![],
                },
            },
            expected_tag: "dependency_detail",
        },
        // --- Live status ---
        TagCase {
            label: "LiveStatusEnabledSet",
            event: FrontendEvent::LiveStatusEnabledSet {
                slot_id: 3,
                enabled: true,
            },
            expected_tag: "live_status_enabled_set",
        },
        TagCase {
            label: "LiveStatusDisabledSlotsList",
            event: FrontendEvent::LiveStatusDisabledSlotsList { slot_ids: vec![] },
            expected_tag: "live_status_disabled_slots_list",
        },
        TagCase {
            label: "LiveStatusDebugReportEvent",
            event: FrontendEvent::LiveStatusDebugReportEvent {
                report: live_status_debug_report(),
            },
            expected_tag: "live_status_debug_report_event",
        },
        TagCase {
            label: "ProjectDesignDocResolved",
            event: FrontendEvent::ProjectDesignDocResolved {
                output: ResolveProjectDesignDocOutput {
                    project_id: "proj_1".into(),
                    state: ProjectDesignDocState::NotSet,
                },
            },
            expected_tag: "project_design_doc_resolved",
        },
        TagCase {
            label: "ProductDesignDocsList",
            event: FrontendEvent::ProductDesignDocsList {
                product_id: "prod_1".into(),
                state: DesignDocTreeState::Loaded {
                    tree: DesignDocTree::builder()
                        .repo_remote_url("git@github.com:brianduff/flunge.git")
                        .owner_repo("brianduff/flunge")
                        .branch("main")
                        .git_ref("b95bd654ec91f84f70f62127ef8d53317bd52ebb")
                        .entries(vec![DesignDocEntry {
                            path: "docs/design-docs/backend-preview-environments.md".into(),
                            size: Some(4096),
                        }])
                        .fetched_at("2026-07-23T12:00:00Z")
                        .build(),
                },
            },
            expected_tag: "product_design_docs_list",
        },
        TagCase {
            label: "ProductDesignDocContent",
            event: FrontendEvent::ProductDesignDocContent {
                repo_remote_url: "git@github.com:brianduff/flunge.git".into(),
                path: "docs/design-docs/backend-preview-environments.md".into(),
                git_ref: "b95bd654ec91f84f70f62127ef8d53317bd52ebb".into(),
                content: DesignDocContent::Loaded {
                    markdown: "# Backend preview environments".into(),
                },
            },
            expected_tag: "product_design_doc_content",
        },
        // --- Conflict-resolution receipts ---
        TagCase {
            label: "ConflictResolutionMarkedFailed",
            event: FrontendEvent::ConflictResolutionMarkedFailed {
                attempt: conflict_resolution(),
            },
            expected_tag: "conflict_resolution_marked_failed",
        },
        // --- CI-remediation receipts ---
        TagCase {
            label: "CiRemediationClassified",
            event: FrontendEvent::CiRemediationClassified {
                attempt: ci_remediation(),
            },
            expected_tag: "ci_remediation_classified",
        },
        TagCase {
            label: "CiRemediationMarkedFailed",
            event: FrontendEvent::CiRemediationMarkedFailed {
                attempt: ci_remediation(),
            },
            expected_tag: "ci_remediation_marked_failed",
        },
        TagCase {
            label: "CiRemediationRetriggered",
            event: FrontendEvent::CiRemediationRetriggered {
                attempt: ci_remediation(),
                new_id: "run_99".into(),
            },
            expected_tag: "ci_remediation_retriggered",
        },
        TagCase {
            label: "CiRemediationSucceededViaRebase",
            event: FrontendEvent::CiRemediationSucceededViaRebase {
                attempt: ci_remediation(),
                budget_refunded: true,
            },
            expected_tag: "ci_remediation_succeeded_via_rebase",
        },
        TagCase {
            label: "CiRemediationSucceededViaRebaseRejected",
            event: FrontendEvent::CiRemediationSucceededViaRebaseRejected {
                attempt_id: "cir_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/1".into(),
                status: "still_pending".into(),
                live_sha: Some("abc123".into()),
            },
            expected_tag: "ci_remediation_succeeded_via_rebase_rejected",
        },
        TagCase {
            label: "CiRemediationNoopValidated",
            event: FrontendEvent::CiRemediationNoopValidated {
                attempt: ci_remediation(),
                validated_sha: Some("abc123".into()),
                observed_sha: Some("abc123".into()),
            },
            expected_tag: "ci_remediation_noop_validated",
        },
        TagCase {
            label: "CiRemediationNoopRejected",
            event: FrontendEvent::CiRemediationNoopRejected {
                attempt_id: "cir_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/1".into(),
                status: "still_pending".into(),
                live_sha: Some("abc123".into()),
                observed_sha: Some("abc123".into()),
            },
            expected_tag: "ci_remediation_noop_rejected",
        },
        TagCase {
            label: "ConflictResolutionsList",
            event: FrontendEvent::ConflictResolutionsList { attempts: vec![] },
            expected_tag: "conflict_resolutions_list",
        },
        TagCase {
            label: "ConflictHotspots",
            event: FrontendEvent::ConflictHotspots {
                report: conflict_hotspot_report(),
            },
            expected_tag: "conflict_hotspots",
        },
        TagCase {
            label: "ConflictResolution",
            event: FrontendEvent::ConflictResolution {
                attempt: conflict_resolution(),
            },
            expected_tag: "conflict_resolution",
        },
        TagCase {
            label: "ConflictResolutionRetried",
            event: FrontendEvent::ConflictResolutionRetried {
                attempt: conflict_resolution(),
            },
            expected_tag: "conflict_resolution_retried",
        },
        TagCase {
            label: "ConflictResolutionMarkedAbandoned",
            event: FrontendEvent::ConflictResolutionMarkedAbandoned {
                attempt: conflict_resolution(),
            },
            expected_tag: "conflict_resolution_marked_abandoned",
        },
        // --- Activity-feed pushes: conflict resolution ---
        TagCase {
            label: "ConflictResolutionStarted",
            event: FrontendEvent::ConflictResolutionStarted {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "crz_1".into(),
                pr_url: "https://example.test/pr/42".into(),
            },
            expected_tag: "conflict_resolution_started",
        },
        TagCase {
            label: "ConflictResolutionSucceeded",
            event: FrontendEvent::ConflictResolutionSucceeded {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "crz_1".into(),
                pr_url: "https://example.test/pr/42".into(),
            },
            expected_tag: "conflict_resolution_succeeded",
        },
        TagCase {
            label: "ConflictResolutionFailed",
            event: FrontendEvent::ConflictResolutionFailed {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "crz_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                failure_reason: "gave up".into(),
            },
            expected_tag: "conflict_resolution_failed",
        },
        TagCase {
            label: "ConflictResolutionAbandoned",
            event: FrontendEvent::ConflictResolutionAbandoned {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "crz_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                failure_reason: "pr closed".into(),
            },
            expected_tag: "conflict_resolution_abandoned",
        },
        TagCase {
            label: "StackProposalOffered",
            event: FrontendEvent::StackProposalOffered {
                product_id: "prod_1".into(),
                base_pr_url: "https://example.test/pr/1".into(),
                base_pr_number: 1,
                dependent_pr_url: "https://example.test/pr/2".into(),
                dependent_pr_number: 2,
                overlapping_files: vec![],
            },
            expected_tag: "stack_proposal_offered",
        },
        // --- Activity-feed pushes: CI remediation ---
        TagCase {
            label: "CiRemediationStarted",
            event: FrontendEvent::CiRemediationStarted {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "cir_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                attempt_kind: "fix".into(),
            },
            expected_tag: "ci_remediation_started",
        },
        TagCase {
            label: "CiRemediationSucceeded",
            event: FrontendEvent::CiRemediationSucceeded {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "cir_1".into(),
                pr_url: "https://example.test/pr/42".into(),
            },
            expected_tag: "ci_remediation_succeeded",
        },
        TagCase {
            label: "CiFailureCleared",
            event: FrontendEvent::CiFailureCleared {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/42".into(),
            },
            expected_tag: "ci_failure_cleared",
        },
        TagCase {
            label: "CiRemediationFailed",
            event: FrontendEvent::CiRemediationFailed {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "cir_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                failure_reason: "gave up".into(),
            },
            expected_tag: "ci_remediation_failed",
        },
        TagCase {
            label: "CiRemediationAbandoned",
            event: FrontendEvent::CiRemediationAbandoned {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "cir_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                failure_reason: "opt-out".into(),
            },
            expected_tag: "ci_remediation_abandoned",
        },
        TagCase {
            label: "CiRemediationExhausted",
            event: FrontendEvent::CiRemediationExhausted {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                attempts_used: 3,
                budget: 3,
            },
            expected_tag: "ci_remediation_exhausted",
        },
        TagCase {
            label: "CiRemediationFlakyRetriggered",
            event: FrontendEvent::CiRemediationFlakyRetriggered {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                attempt_id: "cir_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                new_run_id: "run_99".into(),
            },
            expected_tag: "ci_remediation_flaky_retriggered",
        },
        TagCase {
            label: "CiNeverStartsAlert",
            event: FrontendEvent::CiNeverStartsAlert {
                product_id: "prod_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                head_sha: "abc123".into(),
                level: "30m".into(),
                elapsed_seconds: 1800,
            },
            expected_tag: "ci_never_starts_alert",
        },
        // --- Effort audit ---
        TagCase {
            label: "EffortAuditReport",
            event: FrontendEvent::EffortAuditReport {
                report: effort_audit_report(),
            },
            expected_tag: "effort_audit_report",
        },
        TagCase {
            label: "EffortEscalationRecorded",
            event: FrontendEvent::EffortEscalationRecorded {
                event: effort_escalation(),
            },
            expected_tag: "effort_escalation_recorded",
        },
        // --- Planner / project lifecycle ---
        TagCase {
            label: "PlannerRunsList",
            event: FrontendEvent::PlannerRunsList {
                project_id: "proj_1".into(),
                runs: vec![],
            },
            expected_tag: "planner_runs_list",
        },
        TagCase {
            label: "PlanProjectResult",
            event: FrontendEvent::PlanProjectResult {
                project_id: "proj_1".into(),
                outcome: "staged".into(),
                message: "ok".into(),
                created: 0,
                edges: 0,
                skipped: 0,
                run_id: None,
                proposal: None,
            },
            expected_tag: "plan_project_result",
        },
        TagCase {
            label: "ReleaseProjectResult",
            event: FrontendEvent::ReleaseProjectResult {
                project_id: "proj_1".into(),
                run_id: "run_1".into(),
                released: 0,
            },
            expected_tag: "release_project_result",
        },
        TagCase {
            label: "UnpopulateProjectResult",
            event: FrontendEvent::UnpopulateProjectResult {
                project_id: "proj_1".into(),
                run_id: "run_1".into(),
                deleted: vec![],
                preserved: vec![],
            },
            expected_tag: "unpopulate_project_result",
        },
        // --- Worker proposals ---
        TagCase {
            label: "ProposalSubmitted",
            event: FrontendEvent::ProposalSubmitted {
                proposal: worker_proposal(),
                already_submitted: false,
            },
            expected_tag: "proposal_submitted",
        },
        TagCase {
            label: "ProposalRejected",
            event: FrontendEvent::ProposalRejected {
                error: ProposalSubmissionError::validation(vec![ProposalFieldError::new(
                    "reason",
                    "required field is missing",
                )]),
            },
            expected_tag: "proposal_rejected",
        },
        TagCase {
            label: "ProposalsList",
            event: FrontendEvent::ProposalsList {
                work_item_id: "task_1".into(),
                proposals: vec![worker_proposal()],
            },
            expected_tag: "proposals_list",
        },
        TagCase {
            label: "WorkerContextResult",
            event: FrontendEvent::WorkerContextResult {
                bundle: Box::new(worker_context_bundle()),
            },
            expected_tag: "worker_context_result",
        },
        TagCase {
            label: "WorkerTierDenied",
            event: FrontendEvent::WorkerTierDenied {
                denial: WorkerTierDenial::redirect(
                    "CreateTask",
                    WorkerTierDenialReason::MutatingTaxonomy,
                    "boss propose followup-task",
                ),
            },
            expected_tag: "worker_tier_denied",
        },
        // --- Feature flags & settings ---
        TagCase {
            label: "FeatureFlagsList",
            event: FrontendEvent::FeatureFlagsList { flags: vec![] },
            expected_tag: "feature_flags_list",
        },
        TagCase {
            label: "FeatureFlagSet",
            event: FrontendEvent::FeatureFlagSet {
                name: "some_flag".into(),
                enabled: true,
            },
            expected_tag: "feature_flag_set",
        },
        // --- Engine metadata ---
        TagCase {
            label: "EngineVersionResult",
            event: FrontendEvent::EngineVersionResult {
                git_sha: "abc123".into(),
                build_time: "2026-01-01T00:00:00Z".into(),
                binary_fingerprint: "def456".into(),
            },
            expected_tag: "engine_version_result",
        },
        TagCase {
            label: "EngineHealthResult",
            event: FrontendEvent::EngineHealthResult {
                report: EngineHealthReport {
                    anthropic_api_key_present: true,
                    dispatch_paused: false,
                    automation_paused: false,
                    issues: vec![],
                },
            },
            expected_tag: "engine_health_result",
        },
        TagCase {
            label: "SettingsList",
            event: FrontendEvent::SettingsList { settings: vec![] },
            expected_tag: "settings_list",
        },
        TagCase {
            label: "SettingSet",
            event: FrontendEvent::SettingSet {
                key: "some_setting".into(),
                enabled: true,
            },
            expected_tag: "setting_set",
        },
        // --- Hosts ---
        TagCase {
            label: "HostsList",
            event: FrontendEvent::HostsList { hosts: vec![] },
            expected_tag: "hosts_list",
        },
        TagCase {
            label: "HostResult",
            event: FrontendEvent::HostResult { host: host_snapshot() },
            expected_tag: "host_result",
        },
        TagCase {
            label: "HostUpdated",
            event: FrontendEvent::HostUpdated { host: host_snapshot() },
            expected_tag: "host_updated",
        },
        TagCase {
            label: "HostRemoved",
            event: FrontendEvent::HostRemoved { id: "local".into() },
            expected_tag: "host_removed",
        },
        // --- Metrics ---
        TagCase {
            label: "MetricsShowLiveResult",
            event: FrontendEvent::MetricsShowLiveResult { entry: None },
            expected_tag: "metrics_show_live_result",
        },
        TagCase {
            label: "MetricsListLiveResult",
            event: FrontendEvent::MetricsListLiveResult { entries: vec![] },
            expected_tag: "metrics_list_live_result",
        },
        TagCase {
            label: "MetricsResetDone",
            event: FrontendEvent::MetricsResetDone {
                name: None,
                counters_reset: 0,
                gauges_reset: 0,
            },
            expected_tag: "metrics_reset_done",
        },
        // --- Dispatch / reconcilers ---
        TagCase {
            label: "PrReconcilersKicked",
            event: FrontendEvent::PrReconcilersKicked { kicked: true },
            expected_tag: "pr_reconcilers_kicked",
        },
        TagCase {
            label: "DispatchStateResult",
            event: FrontendEvent::DispatchStateResult {
                paused: false,
                paused_since_epoch_s: None,
                reviews_exempt: false,
            },
            expected_tag: "dispatch_state_result",
        },
        TagCase {
            label: "ExternalTrackerSyncStarted",
            event: FrontendEvent::ExternalTrackerSyncStarted {
                product_id: "prod_1".into(),
            },
            expected_tag: "external_tracker_sync_started",
        },
        // --- CI remediation lists / budget ---
        TagCase {
            label: "CiRemediationsList",
            event: FrontendEvent::CiRemediationsList { attempts: vec![] },
            expected_tag: "ci_remediations_list",
        },
        TagCase {
            label: "CiRemediation",
            event: FrontendEvent::CiRemediation {
                attempt: ci_remediation(),
            },
            expected_tag: "ci_remediation",
        },
        TagCase {
            label: "CiRemediationRetryDone",
            event: FrontendEvent::CiRemediationRetryDone {
                work_item_id: "task_1".into(),
                budget: ci_budget(),
                was_exhausted: false,
            },
            expected_tag: "ci_remediation_retry_done",
        },
        TagCase {
            label: "CiRemediationMarkedAbandoned",
            event: FrontendEvent::CiRemediationMarkedAbandoned {
                attempt: ci_remediation(),
            },
            expected_tag: "ci_remediation_marked_abandoned",
        },
        TagCase {
            label: "CiBudget",
            event: FrontendEvent::CiBudget { budget: ci_budget() },
            expected_tag: "ci_budget",
        },
        TagCase {
            label: "CiBudgetUpdated",
            event: FrontendEvent::CiBudgetUpdated { budget: ci_budget() },
            expected_tag: "ci_budget_updated",
        },
        TagCase {
            label: "EngineAttemptsList",
            event: FrontendEvent::EngineAttemptsList { attempts: vec![] },
            expected_tag: "engine_attempts_list",
        },
        // --- Reveal / shutdown / auth ---
        TagCase {
            label: "WorkItemRevealed",
            event: FrontendEvent::WorkItemRevealed { id: "task_1".into() },
            expected_tag: "work_item_revealed",
        },
        TagCase {
            label: "DocumentOpened",
            event: FrontendEvent::DocumentOpened {
                path: "/tmp/design.md".into(),
            },
            expected_tag: "document_opened",
        },
        TagCase {
            label: "ShutdownAccepted",
            event: FrontendEvent::ShutdownAccepted,
            expected_tag: "shutdown_accepted",
        },
        TagCase {
            label: "ShutdownRejected",
            event: FrontendEvent::ShutdownRejected {
                reason: "token_mismatch".into(),
            },
            expected_tag: "shutdown_rejected",
        },
        TagCase {
            label: "GitHubAuthState",
            event: FrontendEvent::GitHubAuthState {
                state: GitHubAuthStateDto::Disconnected,
            },
            expected_tag: "git_hub_auth_state",
        },
        TagCase {
            label: "TrunkStatus",
            event: FrontendEvent::TrunkStatus {
                configured: true,
                source: Some("keychain".into()),
                queue_check: None,
                note: None,
            },
            expected_tag: "trunk_status",
        },
        // --- Comments ---
        TagCase {
            label: "CommentResult",
            event: FrontendEvent::CommentResult {
                comment: work_comment(),
            },
            expected_tag: "comment_result",
        },
        TagCase {
            label: "CommentsList",
            event: FrontendEvent::CommentsList {
                artifact_kind: "work_item".into(),
                artifact_id: "task_1".into(),
                comments: vec![],
            },
            expected_tag: "comments_list",
        },
        TagCase {
            label: "CommentsBannerState",
            event: FrontendEvent::CommentsBannerState {
                artifact_kind: "pr_doc".into(),
                artifact_id: "pr_doc:git@example.com:foo.git:main:doc.md".into(),
                state: CommentsBannerState {
                    revisable: false,
                    unresolved_count: 0,
                    in_revision_count: 0,
                    doc_kind: None,
                },
            },
            expected_tag: "comments_banner_state",
        },
        TagCase {
            label: "CommentsResolved",
            event: FrontendEvent::CommentsResolved {
                artifact_kind: "work_item".into(),
                artifact_id: "task_1".into(),
                comments: vec![],
            },
            expected_tag: "comments_resolved",
        },
        TagCase {
            label: "CommentsReviseDocResult",
            event: FrontendEvent::CommentsReviseDocResult {
                outcome: ReviseDocOutcome::NoUnresolvedComments,
            },
            expected_tag: "comments_revise_doc_result",
        },
        // --- Terminals & merge ---
        TagCase {
            label: "ReviewTerminalReady",
            event: FrontendEvent::ReviewTerminalReady {
                work_item_id: "task_1".into(),
                workspace_path: "/tmp/ws".into(),
                lease_id: "lease_1".into(),
            },
            expected_tag: "review_terminal_ready",
        },
        TagCase {
            label: "LiveWorkspaceTerminalReady",
            event: FrontendEvent::LiveWorkspaceTerminalReady {
                work_item_id: "task_1".into(),
                workspace_path: "/tmp/ws".into(),
            },
            expected_tag: "live_workspace_terminal_ready",
        },
        TagCase {
            label: "MergeWhenReadyAccepted",
            event: FrontendEvent::MergeWhenReadyAccepted {
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/42".into(),
                action: "enqueued".into(),
            },
            expected_tag: "merge_when_ready_accepted",
        },
        // --- Automations ---
        TagCase {
            label: "AutomationCreated",
            event: FrontendEvent::AutomationCreated {
                automation: automation(),
            },
            expected_tag: "automation_created",
        },
        TagCase {
            label: "AutomationsList",
            event: FrontendEvent::AutomationsList {
                product_id: "prod_1".into(),
                automations: vec![],
                open_task_counts: std::collections::HashMap::new(),
            },
            expected_tag: "automations_list",
        },
        TagCase {
            label: "AutomationResult",
            event: FrontendEvent::AutomationResult {
                automation: automation(),
            },
            expected_tag: "automation_result",
        },
        TagCase {
            label: "AutomationUpdated",
            event: FrontendEvent::AutomationUpdated {
                automation: automation(),
            },
            expected_tag: "automation_updated",
        },
        TagCase {
            label: "AutomationDeleted",
            event: FrontendEvent::AutomationDeleted {
                automation_id: "aut_1".into(),
            },
            expected_tag: "automation_deleted",
        },
        TagCase {
            label: "AutomationOpenTaskCount",
            event: FrontendEvent::AutomationOpenTaskCount {
                automation_id: "aut_1".into(),
                count: 0,
            },
            expected_tag: "automation_open_task_count",
        },
        TagCase {
            label: "AutomationRunResult",
            event: FrontendEvent::AutomationRunResult { run: automation_run() },
            expected_tag: "automation_run_result",
        },
        // --- Editorial controls ---
        TagCase {
            label: "EditorialActionsList",
            event: FrontendEvent::EditorialActionsList {
                product_id: "prod_1".into(),
                actions: vec![],
            },
            expected_tag: "editorial_actions_list",
        },
        TagCase {
            label: "EditorialRulesEvaluated",
            event: FrontendEvent::EditorialRulesEvaluated {
                product_id: "prod_1".into(),
                decision: "allow".into(),
                findings: vec![],
                rewritten_body: None,
            },
            expected_tag: "editorial_rules_evaluated",
        },
        TagCase {
            label: "AutomationRunsList",
            event: FrontendEvent::AutomationRunsList {
                automation_id: "aut_1".into(),
                runs: vec![],
            },
            expected_tag: "automation_runs_list",
        },
        TagCase {
            label: "AutomationDedupSuppressionsList",
            event: FrontendEvent::AutomationDedupSuppressionsList {
                automation_id: "aut_1".into(),
                suppressions: vec![],
            },
            expected_tag: "automation_dedup_suppressions_list",
        },
        TagCase {
            label: "AutomationTasksList",
            event: FrontendEvent::AutomationTasksList {
                automation_id: "aut_1".into(),
                tasks: vec![],
            },
            expected_tag: "automation_tasks_list",
        },
        TagCase {
            label: "AutomationRunEnqueued",
            event: FrontendEvent::AutomationRunEnqueued {
                automation_id: "aut_1".into(),
            },
            expected_tag: "automation_run_enqueued",
        },
        TagCase {
            label: "AutomationStateResult",
            event: FrontendEvent::AutomationStateResult {
                paused: false,
                paused_since_epoch_s: None,
            },
            expected_tag: "automation_state_result",
        },
        TagCase {
            label: "BoothbyPassesList",
            event: FrontendEvent::BoothbyPassesList { passes: vec![] },
            expected_tag: "boothby_passes_list",
        },
        TagCase {
            label: "BoothbyState",
            event: FrontendEvent::BoothbyState {
                mode: "auto".to_string(),
                open_pass: None,
                last_pass: None,
            },
            expected_tag: "boothby_state",
        },
        TagCase {
            label: "BoothbyPassStarted",
            event: FrontendEvent::BoothbyPassStarted {
                pass: sample_boothby_pass(),
            },
            expected_tag: "boothby_pass_started",
        },
        TagCase {
            label: "BoothbyActivity",
            event: FrontendEvent::BoothbyActivity {
                pass: sample_boothby_pass(),
            },
            expected_tag: "boothby_activity",
        },
    ]
}

fn sample_boothby_pass() -> BoothbyPass {
    BoothbyPass::builder()
        .id("bp_1")
        .started_at("1700000000")
        .trigger("schedule")
        .build()
}

/// Compile-time guard that every `FrontendEvent` variant is represented in
/// [`tag_cases`]. This is a `match` with no wildcard arm: adding or renaming
/// a variant in `events.rs` breaks compilation here until a matching arm (and
/// a corresponding `tag_cases` row) is added. It is the compile-time signal
/// the module doc-comment promises — the antidote to a silent wire-break.
///
/// The function is never called; its body exists only to be type-checked.
#[allow(dead_code)]
fn every_variant_is_pinned(e: &FrontendEvent) {
    match e {
        FrontendEvent::Hello { .. }
        | FrontendEvent::Subscribed { .. }
        | FrontendEvent::Unsubscribed { .. }
        | FrontendEvent::TopicEvent { .. }
        | FrontendEvent::ProductsList { .. }
        | FrontendEvent::ProjectsList { .. }
        | FrontendEvent::TasksList { .. }
        | FrontendEvent::ChoresList { .. }
        | FrontendEvent::RevisionsList { .. }
        | FrontendEvent::WorkTree { .. }
        | FrontendEvent::WorkItemResult { .. }
        | FrontendEvent::WorkItemsByPrResult { .. }
        | FrontendEvent::WorkItemCreated { .. }
        | FrontendEvent::WorkItemsCreated { .. }
        | FrontendEvent::WorkItemUpdated { .. }
        | FrontendEvent::ProjectTasksReordered { .. }
        | FrontendEvent::ExecutionsList { .. }
        | FrontendEvent::TaskRuntimeResult { .. }
        | FrontendEvent::ExecutionResult { .. }
        | FrontendEvent::ExecutionCreated { .. }
        | FrontendEvent::ExecutionRequested { .. }
        | FrontendEvent::PrReviewTriggered { .. }
        | FrontendEvent::RunsList { .. }
        | FrontendEvent::RunResult { .. }
        | FrontendEvent::RunCreated { .. }
        | FrontendEvent::AttentionItemsList { .. }
        | FrontendEvent::AttentionItemResult { .. }
        | FrontendEvent::AttentionItemCreated { .. }
        | FrontendEvent::AttentionItemsForWorkItemList { .. }
        | FrontendEvent::AttentionItemUpdated { .. }
        | FrontendEvent::AttentionItemConverted { .. }
        | FrontendEvent::DeferredScopeAttentionsList { .. }
        | FrontendEvent::AttentionGroupsList { .. }
        | FrontendEvent::AttentionGroupResult { .. }
        | FrontendEvent::AttentionCreated { .. }
        | FrontendEvent::AttentionGroupUpdated { .. }
        | FrontendEvent::AttentionGroupActioned { .. }
        | FrontendEvent::AttentionMergesList { .. }
        | FrontendEvent::WorkItemDeleted { .. }
        | FrontendEvent::WorkItemRestored { .. }
        | FrontendEvent::WorkError { .. }
        | FrontendEvent::WorkItemDuplicateBlocked { .. }
        | FrontendEvent::Error { .. }
        | FrontendEvent::AppSessionRegistered
        | FrontendEvent::EnginePoolConfig { .. }
        | FrontendEvent::BossSessionRegistered
        | FrontendEvent::ProbeQueued { .. }
        | FrontendEvent::ProbeReplied { .. }
        | FrontendEvent::ProbeDeliveryEscalated { .. }
        | FrontendEvent::RunStopped { .. }
        | FrontendEvent::WorkerPaneFocused { .. }
        | FrontendEvent::WorkerInputSent { .. }
        | FrontendEvent::WorkerPaneInterrupted { .. }
        | FrontendEvent::EngineRequest { .. }
        | FrontendEvent::WorkerLiveStatesList { .. }
        | FrontendEvent::ExecutionCancelled { .. }
        | FrontendEvent::RunReaped { .. }
        | FrontendEvent::PaneRetired { .. }
        | FrontendEvent::HuskPanesList { .. }
        | FrontendEvent::RunTranscriptTail { .. }
        | FrontendEvent::ExecutionTranscriptResult { .. }
        | FrontendEvent::ExecutionTranscriptUnavailable { .. }
        | FrontendEvent::WorkspacePoolSummaryResult { .. }
        | FrontendEvent::WorkerPoolSummaryResult { .. }
        | FrontendEvent::DependencyAdded { .. }
        | FrontendEvent::DependencyRemoved { .. }
        | FrontendEvent::DependencyList { .. }
        | FrontendEvent::DependencyDetail { .. }
        | FrontendEvent::LiveStatusEnabledSet { .. }
        | FrontendEvent::LiveStatusDisabledSlotsList { .. }
        | FrontendEvent::LiveStatusDebugReportEvent { .. }
        | FrontendEvent::ProjectDesignDocResolved { .. }
        | FrontendEvent::ProductDesignDocsList { .. }
        | FrontendEvent::ProductDesignDocContent { .. }
        | FrontendEvent::ConflictResolutionMarkedFailed { .. }
        | FrontendEvent::CiRemediationClassified { .. }
        | FrontendEvent::CiRemediationMarkedFailed { .. }
        | FrontendEvent::CiRemediationRetriggered { .. }
        | FrontendEvent::CiRemediationSucceededViaRebase { .. }
        | FrontendEvent::CiRemediationSucceededViaRebaseRejected { .. }
        | FrontendEvent::CiRemediationNoopValidated { .. }
        | FrontendEvent::CiRemediationNoopRejected { .. }
        | FrontendEvent::ConflictResolutionsList { .. }
        | FrontendEvent::ConflictHotspots { .. }
        | FrontendEvent::ConflictResolution { .. }
        | FrontendEvent::ConflictResolutionRetried { .. }
        | FrontendEvent::ConflictResolutionMarkedAbandoned { .. }
        | FrontendEvent::ConflictResolutionStarted { .. }
        | FrontendEvent::ConflictResolutionSucceeded { .. }
        | FrontendEvent::ConflictResolutionFailed { .. }
        | FrontendEvent::ConflictResolutionAbandoned { .. }
        | FrontendEvent::StackProposalOffered { .. }
        | FrontendEvent::CiRemediationStarted { .. }
        | FrontendEvent::CiRemediationSucceeded { .. }
        | FrontendEvent::CiFailureCleared { .. }
        | FrontendEvent::CiRemediationFailed { .. }
        | FrontendEvent::CiRemediationAbandoned { .. }
        | FrontendEvent::CiRemediationExhausted { .. }
        | FrontendEvent::CiRemediationFlakyRetriggered { .. }
        | FrontendEvent::CiNeverStartsAlert { .. }
        | FrontendEvent::EffortAuditReport { .. }
        | FrontendEvent::EffortEscalationRecorded { .. }
        | FrontendEvent::PlannerRunsList { .. }
        | FrontendEvent::PlanProjectResult { .. }
        | FrontendEvent::ReleaseProjectResult { .. }
        | FrontendEvent::ProposalSubmitted { .. }
        | FrontendEvent::ProposalRejected { .. }
        | FrontendEvent::ProposalsList { .. }
        | FrontendEvent::WorkerContextResult { .. }
        | FrontendEvent::WorkerTierDenied { .. }
        | FrontendEvent::UnpopulateProjectResult { .. }
        | FrontendEvent::FeatureFlagsList { .. }
        | FrontendEvent::FeatureFlagSet { .. }
        | FrontendEvent::EngineVersionResult { .. }
        | FrontendEvent::EngineHealthResult { .. }
        | FrontendEvent::SettingsList { .. }
        | FrontendEvent::SettingSet { .. }
        | FrontendEvent::HostsList { .. }
        | FrontendEvent::HostResult { .. }
        | FrontendEvent::HostUpdated { .. }
        | FrontendEvent::HostRemoved { .. }
        | FrontendEvent::MetricsShowLiveResult { .. }
        | FrontendEvent::MetricsListLiveResult { .. }
        | FrontendEvent::MetricsResetDone { .. }
        | FrontendEvent::PrReconcilersKicked { .. }
        | FrontendEvent::DispatchStateResult { .. }
        | FrontendEvent::ExternalTrackerSyncStarted { .. }
        | FrontendEvent::CiRemediationsList { .. }
        | FrontendEvent::CiRemediation { .. }
        | FrontendEvent::CiRemediationRetryDone { .. }
        | FrontendEvent::CiRemediationMarkedAbandoned { .. }
        | FrontendEvent::CiBudget { .. }
        | FrontendEvent::CiBudgetUpdated { .. }
        | FrontendEvent::EngineAttemptsList { .. }
        | FrontendEvent::WorkItemRevealed { .. }
        | FrontendEvent::DocumentOpened { .. }
        | FrontendEvent::ShutdownAccepted
        | FrontendEvent::ShutdownRejected { .. }
        | FrontendEvent::GitHubAuthState { .. }
        | FrontendEvent::TrunkStatus { .. }
        | FrontendEvent::CommentResult { .. }
        | FrontendEvent::CommentsList { .. }
        | FrontendEvent::CommentsBannerState { .. }
        | FrontendEvent::CommentsResolved { .. }
        | FrontendEvent::CommentsReviseDocResult { .. }
        | FrontendEvent::ReviewTerminalReady { .. }
        | FrontendEvent::LiveWorkspaceTerminalReady { .. }
        | FrontendEvent::MergeWhenReadyAccepted { .. }
        | FrontendEvent::AutomationCreated { .. }
        | FrontendEvent::AutomationsList { .. }
        | FrontendEvent::AutomationResult { .. }
        | FrontendEvent::AutomationUpdated { .. }
        | FrontendEvent::AutomationDeleted { .. }
        | FrontendEvent::AutomationOpenTaskCount { .. }
        | FrontendEvent::AutomationRunResult { .. }
        | FrontendEvent::EditorialActionsList { .. }
        | FrontendEvent::EditorialRulesEvaluated { .. }
        | FrontendEvent::AutomationRunsList { .. }
        | FrontendEvent::AutomationDedupSuppressionsList { .. }
        | FrontendEvent::AutomationTasksList { .. }
        | FrontendEvent::AutomationRunEnqueued { .. }
        | FrontendEvent::AutomationStateResult { .. }
        | FrontendEvent::BoothbyPassesList { .. }
        | FrontendEvent::BoothbyState { .. }
        | FrontendEvent::BoothbyPassStarted { .. }
        | FrontendEvent::BoothbyActivity { .. } => {}
    }
}

#[test]
fn variants_serialize_under_their_snake_case_type_tag() {
    for case in tag_cases() {
        let v = serde_json::to_value(&case.event).unwrap();
        assert_eq!(
            v["type"], case.expected_tag,
            "{} must serialize with type={:?}, got {v}",
            case.label, case.expected_tag
        );
    }
}

#[test]
fn variants_round_trip_structurally() {
    // Serialize → deserialize → re-serialize and compare the two JSON
    // values. Structural equality across the round-trip proves the
    // deserializer accepts exactly what the serializer emits — the
    // property the frontends depend on.
    for case in tag_cases() {
        let json = serde_json::to_string(&case.event).unwrap();
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap_or_else(|e| {
            panic!("{} failed to deserialize from {json}: {e}", case.label);
        });
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&case.event).unwrap(),
            "{} did not survive a serde round-trip structurally",
            case.label
        );
    }
}

// --- Field-name grammar ----------------------------------------------------

#[test]
fn hello_pins_session_id_field() {
    let v = serde_json::to_value(FrontendEvent::Hello {
        session_id: "sess_1".into(),
    })
    .unwrap();
    assert_eq!(v["type"], "hello");
    assert_eq!(v["session_id"], "sess_1");
}

#[test]
fn subscribed_pins_topics_and_current_revision_fields() {
    let v = serde_json::to_value(FrontendEvent::Subscribed {
        topics: vec!["a".into(), "b".into()],
        current_revision: 42,
    })
    .unwrap();
    assert_eq!(v["type"], "subscribed");
    assert_eq!(v["topics"], serde_json::json!(["a", "b"]));
    assert_eq!(v["current_revision"], 42);
}

#[test]
fn topic_event_pins_field_names_and_nested_payload_tag() {
    let v = serde_json::to_value(FrontendEvent::TopicEvent {
        topic: "work.product.prod_1".into(),
        revision: 5,
        origin_session_id: "sess_2".into(),
        origin_request_id: Some("req_9".into()),
        event: TopicEventPayload::WorkInvalidated {
            reason: "updated".into(),
            product_id: Some("prod_1".into()),
            item_ids: vec!["task_1".into(), "task_2".into()],
        },
    })
    .unwrap();
    assert_eq!(v["type"], "topic_event");
    assert_eq!(v["topic"], "work.product.prod_1");
    assert_eq!(v["revision"], 5);
    assert_eq!(v["origin_session_id"], "sess_2");
    assert_eq!(v["origin_request_id"], "req_9");
    // The nested payload carries its OWN `type` discriminator under the
    // `event` key — the frontend decodes it as a tagged sub-object.
    assert_eq!(v["event"]["type"], "work_invalidated");
    assert_eq!(v["event"]["reason"], "updated");
    assert_eq!(v["event"]["product_id"], "prod_1");
    assert_eq!(v["event"]["item_ids"], serde_json::json!(["task_1", "task_2"]));
}

// --- Option omission grammar (skip_serializing_if) -------------------------

#[test]
fn skipped_option_field_is_omitted_not_null_when_none() {
    // `live_sha` is `#[serde(skip_serializing_if = "Option::is_none")]`.
    // A `None` must vanish from the wire entirely — absent, not JSON
    // `null`. Frontends rely on absence to distinguish "no value" from an
    // explicit null; emitting `null` would break that distinction.
    let v = serde_json::to_value(FrontendEvent::CiRemediationSucceededViaRebaseRejected {
        attempt_id: "cir_1".into(),
        work_item_id: "task_1".into(),
        pr_url: "https://example.test/pr/1".into(),
        status: "pr_closed".into(),
        live_sha: None,
    })
    .unwrap();
    assert_eq!(v["type"], "ci_remediation_succeeded_via_rebase_rejected");
    assert_eq!(v["attempt_id"], "cir_1");
    assert_eq!(v["work_item_id"], "task_1");
    assert_eq!(v["pr_url"], "https://example.test/pr/1");
    assert_eq!(v["status"], "pr_closed");
    assert!(
        v.get("live_sha").is_none(),
        "live_sha must be omitted (not null) when None, got: {v}"
    );
}

#[test]
fn skipped_option_field_is_present_when_some() {
    // The companion to the omission test: a `Some` value serializes under
    // its documented name, so absence in the test above is genuinely
    // driven by `None`, not by the field never serializing at all.
    let v = serde_json::to_value(FrontendEvent::CiRemediationSucceededViaRebaseRejected {
        attempt_id: "cir_1".into(),
        work_item_id: "task_1".into(),
        pr_url: "https://example.test/pr/1".into(),
        status: "still_pending".into(),
        live_sha: Some("deadbeef".into()),
    })
    .unwrap();
    assert_eq!(v["live_sha"], "deadbeef");
}
