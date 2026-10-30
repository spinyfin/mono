//! Stable worker and dispatch attention-kind names.
//! Filing and clearing policy remains with the engine producers and lifecycle registry.
//! References to producer functions and policy types below refer to engine core.

pub const ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND: &str = "abandoned_execution_bookmark";

/// Attention-item kind filed when this sweep completes an execution. Its
/// presence means the answer-agent turn boundary never reached the engine for
/// that run — the slot was reclaimed, but the ingress that lost the signal is
/// still broken and wants a human.
pub const ANSWER_AGENT_STRANDED_ATTENTION_KIND: &str = "answer_agent_completion_signal_lost";

/// `work_attention_items.kind` for a probe the engine accepted and then never
/// delivered.
///
/// Filed against the *execution* the probe targeted, so the abandonment is
/// visible in that run's own history rather than only through a
/// `bossctl probe-status <probe-id>` query the issuer has no reason to make.
/// That asymmetry was the defect: the engine recorded the terminal state
/// correctly and told nobody, so a coordinator whose instruction was dropped
/// carried on as if it had landed.
///
/// Registered in `attention_lifecycle::ATTENTION_LIFECYCLES` as
/// `attention_lifecycle::ClearedBy::HumanDecision` — see there for
/// why no automatic evidence lowers it.
///
/// **Deliberately one row per abandoned probe, not deduped onto a single
/// open row per `(execution, kind)`** the way `work/attention_filing.rs`'s
/// module doc otherwise documents as the filing contract (see
/// `reraise_open_execution_attention`). Each lost probe is a distinct
/// instruction a coordinator issued and must individually decide what to do
/// about — a run that lost three different probes needs three answers, and
/// collapsing them onto one row would silently drop the second and third
/// probe ids from view the moment the first row existed. If this ever proves
/// noisy in practice (the same run repeatedly losing probes), the dedup
/// should key on the *probe*, not the execution: fold a probe's own retries
/// onto its own row, never a sibling probe's.
pub const PROBE_UNDELIVERED_ATTENTION_KIND: &str = "probe_undelivered";

/// Filed when a re-adopted worker's file progress ingress cannot be put back.
///
/// The worker is live but unobservable: it holds a pane, a pool slot and a
/// cube workspace lease, and nothing it does will produce a turn boundary. No
/// sweep resolves that, because every sweep that could reads liveness — and
/// the worker *is* alive. Only a human can.
pub const PROGRESS_INGRESS_UNRECOVERABLE_ATTENTION_KIND: &str = "progress_ingress_unrecoverable";

/// `work_attention_items.kind` for a filed unobserved-command attention.
/// Mirrors `deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND`'s pattern
/// of one constant per marker-class attention kind.
pub const UNOBSERVED_COMMAND_ATTENTION_KIND: &str = "codex_unobserved_command";

/// `work_attention_items.kind` for the one-time "audit trail overflowed"
/// attention — distinct from [`UNOBSERVED_COMMAND_ATTENTION_KIND`] because it
/// reports a different fact (Boss stopped being able to name every abandoned
/// command for this execution) rather than one more abandoned command.
pub const UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND: &str = "codex_unobserved_command_overflow";

/// Attention-item kind filed when the run-done backstop parks a run that
/// never declared itself finished.
///
/// Distinct from [`NUDGE_BREAKER_ATTENTION_KIND`], which the park itself
/// also files, because the two say different things to whoever reads the
/// row: the breaker item says "we asked repeatedly and nothing changed",
/// this one says "this run's outcome is unknown — the worker never told us
/// it was finished, and the engine deliberately did not guess". That
/// distinction is the whole point of the declaration, so it gets its own
/// kind rather than being folded into a generic park.
pub const RUN_UNDECLARED_ATTENTION_KIND: &str = "run_undeclared";

/// Attention-item kind filed when a `run_done` declaration's synchronous
/// finalize (see `completion::run_done_declaration`) could not fully trust
/// the claim: `delivered` with no PR the engine could resolve, or a
/// post-hoc audit finding the bound PR's head did not move. The execution
/// still terminalizes either way — a worker's declaration is the completion
/// signal, never a gate the engine gets to block on — this only flags the
/// mismatch for a human, exactly as the design's "post-hoc audit" intends.
pub const RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND: &str = "run_done_audit_flagged";

/// Attention-item kind filed when a `run_done --outcome blocked` declaration
/// terminalizes its execution. Distinct from [`WORKER_BLOCKED_ATTENTION_KIND`],
/// which the companion (still-live-run) `blocked` proposal files with the
/// blocker's own reason — this one marks that the *run itself* has ended,
/// not merely that it hit a blocker while continuing.
pub const RUN_DONE_BLOCKED_ATTENTION_KIND: &str = "run_done_declared_blocked";

/// Attention-item `kind` filed when the auto-nudge circuit breaker
/// parks an execution. Distinct kind so the coordinator/UI can surface
/// "worker parked: nudge loop bounded" separately from other attention
/// flows, and so repeated Stops dedupe against an already-open item.
pub const NUDGE_BREAKER_ATTENTION_KIND: &str = "nudge_breaker_tripped";

