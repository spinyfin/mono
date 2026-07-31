//! Worker hook-event dispatch functions.
//!
//! Split out of `app.rs`; all `dispatch_*` functions that react to worker
//! hook events live here. Pure structural move — no behavioural change.

use super::*;
use crate::driver::{AgentDriver, Capability, ClaudeDriver};
use crate::live_worker_state::DriverSignalKind;

impl ServerState {
    /// First call for a given `execution_id` returns `true` (and remembers
    /// it); every subsequent call for the same id returns `false`. Used to
    /// downgrade the post-hoc-interception loss-of-guards warning from a
    /// per-`PostToolUse`-event `warn!` to a per-execution one, so a
    /// long-running hookless-driver worker logs the signal once instead of
    /// once per tool call.
    fn should_warn_post_hoc_interception_loss(&self, execution_id: &str) -> bool {
        self.post_hoc_interception_warned
            .lock()
            .expect("post_hoc_interception_warned mutex poisoned")
            .insert(execution_id.to_owned())
    }
}

/// Update the per-slot LiveWorkerState for the run this hook event
/// belongs to and push the new snapshot on the
/// `worker.live_states` topic if anything changed. Hook events that
/// arrive before the run has been registered (e.g., the spawn flow
/// hasn't recorded the slot yet) are silently dropped — once the
/// registration lands, subsequent events will hit the live entry.
fn worker_event_kind(event: &crate::protocol::WorkerEvent) -> &'static str {
    use crate::protocol::WorkerEvent;
    match event {
        WorkerEvent::SessionStart { .. } => "session_start",
        WorkerEvent::UserPromptSubmit { .. } => "user_prompt_submit",
        WorkerEvent::PreToolUse { .. } => "pre_tool_use",
        WorkerEvent::PostToolUse { .. } => "post_tool_use",
        WorkerEvent::Stop { .. } => "stop",
        WorkerEvent::Notification { .. } => "notification",
        WorkerEvent::SessionEnd { .. } => "session_end",
    }
}

/// The engine's [`crate::stdout_progress::WorkerEventSink`]: a
/// A byte-stream driver's progress (`StdoutJsonl` or `AgentJsonlFile`) lands
/// here and takes the identical fan-out the events-socket accept loop takes for a
/// `ProgressIngress::HookCallback` driver. This impl is the whole of the
/// byte-stream arm's engine-side behaviour — everything downstream is shared
/// with the hook path, by construction.
/// Implemented on `Arc<ServerState>` rather than `ServerState` because the
/// fan-out's handlers clone the `Arc` into spawned work; `&self` is then
/// already the `&Arc<ServerState>` they need.
#[async_trait::async_trait]
impl crate::stdout_progress::WorkerEventSink for Arc<ServerState> {
    fn progress_identity_store(&self) -> Option<Arc<dyn crate::driver::ProgressIdentityStore>> {
        Some(self.work_db.clone())
    }

    async fn dispatch_worker_event(&self, incoming: crate::events_socket::IncomingHookEvent) {
        dispatch_worker_event_fanout(self, &incoming).await;
    }
}

/// Fan one normalised worker event out to every engine subsystem that reacts
/// to worker progress, in the order those subsystems require.
///
/// Transport-agnostic on purpose. Both progress ingresses converge here:
/// [`crate::events_socket`] (a `ProgressIngress::HookCallback` driver's
/// `boss-event` shim over the unix socket) and
/// [`crate::stdout_progress`] (a byte-stream driver's stdout or rollout
/// stream). By the time an event reaches this function the driver has
/// already normalised it, so which transport carried it is no longer
/// observable — which is exactly the property that makes live-status, the
/// staleness sweeps, and the kanban see the same shapes from either one.
///
/// Ordering here is load-bearing; see the per-call comments.
pub(super) async fn dispatch_worker_event_fanout(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    // Audit *before* the live-state fan-out
    // so an engine-side mismatch in the
    // dispatch path can't drop the audit line
    // — the deny is enforced harness-side by
    // claude already, this is the independent
    // forensic record. See
    // [`worker_sandbox_audit`] for why.
    crate::worker_sandbox_audit::record_if_sandbox_attempt(
        &server_state.dispatch_event_root,
        incoming.run_id.as_deref(),
        &incoming.event,
    );
    crate::events_socket::publish_hook_derived_events(&server_state.event_bus, incoming).await;
    dispatch_live_worker_state(server_state, incoming).await;
    // Codex unobserved-command detection: stage any abandoned-command
    // Notification the progress session emitted ahead of its Stop, so
    // `on_stop`'s NO_CHANGES_NEEDED gate (dispatched later in this same
    // fan-out chain, on the following Stop event) sees it.
    dispatch_codex_unobserved_command_on_notification(server_state, incoming);
    // Codex guard-trace observation: record whether this turn's PreToolUse
    // guards ran and what they decided — and, when tool calls ran with no
    // guard invocation at all, that Boss's command guardrails were not
    // enforced for it. Nothing in Codex's own stream carries either fact.
    dispatch_codex_guard_trace_on_notification(server_state, incoming);
    // Editorial PreToolUse audit: evaluate every
    // `gh pr|issue` Bash invocation against the
    // product's editorial rules and record the
    // decision in `editorial_actions`. Fire-and-
    // forget; never blocks the event dispatch.
    dispatch_editorial_on_pretooluse(server_state, incoming).await;
    // Post-hoc interception fallback: for any driver
    // that lacks real-time PreToolUse hooks (the
    // ToolUseInterception Degrade path), this is the
    // only place editorial/path/revision-PR/
    // checkleft loss ever gets surfaced. No-op for
    // Claude, which already ran those guards above.
    dispatch_post_hoc_interception_on_post_tool_use(server_state, incoming).await;
    // Probes fire on PostToolUse so the
    // coordinator can redirect a worker
    // mid-task without waiting for Stop —
    // which, for a long autonomous turn, is
    // effectively the worker's terminal one.
    // The tool call has already returned at
    // this point, so no in-flight work is lost.
    let _ = dispatch_probe_on_post_tool_use(server_state, incoming).await;
    // ProbeReplied runs first: emit the reply for the
    // prior probe before dispatching the next one so
    // a single Stop never fires both reply and dispatch
    // for the same probe (the reply text hasn't been
    // written yet at dispatch time).
    //
    // Completion runs before probe dispatch: probes
    // queued by the completion handler (e.g.
    // PROBE_NO_PR) must be visible to `dispatch_probe_on_stop`
    // so they are delivered on the *same* Stop that
    // triggered them rather than stalling until the
    // next Stop (which never comes for an idle worker).
    // Durably record the boundary BEFORE anything reacts to it, so it
    // survives a completion handler that errors, an engine restart, or the
    // pane going away milliseconds later.
    record_turn_boundary_on_stop(server_state, incoming);
    dispatch_probe_reply_on_stop(server_state, incoming).await;
    dispatch_completion_on_stop(server_state, incoming).await;
    let _ = dispatch_probe_on_stop(server_state, incoming).await;
}

/// Stamp `work_runs.turn_boundary_at` for the run whose driver just reported a
/// turn boundary.
///
/// Keyed on [`crate::events_socket::IncomingHookEvent::is_turn_boundary`] —
/// the driver-resolved signal — not on the `Stop` variant, so a driver whose
/// turn ends some other way records it too.
///
/// Best-effort by design: a failed write is logged and otherwise ignored.
pub(super) fn record_turn_boundary_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    if !incoming.is_turn_boundary() {
        return;
    }
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let at = crate::live_worker_state::iso8601_utc(boss_engine_utils::epoch_time::now_epoch_secs());
    match server_state.work_db.record_run_turn_boundary_for_execution(run_id, &at) {
        Ok(true) => tracing::debug!(run_id, at = %at, "recorded turn boundary on run"),
        Ok(false) => tracing::debug!(
            run_id,
            "turn boundary observed before any run row existed; not recorded"
        ),
        Err(err) => tracing::warn!(
            run_id,
            ?err,
            "failed to record turn boundary on run; a one-shot worker's clean exit \
             will now be reaped as a death (fail-safe direction)",
        ),
    }
}

