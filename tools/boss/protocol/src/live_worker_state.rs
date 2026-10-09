//! Live per-slot worker state, derived from hook events delivered to
//! the engine's events socket.
//!
//! [`LiveWorkerState`] is the source of truth for "what is the worker
//! in slot N doing right now". It is keyed by slot rather than by run
//! id because run records finalise quickly after spawn (the spawn act
//! is the run; the worker's *life* is what `LiveWorkerState` models),
//! and because the slot is the durable identifier the UI cares about
//! — a slot persists across the run-record finalisation and is what
//! the kanban Doing icon and the per-pane titlebar pill bind to.
//!
//! The activity values mirror the lifecycle hook events:
//! `Spawning` is the initial state set by the engine spawn flow before
//! any hook has fired; `SessionStart(Startup)` leaves `Spawning` for
//! `Idle` and stamps the authoritative model when the hook payload
//! carries one. `SessionStart(Resume)` stamps model + `last_event_at`
//! without leaving `Spawning` (reattach proof-of-life); a stale-activity
//! timer then downgrades `Spawning` → `Idle` once `last_event_at` ages
//! out so a degraded events stream cannot leave the slot advertising
//! "spawning" forever. Once the session is up, activity flip-flops
//! between `Working` (PreToolUse → PostToolUse) and `Idle` (Stop with
//! no pending probe / notification). `WaitingForInput` is set when a
//! `Notification` immediately precedes a `Stop`, indicating claude is
//! paused on a permission prompt. `Errored` and `Terminated` are
//! terminal-ish — `SessionEnd` moves the slot to `Terminated` and the
//! engine's slot allocator clears the entry on release.

use serde::{Deserialize, Serialize};

use crate::ExecutionKind;

/// Where a worker is in its life. The engine derives this from hook
/// events arriving on the events socket; UI code maps it to a colour
/// or icon variant. Order is roughly "earlier in the lifecycle" →
/// "later", but the type is not totally ordered — `Idle` and
/// `WaitingForInput` may alternate as the worker runs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerActivity {
    /// Engine has asked the app to allocate a pane and started the
    /// shell, but no `SessionStart` hook has fired yet. The default
    /// state stamped at spawn time.
    Spawning,
    /// Most recent event was `PreToolUse` (without a balancing
    /// `PostToolUse`). The worker is mid-tool-call.
    Working,
    /// `Notification` (or a Stop while a notification was pending) —
    /// claude is awaiting a permission prompt or user redirect. The
    /// kanban Doing icon should signal that the human's attention is
    /// needed.
    WaitingForInput,
    /// `Stop` with no pending probe and no preceding notification.
    /// The worker is between turns, alive but not currently doing
    /// work.
    Idle,
    /// Engine logged an error reading from the worker (malformed hook
    /// payload, repeated socket failure). The slot is still
    /// allocated; the human likely needs to look at logs.
    Errored,
    /// `SessionEnd` fired or the engine released the pane. The entry
    /// is kept around until the slot is reused so callers see the
    /// final state.
    Terminated,
}

impl WorkerActivity {
    /// True iff the activity indicates the worker is no longer attached
    /// to its slot — `Terminated` because it exited, `Errored` because
    /// the events socket gave up on it. The remaining activity values
    /// (`Spawning`, `Working`, `WaitingForInput`, `Idle`) all describe
    /// a live, slot-holding worker.
    pub fn is_terminal(&self) -> bool {
        matches!(self, WorkerActivity::Terminated | WorkerActivity::Errored)
    }

