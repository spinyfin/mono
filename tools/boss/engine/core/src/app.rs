use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex, Notify, oneshot};

use crate::audit_effort;
use crate::cli::Cli;
use crate::completion::{
    CommandPrDetector, PaneReleaseOutcome, PrDetector, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::config::RuntimeConfig;
use crate::coordinator::{CommandCubeClient, CubeClient, ExecutionCoordinator, ExecutionPublisher, WorkerPool};
use crate::events_socket::{bind_events_socket, handle_connection, peer_pid};
use crate::external_tracker::WorkDbOrgStateSink;
use crate::external_tracker::github_oauth::{
    DeviceFlow, GitHubAuthController, GitHubAuthState, KeychainTokenStore, probe_and_record_org_state,
};
use crate::ipc_log::IpcLogger;
use crate::live_status_loop::{LiveStatusBroadcaster, LiveStatusManager, TranscriptPathResolver, Trigger};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::merge_poller::{CommandMergeProbe, MergeProbe, PrReconcilerTargetedKick, spawn_loop as spawn_merge_poller};
use crate::merge_when_ready;
use crate::protocol::{
    EngineToAppError, EngineToAppRequest, EngineToAppResponse, FocusWorkerPaneInput, FrontendEvent,
    FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope, GitHubAuthStateDto, HostedPaneEntry,
    InterruptWorkerPaneInput, ListHostedPanesInput, OpenDocumentInput, OrgAuthState, ReleaseWorkerPaneInput,
    RequestExecutionInput, RevealWorkItemInput, SendToPaneInput, TOPIC_ENGINE_HEALTH, TOPIC_GITHUB_AUTH,
    TOPIC_WORK_PRODUCTS, TOPIC_WORKER_LIVE_STATES, TopicEventPayload, comment_topic, editorial_actions_topic,
    execution_topic, probe_topic, work_product_topic,
};
use crate::repo_slug;
use crate::work::{
    ANSWER_AGENT_RUN_STATUS_FAILED, ANSWER_AGENT_RUN_STATUS_REPLIED, ActionedAttentionGroup, DuplicateTaskError,
    ExecutionKind, ExecutionStatus, GhPrStateChecker, INTENT_QUESTION, ProducerConflictInsertInput, ReviseDocOutcome,
    SetRunTranscriptPathOutcome, THREAD_ENTRY_KIND_ANSWER, Task, TaskStatus, WorkComment, WorkDb, WorkItem,
};
use crate::worker_registry::WorkerRegistry;
use async_trait::async_trait;
use tokio::time::{Duration, timeout};

mod app_session;
mod attentions;
mod automations;
mod broadcasts;
mod ci_remediation;
mod comments;
mod conflict_resolution;
mod context;
mod dependencies;
mod effort;
mod engine_meta;
mod executions;
mod external_tracker;
mod github_auth;
// `pub(crate)` so the spawn-capability circuit breaker (`crate::spawn_health`)
// can persist its auto-pause through the same `METADATA_KEY_DISPATCH_PAUSED*`
// keys the human dispatch toggle uses. Individual items stay `pub(crate)`/
// `pub(super)`; only the module path is widened.
pub(crate) mod handler_helpers;
mod hosts;
mod live_status;
mod metrics;
mod pane_delivery;
mod pane_ops;
mod panes;
mod planner_ops;
mod probes;
mod products;
mod projects;
mod proposals;
mod review;
mod server;
mod sessions;
mod subscriptions;
#[cfg(test)]
mod tests;
mod trunk_auth;
mod trust;
mod work_items;
mod worker_events;

// Re-export public items from server module for external callers.
pub use server::{process_is_alive, run, serve, serve_with_merge_probe};

// Re-import server-internal helpers so child modules can access them via `use super::*`.
use server::{
    constant_time_eq, is_descendant_of_any, reap_worker_process_tree, register_app_session_trust_ok,
    resolve_status_actor, signal_shell_pids,
};

// Re-import pane-op error types so the `tests` child module can access them via `use super::*`.
// Only the test module references these by name; production code calls the
// `ServerState` methods and matches on the returned error with `{err}`/`{err:?}`,
// never the concrete type — so this import is dead outside `#[cfg(test)]`.
#[cfg(test)]
use pane_ops::{FocusPaneError, InterruptPaneError, OpenDocumentError, RetirePaneError, SendInputError};

// Re-import worker event dispatch functions so child modules can access them via `use super::*`.
use worker_events::{
    dispatch_completion_on_stop, dispatch_editorial_on_pretooluse, dispatch_live_worker_state, dispatch_probe_if_idle,
    dispatch_probe_on_stop, dispatch_probe_reply_on_stop, dispatch_urgent_probe_on_post_tool_use,
};

// Re-import verified pane-injection types so child modules can access them via `use super::*`.
use pane_delivery::{PaneInjectOutcome, PaneSendFailure};

// Re-import the split-out `ServerState` submodules' items so `app.rs` and every
// child module can reach them via `use super::*`. `RpcTier` and `SendToAppError`
// keep their public `boss_engine::app::` paths.
pub use app_session::SendToAppError;
use app_session::{APP_CHANNEL_UNHEALTHY_STREAK, AppChannelHealth, AppSessionHandle};
use probes::{InFlightProbe, PendingProbe, ProbeLifecycleState, ServerStateProbeQueuer};
use trust::PidFileGuard;
pub use trust::{PeerClass, RpcTier};

// The worker exposure boundary: the verb policy `trust::worker_tier_denial`
// consults, and the row sanitizer the writer task applies. Both are pure
// functions over the wire types — see the crate docs for why they live
// outside `boss-engine`.
use boss_engine_worker_policy::{sanitize_event_for_worker, variant_name, worker_verb_decision};
use boss_protocol::{WorkerTierDenial, WorkerTierDenialReason};

// Re-import handler helpers so all handler submodules can access them via `use super::*`.
use handler_helpers::{
    METADATA_KEY_AUTOMATION_PAUSED, METADATA_KEY_AUTOMATION_PAUSED_SINCE, METADATA_KEY_DISPATCH_PAUSE_ORIGIN,
    METADATA_KEY_DISPATCH_PAUSED, METADATA_KEY_DISPATCH_PAUSED_SINCE, TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX,
    TranscriptResolution, active_chore_run_id, active_to_todo_execution, build_chore_update_message,
    build_effort_audit_report, build_engine_health_report, build_live_status_debug_report, duplicate_or_work_error,
    handle_create_many, in_review_chore_execution, live_execution_for_task_id, load_automation_paused_state,
    load_dispatch_paused_state, load_live_status_disabled_slots, open_review_terminal_async,
    persist_live_status_disabled_slots, publish_comment_invalidation, publish_work_invalidation, read_transcript_tail,
    resolve_transcript_for_tail, segment_to_wire, send_push, send_response, send_response_with_revision,
    send_work_error, tail_lines_from_content, task_name_description_for_id, task_status_for_id,
    task_transitioned_to_active, terminal_chore_execution, transport_default_created_via,
    validate_external_tracker_config, work_item_id, work_item_needs_dispatch,
};

/// Per-request handler context: the connection-scoped state every
/// [`FrontendRequest`] handler needs. Built once per request in
/// [`handle_frontend_connection`] and consumed by the dispatched handler.
/// Bundling these into one struct keeps the dispatch match a thin
/// alphabetical table of `Variant => module::handler(ctx, r)` arms so
/// concurrent PRs adding new requests don't all collide at the tail.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Dispatch {
    server_state: Arc<ServerState>,
    work_db: Arc<WorkDb>,
    sink: Arc<SessionSink>,
    session_id: String,
    request_id: String,
    peer_pid: Option<libc::pid_t>,
    /// When this request's line was received off the socket, before decode.
    /// Seeds the population-timing `total` window and, with `decode_ms`, the
    /// `decode` segment. Cheap to carry on every request; only the
    /// `get_work_tree` handler reads it.
    recv_instant: Instant,
    /// Wall-clock ms spent deserializing this request's envelope.
    decode_ms: f64,
}

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";

/// Shared HTTP client for the GitHub OAuth device flow. Installs the rustls
/// ring crypto provider lazily (the first TLS handshake panics otherwise,
/// mirroring `live_status::http_client`) and applies a per-request timeout —
/// the device-flow poll loop manages its own cadence, so this only bounds an
/// individual round-trip, never the overall flow.
fn github_oauth_http_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest::Client build should not fail with default config")
}

#[async_trait]
impl LiveStatusBroadcaster for ServerState {
    async fn broadcast_live_worker_states(&self) {
        // Disambiguate against the trait method of the same name —
        // call the inherent publisher directly via UFCS so this
        // doesn't recurse.
        ServerState::broadcast_live_worker_states(self).await;
    }
}

#[async_trait]
impl TranscriptPathResolver for ServerState {
    async fn transcript_path(&self, run_id: &str) -> Option<std::path::PathBuf> {
        // The "run_id" the live-status manager hands us is actually the
        // execution id (`exec_*`) — `LiveWorkerState.run_id` is stamped
        // from `WorkItemBinding.execution_id` at spawn, and the rest of
        // the engine is consistent with that aliasing. The pre-fix
        // version of this resolver called `work_db.get_run(run_id)`
        // (which joins on `work_runs.id`, an `run_*` namespace), so the
        // lookup never matched and the per-slot summarizer never
        // resolved a transcript path. That blocked `tail` from ever
        // being instantiated, which in turn meant `snap.transcript_path`
        // was never populated in the debug store — visible to the user
        // as `bossctl live-status debug --json` reporting
        // `slots[*].transcript_path: null` for every live slot.
        //
        // PR #384 fixed the same cross-namespace bug on the write side
        // (`set_run_transcript_path_if_unset`). This is the read-side
        // pair. Keep both routed through helpers that explicitly take
        // an execution id so a future grep for `work_db.get_run` in this
        // file can stay a strong "this is the wrong namespace" signal.
        match self.work_db.transcript_path_for_execution(run_id) {
            Ok(Some(path)) => Some(std::path::PathBuf::from(path)),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(run_id, ?err, "live_status: transcript path lookup failed");
                None
            }
        }
    }
}