/// Stage a Codex "unobserved command" signal: a `command_execution` whose
/// start record was observed but never completed before its turn boundary.
/// The progress session in `boss_engine_driver::codex` detects the gap
/// structurally (it already correlates start/complete pairs to emit
/// `PreToolUse`/`PostToolUse`) and surfaces it as a `WorkerEvent::Notification`
/// carrying [`crate::driver::codex::UNOBSERVED_COMMAND_MARKER`], ordered
/// ahead of the `Stop` it precedes in the same normalised batch. Staging it
/// here — mirroring the `boss propose` failure capture in
/// `proposal_channel_error` — lets `WorkerCompletionHandler::on_stop` see it
/// by the time that later `Stop` event's own dispatch call reaches
/// `dispatch_completion_on_stop`.
///
/// A no-op for every event that isn't a matching `Notification` — in
/// particular, every hook-shaped event Claude and every other driver emits.
pub(super) fn dispatch_codex_unobserved_command_on_notification(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::WorkerEvent;
    let WorkerEvent::Notification { message, .. } = &incoming.event else {
        return;
    };
    let Some(command) = message.strip_prefix(crate::driver::codex::UNOBSERVED_COMMAND_MARKER) else {
        return;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    use crate::codex_unobserved_command::RecordOutcome;
    match server_state.staged_unobserved_commands.record(run_id, command.trim()) {
        RecordOutcome::Staged => {
            tracing::warn!(
                execution_id = run_id,
                command = command.trim(),
                "codex_unobserved_command: staged an abandoned command_execution (item.started with no \
                 item.completed observed before the turn boundary)",
            );
        }
        RecordOutcome::Duplicate => {}
        RecordOutcome::CapExceeded => {
            // Loud by design: the audit trail stopped growing, but the
            // NO_CHANGES_NEEDED refusal gate (`consume_unresolved`) does not
            // depend on this cap and still fires — see
            // `codex_unobserved_command::MAX_COMMANDS_PER_EXECUTION`.
            crate::codex_unobserved_command::CODEX_UNOBSERVED_COMMAND_OVERFLOW.inc(&server_state.metrics);
            tracing::error!(
                execution_id = run_id,
                command = command.trim(),
                "codex_unobserved_command: audit trail exceeded MAX_COMMANDS_PER_EXECUTION distinct \
                 abandoned commands for this execution; this command was not added to the trail (the \
                 NO_CHANGES_NEEDED refusal gate still fires)",
            );
        }
    }
}

/// Record a Codex guard-trace notification (see [`crate::codex_guard_trace`]).
///
/// Unlike the unobserved-command handler this stages nothing: the question it
/// answers — "did this execution's PreToolUse guards run, and what did they
/// decide?" — is a forensic one, answered from the engine log and the run's
/// `guard-trace.jsonl`. The silent-guard case is logged at `error` because it
/// means the worker prompt's "pushes are blocked" assertion was not being
/// enforced for that turn.
///
/// A no-op for every event that isn't a matching `Notification` — in
/// particular every hook-shaped event Claude and every other driver emits.
pub(super) fn dispatch_codex_guard_trace_on_notification(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::WorkerEvent;
    let WorkerEvent::Notification { message, .. } = &incoming.event else {
        return;
    };
    let Some(signal) = crate::codex_guard_trace::classify(message) else {
        return;
    };
    crate::codex_guard_trace::record(&server_state.metrics, incoming.run_id.as_deref(), &signal);
}

pub(super) async fn dispatch_live_worker_state(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    let event_kind = worker_event_kind(&incoming.event);
    server_state.dispatcher_stats.inc_hook_events_total();
    tracing::info!(
        run_id = ?incoming.run_id,
        peer_pid = ?incoming.peer_pid,
        kind = event_kind,
        has_transcript_path = incoming.transcript_path.is_some(),
        "live_status: hook payload arrived at dispatcher",
    );
    let Some(run_id) = incoming.run_id.as_deref() else {
        server_state.dispatcher_stats.inc_dropped_missing_run_id();
        tracing::warn!(
            kind = event_kind,
            peer_pid = ?incoming.peer_pid,
            "live_status: dropping hook — neither _boss_run_id payload nor peer-pid ancestor walk produced a run_id",
        );
        return;
    };
    server_state.dispatcher_stats.record_last_hook(run_id, event_kind);
    // **Driver-start verification.** A hook is the earliest
    // driver-ORIGINATED evidence Boss ever gets that the driver binary
    // actually executed — a login shell cannot emit one. Record it here,
    // at the top of the ingress, rather than down in the `apply_event`
    // fan-out: the slot lookup below can legitimately miss (a hook racing
    // `register_run_slot`, a released slot), and driver-start proof must
    // not be contingent on that. First signal wins; later hooks are a
    // cheap no-op.
    //
    // Until this existed, "the spawn is acked" meant only that the app
    // accepted a slot and a foreground pid appeared — both true of a pane
    // hosting nothing but an idle login shell. See
    // `LiveWorkerStateRegistry::unverified_driver_starts`.
    server_state
        .live_worker_states
        .record_driver_signal(run_id, DriverSignalKind::HookEvent);
    // Resolve any outstanding pane-injection delivery waiter for this
    // run. A `UserPromptSubmit` hook is the CLI's own confirmation
    // that it enqueued *something* as the next prompt; when a probe
    // or chore-update notice is mid-flight (see
    // `ServerState::inject_pane_text_verified`), this is what turns
    // "bytes reached the pty" into "the worker actually got it". A
    // no-op when nothing is waiting, which is the ordinary case for
    // the worker's own prompts.
    if let crate::protocol::WorkerEvent::UserPromptSubmit { prompt, .. } = &incoming.event {
        server_state.resolve_delivery_waiter(run_id, prompt);
    }
    // Persist the transcript path the moment we see it on a hook
    // payload. `start_execution_run` inserts the work_runs row with
    // `transcript_path = NULL` (the engine has no way to know the
    // path until the worker tells us via its first hook), so without
    // this write the live-status summarizer's `TranscriptPathResolver`
    // returns None forever and the per-slot loop early-outs every
    // tick on "no transcript path yet". The setter is idempotent
    // (first-writer-wins) so we don't clobber the path the tail
    // watcher has already opened across later sessions/resumes.
    //
    // This runs BEFORE the slot lookup so it survives the cases where
    // `slot_for_run` would otherwise drop the event: a first hook
    // racing ahead of `register_run_slot`, an engine restart that
    // wipes the in-memory `WorkerRegistry` while pre-existing workers
    // keep firing hooks, or a late hook arriving after the slot has
    // been released. The persist is keyed solely on `run_id` and does
    // not need the slot mapping — gating it under that lookup was the
    // gap that pinned `work_runs.transcript_path` at NULL across
    // engine restarts.
    //
    // **2026-05-12 follow-up:** PR #366's persist branch only fires
    // when the current hook's payload carries `transcript_path`. In
    // production that turned out to be insufficient — claude does
    // not include the field on every event kind, and the work_runs
    // row may not even exist yet at the moment a SessionStart fires
    // (the engine inserts it from a separate code path that races
    // the worker's startup hooks). The fix is to cache the path
    // engine-side keyed by run id, so a later PostToolUse / Stop /
    // whatever can persist the cached value even when its own
    // payload omits the field.
    let payload_path = incoming.transcript_path.as_deref();
    let (resolved_path, from_cache) = match payload_path {
        Some(path) => {
            server_state.dispatcher_stats.inc_with_transcript_path();
            let _ = server_state.transcript_path_cache.record_if_unset(run_id, path);
            (Some(path.to_owned()), false)
        }
        None => {
            server_state.dispatcher_stats.inc_without_transcript_path();
            match server_state.transcript_path_cache.get(run_id) {
                Some(cached) => (Some(cached), true),
                None => (None, false),
            }
        }
    };
    if let Some(path) = resolved_path.as_deref() {
        // The second driver-originated signal named in the driver-start
        // contract: a resolved transcript path means the driver created
        // its transcript. Redundant with the hook stamp above on this
        // code path (both are reached from the same ingress), and
        // recorded anyway so the contract holds at every site that
        // learns a transcript path rather than depending on the two
        // staying adjacent.
        server_state
            .live_worker_states
            .record_driver_signal(run_id, DriverSignalKind::TranscriptPath);
        // `run_id` here is the `_boss_run_id` from the hook payload,
        // which carries the **execution_id** (`exec_*`) — not a
        // `work_runs.id` (`run_*`). The setter joins on
        // `work_runs.execution_id` so the caller doesn't have to
        // care; the local `execution_id` binding is just to make
        // the namespace explicit at the call site, since the
        // historical "run_id" naming all the way through the
        // dispatcher is what hid the wrong-namespace bug.
        let execution_id = run_id;
        match server_state
            .work_db
            .set_run_transcript_path_if_unset(execution_id, path)
        {
            Ok(SetRunTranscriptPathOutcome::Updated) => {
                server_state.dispatcher_stats.inc_persist_updated();
                if from_cache {
                    server_state.dispatcher_stats.inc_persist_from_cache();
                }
                tracing::info!(
                    execution_id,
                    transcript_path = %path,
                    from_cache,
                    "recorded transcript_path on work_run from hook payload",
                );
            }
            Ok(SetRunTranscriptPathOutcome::AlreadySet) => {
                server_state.dispatcher_stats.inc_persist_noop();
            }
            Ok(SetRunTranscriptPathOutcome::RowMissing) => {
                server_state.dispatcher_stats.inc_persist_row_missing();
                tracing::warn!(
                    execution_id,
                    transcript_path = %path,
                    "no work_runs row for execution yet; transcript_path persist deferred to a later hook",
                );
            }
            Err(err) => {
                server_state.dispatcher_stats.inc_persist_err();
                tracing::warn!(
                    execution_id,
                    ?err,
                    "failed to persist transcript_path from hook payload",
                );
            }
        }

        // Fold transcript records appended since the prior hook into a
        // cumulative, idempotent snapshot. The driver-owned containment root
        // was resolved at ingress alongside the turn boundary; failure to
        // resolve it refuses the read instead of degrading a contained driver
        // (notably Codex) to an unrestricted tail.
        let containment_root = match incoming.transcript_containment_root() {
            Ok(root) => Some(root),
            Err(error) => {
                tracing::warn!(
                    execution_id = run_id,
                    transcript_path = %path,
                    %error,
                    "refusing run cost capture because transcript containment could not be resolved",
                );
                None
            }
        };

        // A provider can fire its turn boundary just before flushing the
        // final assistant usage and turn-duration records. Poll with the same
        // bounded linear-backoff shape used by triage transcript readback.
        // This remains independent of successful finalization and runs before
        // slot lookup, so orphaned and late-hook executions retain the final
        // observed cost too. Missing synthetic/test paths take one poll only.
        if let Some(containment_root) = containment_root {
            let settle_attempts = if incoming.is_turn_boundary() && tokio::fs::try_exists(path).await.unwrap_or(false) {
                crate::completion::TRIAGE_TRANSCRIPT_READ_ATTEMPTS
            } else {
                1
            };
            for attempt in 1..=settle_attempts {
                if attempt > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        crate::completion::TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * u64::from(attempt - 1),
                    ))
                    .await;
                }
                if let Err(error) = server_state
                    .run_cost_capture
                    .capture_and_persist(
                        &server_state.work_db,
                        run_id,
                        std::path::Path::new(path),
                        containment_root,
                    )
                    .await
                {
                    tracing::warn!(
                        execution_id = run_id,
                        transcript_path = %path,
                        %error,
                        "failed to capture cumulative run cost from transcript",
                    );
                    break;
                }
            }
        }
    }
    let slot_id = match server_state.worker_registry.slot_for_run(run_id) {
        Some(slot_id) => slot_id,
        None => {
            // No slot mapping. A *remote* worker never gets a libghostty
            // pane (it holds no local slot), so the spawn flow never
            // called `register_run_slot` for it — yet its hooks tunnel
            // back here over the forwarded events socket. Lazily assign a
            // virtual slot so the live-status surface tracks the remote
            // worker's activity (Spawning/Working/Idle/…) just like a
            // local one. This is also how a worker reattached after an
            // engine restart re-acquires its live-status slot: the first
            // hook over the re-established forward lands here. Local runs
            // with no slot are genuinely gone or racing ahead of
            // registration (the historical drop case) and fall through.
            match register_remote_worker_slot(server_state, run_id).await {
                Some(slot_id) => slot_id,
                None => {
                    // No slot, and not a live remote worker. Before we
                    // drop the event, check whether it belongs to an
                    // execution the engine already believes is terminal —
                    // the contradiction where a run we think is dead is
                    // demonstrably alive (its worker is still emitting
                    // hooks). If so, make it LOUD and countable instead
                    // of swallowing it silently.
                    if !converge_terminal_execution_contradiction(server_state, run_id, event_kind).await {
                        tracing::warn!(
                            run_id,
                            kind = event_kind,
                            "live_status: dropping hook fan-out — run_id is not registered against a slot (event ahead of register_run_slot or after take_slot_for_run, or a non-remote run); transcript_path already persisted",
                        );
                    }
                    return;
                }
            }
        }
    };
    // Remote workers get a virtual slot but no per-slot live-status
    // summarizer task (the AI-summary loop tails a *local* transcript
    // file, which a remote run does not have — wiring it to the
    // over-SSH pull is a documented follow-up). The activity surface
    // (`apply_event` + broadcast below) is what drives the live dot and
    // works for remote runs; only the summarizer-trigger `notify` calls
    // are gated off so they don't emit a misleading "notify dropped — no
    // per-slot task" warn on every remote hook.
    let is_remote_slot = slot_id >= crate::worker_registry::REMOTE_SLOT_BASE;
    // Driver-start verification, second attempt — deliberately AFTER slot
    // resolution as well as before it.
    //
    // The stamp at the top of this function is keyed by run id and only
    // lands if the registry already knows the run. For a REMOTE worker it
    // does not: `register_remote_worker_slot` above creates the live-state
    // entry lazily, driven by this very hook. So the first remote hook
    // would stamp nothing, and a remote run that then went quiet would be
    // judged as never having started a driver. Repeating the call here —
    // idempotent and first-write-wins — makes the record land for local and
    // remote runs alike, whichever side of registration the hook fell on.
    server_state
        .live_worker_states
        .record_driver_signal(run_id, DriverSignalKind::HookEvent);
    let prior_activity = server_state.live_worker_states.get(slot_id).map(|s| s.activity);
    let changed = server_state.live_worker_states.apply_event(slot_id, &incoming.event);
    if changed {
        server_state.broadcast_live_worker_states().await;
    }
    // Fan out the matching trigger to the per-slot live-status loop.
    // The manager drops the trigger if no slot task is running, so a
    // hook arriving before `register_spawn` or after `release_slot`
    // is a benign no-op.
    let new_activity = server_state.live_worker_states.get(slot_id).map(|s| s.activity);
    // The end-of-turn trigger comes from the run's driver
    // (`Capability::TurnBoundary`), resolved once at ingress — not from this
    // dispatcher recognising a Claude-shaped `Stop`. Checked ahead of the
    // match because the two are disjoint: no event is both a turn boundary
    // and a tool result.
    if incoming.is_turn_boundary() && !is_remote_slot {
        server_state.live_status_manager.notify(slot_id, Trigger::Stop);
    }
    if let crate::protocol::WorkerEvent::PostToolUse {
        tool_name,
        tool_input,
        tool_response,
        ..
    } = &incoming.event
    {
        if !is_remote_slot {
            server_state.live_status_manager.notify(slot_id, Trigger::PostToolUse);
        }
        // Primary-path PR URL capture. Every worker that opens a
        // PR does it via a shell `gh pr create` / `cube pr create`
        // (and also `gh pr view` / `gh pr edit`); the PR URL is
        // printed on the command's output. The *driver* supplies
        // the free-text slice (and command string) via
        // `AgentDriver::pr_url_capture_feed` — Claude from the
        // PostToolUse `tool_response.{stdout,stderr}` object,
        // Codex from the correlated rollout
        // `response_item.payload.output` value. The engine then runs the *shared*
        // regex + command gates and stages against the
        // execution_id so the on-Stop handler picks it up without
        // shelling out to `jj log` or polling GitHub for the
        // branch's PR.
        //
        // Layer-1 gate: only capture URLs from deliberate `gh pr`
        // / `cube pr` invocations. Arbitrary shell output (file
        // reads, test runs, chore descriptions) can contain PR
        // URLs from unrelated executions; filtering by command
        // prevents those from staging the wrong PR.
        if let Some(feed) =
            pr_url_capture_feed_for_execution(server_state, run_id, tool_name, tool_input, tool_response)
        {
            // Check for any PR URL first so we can log a rejection
            // when the command isn't a gh/cube pr invocation.
            if let Some(pr_url) = crate::pr_url_capture::extract_pr_url_from_text(&feed.output_text) {
                if !crate::pr_url_capture::is_gh_pr_command_str(&feed.command) {
                    tracing::info!(
                        execution_id = run_id,
                        rejected_url = %pr_url,
                        reason = "not_a_gh_pr_command",
                        "pr_url_capture_rejected: URL in Bash stdout rejected — command is not a gh pr invocation",
                    );
                } else {
                    // Gate the URL against the product's repo before
                    // staging. Workers running tests can emit fixture
                    // URLs (e.g. `https://github.com/foo/bar/pull/42`)
                    // in tool output; without this gate those bind
                    // to the work_item as if they were real PRs.
                    let execution_id = run_id;
                    let repo_url_result = server_state
                        .work_db
                        .get_execution(execution_id)
                        .map(|e| e.repo_remote_url);
                    let valid = match repo_url_result {
                        Ok(ref repo_url) => match crate::pr_url_capture::validate_pr_url(&pr_url, repo_url) {
                            Ok(()) => true,
                            Err(reason) => {
                                tracing::info!(
                                    execution_id,
                                    rejected_url = %pr_url,
                                    %reason,
                                    "pr_url_capture: dropping URL — failed product-repo gate",
                                );
                                false
                            }
                        },
                        Err(err) => {
                            tracing::warn!(
                                execution_id,
                                rejected_url = %pr_url,
                                ?err,
                                "pr_url_capture: could not load execution to validate URL; dropping for safety",
                            );
                            false
                        }
                    };
                    if valid {
                        let outcome = server_state.staged_pr_urls.record_if_unset(run_id, &pr_url);
                        match outcome {
                            crate::pr_url_capture::StagePrUrlOutcome::Staged => {
                                tracing::info!(
                                    execution_id = run_id,
                                    pr_url = %pr_url,
                                    "pr_url_capture: staged PR URL from worker progress stream",
                                );
                            }
                            crate::pr_url_capture::StagePrUrlOutcome::AlreadyStaged => {
                                // Worker emitted another PR URL after
                                // already staging one — typically a
                                // `gh pr view` follow-up referencing a
                                // different PR. First-writer-wins so
                                // the original (the worker's own
                                // `gh pr create`) is kept.
                                tracing::debug!(
                                    execution_id = run_id,
                                    pr_url = %pr_url,
                                    "pr_url_capture: ignoring later URL (already staged for this execution)",
                                );
                            }
                        }
                    }
                } // else (is_gh_pr_command)

                // Revision push detection: record when a revision worker
                // runs `cube pr update` (or, defensively, a direct
                // `jj git push`) so the on_stop_inner SHA-delta gate can
                // confirm the revision was the one that moved the PR
                // head (not a concurrently-active parent worker).
                // Nested under the URL-found branch intentionally —
                // preserves the historical Claude control flow (a
                // successful `cube pr update` always prints a PR URL).
                if crate::pr_url_capture::is_revision_push_command_str(&feed.command) {
                    let execution_id = run_id;
                    match server_state.work_db.get_execution(execution_id) {
                        Ok(execution) if execution.kind == crate::work::ExecutionKind::RevisionImplementation => {
                            server_state.staged_revision_pushes.record(execution_id);
                            tracing::info!(execution_id, "revision_push_capture: staged push evidence for revision",);
                        }
                        _ => {}
                    }
                }
            }
        }

        // proposal_channel_error detection: a `boss propose <kind>`
        // Bash invocation that failed. Staged in-memory against the
        // execution id; `on_stop` files an attention and increments
        // `worker_proposals.channel_error`. See
        // `crate::proposal_channel_error`.
        // Still Claude-shaped (`tool_input.command` object): proposal
        // channel capture is out of scope for this seam.
        if tool_name == "Bash"
            && crate::proposal_channel_error::is_boss_propose_submit_command(tool_input)
            && let Some(error_text) = crate::proposal_channel_error::extract_channel_error(tool_response)
        {
            let command = tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("boss propose")
                .to_owned();
            if server_state
                .staged_proposal_channel_errors
                .record_if_unset(run_id, &command, &error_text)
            {
                tracing::warn!(
                    execution_id = run_id,
                    command = %command,
                    error_text = %error_text,
                    "proposal_channel_error: staged a failed `boss propose` submission",
                );
            }
        }
    }
    if !is_remote_slot {
        if let (Some(prior), Some(new)) = (prior_activity, new_activity) {
            if prior != new {
                server_state
                    .live_status_manager
                    .notify(slot_id, Trigger::ActivityChanged(new));
            }
        } else if let Some(new) = new_activity {
            // First event lands on a freshly spawned slot — the trigger
            // gives the loop the activity it should base its initial
            // policy on (in particular, Working → starts the timer
            // floor).
            server_state
                .live_status_manager
                .notify(slot_id, Trigger::ActivityChanged(new));
        }
    }
}