    /// True iff the pane's foreground worker is **parked at its prompt**:
    /// [`Self::Idle`] (between turns) or [`Self::WaitingForInput`] (on a
    /// permission prompt / human redirect). In those postures a typed /
    /// pane write becomes the worker's next prompt under any driver,
    /// so this is the driver-independent floor for pane injection.
    ///
    /// This is **not** the whole injection decision, and callers must not use
    /// it as one. Pre-session [`Self::Spawning`] and the terminal states are
    /// never injectable. Mid-turn [`Self::Working`] depends on the driver: an
    /// interactive-TUI driver (Claude Code, Codex's bare TUI) reads stdin
    /// continuously and holds mid-turn input in its composer, whereas a
    /// foreground process that never reads stdin leaves bytes written
    /// mid-turn lingering in the tty buffer, to be executed by the
    /// interactive shell once it exits (ghostty-codex-pane-viability, Q2
    /// Layer D, measured against the retired `codex exec` shape). The trait
    /// default is the latter, so a driver holds mid-turn input only once it
    /// has measured that it does.
    ///
    /// A driver that holds it may still differ in *when* it acts on it —
    /// Codex folds the buffered prompt into the running turn, Claude starts a
    /// fresh one — which does not change injectability but does mean nothing
    /// may assume one turn boundary per delivered prompt.
    ///
    /// Treating that difference as a property of activity alone is what made
    /// mid-turn probe delivery structurally impossible: the tool-boundary
    /// path fires on `PostToolUse`, where the activity is `Working` by
    /// construction, so a parked-only predicate refuses every probe on every
    /// driver. The driver half of the decision is
    /// `boss_engine_driver::AgentDriver::mid_turn_pane_input`; the engine
    /// combines both in `PaneInputPosture`.
    pub fn accepts_typed_input(self) -> bool {
        matches!(self, WorkerActivity::Idle | WorkerActivity::WaitingForInput)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WorkerActivity::Spawning => "spawning",
            WorkerActivity::Working => "working",
            WorkerActivity::WaitingForInput => "waiting_for_input",
            WorkerActivity::Idle => "idle",
            WorkerActivity::Errored => "errored",
            WorkerActivity::Terminated => "terminated",
        }
    }
}

/// Identifies the work item a worker slot was dispatched against.
/// Stamped onto [`LiveWorkerState`] at spawn time so the coordinator
/// can resolve "the worker on chore X" without prompting the user
/// for a slot number — see `bossctl agents list` / `agents status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemBinding {
    /// `task_*` / `chore_*` id of the work item powering this run.
    pub work_item_id: String,
    /// Short human-readable name (the work item's `name` column),
    /// useful when the coordinator renders text output.
    pub work_item_name: String,
    /// `work_executions` row id powering this run. The engine
    /// currently uses the same value for `LiveWorkerState.run_id`,
    /// but exposing it under its semantic name keeps callers honest.
    pub execution_id: String,
}