#[async_trait]
impl crate::spawn_flow::WorkerSpawner for ServerState {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        // Serialize SpawnWorkerPane round-trips. Concurrent bursts of
        // surface_new on the macOS side crashed the app
        // (slot 4 spawned, then 3 follow-ups timed out into a dead
        // process). The app reasonably allocates panes one at a time,
        // and there's no benefit to dispatching parallel spawns —
        // gating the engine side keeps libghostty from being asked to
        // stand up multiple surfaces inside a single runloop tick.
        // ReleaseWorkerPane / SendToPane don't share this hazard, so
        // they go through unsynchronized.
        if matches!(request, EngineToAppRequest::SpawnWorkerPane(_)) {
            let _guard = self.spawn_pane_lock.lock().await;
            return self.send_to_app(request, timeout).await;
        }
        self.send_to_app(request, timeout).await
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.worker_registry
    }

    async fn reap_worker_pane(&self, run_id: &str) {
        // Delegate to the inherent reaper, discarding the outcome — the
        // spawn-completion path already knows the worker came up (it just
        // returned a pid); it calls this purely to kill it.
        let _ = ServerState::release_worker_pane(self, run_id).await;
    }

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_worker_states)
    }

    async fn publish_live_worker_states(&self) {
        self.broadcast_live_worker_states().await;
    }

    fn start_live_status_slot(&self, slot_id: u8, run_id: &str) {
        let Some(arc_self) = self._self_weak.upgrade() else {
            tracing::debug!(slot_id, "start_live_status_slot: ServerState already dropped",);
            return;
        };
        // Snapshot the API key once at slot start — picking it up
        // lazily inside the task would require sharing the config or
        // a closure, and the key doesn't change for the worker's
        // lifetime anyway.
        let api_key = arc_self.anthropic_api_key.clone();
        let broadcaster: Arc<dyn LiveStatusBroadcaster> = arc_self.clone();
        let resolver: Arc<dyn TranscriptPathResolver> = arc_self.clone();
        self.live_status_manager.start_slot(
            slot_id,
            run_id.to_owned(),
            api_key,
            self.live_worker_states.clone(),
            broadcaster,
            resolver,
        );
    }

    fn draft_pr_mode(&self) -> bool {
        self.settings.is_enabled("default_pr_draft_mode")
    }

    fn non_opus_auto_mode(&self) -> bool {
        self.settings.is_enabled("workers.non_opus_permission_mode")
    }
}

#[async_trait]
impl crate::stale_worker_sweep::StaleWorkerReaper for ServerState {
    /// Route the stale-worker reconcile through the exact teardown
    /// `bossctl agents stop` performs: `release_worker_pane` tears down
    /// the libghostty pane, fires the `reap_worker_process_tree`
    /// SIGTERM/SIGKILL ladder at the worker's process group, releases the
    /// pool slot, and drops the live-state entry. This is what was
    /// missing — the sweep used to free the pool slot without ever
    /// killing the `claude` process, so a redispatch could re-lease the
    /// still-occupied workspace.
    async fn reap_worker(&self, execution_id: &str) {
        let _ = ServerState::release_worker_pane(self, execution_id).await;
    }
}

#[async_trait]
impl crate::spawn_ack_sweep::SpawnAckReaper for ServerState {
    /// Route the spawn-ack-timeout reconcile through the same
    /// `release_worker_pane` teardown as the stale-worker sweep: tears
    /// down whatever (possibly ghost) pane the app is holding for the
    /// slot, signals the recorded shell pid's process group as a
    /// backstop (a no-op when `shell_pid == 0`, which is always true for
    /// this sweep's candidates), releases the pool slot, and drops the
    /// live-state entry.
    async fn reap_worker(&self, execution_id: &str) {
        let _ = ServerState::release_worker_pane(self, execution_id).await;
    }
}

/// `WorkerPaneReleaser` implementation backed by a `Weak<ServerState>`.
/// Late-bound via `set_server_state` to break the ownership cycle:
/// ServerState owns the completion handler, which owns the releaser,
/// which calls back into ServerState.
#[derive(Default)]
struct ServerStatePaneReleaser {
    server: std::sync::OnceLock<Weak<ServerState>>,
}

impl ServerStatePaneReleaser {
    fn set_server_state(&self, weak: Weak<ServerState>) {
        let _ = self.server.set(weak);
    }
}

#[async_trait]
impl WorkerPaneReleaser for ServerStatePaneReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "pane releaser called before server state was bound");
            // No server bound: nothing could be reaped. Treat as
            // "no live worker" so the caller does not free a lease on
            // the strength of a release that never happened.
            return PaneReleaseOutcome::NoLiveWorker;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "pane releaser: server state already dropped");
            return PaneReleaseOutcome::NoLiveWorker;
        };
        server.release_worker_pane(run_id).await
    }
}