/// Assign a virtual live-status slot to a slotless run when it is a
/// live **remote** worker, returning the slot to fan the hook out to.
///
/// `run_id` is the worker's `BOSS_RUN_ID`, which is the execution id.
/// A remote worker holds no libghostty pane, so the local spawn flow
/// never registered a slot for it — but the live-status surface is
/// slot-keyed, so we allocate a synthetic slot from the reserved remote
/// range (see [`crate::worker_registry::REMOTE_SLOT_BASE`]) and seed the
/// initial `LiveWorkerState` the first time we see the run. Returns
/// `None` (so the caller drops the event) when the run is not a live
/// remote worker: a local run, a run with no recorded host, a run on a
/// settled execution (late/duplicate hook for a finished worker), or
/// when the remote slot range is exhausted.
async fn register_remote_worker_slot(server_state: &Arc<ServerState>, run_id: &str) -> Option<u8> {
    let host = server_state
        .work_db
        .latest_run_host_for_execution(run_id)
        .ok()
        .flatten()?;
    if host == "local" {
        return None;
    }
    // Don't resurrect a finished run from a late or duplicate hook.
    let execution = server_state.work_db.get_execution(run_id).ok()?;
    if execution.status.is_terminal() {
        return None;
    }
    let (slot_id, freshly_allocated) = server_state.worker_registry.get_or_allocate_remote_slot(run_id)?;
    if freshly_allocated {
        // Resolve the work item once for both the binding (name) and
        // the model label. `model_override` is the user's explicit
        // choice when set; otherwise fall back to a generic label (the
        // effort-resolved model lives in the spawn-time config, which is
        // not persisted, so it is not recoverable here).
        let work_item = server_state.work_db.get_work_item(&execution.work_item_id).ok();
        let binding = work_item.as_ref().map(|item| boss_protocol::WorkItemBinding {
            work_item_id: execution.work_item_id.clone(),
            work_item_name: crate::runner::work_item_name(item).to_owned(),
            execution_id: run_id.to_owned(),
        });
        let model = work_item
            .as_ref()
            .and_then(remote_worker_model_override)
            .unwrap_or_else(|| "claude".to_owned());
        // Attributed pool + execution kind, same stamps the local spawn
        // path writes via `StartWorkerInput` — remote workers never go
        // through `start_worker`, so this registration is the only
        // chance to surface them on `LiveWorkerState`.
        let has_source_automation = matches!(
            server_state
                .work_db
                .source_automation_id_for_work_item(&execution.work_item_id),
            Ok(Some(_))
        );
        let pool = crate::live_worker_state::attributed_pool_label(execution.kind.clone(), has_source_automation);
        // shell_pid is a local-process concept; a remote worker has no
        // local pid, so 0 (the live state stores it but the value is
        // only meaningful for the local ancestor-walk correlation that
        // remote runs bypass via the `_boss_run_id` token).
        // Remote workers are Claude-only today (the `model` fallback above
        // is the literal label `"claude"` — see the driver-abstraction
        // design doc's "Remote/SSH driver-awareness" future task), so this
        // mirrors the local spawn path's derivation, passing the capability
        // straight into registration rather than a follow-up setter call.
        server_state.live_worker_states.register_spawn_with_capabilities(
            slot_id,
            run_id,
            model,
            0,
            binding,
            ClaudeDriver.capabilities().provides(Capability::AwaitingInputSignal),
            crate::live_worker_state::LiveSpawnRouting::new(pool, execution.kind.as_str()),
        );
        tracing::info!(
            run_id,
            slot_id,
            host = %host,
            "live_status: assigned virtual slot to remote worker (no local pane); activity tracks the forwarded hook stream",
        );
        server_state.broadcast_live_worker_states().await;
    }
    Some(slot_id)
}

/// Resolve a hook event that arrived for a run the engine has already
/// terminalized.
///
/// A terminal execution that is *still emitting worker hook events* is a
/// contradiction: the engine believed the run dead — because an ack-timeout
/// was mis-handled as a spawn failure, or a sweep reaped it — yet its worker
/// is demonstrably alive. A hook is the strongest liveness evidence the engine
/// ever gets: it is produced by the worker's own process, in-band, and cannot
/// be forged by stale bookkeeping the way a pool claim or a registry entry can.
///
/// Convergence happens here rather than being left to the sweeps. Every
/// sweep's only verb is reap, and each correctly refuses to reap a live
/// worker, so the case where the engine — not the worker — is wrong has no
/// other resolution: left to them the contradiction is re-detected on every
/// hook, filed as a diagnostic, and stands indefinitely while the row goes on
/// being re-dispatched. See [`crate::worker_readoption`] for the full
/// argument; this is the call site that acts on it.
///
/// Returns `true` when the event belonged to a terminal execution (so the
/// caller skips the ordinary "dropping hook" warning — this path is the more
/// specific signal). Returns `false` for a healthy not-yet-terminal run whose
/// hook merely raced ahead of `register_run_slot`, or for a run id with no
/// matching execution row (a non-worker token); those fall through to the
/// ordinary drop path.
pub(super) async fn converge_terminal_execution_contradiction(
    server_state: &Arc<ServerState>,
    run_id: &str,
    event_kind: &str,
) -> bool {
    let execution = match server_state.work_db.get_execution(run_id) {
        Ok(execution) => execution,
        Err(_) => return false,
    };
    if !execution.status.is_terminal() {
        return false;
    }
    server_state.dispatcher_stats.inc_hook_events_for_terminal_execution();
    tracing::warn!(
        run_id,
        kind = event_kind,
        status = %execution.status,
        work_item_id = %execution.work_item_id,
        "[engine-reconcile] live hook event arrived for a TERMINAL execution — the engine believes \
         this run is dead but its worker is still emitting hooks. This is the ack-timeout / \
         stale-reap contradiction (a run that should have stayed tracked was terminalized). \
         Converging: the run will be re-adopted or reaped.",
    );

    let outcome = server_state
        .converge_terminal_execution(&execution, "hook_after_terminal")
        .await;
    tracing::info!(
        run_id,
        work_item_id = %execution.work_item_id,
        verdict = outcome,
        "[engine-reconcile] terminal-execution contradiction resolved",
    );
    true
}