/// Live runtime status for one allocated worker slot. The shape is
/// flat so it serializes cleanly into both the bossctl JSON output
/// and a frontend-socket push.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveWorkerState {
    pub slot_id: u8,
    /// Engine-assigned durable persona, independent of the capacity slot.
    /// Remote workers retain a display-only ` (Remote)` host qualifier.
    /// Constructors use execution identity until the engine supplies a lease.
    #[serde(default)]
    pub name: String,
    pub run_id: String,
    /// Model identifier the worker is running on, e.g. `opus` (the
    /// family alias the engine defaults to) or a full slug such as
    /// `claude-opus-4-7`. Initially the engine-launched default; once a
    /// `SessionStart` hook payload carries `model`, the reducer overwrites
    /// this with that authoritative value. Absent model on the hook
    /// (Codex stdout `thread.started`) leaves the launch default in place.
    pub model: String,
    /// Tmux pane pid (`#{pane_pid}`). `0` for remote workers or before
    /// the local pane pid has been registered.
    pub shell_pid: i32,
    /// ISO-8601 timestamp of the most recent hook event observed for
    /// this slot. Useful for staleness detection — a worker that has
    /// not emitted any hook in N minutes is likely wedged.
    pub last_event_at: Option<String>,
    /// Tool name in the most recent `PreToolUse` that has not been
    /// balanced by a `PostToolUse`. `None` while the worker is idle.
    pub current_tool: Option<String>,
    /// ISO-8601 timestamp of the most recent `PostToolUse`. Lets
    /// callers compute "tool runtime" or detect a wedged tool.
    pub last_tool_ended_at: Option<String>,
    pub activity: WorkerActivity,
    /// Free-text one-sentence description of what the worker is
    /// doing right now, generated on the engine side from a tail of
    /// the worker's transcript by a cheap summarizer model
    /// (see `engine/src/live_status.rs`). `None` while a slot is
    /// `Spawning`, has never been summarized, or has been idle long
    /// enough that the prior text would be misleading. The string is
    /// short (≤120 chars, single line) and intended for direct render
    /// — Doing-card subtitle, Agents-tab worker header subtitle.
    /// Sits alongside [`Self::activity`] rather than replacing it:
    /// the enum is still load-bearing for the kanban dot and gating.
    ///
    /// Always serialized (as `null` while unset) so JSON consumers can
    /// distinguish "engine doesn't know about this field" from "engine
    /// hasn't summarized this slot yet". `serde(default)` keeps the
    /// decode path tolerant of payloads from older engines that omit
    /// the key entirely.
    #[serde(default)]
    pub live_status: Option<String>,
    /// ISO-8601 timestamp of the most recent successful update to
    /// `live_status`. The UI uses this to dim/strike-through stale
    /// values; the engine uses it to drive the timer-floor cadence
    /// in `live_status::tick`. Always serialized (as `null` while
    /// unset) — see `live_status` for why.
    #[serde(default)]
    pub live_status_at: Option<String>,
    /// Set by the engine's transient-recovery sweep
    /// (`engine/src/transient_recovery.rs`) while this slot is being
    /// auto-recovered from a transient Claude API error (529/5xx/network),
    /// e.g. `"recovering from API error (attempt 2/5)"`. Distinct from
    /// [`Self::live_status`] — that field is owned by the summarizer loop,
    /// which clears it after `IDLE_CLEAR_AFTER` of continuous `Idle`
    /// activity; a wedged-on-error worker is idle for far longer than
    /// that grace period, so coupling the recovery banner to the same
    /// field would have it wiped before a human ever saw it. Cleared as
    /// soon as any hook event arrives for the slot (proof the worker
    /// picked the nudge back up), or when the slot is released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_status: Option<String>,
    /// Work item this run was dispatched against. `None` for spawns
    /// that happen outside the work-item dispatch path (today: tests
    /// and any future direct-launch flow that bypasses the work
    /// tables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// Attributed worker pool this run belongs to: `"main"`,
    /// `"automation"`, or `"review"`. Independent of which physical
    /// slot it occupies — automation work can spill into a main-pool
    /// Lower Decks slot and still reports `"automation"` here, matching
    /// [`crate::WorkerPoolEntry::name`] / the coordinator's
    /// attributed-pool label. `None` for spawns outside the work-item
    /// dispatch path (tests; any future direct-launch flow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Execution kind of the run powering this slot (e.g.
    /// `"task_implementation"`, `"automation_triage"`, `"pr_review"`),
    /// matching [`crate::ExecutionKind::as_str`]. Surfaced so
    /// `bossctl agents list` can show what kind of work a live pane is
    /// doing without a separate execution-table join. `None` for
    /// spawns outside the work-item dispatch path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// True while an operator has placed an explicit hold on this run via
    /// `bossctl agents hold` — exempting it from the idle-park and
    /// auto-reap sweeps until released or the run ends. Surfaced so
    /// `bossctl agents list`/`status` can show held workers distinctly
    /// from a normal `idle` row. `#[serde(default)]` keeps decode
    /// tolerant of payloads from older engines that omit the key.
    #[serde(default)]
    pub held: bool,
    /// `Some(true)` for a local worker hosted in a tmux session — stamped
    /// once, at spawn (or from the durable run row on re-adoption).
    /// `Some(false)` marks a local run whose durable tmux stamp is missing:
    /// an invariant failure that clients must surface as unavailable, never
    /// a supported hosting mode. `None` for remote workers, which have no
    /// local pane at all, or for payloads from an older engine that predates
    /// this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_hosted: Option<bool>,
    /// Badge type for the run (`"review"`, `"automation"`, `"design"`,
    /// `"coding"`, `"answer"`), derived by the engine from the execution
    /// kind via [`AgentType::for_execution`]. Kept a plain string on the
    /// wire so a client that meets a value it does not know can render an
    /// Unknown badge instead of failing to decode the whole snapshot —
    /// parse with [`AgentType::parse`]. `None` for spawns outside the
    /// work-item dispatch path and for payloads from an older engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Project the dispatched work item belongs to. `None` means the work
    /// is Unfiled (no project) — it is never guessed from the pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Display name of [`Self::project_id`]. `None` when the project is
    /// unfiled or its row could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// Host the run executes on: `"local"` for this machine, otherwise the
    /// registered remote host id. Explicit so membership never infers
    /// locality from slot ranges or a hosting-mode boolean. `None` only for
    /// spawns outside the dispatch path and for payloads from an older
    /// engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// ISO-8601 (UTC) time the execution started. Sorts lexicographically,
    /// so clients order by `(started_at, run_id)`. Re-adopted workers carry
    /// the original execution start, not the adoption time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