/// One outstanding waiter for a `UserPromptSubmit` hook that would
/// confirm a specific pane-injection attempt. See the
/// `ServerState::delivery_waiters` field docs for why these are kept
/// in a per-run `Vec` rather than a single slot.
struct DeliveryWaiter {
    token: u64,
    /// The (normalized) text this waiter is trying to match against
    /// an arriving `UserPromptSubmit` prompt.
    match_text: String,
    tx: oneshot::Sender<String>,
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct ServerState {
    work_db: Arc<WorkDb>,
    execution_coordinator: Arc<ExecutionCoordinator>,
    completion_handler: Arc<WorkerCompletionHandler>,
    /// Direct handle to the cube client, used by control verbs that
    /// don't otherwise go through the execution coordinator (e.g.
    /// `WorkspacePoolSummary`).
    cube_client: Arc<dyn CubeClient>,
    /// Shared event publisher. The execution coordinator and
    /// completion handler each hold their own `Arc` clones; this
    /// field exists so background tasks spawned out of `Self::new`
    /// (the merge poller, etc.) can publish work-item invalidations
    /// without standing up a second broker.
    publisher: Arc<dyn ExecutionPublisher>,
    /// Live-CI probe shared by the `MarkCiRemediationNoop` and
    /// `MarkCiRemediationSucceededViaRebase` validation gates (T2764
    /// postmortem, PR spinyfin/mono#2023). Both handlers read this
    /// instead of constructing their own `CommandMergeProbe`, so tests can
    /// inject a fake and exercise the green/pending/red classification
    /// without shelling out to `gh`. Defaults to `CommandMergeProbe::new()`
    /// in production (see [`Self::new_arc_with_app_pid_and_merge_probe`]).
    merge_probe: Arc<dyn MergeProbe>,
    /// Executes the Direct merge-mechanism side effect (`gh pr merge --auto
    /// --squash`) for `handle_merge_when_ready`'s `MergeMechanism::Direct`
    /// branch. Defaults to `CommandDirectMergeExecutor` in production (see
    /// [`Self::new_arc_with_app_pid_and_merge_probe`]); tests inject a fake
    /// so exercising the Direct routing decision never shells out to a real
    /// `gh` process.
    direct_merge_executor: Arc<dyn merge_when_ready::DirectMergeExecutor>,
    /// Shared dispatch-event sink. The execution coordinator emits
    /// the per-stage events into this sink during dispatch; the
    /// `UpdateWorkItem` handler emits a `StatusTransition` event
    /// before dispatch even gets a chance to fire, which is the
    /// only signal we have when the auto-dispatch gate decides to
    /// skip (the "I dragged it and nothing happened" symptom).
    dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink>,
    /// Root path the dispatch-event sink writes under. Surfaced on
    /// `ServerState` so the stage-stalled detector (spawned out of
    /// `serve`) can run [`crate::dispatch_reader::pending_stalls`]
    /// against the same files the sink populates.
    dispatch_event_root: PathBuf,
    topic_broker: Arc<TopicBroker>,
    worker_registry: WorkerRegistry,
    /// Live runtime state per allocated worker slot. Updated as hook
    /// events arrive on the events socket; surfaced to bossctl/UI via
    /// `ListWorkerLiveStates` and pushed on the
    /// `worker.live_states` topic whenever any slot changes.
    live_worker_states: Arc<LiveWorkerStateRegistry>,
    /// Cross-work-item spawn-capability circuit breaker. Fed by both the
    /// `ReportWorkerSpawnFailed` NACK handler and the periodic
    /// [`crate::spawn_ack_sweep`]; when too many DISTINCT work items fail to
    /// spawn a shell in a short window it pauses dispatch and raises one loud
    /// attention item. Shared with the sweep loop (wired in
    /// `app::server::serve`). See [`crate::spawn_health`].
    spawn_health: Arc<crate::spawn_health::SpawnHealthTracker>,
    /// Per-slot trigger fan-in for the live-status summarizer. Started
    /// when `spawn_flow` calls `start_live_status_slot`; torn down
    /// in `release_worker_pane`.
    live_status_manager: Arc<LiveStatusManager>,
    /// Engine-wide counters for the hook-event dispatcher. Surfaced
    /// by the `bossctl live-status debug` verb so an operator can
    /// see at a glance whether hooks are arriving, whether their
    /// payloads carry `transcript_path`, and whether the persist
    /// call into `work_runs` succeeded. Added as the visibility
    /// surface that PR #366 did not have — without it, a stalled
    /// pipeline looked indistinguishable from a healthy one.
    dispatcher_stats: Arc<crate::live_status_loop::DispatcherStats>,
    /// Per-run in-memory `transcript_path` cache. The dispatcher
    /// populates this whenever a hook payload carries the field and
    /// uses it as a fallback whenever a subsequent hook for the same
    /// run lacks the field. See [`TranscriptPathCache`] for why this
    /// is the structural fix for the 2026-05-12 incident.
    transcript_path_cache: Arc<crate::live_status_loop::TranscriptPathCache>,
    /// Primary-path `execution_id → pr_url` staging cache. Populated
    /// by [`dispatch_live_worker_state`] from `PostToolUse` Bash
    /// hooks that surface a `gh pr create` (or `view` / `edit`)
    /// URL in `tool_response.stdout`. Read by
    /// [`WorkerCompletionHandler::on_stop`] (and `recheck_for_pr`)
    /// on the matching Stop to skip the `jj log` + `gh api` PR
    /// reconstruction entirely.
    ///
    /// Shared with the completion handler via
    /// [`WorkerCompletionHandler::with_staged_pr_urls`] so writes
    /// here and reads in `on_stop` see the same map.
    staged_pr_urls: Arc<crate::pr_url_capture::StagedPrUrlCache>,
    /// In-memory set of `revision_implementation` execution IDs that ran a
    /// `jj git push` command since their last Stop boundary. Shared with the
    /// completion handler so `on_stop_inner`'s SHA-delta gate can confirm
    /// the revision was the one that moved the PR head.
    staged_revision_pushes: Arc<crate::pr_url_capture::StagedRevisionPushCache>,
    /// Per-execution deny counter for the editorial PreToolUse loop guard
    /// (design R3). State is in-memory only; a restart resets it to zero,
    /// which is the safe direction (worst case a worker gets three fresh
    /// denies rather than an indefinite block).
    editorial_deny_tracker: Arc<crate::editorial_hook::DenyTracker>,
    /// Snapshot of the Anthropic API key captured at engine startup.
    /// Used by the live-status summarizer for the per-slot task; the
    /// pane-titlebar summarizer continues to resolve the key
    /// per-spawn via `cfg.agent()`.
    anthropic_api_key: Option<String>,
    /// Live pool sizes clamped at engine startup. Pushed to the macOS
    /// app as `EnginePoolConfig` on every `RegisterAppSession` so the
    /// app's `WorkersWorkspaceModel` slot ranges always mirror the
    /// engine's actual allocation limits, not independently-maintained
    /// constants that drift when pool sizes change.
    worker_pool_size: u8,
    automation_pool_size: u8,
    review_pool_size: u8,
    /// Model slug the Boss coordinator session launches with. Pushed
    /// verbatim in the same `EnginePoolConfig` payload as the pool sizes
    /// above. Sourced from [`crate::config::WorkConfig::coordinator_model`]
    /// (`BOSS_COORDINATOR_MODEL`, default `"opus"`) — independent of the
    /// worker effort→model table so a coordinator-model change never
    /// silently changes what workers dispatch on, or vice versa.
    coordinator_model: String,
    /// Shared verdict from the `syspolicyd` CPU monitor. The sampler loop
    /// (spawned in `serve`) writes it; [`build_engine_health_report`]
    /// reads it to raise a banner when the daemon wedges and stalls all
    /// builds. See [`crate::syspolicyd_monitor`].
    syspolicyd_health: Arc<crate::syspolicyd_monitor::SyspolicydHealth>,
    next_session_id: AtomicU64,
    work_revision: Arc<AtomicU64>,
    /// Pid of the process the engine trusts as the macOS app — must
    /// match a session's `peer_pid` for `RegisterAppSession` to
    /// succeed. `None` only in tests; production seeds this from
    /// `BOSS_APP_PID` at startup.
    ///
    /// Interior-mutable because the app can restart against a surviving
    /// engine (same-version relaunch — the engine correctly stays up).
    /// The relaunched app has a new pid, so the trust root must be
    /// re-pinned to it on re-registration; otherwise the stale pid
    /// rejects every `RegisterAppSession` and engine→app RPCs
    /// (`SpawnWorkerPane`, reveal) die. See `register_app_session`'s
    /// caller and `current_app_pid`/`set_app_pid`.
    app_pid: StdMutex<Option<libc::pid_t>>,
    /// Pid of the Boss session's shell, set by the app via
    /// `RegisterBossSession` once the Boss libghostty pane has spawned.
    /// Used as the second trust root: a peer whose process tree
    /// includes this pid as an ancestor is treated as the Boss tier
    /// for RPC authorization.
    boss_pid: StdMutex<Option<libc::pid_t>>,
    /// Pending probes per run, FIFO. Each entry is the engine-minted
    /// `probe_id` paired with the verbatim text the caller queued.
    /// The events-socket consumer pops one entry per `Stop` hook event
    /// for the matching run and dispatches it as `SendToPane` to the
    /// app.
    pending_probes: StdMutex<HashMap<String, VecDeque<PendingProbe>>>,
    /// Probes that have been dispatched into a worker pane and are
    /// awaiting the *next* `Stop` boundary so the engine can extract
    /// the worker's reply from its transcript and emit
    /// `FrontendEvent::ProbeReplied`. One entry per run at most — the
    /// next Stop after dispatch consumes it. The transcript byte
    /// offset captured at dispatch time bounds the read, so we don't
    /// re-emit text that pre-dated the probe.
    in_flight_probes: StdMutex<HashMap<String, InFlightProbe>>,
    /// Lifecycle state per `probe_id`. Written at each transition
    /// (queued / injected / consumed / unconfirmed / replied) and read
    /// by `dispatch_probe_reply_on_stop` and by tests asserting the
    /// corrected no-auto-redelivery behavior. See [`ProbeLifecycleState`].
    #[builder(default)]
    probe_lifecycle: StdMutex<HashMap<String, ProbeLifecycleState>>,
    /// One-shot waiters for the next `UserPromptSubmit` hook on a
    /// run, keyed by `run_id`. Each run can have *multiple* waiters
    /// registered concurrently (an urgent probe and a chore-update
    /// notice can both be mid-flight within the same verification
    /// window — they run from unrelated call sites with no shared
    /// serialization), so entries are stored in a `Vec` alongside the
    /// text each waiter is trying to confirm. `resolve_delivery_waiter`
    /// resolves only the entry whose `match_text` is contained in the
    /// arrived prompt (normalized for whitespace/reflow), so an
    /// unrelated prompt from the worker can't steal a waiter that was
    /// registered for different injected text, and a second concurrent
    /// registration for the same run no longer drops the first.
    #[builder(default)]
    delivery_waiters: StdMutex<HashMap<String, Vec<DeliveryWaiter>>>,
    /// Mint tokens for [`Self::delivery_waiters`] entries so
    /// `take_delivery_waiter` can remove exactly the waiter a given
    /// `inject_pane_text_verified` call registered, even when other
    /// waiters for the same run are concurrently outstanding.
    #[builder(default)]
    next_delivery_token: AtomicU64,
    /// Monotonic counter used to mint probe ids (`probe-{n}`). Probe
    /// ids only need to be unique for the lifetime of one engine
    /// process — they correlate a `ProbeRun` request with its
    /// follow-up `ProbeReplied` push, and clients don't persist them.
    next_probe_id: AtomicU64,
    /// Currently-registered app session, if any. Engine→app requests
    /// are routed only to this session.
    app_session: Arc<Mutex<Option<AppSessionHandle>>>,
    /// Liveness signal for the engine→app push channel. Tracks the
    /// consecutive-send-failure streak so a wedged/saturated outbound
    /// queue surfaces as a single engine-health issue instead of only
    /// per-call `Send(Timeout)` WARNs. See [`AppChannelHealth`].
    #[builder(default)]
    app_channel_health: Arc<AppChannelHealth>,
    /// Serializes outbound `SpawnWorkerPane` round-trips so the app
    /// only ever sees one pane allocation in flight at a time. See the
    /// `WorkerSpawner` impl for the why.
    spawn_pane_lock: Arc<Mutex<()>>,
    /// Append-only JSONL log of every engine↔app IPC exchange. Each
    /// `send_to_app` call appends an `engine→app` record; each
    /// `deliver_app_response` call appends an `app→engine` record.
    /// Backed by a background task so log writes never block the hot
    /// path. Files rotate daily under `<state-root>/ipc/`.
    ipc_logger: IpcLogger,
    /// Weak self-reference produced by `Arc::new_cyclic`. Kept so
    /// late-bound consumers (the pane-spawn runner) can resolve back
    /// to the live `Arc<ServerState>` without an outer allocation.
    _self_weak: Weak<ServerState>,
    /// Toggleable feature flags for optional/risk-bearing engine
    /// behaviours (incident 001 AI #5). Loaded from
    /// `~/Library/Application Support/Boss/feature-flags.toml` at
    /// boot, mutated by `SetFeatureFlag` RPC, consulted by callers
    /// via `is_enabled(...)`. See `crate::feature_flags`.
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Registry of capability IDs present in the current running
    /// build. Populated by `RegisterCapabilities` RPC when the macOS
    /// app connects, and by engine-side startup for engine-built
    /// features. Consulted by `snapshot_all` to populate
    /// `capability_present` on every `FeatureFlagSnapshot`.
    capability_registry: Arc<crate::feature_flags::CapabilityRegistry>,
    /// Per-installation settings (e.g. default_pr_draft_mode). Loaded
    /// from `~/Library/Application Support/Boss/settings.toml` at boot,
    /// mutated by `SetSetting` RPC, consulted by the spawn flow to
    /// inject worker directives. See `crate::settings`.
    settings: Arc<crate::settings::SettingsStore>,
    /// Engine-wide counter / gauge registry. Plumbed as an
    /// `Arc<Registry>` per the framework design's recommendation
    /// against globals (see
    /// `tools/boss/docs/designs/engine-counter-metrics-framework.md`
    /// §"Risks / open questions" item 7) — every call site that
    /// increments a counter takes a `&Registry`, which keeps
    /// counter state isolated per `ServerState` instance and
    /// makes unit tests cheap.
    metrics: Arc<crate::metrics::Registry>,
    /// Registry of external-tracker backends. Holds the `GitHubTracker`
    /// at startup; future backends (Jira, Linear) are registered the
    /// same way. Shared between the periodic spawn loop and the
    /// on-demand `SyncProductExternalTracker` handler.
    tracker_registry: Arc<crate::external_tracker::TrackerRegistry>,
    /// Single per-host (github.com) GitHub OAuth device-flow controller.
    /// Owns the auth state machine, the poll loop, and keychain persistence.
    /// The `GitHubAuthStart/Cancel/Disconnect/Status` handlers drive it; a
    /// forwarder task spawned in `serve` watches its state and pushes
    /// [`FrontendEvent::GitHubAuthState`] on [`TOPIC_GITHUB_AUTH`] plus runs
    /// the org/SSO probe. See the OAuth device-flow design (§3, §4, §7).
    github_auth: Arc<GitHubAuthController>,
    /// Stores/reads the Trunk org API token (env override or OS keychain).
    /// Backs the `TrunkSetToken`/`TrunkStatus` RPC handlers
    /// (`boss engine trunk set-token` / `boss engine trunk status`). See
    /// the Trunk merge-queue integration design's "Auth" section.
    trunk_token_store: Arc<dyn trunk_auth::TrunkTokenSource>,
    /// Typed client for Trunk's merge-queue REST API, built from the same
    /// org API token as `trunk_token_store` (env override or OS keychain).
    /// `handle_merge_when_ready` uses it to `submitPullRequest` for a
    /// `trunk_queue`-mechanism product. Cheap to clone — see
    /// `boss_trunk_client::TrunkClient`'s doc comment. See the Trunk
    /// merge-queue integration design's "The merge verb" section.
    trunk_client: boss_trunk_client::TrunkClient,
    /// Resolves credentials for external-tracker sync. Uses
    /// `KeychainOAuthResolver` in production so a stored OAuth token
    /// takes precedence over ambient `gh` auth.
    tracker_credential_resolver: Arc<dyn crate::external_tracker::credentials::TrackerCredentialResolver>,
    /// Shared kick signal for the merge-poller loop. The macOS app
    /// fires [`FrontendRequest::KickPrReconcilers`] on window
    /// activation; the handler calls `notify_one()` here so the
    /// poller's next wait arm resolves immediately (subject to the
    /// 15 s engine-side quiesce window). `None` only between
    /// `new_arc` return and the first `spawn_merge_poller` call in
    /// `serve` — that window is < 1 ms in production.
    pr_reconciler_kick: Arc<Notify>,
    /// Targeted companion to `pr_reconciler_kick`: lets a future caller
    /// (push-event relay, adaptive per-PR timer — see
    /// `tools/boss/docs/investigations/
    /// github-event-detection-webhooks-vs-polling-2026-07-08.md` §9)
    /// request an immediate pass for one specific PR instead of the broad
    /// "sweep everything" kick. No caller fires this yet; it's plumbed
    /// through to the poller so that follow-up work has an entry point.
    #[builder(default)]
    pr_reconciler_targeted_kick: PrReconcilerTargetedKick,
    /// Kick signal for the automation scheduler loop. Notified by any
    /// automation mutation handler (create, update, enable, disable,
    /// delete) so the scheduler recomputes its min-next-fire sleep
    /// immediately on state change rather than waiting out its current
    /// interval. See [`crate::automation_scheduler::spawn_loop`].
    automation_scheduler_kick: Arc<Notify>,
    /// Secret token written to the control-token file at startup. A
    /// frontend `Shutdown { token }` RPC must match this value to
    /// trigger graceful exit. `None` only in tests / in-process
    /// `serve` calls that didn't ask for a control token — those
    /// callers can't shut the engine down over the wire (they always
    /// have direct ownership of the runtime handle and can drop it).
    control_token: Option<Arc<String>>,
    /// Notified by the `Shutdown` RPC handler after a successful token
    /// match. The accept loop in `serve` selects on this alongside the
    /// SIGTERM-style shutdown signal and exits the same graceful path
    /// when either fires.
    shutdown_trigger: Arc<Notify>,
}

impl ServerState {
    /// Construct `ServerState` with optional `MergeProbe`, Trunk
    /// token-store, and Trunk client overrides. Production (via
    /// [`server::serve`]) passes `None` for all three and gets the real
    /// `CommandMergeProbe` (shell out to `gh`), `boss_trunk_auth::TrunkTokenStore`
    /// (OS keychain), and a `TrunkClient` built from that same store; tests
    /// that need to exercise the CI-remediation validation gates (green /
    /// pending / red), the `TrunkSetToken`/`TrunkStatus` handlers, or the
    /// `trunk_queue` merge-when-ready path without a live `gh` call, the
    /// real OS keychain, or a live Trunk API call inject a fake/mock for
    /// any of the three — see `MergeProbe`'s doc comment ("test doubles can
    /// stub it directly"), `trunk_auth::TrunkTokenSource`, and
    /// `boss_trunk_client::TrunkClient::new` (point `CallConfig::base_url`
    /// at a `wiremock::MockServer`).
    fn new_arc_with_app_pid_and_merge_probe(
        cfg: Arc<RuntimeConfig>,
        app_pid: Option<libc::pid_t>,
        control_token: Option<Arc<String>>,
        merge_probe_override: Option<Arc<dyn MergeProbe>>,
        trunk_token_store_override: Option<Arc<dyn trunk_auth::TrunkTokenSource>>,
        trunk_client_override: Option<boss_trunk_client::TrunkClient>,
        direct_merge_executor_override: Option<Arc<dyn merge_when_ready::DirectMergeExecutor>>,
    ) -> Result<Arc<Self>> {
        let work_db = Arc::new(WorkDb::open(cfg.work.db_path.clone())?);
        let anthropic_api_key = cfg.agent().ok().and_then(|agent| agent.anthropic_api_key.clone());
        // One-time startup signal so the missing-API-key case is
        // immediately visible in engine stderr — the chore calls out
        // that the summarizer used to drop this silently and the user
        // wants to confirm it's not the failure mode they're hitting.
        // Logged at `info` for the happy path so a `grep "live_status:"`
        // sweep still shows the engine made a decision.
        if anthropic_api_key.is_some() {
            tracing::info!("live_status: ANTHROPIC_API_KEY is configured; summarizer enabled",);
        } else {
            tracing::error!(
                "live_status: ANTHROPIC_API_KEY is NOT configured — \
                 every summarizer call will return no_api_key and no \
                 worker will get a live_status sentence. Set it in the \
                 engine's agent config or via env to enable.",
            );
        }
        // Engine build identity, logged once at startup so the user
        // can grep `live_status:` and confirm which binary is live.
        //
        // `build_info::init()` here is load-bearing: it pins the
        // binary fingerprint to the engine's on-disk bytes *as they
        // exist right now*, before any installer can replace the file
        // out from under us. Without it, the OnceLock would populate
        // on the first GetEngineVersion query, hashing whatever bytes
        // happen to be on disk at that moment — and if Boss.app was
        // updated while the engine was still running, those are the
        // *new* bytes. The macOS app would see "fingerprint matches
        // bundled engine" and silently attach to the stale engine
        // instead of triggering the version-mismatch restart from
        // T460. See `build_info::binary_fingerprint` doc comment.
        crate::build_info::init();
        tracing::info!(
            engine_build_sha = crate::build_info::git_sha(),
            engine_build_time = crate::build_info::build_time(),
            engine_binary_fingerprint = crate::build_info::binary_fingerprint(),
            "live_status: engine starting (build identity)",
        );
        // Phase 3 of distributed-agent-execution: sweep stale
        // OpenSSH ControlMaster sockets left behind by a previous
        // engine run that crashed before `SshTransport::close`. Per
        // the design's "Risks and Open Questions": this sweep is
        // non-negotiable — without it, a stale socket file can
        // prevent the next dispatch from binding a fresh master.
        if let Some(dir) = crate::ssh_transport::default_control_socket_dir() {
            match crate::ssh_transport::sweep_stale_control_sockets(&dir) {
                Ok(n) if n > 0 => {
                    tracing::info!(
                        swept = n,
                        dir = %dir.display(),
                        "engine startup: swept stale ssh control sockets",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        dir = %dir.display(),
                        "engine startup: ssh control-socket sweep failed (non-fatal)",
                    );
                }
            }
        }
        let worker_pool = WorkerPool::new(cfg.work.worker_pool_size);
        let automation_pool = WorkerPool::new_automation(cfg.work.automation_pool_size);
        let review_pool = WorkerPool::new_review(cfg.work.review_pool_size);
        // Capture clamped pool sizes before they move into the coordinator so
        // we can embed them into ServerState and push EnginePoolConfig to the
        // macOS app on every RegisterAppSession.
        let worker_pool_size = cfg.work.worker_pool_size as u8;
        let automation_pool_size = cfg.work.automation_pool_size as u8;
        let review_pool_size = cfg.work.review_pool_size as u8;
        let coordinator_model = cfg.work.coordinator_model.clone();
        let topic_broker = Arc::new(TopicBroker::default());
        let work_revision = Arc::new(AtomicU64::new(0));
        let publisher_impl = Arc::new(BrokerExecutionPublisher {
            topic_broker: topic_broker.clone(),
            work_revision: work_revision.clone(),
            kick: std::sync::OnceLock::new(),
        });
        let publisher: Arc<dyn ExecutionPublisher> = publisher_impl.clone();
        let cube_client: Arc<dyn CubeClient> = Arc::new(CommandCubeClient::new(cfg.clone()));
        let pr_detector: Arc<dyn PrDetector> = Arc::new(CommandPrDetector::new());
        // The pane releaser and probe queuer both need a Weak<ServerState>
        // to call back into ServerState methods, so they're late-bound
        // after the Arc<ServerState> exists. Same pattern as
        // `PaneSpawnRunner` below.
        let pane_releaser = Arc::new(ServerStatePaneReleaser::default());
        let probe_queuer = Arc::new(ServerStateProbeQueuer::default());
        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        let staged_revision_pushes = Arc::new(crate::pr_url_capture::StagedRevisionPushCache::new());

        // Resolve the Boss state root early — both the feature-flags
        // store (loaded below, before the completion handler is
        // built) and the dispatch-event sink (set up further down)
        // land next to `state.db` under the same root. Empty parent
        // (test configs with `:memory:` for the DB path) falls back
        // to `cwd` so test artifacts stay co-located.
        let state_root: PathBuf = cfg
            .work
            .db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cfg.work.cwd.clone());