/// The work item's explicit model override, if it carries one (only
/// tasks/chores do). Used to label a remote worker's live state.
fn remote_worker_model_override(item: &boss_protocol::WorkItem) -> Option<String> {
    use boss_protocol::WorkItem;
    match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.model_override.clone(),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// DB-free gate for [`dispatch_editorial_on_pretooluse`]: decides whether a
/// `PreToolUse` event should proceed to editorial evaluation, and if so
/// returns the `(command, execution_id)` pair to evaluate. Split out so the
/// `editorial_controls` flag gate can be pinned by a unit test without
/// standing up a full `ServerState`/DB.
fn editorial_pretooluse_candidate<'a>(
    flag_enabled: bool,
    event: &'a crate::protocol::WorkerEvent,
    run_id: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    use crate::protocol::WorkerEvent;

    if !flag_enabled {
        return None;
    }

    let WorkerEvent::PreToolUse {
        tool_name, tool_input, ..
    } = event
    else {
        return None;
    };
    if tool_name != "Bash" {
        return None;
    }
    let command = tool_input.get("command").and_then(|v| v.as_str())?;

    // Fast path: only evaluate commands that match the editorial hook's scope.
    if !boss_engine_gh_invocation::is_editorial_candidate(command) {
        return None;
    }

    let execution_id = run_id?;
    Some((command, execution_id))
}

/// On every `PreToolUse` event whose tool is `Bash` and whose command
/// matches `gh pr|issue {create,edit,comment,review}` (or `cube pr ensure`),
/// evaluate the command against the product's editorial rules and write the
/// decision to `editorial_actions`. Emits a `work_editorial_action` topic
/// event so subscribers (bossctl, kanban) can observe decisions live.
///
/// Gated on the `editorial_controls` feature flag: this is the single
/// choke-point call site [`crate::editorial_hook::evaluate_gh_pretooluse`]'s
/// docs describe, so when the flag is off this function returns immediately
/// and no `editorial_actions` row is written for the event.
///
/// Fails open on every error: a DB failure, a missing execution row, or an
/// unresolvable product are all logged and dropped. The editorial controls are
/// advisory-in-a-partition — never a hard block on the event loop.
pub(super) async fn dispatch_editorial_on_pretooluse(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use boss_editorial::CompiledRules;
    use std::path::Path;

    let Some((command, execution_id)) = editorial_pretooluse_candidate(
        server_state.feature_flags.is_enabled("editorial_controls"),
        &incoming.event,
        incoming.run_id.as_deref(),
    ) else {
        return;
    };

    // Load the product_id and editorial_rules in one synchronous query.
    let (product_id, editorial_rules, workspace_path_opt) =
        match server_state.work_db.get_editorial_context(execution_id) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "editorial_pretooluse: could not load editorial context; skipping",
                );
                return;
            }
        };

    if product_id.is_empty() {
        tracing::debug!(execution_id, "editorial_pretooluse: execution has no product; skipping",);
        return;
    }

    // Compile the user-supplied rules (baked-ins always apply inside evaluate_gh_pretooluse).
    let compiled = match CompiledRules::compile(editorial_rules) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "editorial_pretooluse: could not compile editorial rules; skipping",
            );
            return;
        }
    };

    // Use the workspace path as cwd for --body-file resolution; fall back to
    // an empty path (evaluate_gh_pretooluse fails-open when the file is unreadable).
    let cwd_path: std::path::PathBuf = workspace_path_opt
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let outcome = crate::editorial_hook::evaluate_gh_pretooluse(
        command,
        &cwd_path,
        &compiled,
        None, // PR template support is a follow-up (chore #9)
        execution_id,
        &server_state.editorial_deny_tracker,
    );

    let action_str = outcome.action.as_str();
    let reason_str: Option<String> = if outcome.findings.is_empty() {
        None
    } else {
        Some(
            outcome
                .findings
                .iter()
                .map(|f| f.description.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    // Best-effort PR URL from the staged cache.
    let pr_url = server_state.staged_pr_urls.get(execution_id);

    // Write to DB.
    let insert_result = server_state.work_db.insert_editorial_action(
        &product_id,
        execution_id,
        pr_url.as_deref(),
        command,
        action_str,
        reason_str.as_deref(),
    );
    let row_id = match insert_result {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(
                execution_id,
                %product_id,
                ?err,
                "editorial_pretooluse: DB insert failed",
            );
            return;
        }
    };

    tracing::info!(
        execution_id,
        %product_id,
        action = action_str,
        row_id,
        "editorial_pretooluse: recorded action",
    );

    // Build the EditorialAction for the topic event.
    use crate::work::now_string;
    let editorial_action = boss_protocol::EditorialAction::builder()
        .id(row_id.to_string())
        .product_id(&product_id)
        .execution_id(execution_id)
        .maybe_pr_url(pr_url)
        .tool_command(command)
        .action(action_str)
        .reason(reason_str.unwrap_or_default())
        .created_at(now_string())
        .build();

    // Emit topic event so subscribers can observe decisions live.
    let revision = server_state
        .work_revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    let topic = editorial_actions_topic(&product_id);
    let event = FrontendEvent::TopicEvent {
        topic: topic.clone(),
        revision,
        origin_session_id: String::new(),
        origin_request_id: None,
        event: TopicEventPayload::WorkEditorialAction {
            action: editorial_action,
        },
    };
    server_state
        .topic_broker
        .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
        .await;
}

/// Resolve the driver's PR-URL capture feed for a completed tool observation.
///
/// Looks up the execution's driver slug and asks
/// [`crate::driver::AgentDriver::pr_url_capture_feed`]. When the slug is
/// unknown, unregistered, or the DB lookup fails, falls back to
/// [`crate::driver::default_pr_url_capture_feed`] so Claude's historical
/// object shape (and the Codex bare-string shape the default also
/// understands) still capture — never poll GitHub for the branch's PR as
/// a substitute.
///
/// Non-`Bash` tools never feed PR-URL capture under any current driver
/// (the default feed and every override map command execution onto the
/// Bash tool name). Return `None` before the DB/registry work so ordinary
/// Read/Edit/etc. PostToolUse events do not pay a SQLite round-trip.
fn pr_url_capture_feed_for_execution(
    server_state: &ServerState,
    execution_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_response: &serde_json::Value,
) -> Option<crate::driver::PrUrlCaptureFeed> {
    use crate::driver::{DriverRegistry, default_pr_url_capture_feed};

    // Hot-path filter: match `default_pr_url_capture_feed`'s Bash gate
    // before any execution-row lookup. Capture outcomes are unchanged.
    if tool_name != "Bash" {
        return None;
    }

    let registry = DriverRegistry::default();
    match server_state.work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => match registry.get(&slug) {
            Some(driver) => driver.pr_url_capture_feed(tool_name, tool_input, tool_response),
            None => default_pr_url_capture_feed(tool_name, tool_input, tool_response),
        },
        Ok(None) | Err(_) => default_pr_url_capture_feed(tool_name, tool_input, tool_response),
    }
}

/// Given a driver already resolved for an execution, decide whether the
/// degrade path applies and, if so, what its registered
/// [`crate::driver::PostHocInterceptionFn`] (or the implicit `Accept` when
/// none is registered) decided. Split out so this decision logic is
/// unit-testable against [`boss_engine_driver::test_support::StubDriver`]
/// without a DB or a `ServerState`.
///
/// Returns `None` when `driver`'s declared
/// [`crate::driver::AbsenceDisposition`] for
/// [`crate::driver::Capability::ToolUseInterception`] is not `Degrade` —
/// i.e. the driver either provides real-time interception itself (Claude
/// today) or has explicitly opted into `Refuse`/`Synthesize` for this
/// capability via [`crate::driver::CapabilitySet::with_absence_override`].
/// Either way this is not the degrade path and the caller must not log or
/// act.
fn post_hoc_interception_decision(
    driver: &dyn crate::driver::AgentDriver,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_response: &serde_json::Value,
) -> Option<crate::driver::PostHocInterceptionAction> {
    use crate::driver::{AbsenceDisposition, Capability, PostHocInterceptionAction};

    let caps = driver.capabilities();
    if caps.provides(Capability::ToolUseInterception) {
        return None;
    }
    if caps.absence_disposition(Capability::ToolUseInterception) != AbsenceDisposition::Degrade {
        return None;
    }
    Some(match driver.post_hoc_interception() {
        Some(f) => f(tool_name, tool_input, tool_response),
        None => PostHocInterceptionAction::Accept,
    })
}