/// Host id of workers that run on this machine.
pub const LOCAL_HOST_ID: &str = "local";

/// Badge type of a live worker. A coarser view of [`ExecutionKind`] for
/// display and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentType {
    Review,
    Automation,
    Design,
    Coding,
    Answer,
}

impl AgentType {
    /// Wire value stamped on [`LiveWorkerState::agent_type`].
    pub fn as_str(self) -> &'static str {
        match self {
            AgentType::Review => "review",
            AgentType::Automation => "automation",
            AgentType::Design => "design",
            AgentType::Coding => "coding",
            AgentType::Answer => "answer",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for a value this build does not
    /// know, which callers surface as Unknown rather than dropping the row.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "review" => AgentType::Review,
            "automation" => AgentType::Automation,
            "design" => AgentType::Design,
            "coding" => AgentType::Coding,
            "answer" => AgentType::Answer,
            _ => return None,
        })
    }

    /// Map an execution to its badge type. The per-kind match is exhaustive
    /// on purpose: a new [`ExecutionKind`] fails to compile here until it is
    /// given a deliberate mapping.
    ///
    /// Precedence mirrors the engine's attributed-pool label: review kinds
    /// win over everything, then `automation_triage`, then any other kind
    /// whose work item was produced by an automation
    /// (`has_source_automation`), and only then the kind's own type.
    pub fn for_execution(kind: &ExecutionKind, has_source_automation: bool) -> Self {
        let by_kind = match kind {
            ExecutionKind::PrReview | ExecutionKind::PrReviewGuide => AgentType::Review,
            ExecutionKind::AutomationTriage => AgentType::Automation,
            ExecutionKind::ProjectDesign | ExecutionKind::ProductDesign => AgentType::Design,
            ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::RevisionImplementation
            | ExecutionKind::InvestigationImplementation
            | ExecutionKind::CiRemediation
            | ExecutionKind::ConflictResolution => AgentType::Coding,
            ExecutionKind::AnswerAgent => AgentType::Answer,
        };
        if has_source_automation && by_kind != AgentType::Review {
            AgentType::Automation
        } else {
            by_kind
        }
    }
}

/// Membership and ordering metadata the engine stamps on a [`LiveWorkerState`]
/// at spawn and again on adoption, carried alongside the pool/kind routing.
/// Fields mirror the like-named `LiveWorkerState` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveWorkerMetadata {
    pub agent_type: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub host_id: Option<String>,
    pub started_at: Option<String>,
}

impl LiveWorkerState {
    /// Stamp the engine-resolved [`LiveWorkerMetadata`] onto this state.
    pub fn apply_metadata(&mut self, metadata: LiveWorkerMetadata) {
        self.agent_type = metadata.agent_type;
        self.project_id = metadata.project_id;
        self.project_name = metadata.project_name;
        self.host_id = metadata.host_id;
        self.started_at = metadata.started_at;
    }
}