/// Attention-item kind filed when a reviewer worker exhausts its re-prompts
/// without ever producing a readable `ReviewResult` (neither the
/// structured-output artifact nor the transcript fallback validated). The
/// producing task is advanced to `in_review` without a revision, so this item
/// is the human-visible record that the PR proceeded *unreviewed* — replacing
/// the old silent drop.
pub const REVIEW_RESULT_GIVEUP_ATTENTION_KIND: &str = "review_result_missing";

/// Attention-item kind filed when a worker's driver reports its own terminal
/// turn boundary as an unrecoverable error (e.g. Codex's rollout
/// `task_complete.error`, or a stdout `turn.failed` / fatal `error`
/// envelope). Distinct kind so the coordinator/UI can tell "the provider
/// itself failed the run" apart from the nudge-breaker and give-up cases
/// above, which both imply the worker ran to some kind of conclusion.
pub const DRIVER_TERMINAL_ERROR_ATTENTION_KIND: &str = "driver_terminal_error";

/// Attention-item kind filed when a `revision_implementation` worker ends
/// on a `boss propose done --outcome no-changes-needed` declaration: it
/// pushed nothing, and declared explicitly that the review finding it was
/// dispatched for needs no code change.
///
/// Distinct kind, and always filed, because that terminal closes a
/// revision without the finding ever being addressed. Unlike a primary
/// implementation's no-op — where "the work was already on `main`" is
/// self-evidently complete — a revision exists because a reviewer pass
/// asked for something, so a worker declining it is a judgement a human
/// should see rather than a silent close.
pub const REVISION_NO_OP_ATTENTION_KIND: &str = "revision_no_changes_needed";

/// Attention-item kind filed when `WorkerCompletionHandler::finalize_pr_transition`
/// observes the producing execution's live-state activity as `working`
/// (mid-turn) rather than idle at the moment it decides to terminalize and
/// tear the worker down — i.e. the worker had not reached its own Stop
/// boundary. Fired from the one shared finalize/teardown funnel every
/// completion path (`pr_recheck_staged`, the `pr_recheck` detector branch,
/// `stop_satisfied_clean`, and any future source) routes through, so it is
/// not scoped to any single reap path. Together with `MID_TURN_REAP_TOTAL`
/// this is incident-004 AI-4: make a mid-turn reap countable and alertable
/// in-system instead of requiring the offline trace reconstruction that
/// postmortem needed. See
/// tools/boss/docs/postmortems/incident-004-live-revision-workers-reaped-mid-turn.md.
pub const MID_TURN_REAP_ATTENTION_KIND: &str = "mid_turn_reap";

/// Attention-item kind filed when the engine could not pull a completed
/// remote worker's structured-output artifact off its host (positive proof
/// per `host_adapter::CollectOutcome::Failed` — an invalid
/// descriptor, or an artifact whose copy into place failed). Distinct from
/// [`DRIVER_TERMINAL_ERROR_ATTENTION_KIND`]: the worker's own driver
/// reported nothing wrong here — the remote host has the result, the engine
/// simply could not retrieve it — so this tells the true story instead of
/// implying a provider-side failure that never happened.
pub const REMOTE_COLLECTION_FAILED_ATTENTION_KIND: &str = "remote_collection_failed";

/// `work_attention_items.kind` filed when a `ready` execution has been
/// repeatedly deferred behind the SAME `chain_serialized` live sibling for
/// at least `CHAIN_SERIALIZED_STALL_THRESHOLD_SECS` — the 2026-07-09
/// chain-serialization re-defer stall incident:
/// the dispatcher re-evaluated and
/// re-deferred every ~10s for ~20 minutes with zero durable, user-visible
/// signal, so a human only noticed by grepping `engine-trace.jsonl`. Filed
/// once per stall (idempotent via `WorkDb::upsert_work_item_attention`)
/// and resolved via `WorkDb::resolve_external_tracker_attention` the
/// moment the row is no longer chain-serialized (dispatched, or the sibling
/// was reconciled dead — see `ExecutionCoordinator::live_chain_siblings`).
pub const CHAIN_SERIALIZED_STALL_ATTENTION_KIND: &str = "chain_serialized_stall";

/// Filed against a run when spawn or spawn confirmation failed (tmux creation
/// failure, viewer rejection, prompt composition error, composer never ready,
/// or no turn-start evidence). The pane may have come up and then been reaped.
/// See `attention_lifecycle::ATTENTION_LIFECYCLES` for its clearing
/// rule: `ClearedBy::WorkResumed`, since a later spawn completing for the item
/// is direct evidence the pane-spawn problem is no longer blocking it.
pub const PANE_SPAWN_FAILED_ATTENTION_KIND: &str = "pane_spawn_failed";

/// Work-item-scoped attention kind, filed against the question comment
/// (a `work_comments` id, not the execution), for an answer agent waiting
/// beyond `ANSWER_AGENT_READY_AGE_ALARM_THRESHOLD`. Filed against the
/// comment rather than the execution because the execution the item is
/// about is the one whose *next run start* must clear it, and
/// `work_resumed_evidence()` excludes execution-scoped rows from that
/// clearing path — see `work::WorkDb::list_attention_items_for_work_item`
/// for the matching read-side comment-id fallback.
pub const ANSWER_AGENT_READY_AGE_ATTENTION_KIND: &str = "answer_agent_ready_age";