        // Load the feature-flags store from the on-disk file. A
        // missing or unreadable file is logged but does not block
        // startup: the in-memory store falls back to registry defaults
        // for every flag, which is the same behaviour as a fresh
        // install. Persisting failures inside `set` are caught by
        // the RPC handler.
        let feature_flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            crate::feature_flags::FeatureFlagsStore::default_path(&state_root),
        ));
        if let Err(err) = feature_flags.load() {
            tracing::warn!(
                ?err,
                path = %feature_flags.path().display(),
                "feature-flags: load failed; falling back to registry defaults",
            );
        }
        let feature_flags_for_handler = feature_flags.clone();
        let feature_flags_for_state = feature_flags.clone();

        // Boot-time dedup sweep gate (design: notification-dedup-scoring.md §7).
        // With the flag off (the default) this is a no-op. When on, a one-shot
        // background task will be spawned here to sweep existing Attention items
        // for near-duplicates as a backstop — not yet implemented.
        if feature_flags.is_enabled("notification_dedup") {
            // TODO(@brianduff,2026-12-31): spawn one-shot dedup sweep background task.
        }

        // Load per-installation settings. Same boot contract as feature
        // flags: a missing or unreadable file falls back to registry
        // defaults; parse failures are logged but don't block startup.
        let settings = Arc::new(crate::settings::SettingsStore::new(
            crate::settings::SettingsStore::default_path(&state_root),
        ));
        if let Err(err) = settings.load() {
            tracing::warn!(
                ?err,
                path = %settings.path().display(),
                "settings: load failed; falling back to registry defaults",
            );
        }
        // Log active (non-default) settings at startup so the operator
        // can diagnose unexpected worker behaviour (e.g. draft PRs).
        for snap in settings.snapshot_all() {
            if snap.enabled != snap.default_enabled {
                tracing::info!(
                    key = %snap.key,
                    enabled = snap.enabled,
                    "settings: active non-default setting at startup",
                );
            }
        }
        let settings_for_state = settings.clone();

        // Engine counter-metrics registry. Built up front so it can
        // be cloned into ServerState; the registry is plumbed
        // explicitly rather than stashed in a global per the
        // framework design. `init_all` runs further down once the
        // Arc<ServerState> is in hand so a duplicate registration
        // panics during this boot path instead of inside the first
        // increment.
        let metrics_registry = Arc::new(crate::metrics::Registry::new());
        let metrics_for_state = metrics_registry.clone();
        let metrics_for_dispatcher = metrics_registry.clone();
        let metrics_for_completion = metrics_registry.clone();
        let metrics_for_coordinator = metrics_registry.clone();
        let pr_reconciler_kick = Arc::new(Notify::new());
        let pr_reconciler_kick_for_state = pr_reconciler_kick.clone();
        let automation_scheduler_kick = Arc::new(Notify::new());
        let automation_scheduler_kick_for_state = automation_scheduler_kick.clone();
        let shutdown_trigger = Arc::new(Notify::new());
        let shutdown_trigger_for_state = shutdown_trigger.clone();
        let control_token_for_state = control_token.clone();

        let mut tracker_registry = crate::external_tracker::TrackerRegistry::new();
        tracker_registry
            .register(Arc::new(crate::external_tracker::github::GitHubTracker::new()))
            .expect("github tracker is the only registered kind; duplicate is impossible");
        let tracker_registry = Arc::new(tracker_registry);
        let tracker_registry_for_state = tracker_registry.clone();

        // GitHub OAuth device-flow controller (single per-host: github.com).
        // Backed by the OS keychain; the forwarder spawned in `serve` restores
        // any persisted token at boot and pushes state transitions to the app.
        let (github_auth_controller, _github_auth_rx) = GitHubAuthController::with_store(
            DeviceFlow::production(github_oauth_http_client()),
            Arc::new(KeychainTokenStore::new()),
        );
        let github_auth_for_state = Arc::new(github_auth_controller);

        // Trunk org API token store (env override or OS keychain). Backs
        // `boss engine trunk set-token` / `trunk status`. Production (via
        // `server::serve`) passes `None` and gets the real
        // `boss_trunk_auth::TrunkTokenStore`; tests that need to exercise
        // the handlers without touching the real OS keychain inject a fake
        // via `trunk_token_store_override` — see
        // `trunk_auth::TrunkTokenSource`'s doc comment.
        let trunk_token_store_for_state: Arc<dyn trunk_auth::TrunkTokenSource> =
            trunk_token_store_override.unwrap_or_else(|| Arc::new(boss_trunk_auth::TrunkTokenStore::new()));

        // Trunk queue REST client. Builds its own `TrunkTokenStore` — it reads
        // the same env override / keychain entry as `trunk_token_store`
        // above, but the two traits (`TrunkTokenSource` for set/status,
        // `TrunkTokenProvider` for API calls) are distinct, so the instance
        // above can't be passed here directly; both stores are stateless
        // (every read goes straight to the env var / keychain), so a second
        // instance behaves identically to sharing one. Note that
        // `trunk_token_store_override` alone therefore does not affect this
        // client — a test needs `trunk_client_override` to control the token
        // the client actually sends. Tests point `trunk_client_override` at a
        // `wiremock::MockServer` via `CallConfig::with_base_url`.
        let trunk_client_for_state: boss_trunk_client::TrunkClient = trunk_client_override.unwrap_or_else(|| {
            boss_trunk_client::TrunkClient::new(
                Arc::new(boss_trunk_auth::TrunkTokenStore::new()),
                boss_trunk_client::CallConfig::default(),
            )
        });

        let tracker_credential_resolver: Arc<dyn crate::external_tracker::credentials::TrackerCredentialResolver> =
            Arc::new(crate::external_tracker::credentials::KeychainOAuthResolver::new(
                crate::external_tracker::github_oauth::KeychainTokenStore::new(),
            ));
        let ci_probe: Arc<dyn MergeProbe> = merge_probe_override.unwrap_or_else(|| Arc::new(CommandMergeProbe::new()));
        let merge_probe_for_state = ci_probe.clone();
        let direct_merge_executor_for_state: Arc<dyn merge_when_ready::DirectMergeExecutor> =
            direct_merge_executor_override.unwrap_or_else(|| Arc::new(merge_when_ready::CommandDirectMergeExecutor));
        let completion_handler = Arc::new(
            WorkerCompletionHandler::new(
                work_db.clone(),
                pr_detector,
                cube_client.clone(),
                publisher.clone(),
                pane_releaser.clone(),
                probe_queuer.clone(),
            )
            .with_staged_pr_urls(staged_pr_urls.clone())
            .with_staged_revision_pushes(staged_revision_pushes.clone())
            .with_feature_flags(feature_flags_for_handler)
            .with_merge_probe(ci_probe)
            .with_metrics(metrics_for_completion)
            .with_max_review_cycles(cfg.work.max_review_cycles)
            .with_min_review_changed_lines(cfg.work.min_review_changed_lines)
            .with_enable_revision_triggered_reviews(cfg.work.enable_revision_triggered_reviews),
        );

        // Build PaneSpawnRunner up front, hand its Weak<ServerState>
        // pointer back via set_server_state once the Arc exists. The
        // runner needs to call into ServerState (send_to_app +
        // worker_registry) while ServerState owns the runner —
        // Arc::new_cyclic breaks the cycle.
        let pane_runner = Arc::new(crate::runner::PaneSpawnRunner::new(
            cfg.clone(),
            work_db.clone(),
            feature_flags.clone(),
        ));
        let runner_for_coordinator = pane_runner.clone();
        let cube_client_for_state = cube_client.clone();
        let publisher_for_state = publisher.clone();

        // Dispatch-event JSONL stream lands next to state.db /
        // events.sock under the same `state_root` resolved above.
        let dispatch_event_root: PathBuf = state_root.clone();
        let dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink> =
            Arc::new(crate::dispatch_events::JsonlFileSink::new(dispatch_event_root.clone()));
        let dispatch_events_for_state = dispatch_events.clone();
        let dispatch_event_root_for_state = dispatch_event_root.clone();
        let ipc_logger = IpcLogger::new(&dispatch_event_root);

        let completion_handler_for_coordinator = completion_handler.clone();
        // Distributed-execution PR3 inputs for the SSH-capable host-adapter
        // provider: the engine's local events socket (target of each remote
        // run's reverse `ssh -R` forward), the engine-owned control-socket
        // dir, and a config handle. Resolved out here so the move-closure
        // below can consume them.
        let cfg_for_provider = cfg.clone();
        let provider_events_socket = crate::runner::engine_events_socket_path();
        let provider_control_dir = crate::ssh_transport::default_control_socket_dir();
        // Create the live per-slot worker registry up front so the
        // coordinator's lease-time occupancy guard (defect 3) and
        // ServerState share the SAME registry instance.
        let live_worker_states = Arc::new(LiveWorkerStateRegistry::new());
        let live_worker_states_for_coordinator = live_worker_states.clone();
        let server_state = Arc::new_cyclic(move |weak_self: &Weak<ServerState>| {
            let mut execution_coordinator_inner = ExecutionCoordinator::with_publisher(
                work_db.clone(),
                worker_pool,
                cube_client,
                runner_for_coordinator,
                publisher,
            );
            execution_coordinator_inner.set_dispatch_events(dispatch_events);
            execution_coordinator_inner.set_metrics(metrics_for_coordinator);
            execution_coordinator_inner.set_live_worker_states(live_worker_states_for_coordinator);
            // Bounded merge_order dispatch stagger (direction 2, default off).
            // Already clamped to MAX_MERGE_ORDER_STAGGER_SECS at config load.
            execution_coordinator_inner.set_merge_order_stagger_secs(cfg.work.merge_order_stagger_secs);
            execution_coordinator_inner.set_automation_pool(automation_pool);
            execution_coordinator_inner.set_review_pool(review_pool);
            // Wire the SHA-delta gate's run-start snapshot: when an
            // execution transitions to `running`, the completion
            // handler captures the bound chore PR's head SHA into
            // `work_executions.pr_head_before`.
            execution_coordinator_inner.set_execution_started_hook(completion_handler_for_coordinator.clone());
            // Wire automation preemption: when mainline work is ready and
            // every Bridge Crew and Lower Decks slot is occupied, the
            // dispatcher reclaims a slot from a spilled automation run
            // through the completion handler's `force_release` — the same
            // pane-reap + cube-lease-release teardown `bossctl agents
            // stop` performs. Without this the coordinator keeps its inert
            // default and preemption never fires.
            execution_coordinator_inner.set_automation_preemptor(completion_handler_for_coordinator.clone());
            // Install the SSH-capable provider so the dispatch loop can
            // build a per-host adapter (local vs SSH-remote) for whichever
            // host the scheduler selects. `local` returns the coordinator's
            // own local adapter verbatim, so the common local-only path is
            // unchanged; remote hosts get an `SshHostAdapter` over a cached
            // ControlMaster. Skipped (default local-only provider retained)
            // only when no engine-owned control-socket dir resolves.
            if let Some(control_dir) = provider_control_dir {
                let local_adapter = execution_coordinator_inner.host_adapter();
                execution_coordinator_inner.set_host_adapter_provider(Arc::new(
                    crate::host_adapter::SshHostAdapterProvider::new(
                        local_adapter,
                        work_db.clone(),
                        cfg_for_provider,
                        provider_events_socket,
                        control_dir,
                    ),
                ));
            }
            let execution_coordinator = Arc::new(execution_coordinator_inner);

            ServerState::builder()
                .work_db(work_db)
                .execution_coordinator(execution_coordinator)
                .completion_handler(completion_handler)
                .cube_client(cube_client_for_state)
                .publisher(publisher_for_state)
                .merge_probe(merge_probe_for_state)
                .direct_merge_executor(direct_merge_executor_for_state)
                .dispatch_events(dispatch_events_for_state)
                .dispatch_event_root(dispatch_event_root_for_state)
                .topic_broker(topic_broker)
                .worker_registry(WorkerRegistry::new())
                .live_worker_states(live_worker_states)
                .spawn_health(Arc::new(
                    crate::spawn_health::SpawnHealthTracker::new()
                        .with_breaker_enabled(cfg.work.enable_spawn_capability_breaker),
                ))
                .live_status_manager(Arc::new(LiveStatusManager::new()))
                .dispatcher_stats(Arc::new(crate::live_status_loop::DispatcherStats::new(
                    metrics_for_dispatcher,
                )))
                .transcript_path_cache(Arc::new(crate::live_status_loop::TranscriptPathCache::new()))
                .staged_pr_urls(staged_pr_urls)
                .staged_revision_pushes(staged_revision_pushes)
                .editorial_deny_tracker(Arc::new(crate::editorial_hook::DenyTracker::new()))
                .maybe_anthropic_api_key(anthropic_api_key)
                .worker_pool_size(worker_pool_size)
                .automation_pool_size(automation_pool_size)
                .review_pool_size(review_pool_size)
                .coordinator_model(coordinator_model)
                .syspolicyd_health(Arc::new(crate::syspolicyd_monitor::SyspolicydHealth::new()))
                .next_session_id(AtomicU64::new(1))
                .work_revision(work_revision)
                .app_pid(StdMutex::new(app_pid))
                .boss_pid(StdMutex::new(None))
                .pending_probes(StdMutex::new(HashMap::new()))
                .in_flight_probes(StdMutex::new(HashMap::new()))
                .next_probe_id(AtomicU64::new(1))
                .app_session(Arc::new(Mutex::new(None)))
                .spawn_pane_lock(Arc::new(Mutex::new(())))
                .ipc_logger(ipc_logger)
                .self_weak(weak_self.clone())
                .feature_flags(feature_flags_for_state)
                .capability_registry(Arc::new(crate::feature_flags::CapabilityRegistry::new()))
                .settings(settings_for_state)
                .metrics(metrics_for_state)
                .pr_reconciler_kick(pr_reconciler_kick_for_state)
                .automation_scheduler_kick(automation_scheduler_kick_for_state)
                .tracker_registry(tracker_registry_for_state)
                .github_auth(github_auth_for_state)
                .trunk_token_store(trunk_token_store_for_state)
                .trunk_client(trunk_client_for_state)
                .tracker_credential_resolver(tracker_credential_resolver)
                .maybe_control_token(control_token_for_state)
                .shutdown_trigger(shutdown_trigger_for_state)
                .build()
        });

        // Register every binary-known counter / gauge handle before
        // any rehydrate or increment runs. `init_all` is empty in
        // phase 1; subsequent phases append one line per new
        // counter module so duplicate-name panics trip during this
        // boot path rather than at runtime (design §"Risks / open
        // questions" item 6).
        crate::metrics_init::init_all(&server_state.metrics);

        // Seed the in-memory registry from `state.db` so monotonic
        // counter totals span engine restarts. Failures are logged
        // and the registry is left at zero — better than refusing to
        // start because the metrics table is corrupted.
        if let Err(err) = crate::metrics::seed_from_db(&server_state.metrics, &server_state.work_db) {
            tracing::warn!(?err, "metrics: seed_from_db failed; starting from zeroed counters",);
        }

        // Late-bind the runner to the Arc<ServerState>. Going through
        // the WorkerSpawner trait keeps the runner unaware of
        // ServerState's private fields.
        let weak_spawner: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&server_state) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        pane_runner.set_server_state(weak_spawner);
        pane_releaser.set_server_state(Arc::downgrade(&server_state));
        probe_queuer.set_server_state(Arc::downgrade(&server_state));

        // Late-bind the scheduler kick into the publisher so the
        // conflict-detection path can wake the scheduler after inserting
        // a ready execution. The coordinator must exist before this is
        // called — hence the late bind.
        let coord_for_kick = server_state.execution_coordinator.clone();
        publisher_impl.set_kick(move || coord_for_kick.kick());

        // Seed the live-status manager's disabled-slot set from the
        // engine metadata KV — survives restarts of the engine
        // process. Empty on first boot.
        let persisted = load_live_status_disabled_slots(&server_state.work_db);
        server_state.live_status_manager.set_initial_disabled_slots(persisted);

        // Seed the dispatch-pause flag from the engine metadata KV.
        // A persisted pause survives an engine restart — the flag is set
        // here before any scheduler kicks so no executions slip through
        // the gap between boot and pause restoration.
        let (dispatch_paused, dispatch_paused_since, dispatch_pause_origin) =
            load_dispatch_paused_state(&server_state.work_db);
        if dispatch_paused {
            server_state
                .execution_coordinator
                .set_dispatch_paused(true, dispatch_paused_since, dispatch_pause_origin);
            tracing::info!(
                paused_since_epoch_s = dispatch_paused_since,
                origin = dispatch_pause_origin.as_metadata_str(),
                "dispatch: restoring persisted pause state — dispatch remains \
                 globally paused until `bossctl dispatch resume` is called",
            );
        }

        // Seed the automation-pause flag from the engine metadata KV —
        // independent of the dispatch-pause flag above. Same restart-safety
        // rationale: set before any scheduler kicks so no automation triage
        // pass or automation-pool spawn slips through the gap between boot
        // and pause restoration.
        let (automation_paused, automation_paused_since) = load_automation_paused_state(&server_state.work_db);
        if automation_paused {
            server_state
                .execution_coordinator
                .set_automation_paused(true, automation_paused_since);
            tracing::info!(
                paused_since_epoch_s = automation_paused_since,
                "automation: restoring persisted pause state — automation remains \
                 globally paused until `bossctl automation resume` is called",
            );
        }

        Ok(server_state)
    }

    /// Tear down the libghostty pane allocated for `run_id`.
    /// Idempotent: `take_slot_for_run` returns `None` after the first
    /// call so duplicate releases (completion-detection followed by a
    /// chore-done update or `bossctl agents stop`) don't error out.
    /// Errors talking to the app are logged and swallowed — the slot
    /// mapping has already been removed, so a future release can't
    /// retry without a fresh registration.
    ///
    /// Also drops the matching `LiveWorkerStateRegistry` entry and
    /// broadcasts the snapshot so subscribers (the kanban Doing dot,
    /// the pane titlebar pill) stop showing the worker as attached
    /// to its work item. Without this step a chore-done update would
    /// release the libghostty pane but leave the live state stuck on
    /// `WaitingForInput`, making the UI think the worker was still
    /// running.
    pub async fn release_worker_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(slot_id) = self.worker_registry.take_slot_for_run(run_id) else {
            tracing::debug!(
                run_id,
                "release_worker_pane: no slot mapped (already released or never spawned)",
            );
            // No mapped slot means no pane and no recorded pid to reap —
            // the worker either already released or has not finished
            // spawning. Either way the caller must not treat this as a
            // reap that frees the workspace lease.
            return PaneReleaseOutcome::NoLiveWorker;
        };
        // Snapshot the worker's recorded shell pid *before* we drop the
        // live-state entry further down — the engine-side reap backstop
        // below needs it. `0` means "pid not reported by the app yet",
        // which the reaper treats as a no-op.
        let shell_pid = self
            .live_worker_states
            .get(slot_id)
            .map(|state| state.shell_pid)
            .unwrap_or(0);
        let request = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id,
            kill_grace_seconds: 5,
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ReleaseWorkerPane { result: Ok(_) }) => {
                tracing::info!(run_id, slot_id, "released worker pane");
            }
            Ok(EngineToAppResponse::ReleaseWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            }) => {
                tracing::debug!(
                    run_id,
                    slot_id,
                    "release_worker_pane: app reports unknown slot — already released",
                );
            }
            Ok(other) => {
                tracing::warn!(
                    run_id,
                    slot_id,
                    ?other,
                    "release_worker_pane: app returned unexpected response",
                );
            }
            Err(SendToAppError::NotRegistered) => {
                tracing::debug!(
                    run_id,
                    slot_id,
                    "release_worker_pane: no app session registered; skipping",
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id, slot_id, "release_worker_pane: failed");
            }
        }
        // Engine-side reap backstop. The app's pane teardown above is
        // the primary reaper, but it cannot act when no app session is
        // registered, when the app is unresponsive, or when a wedged
        // surface reports no foreground pid — exactly the `bossctl
        // agents stop` leak from #975, where the engine slot and the
        // cube lease were freed but the worker's `claude` process kept
        // running (orphaned, still holding bazel/swiftc locks). Signal
        // the recorded shell pid's process group directly so the OS
        // process tree goes down even when the app path can't reach it.
        // Idempotent with the app's reap: a process already gone just
        // yields `ESRCH`. The grace mirrors the app's `kill_grace_seconds`.
        reap_worker_process_tree(shell_pid, Duration::from_secs(5));
        // The engine's WorkerPool slot was held for the lifetime of
        // the libghostty pane (the coordinator deferred its release
        // when `run_execution` returned with `slot_id = Some(N)`).
        // Now that the pane has been torn down — successfully or
        // not — the engine and the app are back in agreement that
        // slot N is free, so release the pool slot too and kick the
        // scheduler. `WorkerPool::release_worker` is a find-or-skip
        // no-op for already-idle slots, so this is safe even if the
        // pane was a non-pool spawn (e.g. legacy or test path).
        let worker_id = crate::coordinator::worker_id_for_slot(slot_id);
        self.execution_coordinator
            .release_worker_and_kick(&worker_id, None)
            .await;
        // Always drop the live-state entry — we've already given up
        // ownership of the slot in the worker registry, so a stale
        // entry here would lie to the UI about the slot being live.
        self.live_worker_states.release_slot(slot_id);
        // Tear down the per-slot live-status task. The manager
        // doesn't await the task's exit so a wedged Anthropic call
        // can't block the release path.
        self.live_status_manager.stop_slot(slot_id);
        // Drop the cached transcript path for this run so the cache
        // doesn't grow without bound across long engine lifetimes.
        // No correctness consequence — the work_runs row is the
        // durable source of truth — but a bounded cache is hygienic.
        self.transcript_path_cache.forget(run_id);
        self.broadcast_live_worker_states().await;
        // A slot was mapped, so a worker had finished spawning: its pane
        // was torn down and (above) its OS process tree signalled. Report
        // `Reaped` so the caller may free the workspace lease.
        PaneReleaseOutcome::Reaped
    }

    /// Release every live worker pane the engine knows about. Called
    /// from the engine-shutdown path: walks
    /// `LiveWorkerStateRegistry::snapshot()` and dispatches
    /// [`ServerState::release_worker_pane`] for each `run_id` in
    /// parallel. The app teardown is the primary mechanism — once the
    /// pane is released the worker shell exits and `claude` exits
    /// with it.
    ///
    /// `total_timeout` bounds the whole walk. Each individual
    /// `release_worker_pane` call already has its own ~5s round-trip
    /// budget against the app, but on shutdown we'd rather forcibly
    /// move on than block the engine exit on an unresponsive app.
    ///
    /// After the bounded join we send a best-effort `SIGTERM` (then
    /// `SIGKILL` after `kill_grace`) to every recorded `shell_pid > 0`
    /// — covers the case where the app is gone or didn't ack in time
    /// and the shell would otherwise be reparented to launchd.
    pub async fn shutdown_workers(self: &Arc<Self>, total_timeout: Duration, kill_grace: Duration) {
        let snapshot = self.live_worker_states.snapshot();
        if snapshot.is_empty() {
            tracing::info!("shutdown_workers: no live workers to release");
            return;
        }
        tracing::info!(count = snapshot.len(), "shutdown_workers: releasing live worker panes",);
        let mut set = tokio::task::JoinSet::new();
        for state in &snapshot {
            let server = Arc::clone(self);
            let run_id = state.run_id.clone();
            set.spawn(async move {
                server.release_worker_pane(&run_id).await;
            });
        }
        let join_all = async { while set.join_next().await.is_some() {} };
        if tokio::time::timeout(total_timeout, join_all).await.is_err() {
            tracing::warn!(
                timeout_secs = total_timeout.as_secs(),
                "shutdown_workers: release timed out; falling back to direct kill",
            );
        }
        let pids: Vec<libc::pid_t> = snapshot
            .iter()
            .filter_map(|s| (s.shell_pid > 0).then_some(s.shell_pid as libc::pid_t))
            .collect();
        signal_shell_pids(&pids, kill_grace);
    }

    fn current_work_revision(&self) -> u64 {
        self.work_revision.load(Ordering::SeqCst)
    }

    fn bump_work_revision(&self) -> u64 {
        self.work_revision.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Enable the transient-recovery sweep to nudge a live idle worker via
/// the same `SendToPane` path that `bossctl agents send` uses.
/// `Arc<ServerState>` can then be coerced to `Arc<dyn WorkerNudger>`.
#[async_trait]
impl crate::transient_recovery::WorkerNudger for ServerState {
    async fn nudge_worker(&self, run_id: &str, text: String) -> Result<(), String> {
        self.send_input_to_worker(run_id, text)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn broadcast_live_states(&self) {
        ServerState::broadcast_live_worker_states(self).await;
    }
}

struct BrokerExecutionPublisher {
    topic_broker: Arc<TopicBroker>,
    work_revision: Arc<AtomicU64>,
    /// Late-bound kick function set after the coordinator is created.
    /// `None` until [`BrokerExecutionPublisher::set_kick`] is called;
    /// `kick_scheduler` is a no-op until the coordinator is wired up.
    kick: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl BrokerExecutionPublisher {
    fn set_kick(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.kick.set(Arc::new(f));
    }
}

#[async_trait]
impl ExecutionPublisher for BrokerExecutionPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = execution_topic(execution_id);
        let event = FrontendEvent::TopicEvent {
            topic: topic.clone(),
            revision,
            origin_session_id: String::new(),
            origin_request_id: None,
            event: TopicEventPayload::ExecutionInvalidated {
                reason: reason.to_owned(),
                execution_id: execution_id.to_owned(),
                work_item_id: work_item_id.to_owned(),
                status: status.to_owned(),
            },
        };
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = work_product_topic(product_id);
        let event = FrontendEvent::TopicEvent {
            topic: topic.clone(),
            revision,
            origin_session_id: String::new(),
            origin_request_id: None,
            event: TopicEventPayload::WorkInvalidated {
                reason: reason.to_owned(),
                product_id: Some(product_id.to_owned()),
                item_ids: vec![work_item_id.to_owned()],
            },
        };
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = work_product_topic(product_id);
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    fn kick_scheduler(&self) {
        if let Some(f) = self.kick.get() {
            f();
        }
    }
}

#[async_trait::async_trait]
impl crate::external_tracker::reconcile::WorkInvalidationPublisher for ServerState {
    async fn publish_work_item_invalidated(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.publisher
            .publish_work_item_changed(product_id, work_item_id, reason)
            .await;
    }
}

mod session_queue;
use session_queue::*;

/// Paths derived from a non-default `--socket-path` to ensure a
/// test-fixture engine never touches production state.
///
/// When `socket_path` equals `DEFAULT_SOCKET_PATH` every field is `None` and
/// the engine resolves paths through its normal env-var / home-dir logic.
/// When `socket_path` is non-default, each field is `Some(derived_path)`
/// **unless** the corresponding env override is already set by the caller, in
/// which case the caller's choice wins and that field is `None`.
///
/// The struct is computed once in [`run`] and threaded through to
/// [`run_server`] so both the `WorkConfig` DB path and the socket/pid paths
/// inside [`serve`] use the same derived roots without touching env vars.
struct IsolationPaths {
    /// True when the engine is operating as a test fixture (non-default socket).
    is_test_fixture: bool,
    /// Isolated SQLite DB path derived from the socket stem.
    db_path: Option<std::path::PathBuf>,
    /// Isolated events socket derived from the socket stem.
    events_socket: Option<std::path::PathBuf>,
    /// Isolated pid file derived from the socket stem.
    pid_path: Option<std::path::PathBuf>,
    /// Isolated engine-control token path derived from the socket stem.
    ///
    /// Without this, `default_token_path()` resolved the production
    /// token path unconditionally, entirely outside this struct — a
    /// test-fixture engine would write, and on shutdown delete, the
    /// production control token. See `crate::engine_control` for the
    /// write/delete-time hardening layered on top of this derivation.
    token_path: Option<std::path::PathBuf>,
}

impl IsolationPaths {
    /// Derive isolation paths from `socket_path`.
    ///
    /// Non-default socket → derive paths from the socket's directory and
    /// file-stem (e.g. `/tmp/boss-test-UUID.sock` → `/tmp/boss-test-UUID.db`,
    /// `/tmp/boss-test-UUID.events.sock`, `/tmp/boss-test-UUID.pid`).
    ///
    /// Each derived path is suppressed (left as `None`) when the corresponding
    /// env override is already set, so an explicit `BOSS_DB_PATH=…` in the
    /// environment always wins.
    fn derive(socket_path: &str) -> Self {
        if socket_path == DEFAULT_SOCKET_PATH {
            return Self {
                is_test_fixture: false,
                db_path: None,
                events_socket: None,
                pid_path: None,
                token_path: None,
            };
        }

        let path = std::path::Path::new(socket_path);
        let dir = path.parent().unwrap_or(std::path::Path::new("/tmp"));
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "boss-test".to_owned());

        // Honour explicit env overrides: only set a derived path when the
        // caller hasn't already pointed this socket at an explicit location.
        let db_path = std::env::var_os("BOSS_DB_PATH")
            .is_none()
            .then(|| dir.join(format!("{stem}.db")));
        let events_socket = std::env::var_os("BOSS_EVENTS_SOCKET")
            .is_none()
            .then(|| dir.join(format!("{stem}.events.sock")));
        let pid_path = std::env::var_os("BOSS_ENGINE_PID_PATH")
            .is_none()
            .then(|| dir.join(format!("{stem}.pid")));
        let token_path = std::env::var_os(crate::engine_control::TOKEN_PATH_ENV)
            .is_none()
            .then(|| dir.join(format!("{stem}.token")));

        Self {
            is_test_fixture: true,
            db_path,
            events_socket,
            pid_path,
            token_path,
        }
    }
}

async fn handle_frontend_connection(
    stream: UnixStream,
    server_state: Arc<ServerState>,
    peer_pid: Option<libc::pid_t>,
) -> Result<()> {
    tracing::info!("frontend connected");
    let work_db = server_state.work_db.clone();
    let session_id = server_state.allocate_session_id();

    // Classify the peer once, here, rather than per request. The `boss` CLI
    // opens a connection per invocation so this is still per-command for
    // workers, while the macOS app — which holds one connection for its
    // lifetime and sends thousands of requests over it — pays a single
    // ancestry walk instead of one per frame. Registration normally happens
    // at spawn, before the pane's first `boss` call; on the ack-timeout path
    // (`spawn_flow.rs`, shell_pid 0) it is deferred until the app sends
    // `UpdateWorkerShellPid` once the libghostty surface attaches, so a call
    // in that interval classifies as `Other` and keeps `User` tier — the
    // same fail-open direction `PeerClass::Other` documents for broken
    // lineage.
    let peer_class = server_state.classify_peer(peer_pid);
    if let Some(run_id) = peer_class.worker_run_id() {
        tracing::debug!(
            session_id = %session_id,
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "frontend connection classified as worker tier",
        );
    }
    let peer_is_worker = peer_class.is_worker();

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(shutdown_tx));
    server_state
        .topic_broker
        .register_session(&session_id, sink.clone())
        .await;
    let _ = sink.enqueue(FrontendEventEnvelope::push(FrontendEvent::Hello {
        session_id: session_id.clone(),
    }));

    let writer_sink = sink.clone();
    // Read per event rather than snapshotted at connect, so flipping
    // `worker_rpc_tier` takes effect on connections that are already open —
    // the same evaluation point the verb gate uses. Without this, killing the
    // flag would leave every live worker session still sanitized until it
    // reconnected, which is not what a kill switch means.
    let writer_flags = server_state.feature_flags.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(mut event) = writer_sink.next().await {
            // The exposure boundary's single write choke point. Every frame
            // leaving a worker-classified connection passes through here —
            // request responses and topic pushes alike — so no handler has
            // to remember to sanitize, and a verb added later cannot leak a
            // `transcript_path` by forgetting to. Non-worker connections
            // short-circuit on `peer_is_worker`, so the app pays nothing but
            // a bool test per frame.
            if peer_is_worker && writer_flags.is_enabled("worker_rpc_tier") {
                event.payload = sanitize_event_for_worker(event.payload);
            }

            // If this response has a stashed population-timing trace, complete
            // it here: the serialize and socket-write costs live on this task,
            // not the handler's. `None` for every non-population event.
            let mut trace = event
                .request_id
                .as_deref()
                .and_then(|rid| writer_sink.take_population_trace(rid));
            if let Some(t) = trace.as_mut() {
                t.record_queue_wait();
            }

            let serialize_start = Instant::now();
            let line = match serde_json::to_string(&event) {
                Ok(line) => line,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize frontend event");
                    continue;
                }
            };
            if let Some(t) = trace.as_mut() {
                t.record_serialize(crate::population_timing::elapsed_ms(serialize_start), line.len());
            }

            let write_start = Instant::now();
            let mut write_failed = false;
            if let Err(err) = write_half.write_all(line.as_bytes()).await {
                tracing::error!(?err, "failed to write event to frontend socket");
                write_failed = true;
            } else if let Err(err) = write_half.write_all(b"\n").await {
                tracing::error!(?err, "failed to delimit frontend event line");
                write_failed = true;
            } else if let Err(err) = write_half.flush().await {
                tracing::error!(?err, "failed to flush frontend socket");
                write_failed = true;
            }

            if let Some(mut t) = trace {
                t.record_plain(
                    crate::population_timing::segment::SOCKET_WRITE,
                    crate::population_timing::elapsed_ms(write_start),
                );
                let total_ms = t.elapsed_ms();
                t.record_plain(crate::population_timing::segment::TOTAL, total_ms);
                if let Some(log) = crate::population_timing::global() {
                    t.flush(log);
                }
            }

            if write_failed {
                break;
            }
        }
        // Make sure the reader loop wakes if we exited from a write failure
        // rather than an explicit shutdown.
        writer_sink.close();
        writer_sink.trigger_shutdown();
    });

    loop {
        let line_result = tokio::select! {
            _ = &mut shutdown_rx => {
                tracing::info!(session_id = %session_id, "session shutdown triggered");
                break;
            }
            line = reader.next_line() => line,
        };
        let Some(line) = line_result.context("socket read failed")? else {
            break;
        };
        // Population-timing window opens at line receipt, before decode, so
        // engine-side segments sum to ~the app-observed request duration.
        let recv_instant = Instant::now();
        if line.trim().is_empty() {
            continue;
        }

        let decode_start = Instant::now();
        let envelope: FrontendRequestEnvelope = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                send_push(
                    &sink,
                    FrontendEvent::Error {
                        message: format!("invalid request payload: {err}"),
                    },
                );
                continue;
            }
        };
        let decode_ms = crate::population_timing::elapsed_ms(decode_start);
        let request_id = envelope.request_id.clone();
        let request = envelope.payload;

        // The worker verb gate, ahead of dispatch: a refused verb never
        // reaches its handler, so there is no path by which a partially
        // applied mutation escapes. One gate rather than 171 per-handler
        // checks — the policy itself is an exhaustive match, so a verb added
        // later cannot slip through unclassified.
        let row_scope_denial = server_state.worker_row_scope_denial(&peer_class, &request);
        if let Some(denial) = server_state
            .worker_tier_denial(&peer_class, &request)
            .or(row_scope_denial)
        {
            tracing::warn!(
                session_id = %session_id,
                run_id = peer_class.worker_run_id().unwrap_or("<unresolved>"),
                verb = %denial.verb,
                reason = %denial.reason,
                "worker-tier RPC denied",
            );
            let _ = sink.enqueue(FrontendEventEnvelope::response(
                request_id.clone(),
                FrontendEvent::WorkerTierDenied { denial },
            ));
            continue;
        }

        let ctx = Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(work_db.clone())
            .sink(sink.clone())
            .session_id(session_id.clone())
            .request_id(request_id.clone())
            .maybe_peer_pid(peer_pid)
            .recv_instant(recv_instant)
            .decode_ms(decode_ms)
            .build();
        match request {
            r @ FrontendRequest::AbandonCiRemediation { .. } => {
                ci_remediation::handle_abandon_ci_remediation(ctx, r).await
            }
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
            r @ FrontendRequest::CreateAutomationTask { .. } => {
                automations::handle_create_automation_task(ctx, r).await
            }
            r @ FrontendRequest::CreateChore { .. } => work_items::handle_create_chore(ctx, r).await,
            r @ FrontendRequest::CreateExecution { .. } => executions::handle_create_execution(ctx, r).await,
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
            r @ FrontendRequest::DebugLiveStatusPipeline => {
                live_status::handle_debug_live_status_pipeline(ctx, r).await
            }
            r @ FrontendRequest::DeleteAutomation { .. } => automations::handle_delete_automation(ctx, r).await,
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
            r @ FrontendRequest::GetCiBudget { .. } => ci_remediation::handle_get_ci_budget(ctx, r).await,
            r @ FrontendRequest::GetCiRemediation { .. } => ci_remediation::handle_get_ci_remediation(ctx, r).await,
            r @ FrontendRequest::GetConflictHotspots { .. } => {
                conflict_resolution::handle_get_conflict_hotspots(ctx, r).await
            }
            r @ FrontendRequest::GetConflictResolution { .. } => {
                conflict_resolution::handle_get_conflict_resolution(ctx, r).await
            }
            r @ FrontendRequest::GetDispatchState => engine_meta::handle_get_dispatch_state(ctx, r).await,
            r @ FrontendRequest::GetEngineHealth => engine_meta::handle_get_engine_health(ctx, r).await,
            r @ FrontendRequest::GetEngineVersion => engine_meta::handle_get_engine_version(ctx, r).await,
            r @ FrontendRequest::GetExecution { .. } => executions::handle_get_execution(ctx, r).await,
            r @ FrontendRequest::GetHost { .. } => hosts::handle_get_host(ctx, r).await,
            r @ FrontendRequest::GetRun { .. } => executions::handle_get_run(ctx, r).await,
            r @ FrontendRequest::GetSettings => engine_meta::handle_get_settings(ctx, r).await,
            r @ FrontendRequest::GetTaskRuntime { .. } => executions::handle_get_task_runtime(ctx, r).await,
            r @ FrontendRequest::GetWorkerContext { .. } => context::handle_get_worker_context(ctx, r).await,
            r @ FrontendRequest::GetWorkItem { .. } => work_items::handle_get_work_item(ctx, r).await,
            r @ FrontendRequest::GetWorkItemByShortId { .. } => {
                work_items::handle_get_work_item_by_short_id(ctx, r).await
            }
            r @ FrontendRequest::GetWorkTree { .. } => work_items::handle_get_work_tree(ctx, r).await,
            r @ FrontendRequest::GitHubAuthCancel => github_auth::handle_git_hub_auth_cancel(ctx, r).await,
            r @ FrontendRequest::GitHubAuthDisconnect => github_auth::handle_git_hub_auth_disconnect(ctx, r).await,
            r @ FrontendRequest::GitHubAuthStart => github_auth::handle_git_hub_auth_start(ctx, r).await,
            r @ FrontendRequest::GitHubAuthStatus => github_auth::handle_git_hub_auth_status(ctx, r).await,
            r @ FrontendRequest::InterruptWorkerPane { .. } => panes::handle_interrupt_worker_pane(ctx, r).await,
            r @ FrontendRequest::KickPrReconcilers => engine_meta::handle_kick_pr_reconcilers(ctx, r).await,
            r @ FrontendRequest::LinkWorkItemExternalRef { .. } => {
                external_tracker::handle_link_work_item_external_ref(ctx, r).await
            }
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
            r @ FrontendRequest::ListChores { .. } => work_items::handle_list_chores(ctx, r).await,
            r @ FrontendRequest::ListCiRemediations { .. } => ci_remediation::handle_list_ci_remediations(ctx, r).await,
            r @ FrontendRequest::ListConflictResolutions { .. } => {
                conflict_resolution::handle_list_conflict_resolutions(ctx, r).await
            }
            r @ FrontendRequest::ListDeferredScopeAttentions { .. } => {
                attentions::handle_list_deferred_scope_attentions(ctx, r).await
            }
            r @ FrontendRequest::ListDependencies { .. } => dependencies::handle_list_dependencies(ctx, r).await,
            r @ FrontendRequest::ListDependenciesDetailed { .. } => {
                dependencies::handle_list_dependencies_detailed(ctx, r).await
            }
            r @ FrontendRequest::ListEditorialActions { .. } => {
                automations::handle_list_editorial_actions(ctx, r).await
            }
            r @ FrontendRequest::ListEngineAttempts { .. } => executions::handle_list_engine_attempts(ctx, r).await,
            r @ FrontendRequest::ListExecutions { .. } => executions::handle_list_executions(ctx, r).await,
            r @ FrontendRequest::ListFeatureFlags => engine_meta::handle_list_feature_flags(ctx, r).await,
            r @ FrontendRequest::ListHosts => hosts::handle_list_hosts(ctx, r).await,
            r @ FrontendRequest::ListHuskPanes => panes::handle_list_husk_panes(ctx, r).await,
            r @ FrontendRequest::ListLiveStatusDisabledSlots => {
                live_status::handle_list_live_status_disabled_slots(ctx, r).await
            }
            r @ FrontendRequest::ListPlannerRuns { .. } => planner_ops::handle_list_planner_runs(ctx, r).await,
            r @ FrontendRequest::ListProducts => products::handle_list_products(ctx, r).await,
            r @ FrontendRequest::ListProjects { .. } => projects::handle_list_projects(ctx, r).await,
            r @ FrontendRequest::ListProposals { .. } => proposals::handle_list_proposals(ctx, r).await,
            r @ FrontendRequest::ListRuns { .. } => executions::handle_list_runs(ctx, r).await,
            r @ FrontendRequest::ListTasks { .. } => work_items::handle_list_tasks(ctx, r).await,
            r @ FrontendRequest::ListRevisions { .. } => work_items::handle_list_revisions(ctx, r).await,
            r @ FrontendRequest::ListWorkerLiveStates => panes::handle_list_worker_live_states(ctx, r).await,
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
            r @ FrontendRequest::ReapRun { .. } => executions::handle_reap_run(ctx, r).await,
            r @ FrontendRequest::RecordEffortEscalation { .. } => effort::handle_record_effort_escalation(ctx, r).await,
            r @ FrontendRequest::RecordProducerSideConflict { .. } => {
                conflict_resolution::handle_record_producer_side_conflict(ctx, r).await
            }
            r @ FrontendRequest::RegisterAppSession => sessions::handle_register_app_session(ctx, r).await,
            r @ FrontendRequest::RegisterBossSession { .. } => sessions::handle_register_boss_session(ctx, r).await,
            r @ FrontendRequest::RegisterCapabilities { .. } => engine_meta::handle_register_capabilities(ctx, r).await,
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
            r @ FrontendRequest::RunAutomation { .. } => automations::handle_run_automation(ctx, r).await,
            r @ FrontendRequest::SendInputToWorker { .. } => panes::handle_send_input_to_worker(ctx, r).await,
            r @ FrontendRequest::SetAutomationPaused { .. } => engine_meta::handle_set_automation_paused(ctx, r).await,
            r @ FrontendRequest::SetCiBudget { .. } => ci_remediation::handle_set_ci_budget(ctx, r).await,
            r @ FrontendRequest::SetDispatchPaused { .. } => engine_meta::handle_set_dispatch_paused(ctx, r).await,
            r @ FrontendRequest::SetFeatureFlag { .. } => engine_meta::handle_set_feature_flag(ctx, r).await,
            r @ FrontendRequest::SetHostEnabled { .. } => hosts::handle_set_host_enabled(ctx, r).await,
            r @ FrontendRequest::SetLiveStatusEnabled { .. } => {
                live_status::handle_set_live_status_enabled(ctx, r).await
            }
            r @ FrontendRequest::SetProductDefaultModel { .. } => {
                products::handle_set_product_default_model(ctx, r).await
            }
            r @ FrontendRequest::SetProductDefaultDriver { .. } => {
                products::handle_set_product_default_driver(ctx, r).await
            }
            r @ FrontendRequest::SetProductMergeMechanism { .. } => {
                products::handle_set_product_merge_mechanism(ctx, r).await
            }
            r @ FrontendRequest::SetProductEditorialRules { .. } => {
                products::handle_set_product_editorial_rules(ctx, r).await
            }
            r @ FrontendRequest::EvaluateEditorialRules { .. } => {
                products::handle_evaluate_editorial_rules(ctx, r).await
            }
            r @ FrontendRequest::SetProductExternalTracker { .. } => {
                external_tracker::handle_set_product_external_tracker(ctx, r).await
            }
            r @ FrontendRequest::SetProjectDesignDoc { .. } => projects::handle_set_project_design_doc(ctx, r).await,
            r @ FrontendRequest::SetSetting { .. } => engine_meta::handle_set_setting(ctx, r).await,
            r @ FrontendRequest::Shutdown { .. } => sessions::handle_shutdown(ctx, r).await,
            r @ FrontendRequest::SpawnCapabilityRestored => sessions::handle_spawn_capability_restored(ctx, r).await,
            r @ FrontendRequest::StopRun { .. } => executions::handle_stop_run(ctx, r).await,
            r @ FrontendRequest::SubmitProposal { .. } => proposals::handle_submit_proposal(ctx, r).await,
            r @ FrontendRequest::Subscribe { .. } => subscriptions::handle_subscribe(ctx, r).await,
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
            r @ FrontendRequest::UpdateWorkItem { .. } => work_items::handle_update_work_item(ctx, r).await,
            r @ FrontendRequest::ReportWorkerSpawnFailed { .. } => {
                sessions::handle_report_worker_spawn_failed(ctx, r).await
            }
            r @ FrontendRequest::UpdateWorkerShellPid { .. } => sessions::handle_update_worker_shell_pid(ctx, r).await,
            r @ FrontendRequest::WorkerPaneDied { .. } => sessions::handle_worker_pane_died(ctx, r).await,
            r @ FrontendRequest::WorkerPoolSummary => engine_meta::handle_worker_pool_summary(ctx, r).await,
            r @ FrontendRequest::WorkspacePoolSummary => engine_meta::handle_workspace_pool_summary(ctx, r).await,
        }
    }

    tracing::info!(session_id = %session_id, "frontend connection reader loop exited; tearing down session");
    server_state.topic_broker.remove_session(&session_id).await;
    // Clears the app-session registration iff this was the app (logged there).
    server_state.drop_app_session_if_matches(&session_id).await;
    sink.close();
    let _ = writer_task.await;
    Ok(())
}