impl LiveWorkerState {
    /// Initial state for a freshly-spawned slot. Activity is
    /// `Spawning`; the model is whatever the engine launched the
    /// worker with (later replaced by the `SessionStart`-reported
    /// value once a hook arrives). `binding` is the work-item
    /// linkage for the run — pass `None` from call sites that don't
    /// have one (tests; future direct-launch). `pool` and `kind` are
    /// the attributed worker pool (`"main"` / `"automation"` /
    /// `"review"`) and the execution kind (`"task_implementation"`,
    /// …); production dispatch always passes both, tests may leave
    /// them `None`.
    pub fn new_spawning(
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
    ) -> Self {
        Self::new_spawning_with_routing(slot_id, run_id, model, shell_pid, binding, None, None)
    }

    /// Like [`Self::new_spawning`], but also stamps the attributed
    /// worker `pool` and execution `kind` that production dispatch
    /// knows at spawn time. The engine reducer's
    /// `register_spawn_with_capabilities` calls this so
    /// `bossctl agents list` can render pool + kind without joining
    /// the execution table.
    pub fn new_spawning_with_routing(
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
        pool: Option<String>,
        kind: Option<String>,
    ) -> Self {
        Self::new_spawning_with_routing_and_hosting(slot_id, run_id, model, shell_pid, binding, pool, kind, None)
    }

    /// Like [`Self::new_spawning_with_routing`], but also stamps
    /// [`Self::tmux_hosted`], resolved once at the spawn decision (or read
    /// from the durable run row on re-adoption).
    #[allow(clippy::too_many_arguments)]
    pub fn new_spawning_with_routing_and_hosting(
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
        pool: Option<String>,
        kind: Option<String>,
        tmux_hosted: Option<bool>,
    ) -> Self {
        let (work_item_id, work_item_name, execution_id) = match binding {
            Some(b) => (Some(b.work_item_id), Some(b.work_item_name), Some(b.execution_id)),
            None => (None, None, None),
        };
        let run_id = run_id.into();
        Self {
            slot_id,
            name: placeholder_worker_name(&run_id),
            run_id,
            model: model.into(),
            shell_pid,
            last_event_at: None,
            current_tool: None,
            last_tool_ended_at: None,
            activity: WorkerActivity::Spawning,
            live_status: None,
            live_status_at: None,
            recovery_status: None,
            work_item_id,
            work_item_name,
            execution_id,
            pool,
            kind,
            held: false,
            tmux_hosted,
            agent_type: None,
            project_id: None,
            project_name: None,
            host_id: None,
            started_at: None,
        }
    }
}

/// Topic published when any slot's [`LiveWorkerState`] changes.
/// Subscribers receive the whole snapshot via
/// [`crate::FrontendEvent::WorkerLiveStatesList`].
pub const TOPIC_WORKER_LIVE_STATES: &str = "worker.live_states";