/// On the `PostToolUse` boundary, apply the post-hoc fallback for any driver
/// that landed on [`crate::driver::AbsenceDisposition::Degrade`] for
/// [`crate::driver::Capability::ToolUseInterception`] — i.e. a driver with no
/// real-time PreToolUse hook surface, so [`dispatch_editorial_on_pretooluse`]
/// (which only ever fires on a `PreToolUse` event) and the Claude-only path
/// guard, revision-PR guard, and checkleft push guard never ran for this
/// tool call.
///
/// **This is not equivalent to pre-hoc interception.** The tool has already
/// executed by the time this fires — a driver-registered
/// [`crate::driver::PostHocInterceptionFn`] can only flag the artefact for
/// follow-up, never prevent the call. Every degrade-path call logs a visible
/// warning naming exactly what was skipped, whether or not the driver
/// registers a fn, so "this worker ran without editorial/path/revision-PR/
/// checkleft guards" is never silent (design: agent-driver absence-policy
/// model).
///
/// A driver that *does* declare `ToolUseInterception` (Claude today) is
/// untouched — this returns immediately for it, before any logging, so
/// Claude's behaviour and log volume are unchanged.
pub(super) async fn dispatch_post_hoc_interception_on_post_tool_use(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::driver::{DriverRegistry, PostHocInterceptionAction};
    use crate::protocol::WorkerEvent;

    let WorkerEvent::PostToolUse {
        tool_name,
        tool_input,
        tool_response,
        ..
    } = &incoming.event
    else {
        return;
    };
    let Some(execution_id) = incoming.run_id.as_deref() else {
        return;
    };

    let driver_slug = match server_state.work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => return,
        Err(err) => {
            tracing::debug!(
                execution_id,
                ?err,
                "post_hoc_interception: could not resolve execution's driver; skipping",
            );
            return;
        }
    };

    let registry = DriverRegistry::default();
    let Some(driver) = registry.get(&driver_slug) else {
        // An unregistered slug (e.g. a task/product configured with a
        // `driver` that has no `DriverRegistry` entry yet) is exactly the
        // hookless-driver situation this dispatch exists to make non-silent:
        // there is no driver instance to consult a PostHocInterceptionFn on,
        // so the outcome is the implicit Accept, but the loss of guards is
        // real and must be logged the same as the registered-but-hookless
        // case below.
        let message = "post_hoc_interception: driver slug is not registered in the DriverRegistry — this tool call \
             ran WITHOUT editorial enforcement, the path guard, the revision-PR guard, and the checkleft \
             push guard. No driver instance is available to run a post-hoc review, so the outcome is the \
             implicit Accept. See PostHocInterceptionFn / PostHocInterceptionAction in the agent-driver \
             crate.";
        if server_state.should_warn_post_hoc_interception_loss(execution_id) {
            tracing::warn!(execution_id, driver = %driver_slug, tool_name = %tool_name, "{message}");
        } else {
            tracing::debug!(execution_id, driver = %driver_slug, tool_name = %tool_name, "{message}");
        }
        return;
    };

    let Some(action) = post_hoc_interception_decision(driver.as_ref(), tool_name, tool_input, tool_response) else {
        // Driver declares real-time ToolUseInterception (Claude today):
        // the path/revision-PR/checkleft guards and the editorial audit
        // already ran for this exact call via the PreToolUse boundary — do
        // not double-process it here, and do not change its behaviour.
        return;
    };

    let loss_of_guards_message = "post_hoc_interception: driver has no real-time PreToolUse hook surface — this tool call ran \
         WITHOUT editorial enforcement, the path guard, the revision-PR guard, and the checkleft push \
         guard. Post-hoc review can only detect problems after the tool already ran; it cannot prevent \
         the call. See PostHocInterceptionFn / PostHocInterceptionAction in the agent-driver crate.";
    if server_state.should_warn_post_hoc_interception_loss(execution_id) {
        tracing::warn!(execution_id, driver = %driver_slug, tool_name = %tool_name, "{loss_of_guards_message}");
    } else {
        tracing::debug!(execution_id, driver = %driver_slug, tool_name = %tool_name, "{loss_of_guards_message}");
    }

    match action {
        PostHocInterceptionAction::Accept => {
            tracing::debug!(
                execution_id,
                driver = %driver_slug,
                tool_name = %tool_name,
                "post_hoc_interception: driver's post-hoc adapter accepted the tool output",
            );
        }
        PostHocInterceptionAction::RequestEdit { reason } => {
            tracing::warn!(
                execution_id,
                driver = %driver_slug,
                tool_name = %tool_name,
                reason = %reason,
                "post_hoc_interception: driver's post-hoc adapter flagged this tool output for \
                 revision — the underlying command has already run and cannot be undone; this is a \
                 detect-after-the-fact signal, not an enforced block",
            );
        }
    }
}

/// On the driver's turn boundary, pop a pending probe for the run (if
/// any) and `SendToPane` the text to the worker's slot. The injection
/// arrives at the pane just as the worker becomes idle, so the agent
/// treats it as the next user prompt. After a successful dispatch,
/// records an in-flight entry (with the transcript path and current
/// byte offset) so `dispatch_probe_reply_on_stop` can emit the
/// matching `FrontendEvent::ProbeReplied` when the next boundary lands.
///
/// **Posture guard (fail closed):** the shared
/// [`ServerState::pane_input_posture_for_run`]. After a production Stop
/// fan-out the live activity is normally Idle / WaitingForInput, so the
/// write proceeds. Missing live state, a pre-session/terminal slot, or a
/// mid-turn slot on a driver that does not read mid-turn stdin refuses the
/// write and leaves the probe queued — never fail-open into a non-consuming
/// foreground process.
///
/// **Every exit is logged and returned.** This function is the drain path a
/// probe accepted with a `next_turn_boundary` commitment depends on, and it
/// used to have two exits that wrote nothing at all: the bare `pop → None`,
/// and the no-slot branch when the queue was already empty. That silence is
/// what made a dropped probe undiagnosable — a trace showing the completion
/// handler running at a boundary and this function saying nothing was
/// consistent with "no probe was queued", "the slot vanished" and "the probe
/// was popped and lost", which are three very different bugs. The returned
/// [`ProbeDispatchOutcome`] names the branch taken so tests can assert on it
/// without scraping log output.
pub(super) async fn dispatch_probe_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) -> ProbeDispatchOutcome {
    if !incoming.is_turn_boundary() {
        return ProbeDispatchOutcome::NotADeliveryBoundary;
    }
    let Some(run_id) = incoming.run_id.as_deref() else {
        tracing::debug!("probe on Stop: hook event carried no run_id; nothing to dispatch against");
        return ProbeDispatchOutcome::NoRunId;
    };
    // Peek before doing anything else so the branches below can be honest
    // about *why* they exited. This does not weaken the fail-closed ordering:
    // the claim still happens only after the posture guard passes. It only
    // separates "there was nothing to deliver" from "there was something and
    // we could not deliver it", which the previous shape conflated into the
    // same silent `return`.
    let queued = server_state.pending_probe_count(run_id);
    if queued == 0 {
        tracing::debug!(run_id, "probe on Stop: no probe queued for this run");
        return ProbeDispatchOutcome::NothingQueued;
    }
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        // Leave queued probes alone — no slot means we cannot deliver
        // and must not drop them by claiming them. If the run is genuinely
        // gone, `release_worker_pane` settles them as `Abandoned`; it is not
        // this path's job to decide that a missing mapping is permanent.
        tracing::warn!(run_id, queued, "probe ready but no slot mapping; leaving probe queued",);
        return ProbeDispatchOutcome::NoSlotMapping;
    };
    // Fail closed before the claim: no injectable posture must not write
    // bytes to the pane. Captured once so the recorded delivery state below
    // can tell a parked write from a mid-turn buffered one.
    let posture = server_state.pane_input_posture_for_run(run_id, slot_id);
    if !posture.permits_write() {
        tracing::warn!(
            run_id,
            slot_id,
            queued,
            activity = server_state
                .pane_typed_input_activity(slot_id)
                .map(boss_protocol::WorkerActivity::as_str),
            "probe on Stop refused: no injectable posture for this worker/driver; leaving probe queued",
        );
        return ProbeDispatchOutcome::PostureRefused;
    }
    deliver_probe_via_pane_write(server_state, run_id, slot_id, posture, "probe injected into pane").await
}

/// Claim the next queued probe for `run_id` and write it into the pane with a
/// plain `SendToPane`, trusting a successful write.
///
/// The delivery mechanism for a pane the worker is *parked* at: the write
/// becomes its next prompt, so nothing further needs to be observed to call
/// it consumed. Shared by the `Stop` boundary ([`dispatch_probe_on_stop`])
/// and the write issued during the `ProbeRun` call ([`dispatch_probe_now`]),
/// which face the identical situation — a worker sitting at its prompt with
/// no boundary of its own coming.
///
/// `posture` may still come through as mid-turn on the `Stop` path (in
/// production the fan-out has set `Idle` by then, so it is a narrow race);
/// that records `Buffered` rather than claiming the worker consumed the text.
/// A mid-turn worker reached deliberately goes through
/// [`inject_probe_mid_turn`] instead, which verifies.
///
/// Like every dispatch path, each exit logs and is named by the returned
/// [`ProbeDispatchOutcome`].
async fn deliver_probe_via_pane_write(
    server_state: &Arc<ServerState>,
    run_id: &str,
    slot_id: u8,
    posture: PaneInputPosture,
    success_message: &'static str,
) -> ProbeDispatchOutcome {
    use crate::protocol::{EngineToAppRequest, SendToPaneInput};

    let queued = server_state.pending_probe_count(run_id);
    if queued == 0 {
        tracing::debug!(run_id, slot_id, "pane write: nothing queued for this run");
        return ProbeDispatchOutcome::NothingQueued;
    }
    // Capture the transcript path + current byte length *before* claiming the
    // probe, so the in-flight entry is complete the moment it exists and no
    // assistant content the worker flushed while we were in this code path is
    // mistaken for the reply.
    let (transcript_path, offset_bytes) = transcript_offset_for_run(server_state, run_id).await;
    let Some(probe) = server_state.try_reserve_probe_for_delivery(run_id, transcript_path, offset_bytes) else {
        // We peeked a non-empty queue moments ago, so either another dispatch
        // path claimed the probe or it claimed the run's single delivery slot
        // first. Whoever won owns the probe and records its outcome, so
        // nothing is lost here; it is logged because an unexplained gap
        // between the peek and the claim is exactly the shape of the bug this
        // function was missing instrumentation for.
        tracing::warn!(
            run_id,
            slot_id,
            queued_at_peek = queued,
            "pane write: could not claim a probe after peeking a non-empty queue; another dispatch \
             path holds the probe or the run's delivery slot",
        );
        return ProbeDispatchOutcome::RacedToEmpty;
    };
    let probe_id = probe.probe_id.clone();
    // Out of the queue means out of `Queued` — before the first `.await`, so
    // the lifecycle table can never claim a delivery is still pending for a
    // probe that is no longer anywhere a dispatcher will find it. `Injected`
    // is corrected to the real outcome below, and reset to `Queued` on the
    // requeue path.
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Injected);
    let request = EngineToAppRequest::SendToPane(SendToPaneInput {
        slot_id,
        text: probe.text.clone(),
    });
    match server_state.send_to_app(request, Duration::from_secs(5)).await {
        Ok(_) => {
            // The claim is conditional on somebody being home: `SendToPane`
            // returning Ok proves the app wrote bytes into the pty, not that a
            // process read them.
            let state = record_pane_write_outcome(
                server_state,
                run_id,
                slot_id,
                &probe_id,
                if posture.is_mid_turn() {
                    ProbeDeliveryState::Buffered
                } else {
                    ProbeDeliveryState::Consumed
                },
                success_message,
            );
            ProbeDispatchOutcome::Dispatched(state)
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                run_id,
                slot_id,
                probe_id = %probe_id,
                "probe injection failed; pushing text back onto queue",
            );
            // Back to the front of the queue with the same probe id — callers
            // waiting on the matching `ProbeReplied` must not see their id
            // silently reissued — and the run's delivery slot freed.
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Queued);
            server_state.release_probe_reservation(run_id, probe);
            ProbeDispatchOutcome::RequeuedAfterFailure
        }
    }
}

/// Record the outcome of a successful pane write, downgrading `intended` to
/// [`ProbeDeliveryState::Orphaned`] when the worker's own process has already
/// exited, and log the result. Returns the state actually recorded.
///
/// `SendToPane` returning `Ok` only means the app wrote bytes into the pty. A
/// pane whose foreground process is gone accepts those bytes with nobody to
/// read them, which is how `probe-1` came to be reported `consumed` after
/// being injected into a `codex` pane whose process had already exited. A
/// `consumed` status that can mean that is not evidence of anything, so the
/// engine checks liveness before making the claim.
fn record_pane_write_outcome(
    server_state: &ServerState,
    run_id: &str,
    slot_id: u8,
    probe_id: &str,
    intended: ProbeDeliveryState,
    success_message: &'static str,
) -> ProbeDeliveryState {
    if server_state.run_process_probes_dead(run_id) {
        tracing::warn!(
            run_id,
            slot_id,
            probe_id,
            would_have_recorded = intended.as_str(),
            "pane write succeeded but the worker's recorded process is gone (kill(pid,0)=ESRCH); \
             the bytes reached a pane nobody is reading — recording orphaned, not delivered",
        );
        server_state.set_probe_lifecycle_detail(
            probe_id,
            ProbeDeliveryState::Orphaned,
            Some(
                "the pane write succeeded but the worker's process had already exited, so nothing \
                 consumed the text; re-issue against a live worker"
                    .to_owned(),
            ),
        );
        return ProbeDeliveryState::Orphaned;
    }
    tracing::info!(
        run_id,
        slot_id,
        probe_id,
        state = intended.as_str(),
        "{success_message}",
    );
    server_state.set_probe_lifecycle(probe_id, intended);
    intended
}

