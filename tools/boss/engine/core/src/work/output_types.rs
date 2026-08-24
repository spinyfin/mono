use super::*;

/// Where the chore should land after [`WorkDb::record_worker_pr_completion`].
/// `InReview` is the typical case (open PR, ready for human review);
/// `Done` is used when the PR was already merged at the time the
/// worker's Stop event fired, so we skip the review column entirely.
/// `PendingReview` (P992) is used when an independent reviewer pass
/// is enqueued: the task's `pr_url` is stamped but its status is *not*
/// advanced — the task stays in the Doing column until the reviewer resolves
/// (or the fallback timeout fires).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPrCompletionTarget {
    InReview,
    Done,
    /// Task `pr_url` is stamped; task `status` is unchanged. The independent
    /// reviewer pass (P992) drives the subsequent `active → in_review`
    /// transition once the review pass resolves (or the timeout fires).
    PendingReview,
    /// incident-002 P2: the both-parents deletion tripwire fired — this PR
    /// resolves a merge/forward-port that removed a surface a merged parent
    /// added. Halt auto-progression: the task lands in `blocked` with
    /// `blocked_reason = 'deletion_signoff'` and `pr_url` stamped, pending an
    /// explicit operator sign-off (a human status move back to `in_review`).
    /// No `task_blocked_signals` row is armed, so the merge poller's auto-clear
    /// paths (which only probe `merge_conflict` / `ci_failure`) never retire it.
    BlockedDeletionSignoff,
}

/// Outcome of [`WorkDb::set_run_transcript_path_if_unset`]. The third
/// variant exists to keep "the latest run for this execution already
/// has a transcript_path" (legitimate no-op) distinguishable from
/// "no `work_runs` row exists for this execution yet" (real problem,
/// either a startup race or a wrong-namespace identifier). Returning
/// a flat `bool` from this call is what hid the 2026-05-12 bug:
/// every hook delivery silently looked like an already-set no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRunTranscriptPathOutcome {
    Updated,
    AlreadySet,
    RowMissing,
}

/// One detached remote run returned by
/// [`WorkDb::list_live_remote_runs`]: the latest `work_runs` row on a
/// non-local host whose execution is still non-terminal. The engine's
/// startup reattach pass (see [`crate::remote_reattach`])
/// re-establishes the reverse events-socket forward for each of these
/// so the still-running worker's hook stream reaches the new engine,
/// and [`crate::remote_lease_reconcile`] probes each one's worker pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRunHandle {
    /// `work_runs.id` (`run_*`).
    pub run_id: String,
    /// `work_runs.execution_id` (`exec_*`) — also the worker's
    /// `BOSS_RUN_ID` and the key for the remote events socket path.
    pub execution_id: String,
    /// The host the run was dispatched to (never `'local'`).
    pub host_id: String,
    /// Remote worker pid captured at spawn, if the wrapper handshake
    /// reported one. Informational for signal addressing; reattach of
    /// the events forward does not depend on it.
    pub remote_pid: Option<i64>,
}

/// One local, non-terminal worker run carrying the durable identity of a tmux
/// session. [`WorkDb::list_adoptable_tmux_runs`] returns these rows for the
/// boot-time adoption pass; callers must match sessions only by the full
/// opaque `tmux_spawn_token`, never by the human-readable session name.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct TmuxRunHandle {
    /// `work_runs.id` (`run_*`).
    pub run_id: String,
    /// `work_runs.execution_id` (`exec_*`).
    pub execution_id: String,
    /// Pool worker identity used to reclaim the recorded slot.
    pub agent_id: String,
    /// Transcript to resume after re-adopting the live worker, if captured.
    pub transcript_path: Option<String>,
    /// Tmux server identity recorded when the session was created: the
    /// absolute `-S` socket path, or the literal `boss` label for a session
    /// that still lives on the pre-move `-L boss` server.
    pub tmux_server_label: String,
    /// Human-readable tmux session name. Not an adoption key.
    pub tmux_session_name: String,
    /// Opaque, unique token used for exact-match adoption.
    pub tmux_spawn_token: String,
    /// Durable spawn phase: `intended` before creation, `created` after it.
    pub tmux_spawn_state: String,
    /// `#{pane_pid}` observed after tmux created the initial pane.
    pub tmux_pane_pid: Option<i64>,
}