/// The execution-identity display name used before a durable persona is
/// known. Single source of truth so callers can detect and repair it.
pub fn placeholder_worker_name(run_id: &str) -> String {
    format!("Worker {run_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_activity_round_trips_through_serde() {
        for activity in [
            WorkerActivity::Spawning,
            WorkerActivity::Working,
            WorkerActivity::WaitingForInput,
            WorkerActivity::Idle,
            WorkerActivity::Errored,
            WorkerActivity::Terminated,
        ] {
            let json = serde_json::to_string(&activity).unwrap();
            let parsed: WorkerActivity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, activity);
        }
    }

    #[test]
    fn worker_activity_serializes_as_snake_case() {
        let json = serde_json::to_string(&WorkerActivity::WaitingForInput).unwrap();
        assert_eq!(json, "\"waiting_for_input\"");
    }

    #[test]
    fn accepts_typed_input_only_when_parked_at_prompt() {
        assert!(WorkerActivity::Idle.accepts_typed_input());
        assert!(WorkerActivity::WaitingForInput.accepts_typed_input());
        assert!(!WorkerActivity::Spawning.accepts_typed_input());
        assert!(!WorkerActivity::Working.accepts_typed_input());
        assert!(!WorkerActivity::Errored.accepts_typed_input());
        assert!(!WorkerActivity::Terminated.accepts_typed_input());
    }

    #[test]
    fn new_spawning_sets_defaults() {
        let state = LiveWorkerState::new_spawning(3, "run-1", "claude-opus-4-7", 42, None);
        assert_eq!(state.slot_id, 3);
        assert_eq!(state.name, "Worker run-1");
        assert_eq!(state.run_id, "run-1");
        assert_eq!(state.model, "claude-opus-4-7");
        assert_eq!(state.shell_pid, 42);
        assert_eq!(state.activity, WorkerActivity::Spawning);
        assert!(state.current_tool.is_none());
        assert!(state.last_event_at.is_none());
        assert!(state.last_tool_ended_at.is_none());
        assert!(state.work_item_id.is_none());
        assert!(state.work_item_name.is_none());
        assert!(state.execution_id.is_none());
        assert!(state.pool.is_none());
        assert!(state.kind.is_none());
        assert!(state.live_status.is_none());
        assert!(state.live_status_at.is_none());
        assert!(state.recovery_status.is_none());
        assert!(!state.held);
    }

    #[test]
    fn new_spawning_with_routing_stamps_pool_and_kind() {
        let state = LiveWorkerState::new_spawning_with_routing(
            2,
            "exec-9",
            "claude-opus-4-7",
            0,
            Some(WorkItemBinding {
                work_item_id: "task_abc".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-9".into(),
            }),
            Some("automation".into()),
            Some("chore_implementation".into()),
        );
        assert_eq!(state.pool.as_deref(), Some("automation"));
        assert_eq!(state.kind.as_deref(), Some("chore_implementation"));
        assert_eq!(state.work_item_id.as_deref(), Some("task_abc"));
    }

    #[test]
    fn held_defaults_false_when_key_is_absent_from_json() {
        // Decode tolerance for payloads from older engines that predate
        // the `held` field.
        let json = r#"{
            "slot_id": 1, "name": "Riker", "run_id": "run-1", "model": "opus",
            "shell_pid": 0, "last_event_at": null, "current_tool": null,
            "last_tool_ended_at": null, "activity": "idle",
            "live_status": null, "live_status_at": null
        }"#;
        let parsed: LiveWorkerState = serde_json::from_str(json).unwrap();
        assert!(!parsed.held);
    }

    #[test]
    fn new_spawning_placeholder_tracks_identity_instead_of_slot() {
        let s1 = LiveWorkerState::new_spawning(1, "r", "m", 0, None);
        assert_eq!(s1.name, "Worker r");
        let s2 = LiveWorkerState::new_spawning(2, "r", "m", 0, None);
        assert_eq!(s2.name, s1.name);
        let s8 = LiveWorkerState::new_spawning(8, "r", "m", 0, None);
        assert_eq!(s8.name, s1.name);
    }

    #[test]
    fn new_spawning_with_binding_carries_work_item_fields() {
        let state = LiveWorkerState::new_spawning(
            2,
            "exec-9",
            "claude-opus-4-7",
            0,
            Some(WorkItemBinding {
                work_item_id: "task_abc".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-9".into(),
            }),
        );
        assert_eq!(state.work_item_id.as_deref(), Some("task_abc"));
        assert_eq!(state.work_item_name.as_deref(), Some("Fix fencer scraping"));
        assert_eq!(state.execution_id.as_deref(), Some("exec-9"));
    }

    #[test]
    fn live_worker_state_round_trips() {
        let original = LiveWorkerState {
            slot_id: 1,
            name: "Riker".into(),
            run_id: "run-7".into(),
            model: "claude-sonnet-4-6".into(),
            shell_pid: 12345,
            last_event_at: Some("2026-05-06T12:00:00Z".into()),
            current_tool: Some("Bash".into()),
            last_tool_ended_at: Some("2026-05-06T11:59:50Z".into()),
            activity: WorkerActivity::Working,
            live_status: Some("investigating why the scroll handler doesn't fire".into()),
            live_status_at: Some("2026-05-06T12:00:01Z".into()),
            recovery_status: None,
            work_item_id: Some("task_42".into()),
            work_item_name: Some("Fix fencer scraping".into()),
            execution_id: Some("run-7".into()),
            pool: Some("main".into()),
            kind: Some("task_implementation".into()),
            held: false,
            tmux_hosted: Some(true),
            agent_type: Some("coding".into()),
            project_id: Some("proj_1".into()),
            project_name: Some("Dynamic Agents pane layout".into()),
            host_id: Some("local".into()),
            started_at: Some("2026-05-06T11:58:00Z".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: LiveWorkerState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn live_worker_state_omits_unbound_fields_when_serializing() {
        let state = LiveWorkerState::new_spawning(1, "exec-1", "claude-opus-4-7", 0, None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("work_item_id"), "json: {json}");
        assert!(!json.contains("work_item_name"), "json: {json}");
        assert!(!json.contains("execution_id"), "json: {json}");
        assert!(!json.contains("\"pool\""), "json: {json}");
        assert!(!json.contains("\"kind\""), "json: {json}");
    }

    #[test]
    fn live_worker_state_always_serializes_live_status_fields() {
        // The kanban Doing-card and Agents-tab header bind to
        // `live_status` / `live_status_at` and treat "key absent" as a
        // protocol-version signal rather than a missing summary. Always
        // emitting the keys (even when `None` serializes as `null`)
        // keeps `bossctl agents list --json` self-describing — a JSON
        // consumer can distinguish "the engine doesn't ship live_status"
        // from "this worker hasn't been summarized yet".
        let state = LiveWorkerState::new_spawning(1, "exec-1", "claude-opus-4-7", 0, None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"live_status\":null"), "json: {json}");
        assert!(json.contains("\"live_status_at\":null"), "json: {json}");
    }

    #[test]
    fn live_worker_state_round_trip_includes_live_status_fields() {
        let original = LiveWorkerState {
            slot_id: 4,
            name: "Worf".into(),
            run_id: "run-9".into(),
            model: "claude-opus-4-7".into(),
            shell_pid: 0,
            last_event_at: Some("2026-05-06T12:00:00Z".into()),
            current_tool: None,
            last_tool_ended_at: None,
            activity: WorkerActivity::Working,
            live_status: Some("running tests after the layout fix".into()),
            live_status_at: Some("2026-05-06T12:00:30Z".into()),
            recovery_status: None,
            work_item_id: None,
            work_item_name: None,
            execution_id: None,
            pool: None,
            kind: None,
            held: false,
            tmux_hosted: None,
            agent_type: None,
            project_id: None,
            project_name: None,
            host_id: None,
            started_at: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: LiveWorkerState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
        // live_status_at travels alongside live_status. Sanity-check
        // both keys are on the wire when set.
        assert!(json.contains("\"live_status\":"), "json: {json}");
        assert!(json.contains("\"live_status_at\":"), "json: {json}");
    }

    #[test]
    fn live_worker_state_decodes_payload_without_live_status_fields() {
        // Older engines (and snapshot fixtures) won't carry the new
        // fields. Decoding must succeed with both options as `None`.
        let json = r#"{
            "slot_id": 1,
            "name": "Riker",
            "run_id": "exec-1",
            "model": "claude-opus-4-7",
            "shell_pid": 0,
            "activity": "working"
        }"#;
        let parsed: LiveWorkerState = serde_json::from_str(json).unwrap();
        assert!(parsed.live_status.is_none());
        assert!(parsed.live_status_at.is_none());
        assert!(parsed.pool.is_none());
        assert!(parsed.kind.is_none());
        assert!(parsed.tmux_hosted.is_none());
    }

    #[test]
    fn new_spawning_with_routing_and_hosting_stamps_tmux_hosted() {
        let state = LiveWorkerState::new_spawning_with_routing_and_hosting(
            2,
            "exec-9",
            "claude-opus-4-7",
            0,
            None,
            Some("main".into()),
            Some("task_implementation".into()),
            Some(false),
        );
        assert_eq!(state.tmux_hosted, Some(false));
    }

    #[test]
    fn live_worker_state_omits_tmux_hosted_when_unset() {
        let state = LiveWorkerState::new_spawning(1, "exec-1", "claude-opus-4-7", 0, None);
        assert!(state.tmux_hosted.is_none());
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("tmux_hosted"), "json: {json}");
    }
    /// Every execution kind, with the badge type it must map to when the
    /// work item has no automation source.
    const KIND_TYPES: [(&str, AgentType); 12] = [
        ("answer_agent", AgentType::Answer),
        ("automation_triage", AgentType::Automation),
        ("chore_implementation", AgentType::Coding),
        ("ci_remediation", AgentType::Coding),
        ("conflict_resolution", AgentType::Coding),
        ("investigation_implementation", AgentType::Coding),
        ("pr_review", AgentType::Review),
        ("pr_review_guide", AgentType::Review),
        ("product_design", AgentType::Design),
        ("project_design", AgentType::Design),
        ("revision_implementation", AgentType::Coding),
        ("task_implementation", AgentType::Coding),
    ];

    #[test]
    fn agent_type_maps_all_twelve_execution_kinds() {
        for (wire, expected) in KIND_TYPES {
            let kind: ExecutionKind = wire.parse().unwrap();
            assert_eq!(AgentType::for_execution(&kind, false), expected, "{wire}");
        }
    }

    #[test]
    fn agent_type_automation_source_overrides_all_but_review() {
        for (wire, plain) in KIND_TYPES {
            let kind: ExecutionKind = wire.parse().unwrap();
            let expected = if plain == AgentType::Review {
                AgentType::Review
            } else {
                AgentType::Automation
            };
            assert_eq!(AgentType::for_execution(&kind, true), expected, "{wire}");
        }
    }

    #[test]
    fn agent_type_wire_values_round_trip_and_unknown_does_not_parse() {
        for (_, ty) in KIND_TYPES {
            assert_eq!(AgentType::parse(ty.as_str()), Some(ty));
        }
        assert_eq!(AgentType::parse("hologram"), None);
    }

    #[test]
    fn metadata_fields_default_absent_and_are_omitted_from_json() {
        let state = LiveWorkerState::new_spawning(1, "exec-1", "claude-opus-4-7", 0, None);
        let json = serde_json::to_string(&state).unwrap();
        for key in ["agent_type", "project_id", "project_name", "host_id", "started_at"] {
            assert!(!json.contains(key), "{key} leaked into {json}");
        }
        let old: LiveWorkerState = serde_json::from_str(
            r#"{"slot_id":1,"name":"Riker","run_id":"r","model":"opus","shell_pid":0,"activity":"idle"}"#,
        )
        .unwrap();
        assert_eq!(old.agent_type, None);
        assert_eq!(old.host_id, None);
        assert_eq!(old.started_at, None);
    }

    #[test]
    fn apply_metadata_stamps_every_field() {
        let mut state = LiveWorkerState::new_spawning(1, "exec-1", "m", 0, None);
        state.apply_metadata(LiveWorkerMetadata {
            agent_type: Some("review".into()),
            project_id: Some("proj_9".into()),
            project_name: Some("Nine".into()),
            host_id: Some(LOCAL_HOST_ID.into()),
            started_at: Some("2026-10-09T00:00:00Z".into()),
        });
        assert_eq!(state.agent_type.as_deref(), Some("review"));
        assert_eq!(state.project_id.as_deref(), Some("proj_9"));
        assert_eq!(state.project_name.as_deref(), Some("Nine"));
        assert_eq!(state.host_id.as_deref(), Some("local"));
        assert_eq!(state.started_at.as_deref(), Some("2026-10-09T00:00:00Z"));
    }
}