/// How long a mid-turn probe's pane write waits for a `UserPromptSubmit`
/// hook to confirm the CLI actually enqueued it, before treating the
/// write as unverified and escalating to Stop-boundary delivery. This
/// is the exact injection point implicated in the probe-6 incident:
/// text written into the pane while the worker was mid-turn, which
/// the CLI's TUI never enqueued as a pending prompt.
const MID_TURN_PROBE_VERIFY_TIMEOUT: Duration = Duration::from_secs(6);

/// On the `PostToolUse` boundary, pop the front probe in the per-run queue
/// and attempt a verified pane write (prefixed with `[coordinator-nudge]`).
/// The tool call has already completed at this point, so no in-flight Bash
/// is cancelled.
///
/// **This is where a probe's transport is selected.** Transport follows the
/// worker's *posture*, not the caller's flag: a probe queued against a
/// mid-turn worker whose driver buffers pane input is delivered here, into
/// the agent's composer, exactly as the chore-update path
/// ([`ServerState::send_input_to_worker`]) already does. Only a posture that
/// forbids a mid-turn write falls through to [`dispatch_probe_on_stop`].
/// `urgent` no longer selects a transport — it is queue *priority* alone
/// (front of the per-run FIFO, see [`ServerState::queue_probe`]) — because
/// waiting for a `Stop` that a long autonomous turn never reaches is not a
/// delivery contract anyone wants by default.
///
/// **At most one probe is in flight per run.** `in_flight_probes` holds a
/// single slot per run, so a second delivery inside the same turn would
/// silently discard the first probe's pending `ProbeReplied`. Claiming a
/// probe and claiming that slot are one atomic step
/// ([`ServerState::try_reserve_probe_for_delivery`]); while it is held the
/// queue is left alone, and the next turn boundary takes the in-flight entry
/// in [`dispatch_probe_reply_on_stop`] so the queue drains from there. Probes
/// therefore arrive in queue order, one per reply cycle, never two at once.
///
/// A chore-update written by [`ServerState::send_input_to_worker`] shares the
/// composer with probes but not this queue, so the two are not serialized
/// against each other: the agent consumes whichever text reached the composer
/// first. That is the same ordering a human typing into the pane would get.
///
/// **Posture short-circuit (before pop):** the ordering in
/// [`dispatch_worker_event_fanout`] is load-bearing. `dispatch_live_worker_state`
/// runs first and applies `PostToolUse`, which sets the activity to `Working`
/// unconditionally — so at *this* boundary the activity is `Working` by
/// construction, and a parked-only guard would be false here on every driver,
/// refusing every probe. The guard must therefore reason about mid-turn
/// input, not about being parked.
///
/// The guard is [`ServerState::pane_input_posture_for_run`], which admits
/// mid-turn writes when — and only when — the run's driver declares
/// [`crate::driver::MidTurnPaneInput::Buffers`]. For Claude Code that is the
/// ordinary case, so a probe delivers at the next tool boundary. For a driver
/// whose foreground process is not known to read mid-turn stdin — the trait
/// default — the write is still refused, preserving the tty-leak protection;
/// the probe stays queued (no pop) for the next Stop boundary via
/// [`dispatch_probe_on_stop`], and that deferral is treated as normal — no
/// `ProbeDeliveryEscalated` per tool, since multi-tool turns would otherwise
/// spam once per `PostToolUse`.
///
/// **A folded turn does not change the accounting here, and this is the place
/// it would have.** A buffering driver may fold the delivered prompt into the
/// turn that is already running rather than starting a new one for it (Codex,
/// measured), so that prompt yields no boundary of its own. Nothing in this
/// path counts turns or waits for a per-prompt boundary: it defers on
/// [`ServerState::has_in_flight_probe`], and that slot is cleared
/// unconditionally by whatever boundary comes next
/// ([`dispatch_probe_reply_on_stop`]) — one that the folding driver still
/// emits, because the *running* turn ends. So "deferred by at most one reply
/// cycle" holds for a folding driver too, on one boundary rather than two.
///
/// When the guard passes, the write is not trusted just because
/// `SendToPane` returned Ok: confirmation still requires a matching
/// `UserPromptSubmit` hook or a transcript scan (probe-6). On a
/// transport/app-level failure the probe is pushed back to the front
/// so a later retry keeps the same id.
///
/// A mid-turn injection that has not produced a `UserPromptSubmit` inside the
/// verification window comes back as [`PaneInjectOutcome::Buffered`], which is
/// the *expected* result: the agent cannot submit the text until it finishes
/// the turn it is in. That records `ProbeDeliveryState::Buffered` and needs
/// no escalation — the worker's reply at the next turn boundary is the
/// end-to-end confirmation, and `dispatch_probe_reply_on_stop` already
/// captures it.
///
/// On a genuinely *unconfirmed* write (parked worker, nothing observed), the
/// corrected understanding of the probe-6 incident (2026-07-13) is that the
/// text likely still reached the worker through a channel this engine can't
/// yet observe — the defect was unverifiable delivery, not lost delivery. So
/// this does **not** re-queue the probe for redelivery at the next `Stop`
/// boundary: doing so would hand the worker the same instruction twice in
/// exactly the scenario the incident actually hit. Instead it records the
/// probe as `Unconfirmed` in the lifecycle table, still tracks it as in-flight
/// (so a reply that does arrive is still captured), and pushes
/// `ProbeDeliveryEscalated` so anyone watching the probe topic knows delivery
/// is unverified and can choose to re-issue it deliberately.
///
/// Like every dispatch path, each exit logs and is named by the returned
/// [`ProbeDispatchOutcome`], so a probe that never arrives can be traced to
/// the branch that declined to deliver it.
pub(super) async fn dispatch_probe_on_post_tool_use(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) -> ProbeDispatchOutcome {
    use crate::protocol::WorkerEvent;
    let WorkerEvent::PostToolUse { .. } = incoming.event else {
        return ProbeDispatchOutcome::NotADeliveryBoundary;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        tracing::debug!("probe on PostToolUse: hook event carried no run_id");
        return ProbeDispatchOutcome::NoRunId;
    };

    // Fast no-op when nothing is queued for this run.
    if !server_state.has_pending_probe(run_id) {
        return ProbeDispatchOutcome::NothingQueued;
    }
    let queued = server_state.pending_probe_count(run_id);

    // One probe in flight per run: the previous delivery still owes a
    // `ProbeReplied` at the next turn boundary, and the in-flight slot that
    // carries it is single-valued. Delivering now would overwrite it.
    if server_state.has_in_flight_probe(run_id) {
        tracing::debug!(
            run_id,
            queued,
            "probe deferred: a probe is already in flight for this run and its reply is still \
             outstanding; the queue drains after the next turn boundary",
        );
        return ProbeDispatchOutcome::AlreadyInFlight;
    }

    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        // Leave the probe queued — a later PostToolUse / Stop after slot
        // registration can still deliver it.
        tracing::debug!(run_id, queued, "probe ready but no slot mapping; leaving queued");
        return ProbeDispatchOutcome::NoSlotMapping;
    };

    // Short-circuit *before* pop when there is no injectable posture for
    // this (activity, driver) pair. The fan-out has already applied
    // PostToolUse → Working, so this is `MidTurnBuffered` for an
    // interactive-TUI driver and `Refused` for one that does not read
    // mid-turn stdin. A refusal defers silently to Stop rather than
    // escalating once per tool call.
    let posture = server_state.pane_input_posture_for_run(run_id, slot_id);
    if !posture.permits_write() {
        tracing::debug!(
            run_id,
            slot_id,
            activity = server_state
                .pane_typed_input_activity(slot_id)
                .map(boss_protocol::WorkerActivity::as_str),
            "probe deferred: no injectable posture for this worker/driver; remains queued for Stop",
        );
        return ProbeDispatchOutcome::PostureRefused;
    }

    // Guard passed — now claim a probe and the run's delivery slot.
    inject_probe_mid_turn(server_state, run_id, slot_id, posture).await
}

/// Claim the next queued probe for `run_id` and write it into a mid-turn
/// worker's composer, recording what became of it.
///
/// Shared by the two mid-turn delivery opportunities — the `PostToolUse`
/// boundary ([`dispatch_probe_on_post_tool_use`]) and the write issued while
/// the `ProbeRun` RPC is still in hand ([`dispatch_probe_now`]) — so both
/// record the same lifecycle states and the same `[coordinator-nudge]`
/// marking. `posture` must already have been resolved as write-permitting by
/// the caller; a probe whose write does not happen is put back on the front
/// of the queue with its id intact.
///
/// Like every dispatch path, each exit logs and is named by the returned
/// [`ProbeDispatchOutcome`].
async fn inject_probe_mid_turn(
    server_state: &Arc<ServerState>,
    run_id: &str,
    slot_id: u8,
    posture: PaneInputPosture,
) -> ProbeDispatchOutcome {
    let queued = server_state.pending_probe_count(run_id);
    if queued == 0 {
        tracing::debug!(run_id, slot_id, "mid-turn injection: nothing queued for this run");
        return ProbeDispatchOutcome::NothingQueued;
    }
    // Snapshot before claiming — see `deliver_probe_via_pane_write`.
    let (transcript_path, offset_bytes) = transcript_offset_for_run(server_state, run_id).await;
    let Some(probe) = server_state.try_reserve_probe_for_delivery(run_id, transcript_path.clone(), offset_bytes) else {
        tracing::warn!(
            run_id,
            slot_id,
            queued_at_peek = queued,
            "mid-turn injection: could not claim a probe after peeking a non-empty queue; another \
             dispatch path holds the probe or the run's delivery slot",
        );
        return ProbeDispatchOutcome::RacedToEmpty;
    };
    let marked_text = format!("[coordinator-nudge] {}", probe.text);
    let probe_id = probe.probe_id.clone();
    // Lifecycle `Injected` only after the posture guard has passed and the
    // probe has been claimed (we are about to write). A short-circuit before
    // the claim leaves the probe at `Queued`.
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Injected);
    match server_state
        .inject_pane_text_verified(
            PaneInjectRequest::builder()
                .run_id(run_id)
                .slot_id(slot_id)
                .text(marked_text)
                .maybe_transcript_path(transcript_path.as_deref())
                .offset_bytes(offset_bytes)
                .verify_timeout(MID_TURN_PROBE_VERIFY_TIMEOUT)
                .posture(posture)
                .build(),
        )
        .await
    {
        PaneInjectOutcome::Confirmed => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                "probe injected mid-turn (delivery confirmed)",
            );
            // No liveness downgrade here: `Confirmed` means the worker's own
            // `UserPromptSubmit` hook fired, or the text was found in its
            // transcript. That is direct evidence of consumption by a live
            // process — strictly stronger than a `kill(pid, 0)` probe.
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Consumed);
            ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Consumed)
        }
        PaneInjectOutcome::Buffered => {
            // The normal successful shape of a mid-turn delivery: the text is
            // in the agent's composer. No escalation — the reply at the next
            // boundary the driver emits is the confirmation, and
            // `dispatch_probe_reply_on_stop` reads it there. That is the
            // running turn's own boundary on a driver that folds the prompt
            // into it, and the following turn's on one that defers; this path
            // deliberately does not distinguish them, because waiting for a
            // boundary the folding driver never produces would strand the
            // probe in flight forever.
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                "probe injected and buffered by the mid-turn agent; \
                 it will be picked up as a prompt when the composer drains",
            );
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Buffered);
            ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Buffered)
        }
        PaneInjectOutcome::Unconfirmed => {
            tracing::warn!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                timeout = ?MID_TURN_PROBE_VERIFY_TIMEOUT,
                "probe write reached the pane but delivery could not be confirmed within the \
                 verification window; NOT auto-redelivering (the corrected probe-6 understanding is that \
                 the text likely still reached the worker) — recording Unconfirmed and leaving \
                 redelivery to whoever is watching the probe topic",
            );
            server_state.set_probe_lifecycle_detail(
                &probe_id,
                ProbeDeliveryState::Unconfirmed,
                Some(format!(
                    "pane write succeeded but no UserPromptSubmit hook or transcript match appeared within \
                     {MID_TURN_PROBE_VERIFY_TIMEOUT:?}; not auto-redelivered (redelivery risks a duplicate \
                     instruction) — re-issue deliberately if the worker shows no sign of it"
                )),
            );
            // The claim is deliberately *kept*: the text may well have been
            // consumed even though we couldn't confirm it, so a reply at the
            // next turn boundary should still be captured rather than
            // silently dropped.
            server_state
                .topic_broker
                .publish(
                    &probe_topic(run_id),
                    FrontendEventEnvelope::push(FrontendEvent::ProbeDeliveryEscalated {
                        run_id: run_id.to_owned(),
                        probe_id,
                        reason: "delivery unconfirmed after mid-turn pane injection (not re-delivered)".to_owned(),
                    }),
                )
                .await;
            ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Unconfirmed)
        }
        PaneInjectOutcome::NotAcceptingInput { activity } => {
            // Race: activity flipped after the guard. Leave the probe queued
            // for a later boundary; this is still expected mid-turn deferral,
            // not a delivery escalation.
            tracing::debug!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                activity = activity.map(boss_protocol::WorkerActivity::as_str),
                "probe refused after claim (activity race); re-queuing for a later boundary",
            );
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Queued);
            server_state.release_probe_reservation(run_id, probe);
            ProbeDispatchOutcome::RequeuedAfterFailure
        }
        PaneInjectOutcome::SendFailed(failure) => {
            tracing::warn!(
                ?failure,
                run_id,
                slot_id,
                probe_id = %probe_id,
                "mid-turn probe injection failed; pushing back onto queue",
            );
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Queued);
            server_state.release_probe_reservation(run_id, probe);
            ProbeDispatchOutcome::RequeuedAfterFailure
        }
    }
}