/// A tmux-hosted run's durable identity as needed for token-verified
/// teardown ([`WorkDb::tmux_identity_for_execution`]). Unlike
/// [`TmuxRunHandle`], reading this applies no status filtering — teardown
/// must find a run's tmux identity even after its execution has already
/// gone terminal, which is precisely when most teardown calls happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxIdentity {
    /// Tmux server identity recorded when the session was created: the
    /// absolute `-S` socket path, or the literal [`boss_tmux::SERVER_LABEL`]
    /// for a session that still lives on the pre-move `-L boss` server. Set
    /// atomically alongside `session_name`/`spawn_token`, so it is always
    /// present whenever those are.
    pub server_label: String,
    /// Human-readable tmux session name. Not an adoption/teardown key on
    /// its own — see [`TmuxRunHandle::tmux_session_name`].
    pub session_name: String,
    /// Opaque, unique token a session's live `BOSS_SPAWN_TOKEN` must match
    /// exactly before teardown may touch it.
    pub spawn_token: String,
    /// `#{pane_pid}` recorded when tmux created the session's initial pane.
    pub pane_pid: Option<i64>,
}

/// One non-terminal execution whose latest run landed on a host that is
/// no longer eligible to run it — the host was disabled (operator
/// `bossctl hosts disable` or the dispatch-health circuit breaker) or
/// removed from the registry entirely. Returned by
/// [`WorkDb::list_nonterminal_executions_on_offline_hosts`] and consumed
/// by the host-reconcile sweep ([`crate::host_reconcile`]), which
/// terminalizes the execution, releases its cube lease, and lets the
/// existing orphan→redispatch machinery re-place the work item on a
/// still-eligible host. `host_id` is never `'local'` (a local run is
/// judged by the local-filesystem sweeps, not host state).
#[derive(Debug, Clone)]
pub struct HostBoundExecution {
    pub execution: WorkExecution,
    /// The offline host the latest run was attributed to (`work_runs.host_id`).
    pub host_id: String,
    /// `work_runs.id` of that latest run.
    pub run_id: String,
}

/// Result of a successful [`WorkDb::record_worker_pr_completion`] call (also
/// reused by [`WorkDb::record_worker_no_op_completion`] and
/// [`WorkDb::record_worker_idle_abandonment`], which finalize an execution
/// the same lease/pane-releasing way without a fresh PR). Carries the cube
/// lease/workspace ids that were attached to the execution so the caller can
/// drive cube release out-of-band.
#[derive(Debug, Clone)]
pub struct WorkerPrCompletion {
    pub execution: WorkExecution,
    pub work_item: WorkItem,
    pub released_lease_id: Option<String>,
    pub released_workspace_id: Option<String>,
}

/// Result of a successful [`WorkDb::record_worker_idle_abandonment`] call.
/// Distinct from [`WorkerPrCompletion`] because the idle-abandon path must
/// free the execution's lease/pane even when the task/chore row has been
/// hard-deleted out from under a still-live execution — `work_item` is
/// `None` in that case so the caller can skip the work-item-changed publish
/// instead of failing the whole finalize and leaking the lease/pane.
#[derive(Debug, Clone)]
pub struct IdleAbandonmentCompletion {
    pub execution: WorkExecution,
    pub work_item: Option<WorkItem>,
    pub released_lease_id: Option<String>,
    pub released_workspace_id: Option<String>,
}

/// One row from [`WorkDb::list_chores_pending_merge_check`]: a chore
/// or project_task the merge poller still needs to ask GitHub about.
#[derive(Debug, Clone)]
pub struct PendingMergeCheck {
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
}

impl WorkDb {
    /// Shared body of every merge-poller candidate list
    /// ([`Self::list_chores_pending_merge_check`],
    /// [`Self::list_chores_blocked_on_merge_conflict`],
    /// [`Self::list_chores_blocked_on_ci_failure`],
    /// [`Self::list_chores_stranded_blocked_remediation`]). Those queries
    /// differ only in their state predicate; everything else — the
    /// chore-like kind filter, the bound-PR and soft-delete guards, the
    /// projection, the row mapping, and the oldest-first ordering — is
    /// identical, so it lives here.
    ///
    /// `predicate_sql` is spliced into the `WHERE` chain as an additional
    /// `AND` term. The `tasks` table is aliased `t`, so a predicate may
    /// reference `t.<column>` and may carry correlated subqueries keyed on
    /// `t.id`. It is a caller-supplied SQL literal, never user input.
    pub(super) fn query_pending_merge_checks(&self, predicate_sql: &str) -> Result<Vec<PendingMergeCheck>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT t.id, t.product_id, t.pr_url
             FROM tasks t
             WHERE t.kind IN ({CHORE_LIKE_KINDS_SQL})
               AND {predicate_sql}
               AND t.pr_url IS NOT NULL
               AND t.pr_url != ''
               AND t.deleted_at IS NULL
             ORDER BY t.updated_at ASC",
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingMergeCheck {
                work_item_id: row.get(0)?,
                product_id: row.get(1)?,
                pr_url: row.get(2)?,
            })
        })?;
        collect_rows(rows)
    }
}

