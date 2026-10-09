//! Startup registration of every counter / gauge the engine declares.
//!
//! The metrics framework itself lives in the [`boss_metrics`] crate
//! and knows nothing about the engine's modules. Naming them is this
//! module's job — it is the one place that has to reach across the
//! whole engine, which is exactly why it stays here rather than
//! moving down into `boss_metrics`.

use boss_metrics::Registry;

/// Force registration of every counter / gauge handle the engine
/// declares.
///
/// `LazyLock`-style registration would let a counter living in a
/// rarely-loaded module miss its first flush window (and would push
/// the duplicate-name panic from boot into the middle of a busy
/// sweep — see design §"Risks / open questions" item 6, which is
/// load-bearing for item 2). The cure is this single function that
/// touches every handle so registration happens once, deterministically,
/// at engine startup.
///
/// As each new counter module lands, add one line here to register
/// its handles so duplicate-name panics surface at boot rather than
/// at the first increment (design §"Risks / open questions" item 6).
pub fn init_all(registry: &Registry) {
    crate::work::personas::register_metrics(registry);
    // Question → answer-agent lifecycle counters and queue-wait histogram.
    crate::answer_agent_observability::register_metrics(registry);
    // Phase 3: PR URL capture path counters.
    crate::completion::register_metrics(registry);
    // Phase 3: Dependency-unblock sweep gauge.
    crate::dep_unblock_sweep::register_metrics(registry);
    // Phase 3: Cube workspace lease counters.
    crate::coordinator::register_metrics(registry);
    // Phase 4: DispatcherStats counters migrated to the framework.
    crate::live_status_loop::register_metrics(registry);
    // Phase 5: SweepOutcome / merge_poller counters.
    crate::merge_poller::init(registry);
    // External tracker reconciler pass counters.
    crate::external_tracker::reconcile::register_metrics(registry);
    // Speculative conflict-prediction sweep counters.
    crate::speculative_conflict::init(registry);
    // Stacked-PR auto-structuring offer counters.
    crate::stacked_pr_structuring::init(registry);
    // Queue-level dispatch telemetry: per-pool depth/oldest-wait gauges,
    // dispatch-completed counter, drain-pass-duration gauge.
    crate::dispatch_metrics::register_metrics(registry);
    // Trunk merge-queue poller: probe/lookup volume, state writes, intent
    // retirements, and attention items.
    crate::trunk_queue_poller::init(registry);
    // Worker-proposal API: SubmitProposal counters (submissions by kind,
    // validation failures, rate-limit hits).
    crate::app::proposals::register_metrics(registry);
    // Worker-proposal API: proposal_channel_error detection counter.
    crate::proposal_channel_error::register_metrics(registry);
    // Screenshot-evidence attachments: SubmitAttachment ingest, replay, and
    // refusal counters.
    crate::app::attachments::register_metrics(registry);
    // Codex unobserved-command detection counter (item.started with no
    // item.completed before the turn boundary).
    crate::codex_unobserved_command::register_metrics(registry);
    // Codex PreToolUse guard observation: guard activity reported per turn,
    // and the silent-fail-open signal (tool calls with no guard invocation).
    crate::codex_guard_trace::register_metrics(registry);
    // GitHub API usage telemetry: call/points/rate-limit totals and the
    // remaining-quota gauges. The per-caller breakdown is registered
    // dynamically on first use (the caller x bucket axis isn't a
    // compile-time-known set), so only the statics land here.
    crate::github_api_usage::register_metrics(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_all_registers_all_declared_counters() {
        let registry = Registry::new();
        init_all(&registry);
        let mut counter_names: Vec<_> = registry.counter_snapshots().into_iter().map(|s| s.name).collect();
        counter_names.sort();
        // Exhaustive and one-name-per-line on purpose: a hand-maintained
        // total would let two PRs that each add a counter bump it to the same
        // value and merge silently into a wrong number. Adjacent-line edits
        // here conflict textually (or merge correctly) instead. Keep sorted.
        // Dynamically registered counters (answer-agent error kinds,
        // `completion.mid_turn_reap.<source>.count`, github_api per-caller)
        // are not registered by `init_all` and stay out of this list.
        assert_eq!(
            counter_names,
            vec![
                "answer_agent.enqueued",
                "answer_agent.failed",
                "answer_agent.queue_wait_ms",
                "answer_agent.replied",
                "answer_agent.started",
                "answer_agent.superseded",
                "codex.guard_trace.reported",
                "codex.guard_trace.silent",
                "codex.unobserved_command",
                "codex.unobserved_command_overflow",
                "completion.mid_turn_reap.total",
                "cube_workspace_lease.attempts",
                "cube_workspace_lease.failure",
                "cube_workspace_lease.success",
                "dispatch.completed",
                "dispatcher.hook_events.dropped_missing_run_id",
                "dispatcher.hook_events.for_terminal_execution",
                "dispatcher.hook_events.total",
                "dispatcher.hook_events.with_transcript_path",
                "dispatcher.hook_events.without_transcript_path",
                "dispatcher.transcript_path_persist.err",
                "dispatcher.transcript_path_persist.from_cache",
                "dispatcher.transcript_path_persist.noop",
                "dispatcher.transcript_path_persist.row_missing",
                "dispatcher.transcript_path_persist.updated",
                "external_tracker.closed",
                "external_tracker.fetch_failed",
                "external_tracker.fetch_succeeded",
                "external_tracker.imported",
                "external_tracker.in_progress_set_failed",
                "external_tracker.in_progress_set_succeeded",
                "external_tracker.pr_attached",
                "external_tracker.pr_merge_close_failed",
                "external_tracker.pr_merge_close_succeeded",
                "external_tracker.reverse_close_failed",
                "external_tracker.reverse_close_succeeded",
                "external_tracker.skip_no_credential",
                "external_tracker.skipped_closed_at_first_sight",
                "external_tracker.title_body_conflict",
                "external_tracker.title_body_synced",
                "external_tracker.tracked_label_attach_failed",
                "external_tracker.tracked_label_attach_succeeded",
                "external_tracker.unbound",
                "github_api.calls.total",
                "github_api.calls.unattributed",
                "github_api.calls.without_reading",
                "github_api.points.total",
                "github_api.rate_limited.total",
                "merge_poller.adaptive_batches",
                "merge_poller.adaptive_prs_reconciled",
                "merge_poller.comments_reopened",
                "merge_poller.conflict_cleared",
                "merge_poller.conflict_flagged",
                "merge_poller.late_pr_recovered",
                "merge_poller.merge_queue_rebounced",
                "merge_poller.merged",
                "merge_poller.pass_overrun",
                "merge_poller.pass_timed_out",
                "merge_poller.pr_recheck_recovered",
                "merge_poller.pr_recheck_unresolved",
                "merge_poller.revision_invalidated",
                "merge_poller.trunk_episodes_adopted",
                "merge_poller.worker_stopped_on_review",
                "nudge_ladder.sweep_advanced",
                "persona_roster_exhausted",
                "pr_url_capture.artifact.hit",
                "pr_url_capture.driver_fallback.hit",
                "pr_url_capture.primary_path.hit",
                "pr_url_capture.recheck_staged.branch_mismatch",
                "pr_url_capture.reconstruction_path.failed",
                "pr_url_capture.reconstruction_path.hit",
                "review_pool.admission_deferred",
                "review_pool.admission_recovered",
                "review_pool.batch_reaped",
                "run_done.backstop_asked",
                "run_done.backstop_parked",
                "run_done.gate_held",
                "speculative_conflict.clean",
                "speculative_conflict.predicted",
                "stacked_pr_structuring.offered",
                "trunk_queue_poller.attentions_filed",
                "trunk_queue_poller.entry_lookups",
                "trunk_queue_poller.evictions_detected",
                "trunk_queue_poller.intents_retired",
                "trunk_queue_poller.queue_cancellations",
                "trunk_queue_poller.queue_probe_failures",
                "trunk_queue_poller.queue_probes",
                "trunk_queue_poller.resubmits",
                "trunk_queue_poller.state_writes",
                "work_attachments.rate_limited",
                "work_attachments.replayed",
                "work_attachments.stored",
                "work_attachments.validation_failed",
                "worker_proposals.channel_error",
                "worker_proposals.fallback_hit.automation_outcome",
                "worker_proposals.fallback_hit.blocked",
                "worker_proposals.fallback_hit.deferred_scope",
                "worker_proposals.fallback_hit.effort_escalation",
                "worker_proposals.fallback_hit.followup_task",
                "worker_proposals.fallback_hit.run_done",
                "worker_proposals.rate_limited",
                "worker_proposals.submitted.attention",
                "worker_proposals.submitted.automation_outcome",
                "worker_proposals.submitted.blocked",
                "worker_proposals.submitted.deferred_scope",
                "worker_proposals.submitted.effort_escalation",
                "worker_proposals.submitted.followup_task",
                "worker_proposals.submitted.pr_created",
                "worker_proposals.submitted.review_guide",
                "worker_proposals.submitted.review_report",
                "worker_proposals.submitted.review_verdict",
                "worker_proposals.submitted.run_done",
                "worker_proposals.validation_failed",
            ],
            "init_all must register exactly these counters",
        );
        // Phase 3: dep_unblock gauge, plus the queue-level dispatch gauges.
        let gauge_names: Vec<_> = registry.gauge_snapshots().into_iter().map(|s| s.name).collect();
        assert_eq!(
            gauge_names,
            vec![
                "dependency_unblock.longest_stale_seconds",
                "dispatch.drain_pass_duration_ms",
                "dispatch.queue_depth.automation",
                "dispatch.queue_depth.main",
                "dispatch.queue_depth.review",
                "dispatch.queue_oldest_wait_seconds.automation",
                "dispatch.queue_oldest_wait_seconds.main",
                "dispatch.queue_oldest_wait_seconds.review",
                "github_api.graphql.remaining",
                "github_api.rest.remaining",
                "merge_poller.adaptive_tracked",
                "review_pool.deferred_pre_merge",
                "review_pool.reserved_units",
            ],
            "init_all must register the dep_unblock gauge, the queue-level dispatch gauges, \
             the GitHub remaining-quota gauges, and the adaptive-schedule size gauge",
        );
    }
}