/// Deliver a queued probe to `run_id`'s worker pane during the `ProbeRun`
/// call itself, whenever the worker's pane posture permits a write.
///
/// This is the **first and preferred** delivery opportunity, and the one that
/// makes a probe steer rather than merely arrive:
///
/// * [`PaneInputPosture::Parked`] — the worker is between turns with no
///   further `Stop`/`PostToolUse` coming on its own, so waiting for a
///   boundary would wait forever. The text is written straight in and becomes
///   the worker's next prompt. "Parked" covers `Idle` (Stop with no pending
///   notification) and `WaitingForInput` (Stop while a notification was
///   pending, e.g. a permission prompt the human already dismissed). This is
///   the path the `[effort-escalation-ack]` protocol depends on.
/// * [`PaneInputPosture::MidTurnBuffered`] — the worker is mid-turn on a
///   driver that buffers pane input, so the write lands in the agent's
///   composer and it picks the text up as it works. This is the same
///   transport the chore-update notice already uses via
///   [`ServerState::send_input_to_worker`], and it is why a probe no longer
///   has to wait out a long autonomous turn — or even a single long tool
///   call, which a `PostToolUse`-only design would still stall behind.
/// * [`PaneInputPosture::Refused`] — no write may be issued (no live state,
///   pre-session/terminal, or mid-turn on a driver whose foreground process
///   does not read stdin). The probe stays queued and a later `PostToolUse`
///   or `Stop` retries it.
///
/// Deferring on an already-in-flight probe is the same one-reply-cycle rule
/// [`dispatch_probe_on_post_tool_use`] applies; see
/// [`ServerState::has_in_flight_probe`].
///
/// Like [`dispatch_probe_on_stop`], every exit logs and is named by the
/// returned [`ProbeDispatchOutcome`]. This path runs on a detached
/// `tokio::spawn` from the `ProbeRun` handler, so a caller that never sees a
/// probe arrive has no other way to find out what it decided.
pub(super) async fn dispatch_probe_now(server_state: &Arc<ServerState>, run_id: &str) -> ProbeDispatchOutcome {
    let queued = server_state.pending_probe_count(run_id);
    if queued == 0 {
        tracing::debug!(run_id, "probe-now: nothing queued for this run");
        return ProbeDispatchOutcome::NothingQueued;
    }
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        // Worker not yet mapped to a slot (spawning) — probe stays queued.
        tracing::debug!(
            run_id,
            queued,
            "probe-now: no slot mapping; probe waits for a later boundary"
        );
        return ProbeDispatchOutcome::NoSlotMapping;
    };
    if server_state.has_in_flight_probe(run_id) {
        tracing::debug!(
            run_id,
            queued,
            "probe-now: a probe is already in flight for this run; the queue drains after the \
             next turn boundary",
        );
        return ProbeDispatchOutcome::AlreadyInFlight;
    }
    let posture = server_state.pane_input_posture_for_run(run_id, slot_id);
    if !posture.permits_write() {
        tracing::debug!(
            run_id,
            slot_id,
            queued,
            activity = server_state
                .pane_typed_input_activity(slot_id)
                .map(boss_protocol::WorkerActivity::as_str),
            "probe-now: no injectable posture for this worker/driver; probe waits for a later boundary",
        );
        return ProbeDispatchOutcome::PostureRefused;
    }

    if posture.is_mid_turn() {
        // Mid-turn writes go through the verified injection so the recorded
        // state distinguishes "sitting in the composer" from "we have no
        // idea" — the same treatment they get at a tool boundary.
        return inject_probe_mid_turn(server_state, run_id, slot_id, posture).await;
    }
    // Parked (Idle/WaitingForInput) is a reliable arrival point just like
    // Stop, so a successful `SendToPane` here is treated as consumed —
    // provided the worker's process is actually still there to consume it.
    deliver_probe_via_pane_write(
        server_state,
        run_id,
        slot_id,
        posture,
        "probe injected into parked worker pane (immediate dispatch)",
    )
    .await
}

/// Look up the transcript path the run is currently writing to (via
/// `WorkRun`), and stat its current byte size so we can use that as
/// the lower bound for the next reply-extraction read. Returns
/// `(None, 0)` when the run has no transcript path recorded yet —
/// the in-flight bookkeeping still tracks the dispatched probe, but
/// `dispatch_probe_reply_on_stop` will skip emission with a warning
/// rather than fabricate empty reply text.
///
/// The `run_id` argument is the execution id (`exec_*`) carried on
/// the hook event — the same value
/// `LiveStatusManager`/`dispatch_live_worker_state` plumb everywhere
/// in this file. PR #384 flagged this code path as broken (its
/// "Out of scope" section called out that `work_db.get_run(run_id)`
/// was joining the wrong namespace). Fixed here alongside the
/// `TranscriptPathResolver` impl.
pub(super) async fn transcript_offset_for_run(server_state: &ServerState, run_id: &str) -> (Option<String>, u64) {
    let path = match server_state.work_db.transcript_path_for_execution(run_id) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(run_id, ?err, "transcript path lookup failed for probe dispatch",);
            None
        }
    };
    let Some(path_str) = path else {
        return (None, 0);
    };
    let offset = match tokio::fs::metadata(&path_str).await {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => {
            tracing::warn!(
                run_id,
                path = %path_str,
                ?err,
                "failed to stat transcript at probe dispatch; treating offset as 0",
            );
            0
        }
    };
    (Some(path_str), offset)
}

/// On the driver's turn boundary that follows a probe dispatch, take
/// the in-flight entry for `run_id`, read transcript bytes written since
/// dispatch, and emit `FrontendEvent::ProbeReplied` on the per-run
/// probe topic. Idempotent: a duplicate boundary with no in-flight
/// probe is a no-op, so observers never see the same `probe_id`
/// reported twice.
///
/// **This is the one path that waits for a turn boundary after a prompt was
/// delivered mid-turn**, so it is the path a folded turn can break. A driver
/// that folds a buffered prompt into the *running* turn — Codex's bare TUI,
/// measured — answers the probe inside that turn and emits exactly one
/// boundary for the two prompts. Taking the in-flight entry at the *first*
/// boundary after dispatch is therefore still right: for a folding driver
/// that boundary already carries the reply, and for a non-folding one it is
/// the boundary at which the buffered text becomes the next prompt. What must
/// not happen — and is why this is called out here — is any attempt to wait
/// for a *second* boundary "for the probe's own turn": a folding driver never
/// produces one, and the probe would sit in flight forever, blocking every
/// later probe for the run behind [`ServerState::has_in_flight_probe`].
///
/// The read itself goes through the run's driver
/// ([`crate::driver_transcript`]). The transcript at
/// `in_flight.transcript_path` is written in the agent's own dialect, and
/// this used to parse it with a hand-rolled Claude-shaped scan — which
/// returned nothing for every Codex rollout record, so a Codex probe could be
/// delivered, answered, and still never produce a `ProbeReplied`. Reaching a
/// folding driver's reply at the right boundary is worth nothing if the
/// reader cannot read the dialect it is written in.
pub(super) async fn dispatch_probe_reply_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    if !incoming.is_turn_boundary() {
        return;
    }
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let Some(in_flight) = server_state.take_in_flight_probe(run_id) else {
        return;
    };
    // Guard against extracting a reply for a probe id whose lifecycle
    // was never actually advanced past being queued (e.g. a stale
    // in-flight entry surviving some other bug) — only a probe that
    // was actually written into the pane (Injected/Consumed/Buffered/
    // Unconfirmed/Orphaned) can plausibly have produced a reply.
    //
    // `Orphaned` is included deliberately: that state means the recorded
    // shell pid probed dead at write time, and the engine treats a bare
    // `ESRCH` on that pid as a fragile identity elsewhere too (a wrapper
    // shell that exec'd or exited leaves the real agent alive). A reply
    // arriving is direct evidence the write did land, and it corrects the
    // record to `Replied` — which is strictly better than discarding a real
    // answer to defend a guess.
    match server_state.probe_lifecycle_state(&in_flight.probe_id) {
        Some(
            ProbeDeliveryState::Injected
            | ProbeDeliveryState::Consumed
            | ProbeDeliveryState::Buffered
            | ProbeDeliveryState::Unconfirmed
            | ProbeDeliveryState::Orphaned,
        ) => {}
        other => {
            tracing::warn!(
                run_id,
                probe_id = %in_flight.probe_id,
                ?other,
                "probe reply skipped: lifecycle state was not a dispatched state",
            );
            return;
        }
    }
    let Some(transcript_path) = in_flight.transcript_path.as_deref() else {
        tracing::warn!(
            run_id,
            probe_id = %in_flight.probe_id,
            "probe reply skipped: no transcript path was recorded at dispatch",
        );
        return;
    };
    let driver = crate::driver_transcript::driver_for_execution(&server_state.work_db, run_id);
    let text = match read_assistant_reply(driver.as_deref(), transcript_path, in_flight.offset_bytes).await {
        Ok(Some(text)) => text,
        Ok(None) => {
            tracing::warn!(
                run_id,
                probe_id = %in_flight.probe_id,
                transcript_path,
                "probe reply skipped: transcript had no assistant turn after dispatch offset",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                run_id,
                probe_id = %in_flight.probe_id,
                transcript_path,
                ?err,
                "probe reply skipped: transcript read failed",
            );
            return;
        }
    };
    server_state.set_probe_lifecycle(&in_flight.probe_id, ProbeDeliveryState::Replied);
    let envelope = FrontendEventEnvelope::push(FrontendEvent::ProbeReplied {
        run_id: run_id.to_owned(),
        probe_id: in_flight.probe_id.clone(),
        text,
    });
    server_state.topic_broker.publish(&probe_topic(run_id), envelope).await;
    tracing::info!(
        run_id,
        probe_id = %in_flight.probe_id,
        "probe reply emitted",
    );
}