/// One row from [`WorkDb::list_recently_terminal_executions_pending_pr_detection`]:
/// a terminal execution whose task is still `active` with no `pr_url`. The merge
/// poller's late-PR sweep uses this to recover chores that were orphan-swept
/// while their worker pane was still running (double-spawn race — Bug B).
#[derive(Debug, Clone)]
pub struct LatePrCandidate {
    pub execution_id: String,
    pub work_item_id: String,
    pub repo_remote_url: String,
    /// Branch-naming strategy snapshotted from the product's
    /// `editorial_rules.branch_naming` at execution spawn time. Carried so
    /// the late-PR sweep reconstructs the correct expected branch name via
    /// [`crate::completion::expected_branch_name`]. Defaults to
    /// [`BranchNaming::BossExecPrefix`] for rows created before this column
    /// existed (i.e. `NULL` in the DB).
    pub branch_naming: BranchNaming,
    /// Worker branch-name prefix snapshotted from the product's
    /// `worker_branch_prefix` column at execution spawn time. Carried
    /// alongside `branch_naming` so the late-PR sweep reconstructs the
    /// exact branch name via [`crate::completion::expected_branch_name`]
    /// — under the default `BossExecPrefix` strategy this is what turns
    /// `boss/exec_<id>` into the product's configured `<prefix>exec_<id>`.
    /// `None` → the engine default `boss/`.
    pub worker_branch_prefix: Option<String>,
}

/// Raw external-ref data as stored in the `tasks` table. Returned by
/// [`WorkDb::list_external_refs_for_product`]. The `web_url` field present
/// on [`WorkItemExternalRef`] is tracker-specific and is derived by the
/// reconciler layer; the DB layer does not compute it.
#[derive(Debug, Clone)]
pub struct StoredExternalRef {
    pub kind: String,
    pub canonical_id: String,
    pub raw: serde_json::Value,
    pub synced_at: Option<String>,
    pub unbound_at: Option<String>,
}

/// A `ci_remediations` row that is `pending` but has no live execution
/// (`kind='ci_remediation'` with status in `'ready'`, `'running'`, or
/// `'waiting_human'`). This arises when two merge-queue dequeue events
/// arrive in the same sweep: the first flips the task to
/// `blocked: ci_failure` (consuming the `status='in_review'` WHERE
/// guard) and the second inserts its own `ci_remediations` row but
/// cannot flip the task again — leaving the row orphaned with no
/// executor. The merge poller's stranded-attempt sweep rescues these
/// by re-emitting a fresh execution request so a worker is dispatched
/// without waiting for the task to return to `in_review`.
#[derive(Debug, Clone)]
pub struct StrandedCiRemediationAttempt {
    pub attempt_id: String,
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
}

/// One task an automation is already tracking, as shown to its triage
/// agent in [`crate::automation_triage::render_triage_preamble`].
///
/// Deliberately minimal: the agent needs to recognise "I was about to
/// file this" and cite the existing row, nothing more. Full task bodies
/// would crowd out the standing instruction the agent is there to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationSiblingTask {
    /// Friendly `T<n>` number — what the agent cites in its skip marker.
    pub short_id: i64,
    pub name: String,
    /// Raw status string (`todo`, `active`, `in_review`, `done`, …), so
    /// the agent can tell "someone is on it" from "this was fixed and
    /// has come back".
    pub status: String,
    /// Present once a worker has opened a PR — the strongest signal that
    /// the finding is genuinely in hand.
    pub pr_url: Option<String>,
}

/// One row from `automation_dedup_suppressions`: a task the dedup gate
/// refused to create because an open sibling already tracked the finding.
///
/// The gate's effect is an absence, which is exactly what an operator
/// cannot see. This is how "why has A3 filed nothing all week?" gets an
/// answer, and how over-suppression would be caught — a burst of rows all
/// pointing at one surviving task, matched on `normalized_title`, is the
/// shape of a gate that has become too eager.
///
/// Defined in `boss_protocol` (not here) so it can travel over the wire as
/// a `ListAutomationDedupSuppressions` / `AutomationDedupSuppressionsList`
/// request/event pair, exactly like [`boss_protocol::AutomationRun`]. Built
/// by the mapper in `work/automations.rs`, which sets every column
/// explicitly from a struct literal so an unmapped new column is a compile
/// error.
pub use boss_protocol::AutomationDedupSuppression;