/// `kind` of the attention item this sweep files on an overrun. One kind
/// keeps the surface simple; the overrun specifics live in the title/body.
pub const ENVELOPE_OVERRUN_ATTENTION_KIND: &str = "envelope_overrun";

/// Attention kind raised when a finished PR's execution left preserved work
/// that nothing the engine recorded can vouch for.
pub const EXECUTION_BOOKMARK_PRUNE_UNPROVEN_ATTENTION_KIND: &str = "execution_bookmark_prune_unproven";

pub const EXECUTION_BOOKMARK_RECOVERY_FAILED_ATTENTION_KIND: &str = "execution_bookmark_recovery_failed";

pub const REVISION_CHAIN_RECOVERY_BLOCKED_ATTENTION_KIND: &str = "revision_chain_recovery_blocked";

pub const EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND: &str = "execution_bookmark_create_failed";

/// The engine retired a recovery record whose baseline was gone and whose
/// heads nothing it recorded could vouch for, and started the item fresh.
pub const EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND: &str = "execution_bookmark_recovery_retired";

pub const HUNG_WORKER_ATTENTION_KIND: &str = "hung_worker";

/// Attention-item `kind` filed when the mass-retirement circuit breaker
/// holds panes back. Distinct kind so the Notifications UI can surface
/// "the pool is wedged and the engine is refusing to unwedge it" apart
/// from the per-worker recovery escalations
/// `transient_recovery` files, and so repeated passes dedupe
/// against the already-open item instead of piling up one row a minute.
pub const HUSK_BREAKER_ATTENTION_KIND: &str = "husk_retirement_breaker_tripped";

pub const LOCAL_WORKER_STARTUP_QUARANTINE_ATTENTION_KIND: &str = "local_worker_startup_quarantine";

pub const POOL_CLAIM_TEARDOWN_ATTENTION_KIND: &str = "pool_claim_teardown_pending";

/// `work_attention_items.kind` for a filed proposal-channel-error
/// attention. Mirrors `deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND`'s
/// pattern of one constant per marker-class attention kind.
pub const PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND: &str = "proposal_channel_error";

/// `work_attention_items.kind` for a remote worker the reconciler found
/// dead. Declared in `attention_lifecycle::ATTENTION_LIFECYCLES`.
pub const REMOTE_WORKER_DIED_ATTENTION_KIND: &str = "remote_worker_died";

/// Kind string for the attention item raised when a spawn produced a
/// pane but no driver. Stable — operator tooling pins it.
pub const DRIVER_START_ATTENTION_KIND: &str = "worker_driver_never_started";

/// Kind string for the attention item raised when the liveness veto's
/// answer has stayed `transcript_liveness::TranscriptLiveness::Undeterminable`
/// for `UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD` consecutive passes.
/// Stable — operator tooling pins it.
pub const LIVENESS_UNDETERMINABLE_ATTENTION_KIND: &str = "worker_liveness_undeterminable";

/// Kind string for the single loud attention item raised when the breaker
/// trips. Stable — external tooling and the app's attention pane pin it.
pub const SPAWN_CAPABILITY_ATTENTION_KIND: &str = "app_spawn_capability_unhealthy";

/// Engine-owned attention kind for a worker that is alive but has stopped
/// making observable terminal progress.
pub const STALE_WORKER_ATTENTION_KIND: &str = "stale_worker";

/// `work_attention_items.kind` filed when `refuse_and_reap` reaps a
/// version-skewed session. Registered in `attention_lifecycle` as
/// `attention_lifecycle::ClearedBy::WorkResumed` — a later run
/// starting on the same work item is direct evidence the item is moving
/// again.
pub const TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND: &str = "tmux_adoption_schema_skew";

/// `work_attention_items.kind` filed when live sessions remain on the
/// pre-move `tmux -L boss` server after the engine has switched to an
/// explicit socket. Cleared when a later run starts on the same item.
pub const TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND: &str = "tmux_legacy_label_server";

/// Engine-owned `work_attention_items.kind` for a filed `[effort-escalation]` signal.
pub const WORKER_ESCALATION_ATTENTION_KIND: &str = "worker_escalation";

/// Engine-owned `work_attention_items.kind` for a filed `[blocked]` signal.
pub const WORKER_BLOCKED_ATTENTION_KIND: &str = "worker_blocked";

/// Attention kind filed when a lease is kept because a gate failed.
pub const LEASE_UNRELEASED_ATTENTION_KIND: &str = "workspace_lease_unreleased";

/// Attention kind filed when every gate passed but cube's own release call
/// failed. It is also the durable evidence that the run had settled, which
/// is what lets the leftover-lease sweep retry a row that recorded no pid.
pub const LEASE_RELEASE_FAILED_ATTENTION_KIND: &str = "workspace_lease_release_failed";