/// Read transcript bytes from `offset_bytes` to the end of the file
/// at `transcript_path`, normalize each new JSONL line through `driver`,
/// and return the last assistant-turn text found. Returns `Ok(None)` when
/// no assistant turn appears in the new region (e.g. the worker
/// errored out before producing one).
///
/// `driver` is the run's own driver, `None` only when it could not be
/// resolved — in which case the entries are read as written, which is what
/// this path did for every driver before it became driver-aware.
async fn read_assistant_reply(
    driver: Option<&dyn crate::driver::AgentDriver>,
    transcript_path: &str,
    offset_bytes: u64,
) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut file = tokio::fs::File::open(transcript_path).await?;
    let metadata = file.metadata().await?;
    if metadata.len() <= offset_bytes {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset_bytes)).await?;
    let mut buf = Vec::with_capacity((metadata.len() - offset_bytes) as usize);
    file.read_to_end(&mut buf).await?;
    let chunk = match String::from_utf8(buf) {
        Ok(chunk) => chunk,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transcript bytes are not valid utf-8",
            ));
        }
    };
    Ok(extract_last_assistant_text(driver, &chunk))
}

/// Normalize JSONL `chunk` through `driver` and return the most recent
/// assistant turn's text, concatenating all `text` blocks inside that one
/// entry.
///
/// **The normalization is the load-bearing part.** The bytes on disk are in
/// the agent's own dialect: Claude writes `message`-enveloped JSONL, Codex
/// writes a `session_meta`/`event_msg`/`response_item` rollout. This function
/// used to scan for `type == "assistant"` directly, which no Codex record has
/// — so a probe delivered into a Codex worker, answered by it, and reaching
/// this reader at the right boundary still produced "transcript had no
/// assistant turn after dispatch offset" and no `ProbeReplied` at all. It is
/// the same defect [`crate::driver_transcript`] was written for on the
/// Stop-boundary marker-scan side; the probe-reply read was the site that
/// never got wired to it.
///
/// After [`crate::driver_transcript::normalized_transcript_values`] every
/// dialect arrives in the canonical entry shape, so the only shapes handled
/// here are that canonical one (`content` / `text` at top level, which is
/// what Codex's normalizer emits) and Claude's `message`-enveloped pair
/// (`message.content[*].text`, and `message.text` from older snapshots) —
/// mirroring `transcript_markdown`'s own `turn_content` precedence.
///
/// Entries are inspected one at a time rather than via
/// [`crate::driver_transcript::parse_transcript_with_driver`] because that
/// flattens each message into per-block events, and two adjacent
/// single-block assistant messages then look identical to one two-block
/// message — the difference between "the newest reply" and "the newest reply
/// glued onto the previous one".
///
/// Lines that aren't valid JSON are skipped rather than rejecting the whole
/// chunk: a read that lands mid-flush must still recover the entries that did
/// arrive intact.
pub(super) fn extract_last_assistant_text(
    driver: Option<&dyn crate::driver::AgentDriver>,
    chunk: &str,
) -> Option<String> {
    let mut latest: Option<String> = None;
    for value in crate::driver_transcript::normalized_transcript_values(driver, chunk) {
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        // Canonical normalized entries carry `content`/`text` at the top
        // level; Claude's carry them under `message`. Prefer the envelope so
        // a Claude entry is read exactly as it always was.
        let body = value.get("message").unwrap_or(&value);
        let mut buf = String::new();
        if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    buf.push_str(text);
                }
            }
        }
        if buf.is_empty()
            && let Some(text) = body.get("text").and_then(|t| t.as_str())
        {
            buf.push_str(text);
        }
        if !buf.is_empty() {
            latest = Some(buf);
        }
    }
    latest
}

/// On the driver's turn boundary, ask the completion handler whether
/// the worker has produced a PR for its workspace branch. If so, the
/// linked task/chore moves to `in_review`, the execution finalises,
/// and the cube workspace is released. If not, an `awaiting_input`
/// signal is published for the execution topic so the pane indicator
/// can reflect that the worker is idle without losing the active
/// kanban state.
///
/// This is the gate the whole completion subsystem hangs off — PR-URL
/// capture, the Doing→Review transition, nudge/probe routing,
/// effort-escalation parsing. It opens on
/// [`crate::events_socket::IncomingHookEvent::is_turn_boundary`], the
/// signal the run's driver produced through
/// [`crate::driver::AgentDriver::turn_boundary`], rather than on the
/// `WorkerEvent::Stop` variant that only exists because Claude Code fires
/// a `Stop` hook. For Claude the two coincide exactly, so behaviour is
/// unchanged; for a driver whose turn ends some other way, completion
/// now follows the driver instead of needing a Claude-shaped event to
/// impersonate.
///
/// Runs **before** `dispatch_probe_on_stop` in the event loop so that
/// probes the completion handler queues (e.g. `PROBE_NO_PR`) are
/// visible when probe dispatch fires on the same boundary — if
/// completion ran after, those probes would stall until the next
/// boundary (which never arrives for a worker that is already idle).
pub(super) async fn dispatch_completion_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    if !incoming.is_turn_boundary() {
        return;
    }
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let outcome = server_state
        .completion_handler
        .on_stop_with_turn_end(run_id, incoming.turn_boundary())
        .await;
    // Info-level so non-success outcomes (DetectorFailed, AwaitingInput,
    // StalePr, EmptyDiffPr) appear in the engine log without enabling
    // debug. The 2026-05-13 three-concurrent-workers regression had
    // zero log evidence because this was at debug — operators saw
    // `activity=idle` workers but no record of what `on_stop` returned.
    tracing::info!(run_id, ?outcome, "completion handler stop result");
}

#[cfg(test)]
mod editorial_gate_tests {
    use super::editorial_pretooluse_candidate;
    use crate::protocol::WorkerEvent;
    use serde_json::json;

    fn gh_pretooluse_event() -> WorkerEvent {
        WorkerEvent::PreToolUse {
            session_id: "sess-1".to_string(),
            tool_name: "Bash".to_string(),
            tool_input: json!({ "command": "gh pr create --title t --body b" }),
        }
    }

    #[test]
    fn flag_off_skips_even_a_matching_gh_command() {
        let event = gh_pretooluse_event();
        assert_eq!(editorial_pretooluse_candidate(false, &event, Some("exec_1")), None);
    }

    #[test]
    fn flag_on_matching_bash_gh_command_is_a_candidate() {
        let event = gh_pretooluse_event();
        assert_eq!(
            editorial_pretooluse_candidate(true, &event, Some("exec_1")),
            Some(("gh pr create --title t --body b", "exec_1"))
        );
    }

    #[test]
    fn flag_on_non_bash_tool_is_skipped() {
        let event = WorkerEvent::PreToolUse {
            session_id: "sess-1".to_string(),
            tool_name: "Read".to_string(),
            tool_input: json!({ "command": "gh pr create --title t --body b" }),
        };
        assert_eq!(editorial_pretooluse_candidate(true, &event, Some("exec_1")), None);
    }

    #[test]
    fn flag_on_non_editorial_command_is_skipped() {
        let event = WorkerEvent::PreToolUse {
            session_id: "sess-1".to_string(),
            tool_name: "Bash".to_string(),
            tool_input: json!({ "command": "ls -la" }),
        };
        assert_eq!(editorial_pretooluse_candidate(true, &event, Some("exec_1")), None);
    }

    #[test]
    fn flag_on_missing_run_id_is_skipped() {
        let event = gh_pretooluse_event();
        assert_eq!(editorial_pretooluse_candidate(true, &event, None), None);
    }

    #[test]
    fn flag_on_non_pretooluse_event_is_skipped() {
        let event = WorkerEvent::Stop {
            session_id: "sess-1".to_string(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        };
        assert_eq!(editorial_pretooluse_candidate(true, &event, Some("exec_1")), None);
    }
}

#[cfg(test)]
mod post_hoc_interception_decision_tests {
    use super::post_hoc_interception_decision;
    use crate::driver::test_support::StubDriver;
    use crate::driver::{
        AbsenceDisposition, Capability, CapabilitySet, DriverDescriptor, ModelMenu, PostHocInterceptionAction,
    };
    use serde_json::json;

    fn descriptor(name: &'static str) -> DriverDescriptor {
        DriverDescriptor {
            name,
            label: name,
            binary: name,
            config_dir: ".stub",
            agent_rules_filename: "AGENTS.md",
            initial_prompt_filename: "initial-prompt.txt",
            model_menu: ModelMenu {
                engine_default: "stub-model",
                effort_value_for_level: |_| None,
                default_model_for_level: |_| "stub-model",
                model_for_reasoning: |_| "stub-model",
                prompt_addendum_for_level: |_| None,
                model_requires_auto_permissions: |_| false,
                model_belongs_to_driver: |_| true,
            },
        }
    }

    fn always_request_edit(
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _tool_output: &serde_json::Value,
    ) -> PostHocInterceptionAction {
        PostHocInterceptionAction::RequestEdit {
            reason: "flagged by fixture".to_owned(),
        }
    }

    /// A driver that declares real-time ToolUseInterception (Claude today)
    /// never reaches the degrade path — the caller must not log or act.
    #[test]
    fn driver_with_tool_use_interception_is_not_applicable() {
        let driver = StubDriver::new(
            descriptor("claude-like"),
            CapabilitySet::new([Capability::ToolUseInterception]),
        );
        let outcome =
            post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
        assert_eq!(outcome, None);
    }

    /// A driver without ToolUseInterception and no registered post-hoc fn
    /// still hits the degrade path — the implicit decision is `Accept` (the
    /// caller logs the loss-of-guards warning regardless).
    #[test]
    fn degraded_driver_without_registered_fn_implicitly_accepts() {
        let driver = StubDriver::new(descriptor("hookless"), CapabilitySet::new([]));
        let outcome =
            post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
        assert_eq!(outcome, Some(PostHocInterceptionAction::Accept));
    }

    /// A degraded driver's registered fn is actually called, and its
    /// decision passed through verbatim.
    #[test]
    fn degraded_driver_with_registered_fn_returns_its_decision() {
        let driver = StubDriver::new(descriptor("hookless"), CapabilitySet::new([]))
            .with_post_hoc_interception(always_request_edit);
        let outcome = post_hoc_interception_decision(
            &driver,
            "Bash",
            &json!({"command": "rm -rf /"}),
            &json!({"output": "ok"}),
        );
        assert_eq!(
            outcome,
            Some(PostHocInterceptionAction::RequestEdit {
                reason: "flagged by fixture".to_owned(),
            }),
        );
    }

    /// A driver that does not provide ToolUseInterception but has
    /// explicitly overridden its absence disposition to `Refuse` (rather
    /// than the `Degrade` default) must not be routed through the degrade
    /// path — dispatch is expected to have refused this driver before any
    /// tool call could run, so treating it as "degraded" here would be
    /// logged as if it were silently accepting reduced fidelity when its
    /// declared policy is the opposite.
    #[test]
    fn driver_with_refuse_override_is_not_applicable() {
        let caps =
            CapabilitySet::new([]).with_absence_override(Capability::ToolUseInterception, AbsenceDisposition::Refuse);
        let driver = StubDriver::new(descriptor("refuses-without-interception"), caps);
        let outcome =
            post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
        assert_eq!(outcome, None);
    }
}
