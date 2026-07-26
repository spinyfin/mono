//! Agent-driver abstraction: the capability-oriented interface between Boss
//! and the coding-agent CLI it drives.
//!
//! See `tools/boss/docs/designs/agent-driver-abstraction-*.md` for the full
//! design (§Chosen approach, §Capabilities, §The absence-policy model).
//!
//! `boss_engine` re-exports this crate as `boss_engine::driver`, so engine
//! call sites continue to reach these items via `crate::driver::…`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{EffortLevel, NormalizeError, ReasoningMode, StopReason, TaskKind, WorkerEvent};

/// Worker posture for the [`Capability::PermissionPolicy`] capability's
/// deny-rule selection (reviewer read-only, triage no-work, answer-agent
/// allowlist).
///
/// A deliberate local duplicate of `boss_engine::worker_setup::WorkerKind`:
/// this crate has no dependency on `core` (only the reverse — `core` depends
/// on `driver`), so the two cannot share one definition today. Consolidate
/// once the settings/deny-rule rendering that `WorkerKind` selects between
/// also moves into this crate (tracked alongside
/// [`AgentDriver::write_permission_config`]'s real implementation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerKind {
    /// Normal implementation worker: write access to its workspace, may push
    /// branches and open PRs.
    #[default]
    Standard,
    /// Read-only reviewer worker (design §9).
    Reviewer,
    /// Automation triage worker: investigates and emits a decision marker,
    /// no file edits/commits/pushes/PRs.
    Triage,
    /// Read-only "mini-coordinator" answer agent, enforced via allowlist
    /// rather than blocklist.
    AnswerAgent,
}

/// All inputs Boss provides to a driver for its [`Capability::PermissionPolicy`]:
/// the abstract deny-set and autonomy mode rendered into backend-specific form
/// (for Claude Code: settings.json deny rules + permission-mode; for a future
/// Copilot driver: `--deny-tool` filters + equivalent autonomy flag).
#[derive(bon::Builder, Debug, Clone)]
#[builder(on(String, into))]
pub struct PermissionInput {
    /// Worker posture — determines the per-kind deny rules (reviewer read-only,
    /// triage blanket-write deny, standard implementation no extras) and the
    /// `fastMode` setting for latency-sensitive review passes.
    pub worker_kind: WorkerKind,
    /// Workspace path. Needed by the reviewer deny rules: file-write denies are
    /// scoped to the workspace-parent so out-of-tree artifact writes are allowed.
    pub workspace_path: PathBuf,
    /// Absolute path to the engine events socket. Used to derive the Boss state
    /// directory for sandbox deny globs and the deterministic path-guard hook.
    /// Ignored (no sandbox installed) when `is_remote = true`.
    pub events_socket_path: PathBuf,
    /// Absolute path to the `boss-event` shim binary. Baked into every hook
    /// command as the final argument so the shim fires regardless of `PATH`.
    pub boss_event_path: PathBuf,
    /// Run ID baked into every hook command as `BOSS_RUN_ID=<id>` so the engine
    /// correlates hook events to runs even when env inheritance is unreliable.
    pub run_id: String,
    /// Cube lease ID baked into every hook command as `BOSS_LEASE_ID=<id>`.
    pub lease_id: String,
    /// Execution kind (e.g. `"revision_implementation"`). Triggers the revision
    /// PR-creation guard when set to a revision value.
    pub execution_kind: String,
    /// Task kind (e.g. `"revision"`). Defense-in-depth for the revision PR guard:
    /// the guard fires when either `execution_kind` or `task_kind` signals revision.
    pub task_kind: Option<String>,
    /// When `true`, omit the engine-data-dir sandbox (state-dir deny globs +
    /// path-guard hook). Remote SSH workers set this because their
    /// `events_socket_path` is a forwarded `/tmp` socket, not a Boss data dir.
    pub is_remote: bool,
}

/// What a driver's [`Capability::PermissionPolicy`] rendering produces,
/// broken out by how the spawn flow must apply it — a single settings-file
/// path is not general enough to express every backend's policy shape.
///
/// Claude Code's policy is one settings file passed via `--settings`:
/// `config_files` holds that one path, `extra_args`/`env` are empty. A
/// backend like Codex needs all three: `--sandbox <mode>` / `--ignore-rules`
/// flags (`extra_args`), `CODEX_HOME` (`env`), and `[sandbox_workspace_write]
/// writable_roots` config (`config_files`) — none of which fits in a single
/// returned path.
///
/// `env` composes with whatever mechanism the spawn flow uses to pass
/// driver-supplied environment variables to the worker process; it is not a
/// second, competing channel.
#[derive(Debug, Clone, Default)]
pub struct PermissionArtifacts {
    /// Config file(s) written to the destination directory (e.g. `settings.json`
    /// for Claude). Order matters when a backend takes multiple `--config`-style
    /// flags naming files in a specific sequence.
    pub config_files: Vec<PathBuf>,
    /// Extra CLI arguments the spawn flow must append to the worker invocation
    /// (e.g. Codex's `--sandbox <mode>`).
    pub extra_args: Vec<String>,
    /// Extra environment variables the spawn flow must set on the worker
    /// process (e.g. Codex's `CODEX_HOME`).
    pub env: Vec<(String, String)>,
}

/// A named capability Boss needs from an agent driver.
///
/// A driver declares, per capability, that it provides that capability; for
/// any capability not declared the absence disposition applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Build the command/plan that starts a worker against a workspace with a prompt.
    Spawn,
    /// Materialise per-session files (prompt, agent-rules, gitignore) and
    /// suppress the backend's first-run workspace-trust prompt.
    WorkspaceProvisioning,
    /// Apply Boss's abstract permission policy: autonomous-honour-denies,
    /// reviewer read-only, and the structural deny set (bossctl/state-dir/rm/sudo).
    PermissionPolicy,
    /// Resolve effort+override against the driver's model menu; classify model
    /// families for the autonomy-default branch.
    ModelAndEffortMenu,
    /// Produce a `WorkerEvent` stream driving the activity machine (fidelity
    /// tiers: rich / coarse / minimal).
    ProgressObservation,
    /// Intercept-and-rewrite-or-deny a tool call *before* it runs (editorial
    /// PreToolUse hooks, path guard, revision-PR guard).
    ToolUseInterception,
    /// A "turn ended" signal triggering completion detection and probe injection.
    TurnBoundary,
    /// Receive the worker's structured results (PR URL, ReviewResult, triage,
    /// FOLLOWUPS) via a file-based primary contract.
    StructuredOutput,
    /// A redactable, role-structured view of the run for summarisation and
    /// post-hoc extraction.
    TranscriptAccess,
    /// probe / interrupt / stop / reap / classify-error.
    ControlVerbs,
    /// Inject MCP servers and tool definitions (unused in v1 for any driver;
    /// named seam for future use).
    ToolProvisioning,
    /// Driver supplies the agent-rules filename, hook-enforcement wording, and
    /// the final-output convention; the body is shared.
    PromptComposition,
    /// The driver's [`Capability::ProgressObservation`] stream can positively
    /// signal that the worker is blocked awaiting human input (as distinct
    /// from busy/idle) — for Claude, a `WorkerEvent::Notification` preceding
    /// `Stop`. This is a narrower claim than `ProgressObservation` itself:
    /// a driver can produce a perfectly good event stream while having no
    /// channel that ever means "I am specifically waiting on a human."
    ///
    /// Absence is **Degrade**, never Synthesize: Boss must not guess this
    /// state from a lower-fidelity channel (e.g. "no events for N minutes").
    /// A wedged autonomous worker on such a driver shows `Working`
    /// indefinitely rather than a fabricated `WaitingForInput` — see the
    /// agent-driver design doc's "Stop-reason richness loss" risk and the
    /// `codex-progress-channel-decision` investigation, which found
    /// `codex exec`'s one-turn-per-process model has no live "awaiting
    /// input" state for this signal to attach to at all.
    AwaitingInputSignal,
}

impl Capability {
    /// Returns all defined capability variants in a stable, canonical order.
    ///
    /// Used by [`CapabilityResolver::check_dispatch`] to iterate every
    /// capability and resolve its effective disposition for a `(kind, driver)`
    /// pair. New capabilities should be appended here when added to the enum.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::Spawn,
            Self::WorkspaceProvisioning,
            Self::PermissionPolicy,
            Self::ModelAndEffortMenu,
            Self::ProgressObservation,
            Self::ToolUseInterception,
            Self::TurnBoundary,
            Self::StructuredOutput,
            Self::TranscriptAccess,
            Self::ControlVerbs,
            Self::ToolProvisioning,
            Self::PromptComposition,
            Self::AwaitingInputSignal,
        ]
        .into_iter()
    }

    /// Default absence disposition: what Boss does when a driver does not
    /// declare this capability. Per-kind escalation via [`KindRequirements`]
    /// can upgrade Degrade/Synthesize to Refuse.
    pub fn default_absence_disposition(self) -> AbsenceDisposition {
        match self {
            Self::Spawn => AbsenceDisposition::Refuse,
            Self::WorkspaceProvisioning => AbsenceDisposition::Refuse,
            Self::PermissionPolicy => AbsenceDisposition::Refuse,
            Self::ModelAndEffortMenu => AbsenceDisposition::Degrade,
            Self::ProgressObservation => AbsenceDisposition::Synthesize,
            Self::ToolUseInterception => AbsenceDisposition::Degrade,
            Self::TurnBoundary => AbsenceDisposition::Synthesize,
            Self::StructuredOutput => AbsenceDisposition::Degrade,
            Self::TranscriptAccess => AbsenceDisposition::Degrade,
            Self::ControlVerbs => AbsenceDisposition::Degrade,
            Self::ToolProvisioning => AbsenceDisposition::Degrade,
            Self::PromptComposition => AbsenceDisposition::Refuse,
            Self::AwaitingInputSignal => AbsenceDisposition::Degrade,
        }
    }
}

/// What Boss does when a driver does not declare a required capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbsenceDisposition {
    /// Boss manufactures the signal from a lower-fidelity channel the driver
    /// does provide (e.g. ProgressObservation from JSON stdout).
    Synthesize,
    /// Boss runs with reduced fidelity and records that it did (e.g. 5-value
    /// effort collapsing to 3-value, post-hoc editorial instead of pre-tool).
    Degrade,
    /// Boss refuses to dispatch this work item on this driver, failing at the
    /// dispatch gate with an actionable error before any pane spawns.
    Refuse,
}

/// The capabilities a driver declares, plus optional per-capability
/// absence-disposition overrides.
#[derive(Debug, Clone)]
pub struct CapabilitySet {
    provided: HashSet<Capability>,
    /// Overrides the default absence disposition for specific capabilities this
    /// driver does NOT provide (e.g. to express Refuse instead of the default
    /// Degrade for ToolUseInterception on an editorial-required driver).
    absence_overrides: HashMap<Capability, AbsenceDisposition>,
}

impl CapabilitySet {
    pub fn new(provided: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            provided: provided.into_iter().collect(),
            absence_overrides: HashMap::new(),
        }
    }

    /// Override the absence disposition for a capability this driver does not
    /// provide. Chainable builder method.
    pub fn with_absence_override(mut self, cap: Capability, disposition: AbsenceDisposition) -> Self {
        self.absence_overrides.insert(cap, disposition);
        self
    }

    pub fn provides(&self, cap: Capability) -> bool {
        self.provided.contains(&cap)
    }

    /// Absence disposition for a capability this driver does NOT provide,
    /// combining the driver-level override with the global default.
    pub fn absence_disposition(&self, cap: Capability) -> AbsenceDisposition {
        self.absence_overrides
            .get(&cap)
            .copied()
            .unwrap_or_else(|| cap.default_absence_disposition())
    }
}

/// Per-driver model and effort menu (capability: [`Capability::ModelAndEffortMenu`]).
///
/// All fields are function pointers so the menu can live in a `static`
/// [`DriverDescriptor`]. Each driver supplies its own table; `resolve_spawn_config`
/// resolves model/effort precedence against the selected driver's menu
/// (design §1.4 / §Mix-and-match).
/// The `bon::Builder` derive is here for the repo's builder convention (a
/// struct with more than five named fields must carry it, so an additive field
/// doesn't churn every construction site). Every current construction is a
/// struct literal inside a `static DriverDescriptor`, which a builder call
/// cannot be — the derive is what future non-static callers use.
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct ModelMenu {
    /// Engine-default model slug for this driver (resolve-spawn precedence step 5:
    /// last resort when no override, pool override, effort level, or product default applies).
    pub engine_default: &'static str,
    /// Maps a Boss [`EffortLevel`] to this driver's effort-knob value
    /// (e.g. `"low"`, `"medium"` for Claude Code's `--effort` flag).
    /// Returns `None` when the driver omits the effort flag for this level
    /// (e.g. a 3-value collapse that does not model `trivial` separately).
    ///
    /// `EffortLevel` has exactly five variants, so this is the domain: a
    /// driver whose CLI exposes more reasoning levels than that (e.g.
    /// codex-cli 0.145.0's six on `gpt-5.6-*`) is reachable only through its
    /// top five — the remaining rungs of its ladder have no `EffortLevel` to
    /// map from.
    pub effort_value_for_level: fn(EffortLevel) -> Option<&'static str>,
    /// Maps a Boss [`EffortLevel`] to the default model slug for this driver.
    ///
    /// This is the **legacy size-derived** table, consulted only for rows that
    /// carry no [`ReasoningMode`] (see [`Self::model_for_reasoning`]). Effort
    /// is a size signal, so a table keyed off it can only ever approximate
    /// what kind of thinking a job needs; it is kept because clearing a row's
    /// reasoning must restore exactly the behaviour it had before the
    /// capability signal existed.
    pub default_model_for_level: fn(EffortLevel) -> &'static str,
    /// Maps a Boss [`ReasoningMode`] to the model slug for this driver.
    ///
    /// The capability lever, and the one that decides the model for any row
    /// that has been classified: `Standard` names the driver's
    /// well-articulated-coding tier, `Investigation` the tier worth paying for
    /// when the worker has to diagnose or design before it edits. Neither
    /// depends on [`EffortLevel`], which is what lets a `small` investigation
    /// reach the stronger model without lying about its size and a genuinely
    /// mechanical `large` row stay on the cheaper one.
    pub model_for_reasoning: fn(ReasoningMode) -> &'static str,
    /// Optional per-level worker-prompt addendum to prepend to the initial-prompt body.
    /// `None` for levels where no addendum is appropriate.
    pub prompt_addendum_for_level: fn(EffortLevel) -> Option<&'static str>,
    /// Returns `true` iff the given model slug requires `--permission-mode auto`
    /// (top-tier models such as Opus and Fable on Claude Code).
    /// Used to branch the spawn invocation's permission flag.
    pub model_requires_auto_permissions: fn(&str) -> bool,
}

/// Static data-half of a driver: binary, file layout, display labels, and model/effort menu.
/// The behavioural half is the `AgentDriver` trait methods.
#[derive(bon::Builder, Debug, Clone)]
#[builder(on(String, into))]
pub struct DriverDescriptor {
    /// Canonical slug in `tasks.driver` and CLI `--driver` flag
    /// (e.g. `"claude"`, `"copilot"`, `"codex"`).
    pub name: &'static str,
    /// Human-readable label for UI and logs (e.g. `"Claude Code"`).
    pub label: &'static str,
    /// Binary name to invoke (e.g. `"claude"`, `"copilot"`).
    pub binary: &'static str,
    /// Per-session config directory relative to the workspace root
    /// (e.g. `".claude"`, `".copilot"`).
    pub config_dir: &'static str,
    /// Filename for the agent-rules file inside `config_dir`
    /// (e.g. `"CLAUDE.md"`, `"AGENTS.md"`).
    pub agent_rules_filename: &'static str,
    /// Filename for the initial prompt inside `config_dir`
    /// (e.g. `"initial-prompt.txt"`).
    pub initial_prompt_filename: &'static str,
    /// Per-driver model and effort menu (design §1.4 / §Mix-and-match).
    pub model_menu: ModelMenu,
}

/// Per-[`TaskKind`] capability escalations. A kind can mark specific
/// capabilities as *required-strict*, forcing [`AbsenceDisposition::Refuse`]
/// on absence even when the capability's default is Degrade or Synthesize.
///
/// Example: `TaskKind::Design` marks `StructuredOutput` and
/// `ToolUseInterception` required-strict so a driver lacking them is refused
/// for design tasks without a bespoke per-kind block.
pub struct KindRequirements {
    required_strict: HashSet<Capability>,
}

impl KindRequirements {
    /// Required-strict capability set for a given task kind.
    /// Empty means no escalations beyond per-capability defaults.
    pub fn for_kind(kind: TaskKind) -> Self {
        let required_strict = match kind {
            TaskKind::Design => [Capability::StructuredOutput, Capability::ToolUseInterception]
                .into_iter()
                .collect(),
            _ => HashSet::new(),
        };
        Self { required_strict }
    }

    pub fn is_required_strict(&self, cap: Capability) -> bool {
        self.required_strict.contains(&cap)
    }

    /// Resolved absence disposition for `cap` when dispatching a work item of
    /// this kind to a driver with the given `CapabilitySet`.
    ///
    /// - `None` — driver provides the capability (no absence to resolve).
    /// - `Some(Refuse)` — absent and required-strict for this kind.
    /// - `Some(_)` — absent, not required-strict; driver's default applies.
    pub fn resolve_absence_disposition(
        &self,
        cap: Capability,
        driver_caps: &CapabilitySet,
    ) -> Option<AbsenceDisposition> {
        if driver_caps.provides(cap) {
            return None;
        }
        if self.is_required_strict(cap) {
            return Some(AbsenceDisposition::Refuse);
        }
        Some(driver_caps.absence_disposition(cap))
    }
}

// ── Dispatch-gate types ─────────────────────────────────────────────────────

/// Resolves the capability set for a `(kind, driver)` pair, producing a
/// [`DispatchPlan`] on success or a [`CapabilityGateError`] when any
/// required capability has a `Refuse` disposition.
///
/// Implements the dispatch-gate step from the agent-driver design
/// (§Chosen approach):
/// ```text
/// required(kind) ∩ declared(driver) → synthesize | degrade | refuse
/// ```
///
/// Construct via [`DriverRegistry::resolver`].
pub struct CapabilityResolver<'a> {
    driver: &'a dyn AgentDriver,
}

impl<'a> CapabilityResolver<'a> {
    /// Create a resolver for the given driver instance.
    pub fn new(driver: &'a dyn AgentDriver) -> Self {
        Self { driver }
    }

    /// Check whether this driver can dispatch a work item of `kind`.
    ///
    /// Iterates every [`Capability`], resolves each one's effective
    /// disposition under `(kind, driver)` using [`KindRequirements`] and
    /// the driver's [`CapabilitySet`], and:
    ///
    /// - Returns [`Ok(DispatchPlan)`] if no capability has `Refuse`
    ///   disposition. The plan lists what is provided, synthesized, and
    ///   degraded for observability.
    /// - Returns [`Err(CapabilityGateError)`] listing every refused
    ///   capability with an actionable message if the driver is ineligible.
    pub fn check_dispatch(&self, kind: &TaskKind) -> Result<DispatchPlan, CapabilityGateError> {
        let caps = self.driver.capabilities();
        let kind_reqs = KindRequirements::for_kind(kind.clone());
        let descriptor = self.driver.descriptor();

        let mut provided = Vec::new();
        let mut synthesized = Vec::new();
        let mut degraded = Vec::new();
        let mut refused = Vec::new();

        for cap in Capability::all() {
            match kind_reqs.resolve_absence_disposition(cap, &caps) {
                None => provided.push(cap),
                Some(AbsenceDisposition::Synthesize) => synthesized.push(cap),
                Some(AbsenceDisposition::Degrade) => degraded.push(cap),
                Some(AbsenceDisposition::Refuse) => refused.push(cap),
            }
        }

        if refused.is_empty() {
            Ok(DispatchPlan {
                driver_name: descriptor.name,
                provided,
                synthesized,
                degraded,
            })
        } else {
            Err(CapabilityGateError {
                driver_name: descriptor.name,
                driver_label: descriptor.label,
                task_kind: kind.clone(),
                refused,
            })
        }
    }
}

/// Result of a successful [`CapabilityResolver::check_dispatch`] call.
///
/// Lists the effective capability disposition for the `(kind, driver)` pair.
/// Synthesized and degraded capabilities are noted for observability; the
/// dispatch can proceed for all three.
#[derive(Debug, Clone)]
pub struct DispatchPlan {
    /// Slug of the resolved driver (e.g. `"claude"`).
    pub driver_name: &'static str,
    /// Capabilities the driver fully provides.
    pub provided: Vec<Capability>,
    /// Capabilities absent but synthesizable from a lower-fidelity channel.
    pub synthesized: Vec<Capability>,
    /// Capabilities absent; Boss dispatches with reduced fidelity.
    pub degraded: Vec<Capability>,
}

impl DispatchPlan {
    /// `true` when the driver provides every capability at full fidelity.
    pub fn is_full_fidelity(&self) -> bool {
        self.synthesized.is_empty() && self.degraded.is_empty()
    }
}

/// Error from [`CapabilityResolver::check_dispatch`] when one or more
/// capabilities have a `Refuse` disposition for the `(kind, driver)` pair.
///
/// Fails the dispatch gate before any pane spawns, with an actionable message
/// naming the refused capabilities so the kanban item shows the root cause.
#[derive(Debug)]
pub struct CapabilityGateError {
    /// Slug of the driver that was checked (e.g. `"copilot"`).
    pub driver_name: &'static str,
    /// Human-readable driver label (e.g. `"GitHub Copilot CLI"`).
    pub driver_label: &'static str,
    /// Work-item kind that triggered the refusal.
    pub task_kind: TaskKind,
    /// Capabilities the driver lacks that are required for this kind.
    pub refused: Vec<Capability>,
}

impl std::fmt::Display for CapabilityGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let refused: Vec<_> = self.refused.iter().map(|c| format!("{c:?}")).collect();
        write!(
            f,
            "driver '{}' cannot dispatch a {} work item: refused capabilities: {}",
            self.driver_label,
            self.task_kind,
            refused.join(", "),
        )
    }
}

impl std::error::Error for CapabilityGateError {}

// ── Error classification ─────────────────────────────────────────────────────

/// Abstract classification of a worker error for recovery decisions.
/// Each driver translates its backend-specific error strings into this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerErrorClass {
    /// Retryable infrastructure error — auto-resume is appropriate.
    Transient,
    /// Non-retryable — retrying would reproduce the failure.
    Permanent,
    /// Recognised as an error but not confidently bucketed; treat as Permanent.
    Indeterminate,
}

/// Fidelity tier of the [`WorkerEvent`] stream a driver's
/// [`Capability::ProgressObservation`] produces (design §Capabilities).
///
/// The activity machine downstream consumes the same `WorkerEvent` type at
/// every tier; the tier records how much resolution the driver's event source
/// actually carries, so degrade decisions (and the staleness sweep) can
/// account for a driver that observes less than Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressFidelity {
    /// Per-tool events plus lifecycle. Claude provides this from its hook
    /// stream — `PreToolUse`/`PostToolUse` give per-tool granularity. A
    /// stdout-JSONL driver whose stream carries the same per-tool-call
    /// boundary (e.g. `item.started`/`item.completed`) also declares this
    /// tier — the tier is about resolution, not transport.
    Rich,
    /// Turn + lifecycle boundaries only, with no per-tool granularity.
    Coarse,
    /// Process alive/exited only — no in-run signal.
    Minimal,
}

impl ProgressFidelity {
    /// Cadence-based staleness threshold this tier can support, given the
    /// threshold Claude's `Rich` tier uses today (`stale_worker_sweep`'s
    /// `DEFAULT_STALE_THRESHOLD_SECS`). `None` means this tier must **not**
    /// be judged by event cadence at all — the caller should exempt it from
    /// [`crate`]'s stale-worker sweep and rely on a different backstop
    /// (process liveness).
    ///
    /// ## The decision this encodes
    ///
    /// The sweep's `current_tool.is_some()` guard already protects any
    /// `Rich`-tier driver through an arbitrarily long single tool call — a
    /// `Rich` driver reports a start/end boundary around every tool, so
    /// `Rich` reuses Claude's existing threshold unchanged (this is what
    /// keeps "Claude's sweep behaviour must be unchanged" true by
    /// construction: Claude declares `Rich`, so `default_stale_threshold_secs`
    /// passes straight through).
    ///
    /// `Coarse` has no such guard: a coarse-tier driver reports only turn
    /// boundaries, so `current_tool` never gets set and a legitimately-busy
    /// worker mid-turn (e.g. deep in a long build with no per-tool event to
    /// report) looks identical to a hung one under cadence alone. There is
    /// no empirically-grounded per-tier threshold to substitute — picking
    /// one would be guessing a number that either still false-positives on
    /// a slow-but-healthy turn or is so generous it stops backstopping
    /// anything. Erring toward "too lax" is recoverable by a human; erring
    /// toward "too eager" destroys work — so `Coarse` is exempted rather
    /// than assigned a guessed threshold.
    /// `Minimal` (process-alive-only) has no event stream to key cadence
    /// off at all, so it is exempted for the same reason the design doc's
    /// risk register already named: "hold `Working` while alive, and exempt
    /// minimal-tier drivers from the staleness sweep."
    ///
    /// Exempting `Coarse`/`Minimal` here does not leave those workers with
    /// no liveness backstop at all — `dead_pid_sweep` still catches a
    /// worker whose OS process has actually exited; what it can't catch is
    /// a live-but-wedged one, which is the gap the design doc's risk
    /// register already accepted for these tiers pending a future
    /// liveness-only sweep (tracked separately, not part of this decision).
    ///
    /// ## The rejected alternative
    ///
    /// A blanket "stdout-JSONL drivers are exempt from cadence staleness"
    /// rule was considered and rejected: Codex's stdout stream carries
    /// `item.started`/`item.completed` around each tool call — the same
    /// granularity Claude's hooks give — so a Codex driver correctly
    /// declaring `Rich` gets the exact same protection Claude gets today.
    /// Exempting it wholesale because its transport is stdout rather than
    /// hooks would blind the sweep to a genuinely hung Codex worker for no
    /// reason tied to the actual signal it emits, reintroducing the failure
    /// mode this guard exists to prevent: a genuinely hung Codex worker is
    /// never swept, holds its slot, and wedges dispatch. The fidelity tier —
    /// not the transport —
    /// is what determines whether cadence-based judgement is valid.
    pub fn stale_threshold_secs(self, default_stale_threshold_secs: i64) -> Option<i64> {
        match self {
            ProgressFidelity::Rich => Some(default_stale_threshold_secs),
            ProgressFidelity::Coarse | ProgressFidelity::Minimal => None,
        }
    }
}

/// Inputs the rich-tier ProgressObservation wiring needs to point a worker's
/// event source at the engine: the events-socket endpoint, the run/lease
/// identity tags, the worker's workspace, and the event-forwarder binary the
/// worker invokes (the `boss-event` shim for Claude).
#[derive(Debug, Clone)]
pub struct ProgressObservationConfig {
    /// Engine events-socket path the forwarder connects to.
    pub events_socket_path: PathBuf,
    /// Cube lease id, surfaced to the forwarder via `BOSS_LEASE_ID`.
    pub lease_id: String,
    /// Run id, inline-prefixed as `BOSS_RUN_ID` so the forwarder can splice
    /// `_boss_run_id` into every payload for run correlation.
    pub run_id: String,
    /// Worker workspace, where the forwarder buffers events when the engine
    /// is unreachable (`BOSS_WORKSPACE`).
    pub workspace_path: PathBuf,
    /// Absolute path to the event-forwarder binary (the `boss-event` shim).
    pub forwarder_binary: PathBuf,
}

/// A driver's event-source wiring for [`Capability::ProgressObservation`]
/// when that source is [`ProgressIngress::HookCallback`].
///
/// `hooks` is the settings-file `hooks` map that routes every lifecycle +
/// tool hook event to the `boss-event` shim, which forwards each payload to
/// the engine events socket; the spawn flow merges this fragment into the
/// worker settings file.
#[derive(Debug, Clone, Default)]
pub struct ProgressObservationWiring {
    /// Hook-event name → array of hook entries. Claude wires all seven
    /// lifecycle events to the forwarder; the caller may extend the
    /// `PreToolUse` entry with interception guards (a separate capability).
    pub hooks: serde_json::Map<String, serde_json::Value>,
}

/// The transport a driver's [`Capability::ProgressObservation`] event source
/// rides. Two disjoint transports exist (see
/// `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`):
///
/// - Claude wires a hooks map ([`ProgressObservationWiring`]) that fans every
///   lifecycle/tool event out to the `boss-event` shim, which forwards each
///   payload to the engine's events-socket ingress.
/// - Codex has no equally robust hook signal for *progress*: its hook trust
///   model fails open and silently on an untrusted/misconfigured hook (no
///   error, no log line — reproduction 2 in the decision doc), which is
///   disqualifying for a liveness signal specifically. Its worker process
///   instead emits a `stdout` JSONL stream (`thread.started` → `turn.started`
///   → `item.started`/`item.completed` → `turn.completed`) that a reader
///   parses and feeds to [`AgentDriver::normalize_progress_event`]. That
///   reader is `boss_engine_stdout_progress`, attached to the engine's
///   activity machine by `boss_engine::stdout_progress`; a driver that
///   selects this variant has no settings-file hook wiring to merge.
///
/// A driver without hook-callback wiring returns [`Self::StdoutJsonl`] here
/// rather than an empty [`ProgressObservationWiring`] — the absence of hooks
/// is a distinct, named transport, not a degenerate case of the hook one.
#[derive(Debug, Clone)]
pub enum ProgressIngress {
    /// Claude-style: a hooks map merged into the worker settings file.
    HookCallback(ProgressObservationWiring),
    /// Codex-style: the engine reads and parses the worker's stdout JSONL
    /// stream; no settings-file wiring is produced.
    StdoutJsonl,
}

/// A turn-ended signal for [`Capability::TurnBoundary`]: the driver-agnostic
/// form of "the worker finished a turn and is now idle".
///
/// This is what the engine's turn-boundary consumers key off — completion
/// detection, PR-URL capture, probe injection/delivery, the live-status
/// summariser's `Stop` trigger. None of them matches [`WorkerEvent::Stop`]
/// itself any more; each reads the boundary the driver produced from
/// whatever channel it actually has:
///
/// - **Claude** fires a `Stop` hook into the `boss-event` shim, which the
///   engine decodes into [`WorkerEvent::Stop`] — that variant is the
///   boundary.
/// - **Codex** emits a native, typed `turn.completed` on its `--json` stdout
///   stream (verified against codex-cli 0.145.0; see
///   `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`).
///   Its driver decodes that to [`WorkerEvent::Stop`] in
///   [`AgentDriver::normalize_progress_event`] and reports the boundary
///   here. Routing a native turn event through this method is the
///   first-class path; what a driver must *not* do is manufacture a
///   Claude-shaped hook payload to satisfy Claude-shaped plumbing behind
///   the engine's back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEnd {
    /// Session identity of the turn that ended, as the driver reports it.
    /// Same value the originating [`WorkerEvent`] carries.
    pub session_id: String,
    /// Why the turn ended. Boss's own sequencer refines this beyond what a
    /// single payload can say (a `Notification` immediately before the
    /// boundary implies [`StopReason::AwaitingInput`]), so a driver that
    /// cannot distinguish reasons reports [`StopReason::Completed`] and
    /// loses nothing it had.
    pub reason: StopReason,
    /// The boundary is a re-entrant continuation rather than a fresh idle:
    /// the agent was already stopping and something (Claude's
    /// `stop_hook_active`) pulled it back into another turn. Drivers with no
    /// such concept report `false`.
    pub continuation: bool,
}

/// Inputs the [`Capability::ToolUseInterception`] wiring needs to build the
/// per-session PreToolUse guard hooks.
///
/// Built by the spawn flow from per-session and per-execution data; the driver
/// turns it into the PreToolUse hook entries that guard the tool-call surface
/// (path guard, boss-launch guard, PR-redirect guard, checkleft push guard,
/// revision PR guard).
#[derive(Debug, Clone)]
pub struct ToolUseInterceptionConfig {
    /// Parent directory of the engine events socket (the Boss data dir). The
    /// path guard uses this to fence workers off the engine's runtime state.
    /// `None` for remote workers, which have no local Boss data to sandbox.
    pub data_dir: Option<PathBuf>,
    /// Absolute path to the `boss-path-guard.py` script materialised next to
    /// the worker settings file. `None` for remote workers (the script is
    /// never shipped to the remote host).
    pub path_guard_script: Option<PathBuf>,
    /// Absolute path to the `boss-checkleft-push-guard.py` script materialised
    /// next to the worker settings file. `None` for remote workers.
    pub checkleft_guard_script: Option<PathBuf>,
    /// Whether this is a revision execution. Revision workers must not open
    /// new PRs — the revision PR guard blocks `gh pr create`, `cube pr create`,
    /// and the deprecated `cube pr ensure`.
    pub is_revision: bool,
    /// Whether this is a Standard worker. Reviewer and triage workers skip the
    /// PR-redirect guard and the checkleft push guard because their deny rules
    /// already block push operations.
    pub is_standard_worker: bool,
}

/// The PreToolUse hook entries the driver wires for [`Capability::ToolUseInterception`].
///
/// Returned by [`AgentDriver::tool_use_interception_wiring`]. The spawn flow
/// pushes these entries onto the `PreToolUse` array in the worker settings
/// file, after the progress-observation forwarder entry (which stays first so
/// the live-status machine sees every tool call).
#[derive(Debug, Clone, Default)]
pub struct ToolUseInterceptionWiring {
    /// Ordered hook entries to append to `PreToolUse`. Each entry is a Claude
    /// settings-file hook object:
    /// `{ "matcher": "…", "hooks": [{ "type": "command", "command": "…" }] }`.
    pub pre_tool_use_hooks: Vec<serde_json::Value>,
}

/// What a post-hoc interception adapter does after reviewing a tool output.
///
/// Used when a driver lacks a real-time PreToolUse hook surface and must
/// inspect the artefact after the tool has already run. No driver currently
/// implements this — `ClaudeDriver` provides real-time PreToolUse hook
/// interception via [`AgentDriver::tool_use_interception_wiring`].
///
/// **Post-hoc is not pre-hoc.** By the time the engine can call
/// [`PostHocInterceptionFn`], the tool has already executed — a `RequestEdit`
/// can only ask the worker to clean up after the fact, never prevent the
/// call. A driver on this path ran without editorial enforcement, the path
/// guard, the revision-PR guard, and the checkleft push guard for that call;
/// the engine's post-hoc dispatch (`dispatch_post_hoc_interception_on_post_tool_use`)
/// logs that loss explicitly rather than letting `Accept`/no-registered-fn
/// read as "still guarded".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostHocInterceptionAction {
    /// The artefact is acceptable; no follow-up needed.
    Accept,
    /// The artefact needs editing; the engine should ask the worker to redact
    /// or revise it, with `reason` as the actionable explanation.
    RequestEdit { reason: String },
}

/// Signature of the post-hoc interception adapter for drivers that do not
/// provide real-time [`Capability::ToolUseInterception`].
///
/// Called by the engine after a Bash tool call completes, with the tool name,
/// its input, and its output. Returns the [`PostHocInterceptionAction`] the
/// engine should take.
///
/// This is the declared seam for future hookless drivers (e.g. Copilot,
/// Codex). A driver registers one by overriding [`AgentDriver::post_hoc_interception`];
/// the engine calls it from `dispatch_post_hoc_interception_on_post_tool_use`
/// on the `PostToolUse` boundary for any driver whose `capabilities()` does
/// not provide [`Capability::ToolUseInterception`] (the [`AbsenceDisposition::Degrade`]
/// path). Not yet implemented for any driver: `ClaudeDriver` provides
/// real-time PreToolUse interception and the engine's
/// `dispatch_editorial_on_pretooluse` handles the editorial surface
/// server-side, so it declares [`Capability::ToolUseInterception`] and never
/// reaches this path.
pub type PostHocInterceptionFn =
    fn(tool_name: &str, tool_input: &serde_json::Value, tool_output: &serde_json::Value) -> PostHocInterceptionAction;

/// Free-text feed the engine's primary-path PR-URL capture consumes from a
/// completed tool observation.
///
/// The driver owns the *shape* of tool input/output (Claude's
/// `tool_response.{stdout,stderr}` object vs Codex's bare
/// `aggregated_output` string, Claude's `{ "command": "…" }` vs a bare
/// command string). The engine owns the *algorithm*: it runs the shared
/// `find_first_pr_url` regex over [`Self::output_text`] and the shared
/// `gh pr` / `cube pr` command gates over [`Self::command`]. Drivers must
/// not invent a second extraction algorithm — they only supply the text
/// the existing one scans.
///
/// Built by [`AgentDriver::pr_url_capture_feed`] (and its default,
/// [`default_pr_url_capture_feed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrUrlCaptureFeed {
    /// Free text the engine feeds to the shared PR-URL regex
    /// (`boss_engine_structured_output::pr_url::find_first_pr_url`). For
    /// Claude this is `stdout` then `stderr` (joined so the first URL in
    /// stdout still wins); for a Codex-shaped normaliser this is the
    /// `command_execution.aggregated_output` string.
    pub output_text: String,
    /// The shell command that produced the output, used for the
    /// `is_gh_pr_command` / `is_revision_push_command` gates. Empty when
    /// the observation carries no command surface (the gates then reject).
    pub command: String,
}

/// Default [`AgentDriver::pr_url_capture_feed`] body: Claude's PostToolUse
/// Bash shape **and** the Codex-style bare-string shape produced when a
/// stdout-JSONL normaliser maps `item.command` / `item.aggregated_output`
/// straight into `WorkerEvent::PostToolUse`'s `tool_input` /
/// `tool_response` as strings.
///
/// Returns `None` when `tool_name` is not `"Bash"` (the Claude tool name
/// every current normaliser — including the Codex-shaped one — maps
/// command execution onto) or when `tool_response` has no scannable
/// surface at all.
///
/// Keeping both shapes in the default means:
/// - Claude's capture path is unchanged (object shape hits the object arm).
/// - A hookless driver that normalises to bare strings gets PR-URL capture
///   without a second regex and without waiting for a specialised override.
/// - A future driver with a third shape overrides the trait method.
pub fn default_pr_url_capture_feed(
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_response: &serde_json::Value,
) -> Option<PrUrlCaptureFeed> {
    if tool_name != "Bash" {
        return None;
    }

    let command = tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .or_else(|| tool_input.as_str())
        .unwrap_or("")
        .to_owned();

    let output_text = if let Some(text) = tool_response.as_str() {
        // Codex `command_execution.aggregated_output` (and any other
        // normaliser that puts free text directly on tool_response).
        text.to_owned()
    } else if tool_response.is_object() {
        // Claude Bash tool_response: prefer stdout, fall back to stderr.
        // Concatenating preserves "first URL in stdout wins over stderr"
        // when the shared regex scans left-to-right — identical to
        // `pr_url_capture::extract_pr_url_from_bash_response`.
        let stdout = tool_response.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        let stderr = tool_response.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
        if stdout.is_empty()
            && stderr.is_empty()
            && tool_response.get("stdout").is_none()
            && tool_response.get("stderr").is_none()
        {
            // Object with neither field — not a Bash response shape we know.
            return None;
        }
        if stdout.is_empty() {
            stderr.to_owned()
        } else if stderr.is_empty() {
            stdout.to_owned()
        } else {
            format!("{stdout}\n{stderr}")
        }
    } else {
        return None;
    };

    Some(PrUrlCaptureFeed { output_text, command })
}

/// Inputs for [`AgentDriver::structured_output_wiring`]
/// ([`Capability::StructuredOutput`]).
///
/// The engine designates one absolute [`Self::result_path`] per
/// `(execution, kind)` (see [`boss_engine_structured_output`]) and, when it
/// has one, an opaque JSON Schema the agent should honour. The driver turns
/// those into the backend-specific spawn artifacts: env-file contract for
/// Claude, or CLI flags such as Codex's `--output-schema` /
/// `--output-last-message` for a driver with native schema enforcement.
///
/// The schema's *format* is not defined here — the seam carries whatever the
/// caller supplies. Drivers that cannot enforce a schema ignore it.
#[derive(Debug, Clone, Copy)]
pub struct StructuredOutputRequest<'a> {
    /// Which payload this wiring is for. Selects the env-var name under the
    /// file contract (`BOSS_PR_URL_OUTPUT` vs `BOSS_STRUCTURED_OUTPUT`).
    pub kind: StructuredOutputKind,
    /// Absolute path the engine will read after the run. For the env-file
    /// contract this is also the path the worker is told to write (prompt +
    /// env). A driver that redirects output (e.g. via `--output-last-message`)
    /// should still point that flag at this path so the engine's reader
    /// needs no per-driver knowledge.
    pub result_path: &'a Path,
    /// Optional JSON Schema the caller wants the agent to honour. Opaque —
    /// this seam does not define a schema format. A driver with native
    /// schema enforcement materialises it (typically next to
    /// [`Self::result_path`]) and passes it to the CLI; Claude ignores it
    /// and relies on the prompt + file contract. `None` when the caller has
    /// no schema to enforce.
    pub schema: Option<&'a serde_json::Value>,
}

/// What [`AgentDriver::structured_output_wiring`] produces for
/// [`Capability::StructuredOutput`]: the spawn-time env / CLI-arg
/// adjustments plus the path the engine should read after the run.
///
/// Mirrors [`PermissionArtifacts`]: broken out by how the spawn flow must
/// apply them, so a single returned path is not forced to express every
/// backend's shape. Claude fills only `env` (the `BOSS_*` file-contract
/// vars); a Codex driver fills `extra_args` with `--output-schema` /
/// `--output-last-message` and may still set `env` so the file contract
/// remains a working fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuredOutputArtifacts {
    /// Extra CLI arguments the spawn flow must append to the worker
    /// invocation (e.g. Codex's `--output-schema <file>` and
    /// `--output-last-message <path>`). Empty for the env-file contract.
    pub extra_args: Vec<String>,
    /// Extra environment variables the spawn flow must set on the worker
    /// process (e.g. `BOSS_STRUCTURED_OUTPUT=<path>` /
    /// `BOSS_PR_URL_OUTPUT=<path>`).
    pub env: Vec<(String, String)>,
    /// Absolute path the engine should read the result from after the run.
    /// Equal to the request's `result_path` for the env-file contract and
    /// for a native flag pointed at the same path; a driver that redirects
    /// output names the redirected path here.
    pub result_path: PathBuf,
}

/// Env-var name the file contract exports for `kind`.
///
/// [`StructuredOutputKind::PrUrl`] is a separate var because an implementer
/// produces both a PR URL and (optionally) a designated payload, so the two
/// cannot share one path. Every other kind reuses
/// [`boss_engine_structured_output::STRUCTURED_OUTPUT_ENV`].
pub fn structured_output_env_name(kind: StructuredOutputKind) -> &'static str {
    match kind {
        StructuredOutputKind::PrUrl => boss_engine_structured_output::PR_URL_OUTPUT_ENV,
        _ => boss_engine_structured_output::STRUCTURED_OUTPUT_ENV,
    }
}

/// Default [`AgentDriver::structured_output_wiring`] body: the driver-agnostic
/// **env-file contract**.
///
/// Sets the appropriate `BOSS_*` env var to `request.result_path`, returns
/// that path as the engine's read target, and produces no extra CLI args.
/// Ignores `request.schema` — the common-denominator contract has no native
/// schema enforcement; the prompt carries the shape and the worker writes
/// the file.
///
/// This is the fallback every driver inherits. A driver with a stronger
/// native mechanism (Codex `--output-schema`) overrides the trait method,
/// typically by starting from this default and appending CLI flags so the
/// file path keeps working even when the native path is used.
///
/// The file contract is **not** conditional on the capability being
/// declared: the engine always prepares the path and may always export
/// these env vars. Absence of the capability only drops the driver's
/// prose-scrape fallback ([`AgentDriver::structured_output_fallback`]).
pub fn default_structured_output_wiring(request: &StructuredOutputRequest<'_>) -> StructuredOutputArtifacts {
    StructuredOutputArtifacts {
        extra_args: Vec::new(),
        env: vec![(
            structured_output_env_name(request.kind).to_owned(),
            request.result_path.display().to_string(),
        )],
        result_path: request.result_path.to_path_buf(),
    }
}

/// Driver-neutral inputs for the [`Capability::Spawn`] capability. Every
/// field is a concept that holds across backends: the resolved model/effort,
/// an optional rendered settings/config path, the corp-laptop
/// auto-permissions override, and an optional forced permission mode.
#[derive(Debug, Clone, Copy)]
pub struct SpawnRequest<'a> {
    /// Resolved model slug (e.g. `"opus"`, `"sonnet"`).
    pub model: &'a str,
    /// Driver-specific effort knob value, resolved from the driver's
    /// [`ModelMenu`]. `None` when the row carries no effort level.
    pub effort: Option<&'a str>,
    /// Absolute path to the driver's rendered settings/config file, when the
    /// spawn flow has one to pass (e.g. Claude's `--settings`). `None` for
    /// spawns that carry no settings path.
    pub settings_path: Option<&'a Path>,
    /// Corp-laptop override: force `--permission-mode auto` for non-Opus
    /// models too, instead of the default `--dangerously-skip-permissions`.
    pub non_opus_auto_mode: bool,
    /// Forces a specific permission mode (e.g. `"dontAsk"` for the
    /// capability-restricted answer agent), suppressing the model-derived
    /// choice. `None` keeps the default per-model behaviour.
    pub permission_mode_override: Option<&'a str>,
}

/// One environment adjustment a [`SpawnPlan`] applies to the worker pane
/// shell before its `command` runs.
///
/// A plain `Vec<(String, String)>` of sets cannot express Claude's
/// requirement to *unset* `ANTHROPIC_API_KEY` (so the worker authenticates
/// via OAuth credentials instead of a stray API key inherited from the
/// user's shell profile) — hence the two-variant shape instead of bare pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvDirective {
    /// `export <0>=<1>` in the pane shell before `command` runs.
    Set(String, String),
    /// `unset <0>` in the pane shell before `command` runs.
    Unset(String),
}

/// What [`AgentDriver::spawn_invocation`] produces for [`Capability::Spawn`]:
/// the environment adjustments and command line the spawn flow applies to
/// the worker pane, verbatim and in order. A driver owns both its command
/// line and its environment requirements (e.g. Claude's unset
/// `ANTHROPIC_API_KEY`, a Codex driver's exported `CODEX_HOME`).
#[derive(Debug, Clone, Default)]
pub struct SpawnPlan {
    /// Environment adjustments to apply in the pane shell, in order, before
    /// `command` runs.
    pub env: Vec<EnvDirective>,
    /// The command line to run after `env` has been applied (e.g.
    /// `claude --model … "$(cat …)"\n`).
    pub command: String,
}

/// An agent driver: the abstraction layer between Boss and a coding-agent CLI.
///
/// A driver declares its [`CapabilitySet`] and implements the behavioural
/// methods for each capability it claims. Boss queries the declaration at
/// dispatch time and applies absence policies for undeclared capabilities.
///
/// Held as `Box<dyn AgentDriver>` or `Arc<dyn AgentDriver>` by the resolver;
/// all methods are object-safe.
#[async_trait]
pub trait AgentDriver: Send + Sync {
    // ── Static half (data-descriptor) ──────────────────────────────────────

    fn descriptor(&self) -> &DriverDescriptor;
    fn capabilities(&self) -> CapabilitySet;

    // ── Spawn capability ────────────────────────────────────────────────────

    /// Build the [`SpawnPlan`] — environment adjustments plus the command
    /// line — written into the pane as the spawn command.
    ///
    /// `request.permission_mode_override`, when `Some`, forces
    /// `--permission-mode <mode>` and suppresses the model-derived `auto` /
    /// `--dangerously-skip-permissions` choice. Used by the capability-restricted
    /// answer agent to guarantee `dontAsk` (deny-by-default allowlist), which
    /// must not be downgradable. `None` keeps the default per-model behaviour.
    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan;

    // ── WorkspaceProvisioning capability ────────────────────────────────────

    /// Write per-session workspace files (prompt file, agent-rules, gitignore)
    /// and suppress the backend's first-run trust prompt.
    async fn provision_workspace(&self, workspace: &Path, prompt_text: &str, run_id: &str) -> anyhow::Result<()>;

    /// Tear down whatever per-run state the driver created *outside* the cube
    /// workspace — a per-worker config/cache dir, a socket, a temp credential
    /// file. Paired with [`AgentDriver::provision_workspace`], but not its
    /// mirror: this must NOT touch anything under `workspace` itself, since
    /// cube owns that checkout's lifecycle.
    ///
    /// `workspace` is informational only (some implementations may use it to
    /// namespace their own state) — `run_id` is the actual key for the state
    /// being cleaned up, since drivers that key their out-of-workspace state
    /// by run id (e.g. a per-worker `CODEX_HOME`) must still be torn down
    /// when the workspace path is unknown (never recorded, or already
    /// cleared by a racing teardown). Callers pass `None` rather than
    /// skipping the call in that case.
    ///
    /// Called on every run-termination path (normal completion, stop, reap,
    /// orphaned/husk recovery, app-crash reconciliation) — not just the happy
    /// one, since those are exactly the paths where a driver's out-of-workspace
    /// state would otherwise be orphaned. Callers must treat this as
    /// best-effort: implementations must be idempotent, and callers log a
    /// returned error rather than propagate it, so a teardown hiccup never
    /// fails an otherwise-successful run.
    ///
    /// Must not perform real work — it can run while the process is shutting
    /// down. `ClaudeDriver` implements this as a no-op: Claude creates no
    /// state outside the workspace.
    async fn teardown_workspace(&self, workspace: Option<&Path>, run_id: &str) -> anyhow::Result<()>;

    // ── PermissionPolicy capability ─────────────────────────────────────────

    /// Write the driver's permission/hooks config to `dest_dir` and return the
    /// [`PermissionArtifacts`] the spawn flow must apply: config file path(s)
    /// (passed as `--settings` or equivalent), extra CLI args, and extra env.
    ///
    /// `input` carries the abstract deny-set + autonomy-mode that the driver
    /// renders into its backend-specific format. For Claude Code this produces a
    /// `settings.json` with `permissions.deny` rules and `defaultMode: "auto"`,
    /// plus `boss-event` hook wiring for every hook event, returned as the sole
    /// entry in `config_files` with `extra_args`/`env` empty. A backend like
    /// Codex needs all three fields (design doc: sandbox mode + ignore-rules
    /// flags, `CODEX_HOME`, and writable-roots config).
    async fn write_permission_config(
        &self,
        input: &PermissionInput,
        dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts>;

    // ── ProgressObservation capability ──────────────────────────────────────

    /// Fidelity tier of the [`WorkerEvent`] stream this driver produces.
    /// Claude declares [`ProgressFidelity::Rich`] (per-tool hook events).
    ///
    /// Consumed by the engine's stale-worker sweep (`stale_worker_sweep.rs`)
    /// via [`ProgressFidelity::stale_threshold_secs`] to decide whether — and
    /// at what threshold — a slot running this driver is eligible for
    /// cadence-based staleness detection at all.
    fn progress_fidelity(&self) -> ProgressFidelity;

    /// Build the driver's event-source wiring so the worker emits a lifecycle
    /// + tool-use stream the engine decodes into [`WorkerEvent`]s. Returns a
    /// [`ProgressIngress`]: for the Claude driver this is
    /// [`ProgressIngress::HookCallback`] carrying the `hooks` block routing
    /// every hook event to the `boss-event` shim, which the spawn flow merges
    /// into the worker settings; a driver with no hook-callback wiring
    /// returns [`ProgressIngress::StdoutJsonl`] instead.
    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress;

    /// Decode one raw event-source payload into a typed [`WorkerEvent`] that
    /// drives the (driver-agnostic) activity machine. For the Claude driver
    /// the raw payload is a hook JSON object; this delegates to
    /// [`boss_protocol::normalize_hook_event`].
    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError>;

    // ── TurnBoundary capability ─────────────────────────────────────────────

    /// Report whether `event` — one already-decoded
    /// [`Self::normalize_progress_event`] output — is this driver's
    /// turn-ended signal, and if so describe it.
    ///
    /// This is the engine's *only* route to a turn boundary. Completion
    /// detection, probe injection/delivery, and the live-status `Stop`
    /// trigger all gate on the [`TurnEnd`] this returns instead of matching
    /// [`WorkerEvent::Stop`] themselves, so a driver whose boundary does not
    /// coincide with that variant — or which emits the variant for something
    /// that is *not* a turn boundary — decides for itself rather than
    /// inheriting Claude's hook semantics.
    ///
    /// `ClaudeDriver` reports every [`WorkerEvent::Stop`] (its `Stop` hook)
    /// and nothing else. A Codex driver reports the same variant, reached
    /// from its native `turn.completed` stdout event — see [`TurnEnd`] for
    /// why that mapping belongs here and not in a fake hook payload.
    ///
    /// Returning `None` for every event is the honest answer for a driver
    /// that declares no [`Capability::TurnBoundary`]; its
    /// [`AbsenceDisposition::Synthesize`] default then applies, and the
    /// engine-side synthesiser that would infer a boundary from a
    /// lower-fidelity channel is deliberately not built yet — no driver
    /// Boss ships or plans needs it.
    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd>;

    // ── ToolUseInterception capability ──────────────────────────────────────

    /// Build the PreToolUse hook entries for the `ToolUseInterception`
    /// capability. The spawn flow appends these to the `PreToolUse` array of
    /// the worker settings file, after the progress-observation forwarder entry
    /// (which stays first so the live-status machine sees every tool call).
    ///
    /// For `ClaudeDriver` this returns Python guard scripts (path guard,
    /// boss-launch guard, PR-redirect guard, checkleft push guard, revision PR
    /// guard) as `command`-type hook entries; each guard is conditional on
    /// `config` fields — remote workers, non-standard workers, and
    /// non-revision executions each skip some guards.
    ///
    /// A driver without a synchronous PreToolUse hook surface should not
    /// declare [`Capability::ToolUseInterception`]; the absence disposition
    /// (degrade to [`PostHocInterceptionFn`] or refuse) applies instead.
    fn tool_use_interception_wiring(&self, config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring;

    /// The [`PostHocInterceptionFn`] this driver registers for the
    /// [`Capability::ToolUseInterception`] [`AbsenceDisposition::Degrade`]
    /// path, or `None` to take the bare degrade (no post-hoc review at all,
    /// just the engine's visible loss-of-guards signal).
    ///
    /// Only meaningful for a driver that does **not** declare
    /// [`Capability::ToolUseInterception`] — a driver that does provide it
    /// (Claude today) has real-time PreToolUse hooks and is never routed
    /// through this seam, so its default (`None`) is correct and this method
    /// need not be overridden.
    fn post_hoc_interception(&self) -> Option<PostHocInterceptionFn> {
        None
    }

    // ── ProgressObservation → PR-URL primary-path feed ──────────────────────

    /// Supply free text (and the command that produced it) from a completed
    /// tool observation so the engine's primary-path PR-URL capture can run
    /// the **shared** regex against it.
    ///
    /// The engine never reads a Claude `tool_response` payload shape directly
    /// for this path anymore: it asks the driver. Claude's default
    /// ([`default_pr_url_capture_feed`]) preserves the historical
    /// `stdout`/`stderr` scan; a stdout-JSONL driver whose normaliser places
    /// `aggregated_output` as a bare string on `tool_response` is also
    /// handled by that default. Override only when a driver has a third
    /// shape.
    ///
    /// Returns `None` when this observation is not a scannable tool surface
    /// (wrong tool name, unrecognised response shape). Returning `Some` with
    /// empty `output_text` is fine — the engine finds no URL and does nothing.
    ///
    /// **Do not poll GitHub for the branch's PR here.** That is a different
    /// mechanism (the cold-path reconstruction in `completion::detect_pr`)
    /// with different failure modes; inventing it as a feed would mask a
    /// broken stream extraction path.
    fn pr_url_capture_feed(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        tool_response: &serde_json::Value,
    ) -> Option<PrUrlCaptureFeed> {
        default_pr_url_capture_feed(tool_name, tool_input, tool_response)
    }

    // ── PromptComposition capability ────────────────────────────────────────

    /// Driver-specific preamble injected at the top of the agent-rules file,
    /// naming the hook mechanism and the `.claude/`-style gitignore contract.
    fn agent_rules_preamble(&self) -> &'static str;

    // ── TranscriptAccess capability ──────────────────────────────────────────

    /// Report where this driver's transcript for the current session lives,
    /// given a raw progress-event payload (the same payload passed to
    /// [`Self::normalize_progress_event`]).
    ///
    /// For the Claude driver this reads the `transcript_path` field Claude
    /// stamps on every hook payload. A driver that does not fire Claude-style
    /// hooks — or that locates its transcript some other way — implements
    /// its own discovery here instead of relying on that field; the engine's
    /// path-resolution path calls this rather than assuming the field's
    /// presence.
    ///
    /// Returns `None` when the payload carries no (or an empty) transcript
    /// path; the caller retries on a later payload.
    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String>;

    /// Normalise a raw transcript JSONL entry to the canonical redactable
    /// field shape that `boss_engine_live_status_redact` and the live-status
    /// summariser expect: `tool_name` / `tool_input` / `tool_response` at the
    /// top level, and `content[].type == "tool_use"` blocks with `name` +
    /// `input` sub-fields.
    ///
    /// For the Claude driver this is the identity — Claude's transcript already
    /// uses canonical names. Alternative drivers with different field shapes
    /// implement the remapping here so the redaction layer is unchanged.
    ///
    /// Takes `raw` by value: this runs on every polled transcript line on the
    /// hot live-status path, and Claude's identity impl can then move it
    /// straight through instead of deep-cloning a payload that may carry a
    /// full `tool_response` body.
    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value;

    /// Extract the worker-halting API-error text from a normalised transcript
    /// tail, but only when it is the **last meaningful entry** (i.e. the worker
    /// did not recover and continue working after the error).
    ///
    /// Returns `None` when there is no API error, or when the worker emitted
    /// normal activity (assistant text/tool use, a user/tool result) after the
    /// most-recent error. `lines` is a slice of already-normalised JSONL values,
    /// oldest-first.
    fn extract_error_from_transcript(&self, lines: &[serde_json::Value]) -> Option<String>;

    // ── ControlVerbs capability ─────────────────────────────────────────────

    /// Classify a raw error string from the worker's output for
    /// transient-recovery decisions.
    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass;

    // ── StructuredOutput capability ─────────────────────────────────────────

    /// Primary-channel wiring for [`Capability::StructuredOutput`]: turn a
    /// designated result path (and optional opaque schema) into the
    /// spawn-time env / CLI-arg artifacts the worker needs, plus the path
    /// the engine should read after the run.
    ///
    /// The default is the driver-agnostic **env-file contract**
    /// ([`default_structured_output_wiring`]): export
    /// `BOSS_STRUCTURED_OUTPUT` / `BOSS_PR_URL_OUTPUT` pointing at
    /// `request.result_path`, produce no extra CLI args, ignore
    /// `request.schema`. That contract is the common denominator that works
    /// for every driver and is **not** conditional on this capability being
    /// declared — absence only drops the prose-scrape fallback below.
    ///
    /// A driver with a stronger native mechanism overrides this method. The
    /// seam carries an optional schema so a Codex driver can materialise it
    /// and pass `--output-schema <file>` / `--output-last-message <path>`
    /// without any engine change beyond applying the returned
    /// [`StructuredOutputArtifacts`]. Prefer starting from
    /// [`default_structured_output_wiring`] and *adding* flags, so the
    /// file-contract env vars remain a working fallback.
    ///
    /// Returns `Err` only when a driver that materialises helper files (e.g.
    /// a schema file for the CLI) fails to write them. The env-file default
    /// is infallible.
    fn structured_output_wiring(
        &self,
        request: &StructuredOutputRequest<'_>,
    ) -> anyhow::Result<StructuredOutputArtifacts> {
        Ok(default_structured_output_wiring(request))
    }

    /// Fallback producer for [`Capability::StructuredOutput`]: recover
    /// `kind`'s payload from the worker's prose when the **primary** channel
    /// — the driver-agnostic file contract in
    /// [`boss_engine_structured_output`] — produced nothing.
    ///
    /// `text` is the worker's final assistant message (or, where the engine
    /// has it, the joined assistant prose of the run); the driver's
    /// final-message conventions are what it knows how to read. Returns
    /// candidates most-preferred first, each carrying the payload in exactly
    /// the wire form the file contract defines for `kind`, so the caller runs
    /// one parser over both channels and keeps the first candidate that
    /// validates.
    ///
    /// Returning an empty `Vec` is the honest answer for a driver — or a kind
    /// — with no prose convention to scrape, and is what a driver that
    /// relies on the file channel alone does for every kind. It is never an
    /// error: an absent payload is a normal outcome the caller already
    /// handles.
    fn structured_output_fallback(&self, kind: StructuredOutputKind, text: &str) -> Vec<FallbackCandidate>;
}

pub mod claude;
pub mod registry;

pub use claude::ClaudeDriver;
pub use registry::DriverRegistry;

/// Shared test fixture for crates that need an [`AgentDriver`] stand-in
/// without a second real driver implementation. Unconditionally compiled
/// (not `#[cfg(test)]`) so downstream crates can depend on it from their own
/// `[dev-dependencies]`; this crate's own unit tests use the same fixture.
pub mod test_support {
    use std::path::Path;

    use async_trait::async_trait;
    use boss_engine_structured_output::StructuredOutputKind;
    use boss_engine_structured_output::fallback::FallbackCandidate;
    use boss_protocol::{NormalizeError, WorkerEvent};

    use super::{
        AgentDriver, CapabilitySet, DriverDescriptor, ModelMenu, PermissionArtifacts, PermissionInput,
        PostHocInterceptionFn, ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan, SpawnRequest,
        ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
    };

    /// A minimal [`DriverDescriptor`] to pair with [`StubDriver`]. Its menu
    /// resolves everything to one `"stub-model"` slug, which is enough for
    /// tests that only exercise capability declaration or the seams a stub
    /// stands in for.
    pub fn stub_descriptor() -> DriverDescriptor {
        DriverDescriptor {
            name: "stub",
            label: "Stub Driver",
            binary: "stub",
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
            },
        }
    }

    /// Configurable [`AgentDriver`] stub. Every method beyond
    /// `descriptor`/`capabilities` is unimplemented (or a harmless no-op for
    /// the methods that can't panic on the hot paths, like
    /// `normalize_transcript_entry`) — callers that need this fixture only
    /// ever exercise capability declaration and menu resolution against it.
    ///
    /// `post_hoc_interception_fn` defaults to `None` (same as the trait's
    /// default); set it with [`StubDriver::with_post_hoc_interception`] to
    /// exercise a downstream crate's `AbsenceDisposition::Degrade` dispatch
    /// for [`super::Capability::ToolUseInterception`] without a second real
    /// driver implementation.
    pub struct StubDriver {
        pub descriptor: DriverDescriptor,
        pub caps: CapabilitySet,
        pub post_hoc_interception_fn: Option<PostHocInterceptionFn>,
    }

    impl StubDriver {
        pub fn new(descriptor: DriverDescriptor, caps: CapabilitySet) -> Self {
            Self {
                descriptor,
                caps,
                post_hoc_interception_fn: None,
            }
        }

        /// Chainable: register the fixture's [`PostHocInterceptionFn`].
        pub fn with_post_hoc_interception(mut self, f: PostHocInterceptionFn) -> Self {
            self.post_hoc_interception_fn = Some(f);
            self
        }
    }

    #[async_trait]
    impl AgentDriver for StubDriver {
        fn descriptor(&self) -> &DriverDescriptor {
            &self.descriptor
        }
        fn capabilities(&self) -> CapabilitySet {
            self.caps.clone()
        }
        fn post_hoc_interception(&self) -> Option<PostHocInterceptionFn> {
            self.post_hoc_interception_fn
        }
        fn spawn_invocation(&self, _: SpawnRequest<'_>) -> SpawnPlan {
            unimplemented!()
        }
        async fn provision_workspace(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn teardown_workspace(&self, _: Option<&Path>, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn write_permission_config(&self, _: &PermissionInput, _: &Path) -> anyhow::Result<PermissionArtifacts> {
            unimplemented!()
        }
        fn progress_fidelity(&self) -> ProgressFidelity {
            unimplemented!()
        }
        fn progress_observation_wiring(&self, _: &ProgressObservationConfig) -> ProgressIngress {
            unimplemented!()
        }
        fn normalize_progress_event(&self, _: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
            unimplemented!()
        }
        fn turn_boundary(&self, _: &WorkerEvent) -> Option<TurnEnd> {
            // Declares no turn boundary. Like `transcript_path_for_session`
            // this sits on an ingress hot path, so it answers instead of
            // panicking — and "this driver has no boundary to report" is the
            // answer a stub can honestly give.
            None
        }
        fn tool_use_interception_wiring(&self, _: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
            unimplemented!()
        }
        fn agent_rules_preamble(&self) -> &'static str {
            unimplemented!()
        }
        fn transcript_path_for_session(&self, _: &serde_json::Value) -> Option<String> {
            None
        }
        fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
            raw
        }
        fn extract_error_from_transcript(&self, _: &[serde_json::Value]) -> Option<String> {
            None
        }
        fn classify_error(&self, _: &str) -> WorkerErrorClass {
            unimplemented!()
        }
        fn structured_output_fallback(&self, _: StructuredOutputKind, _: &str) -> Vec<FallbackCandidate> {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_strict_capabilities_refuse_absent_driver() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let no_caps = CapabilitySet::new([]);

        assert_eq!(
            reqs.resolve_absence_disposition(Capability::StructuredOutput, &no_caps),
            Some(AbsenceDisposition::Refuse),
        );
        assert_eq!(
            reqs.resolve_absence_disposition(Capability::ToolUseInterception, &no_caps),
            Some(AbsenceDisposition::Refuse),
        );
    }

    #[test]
    fn non_strict_capability_uses_default_disposition() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let no_caps = CapabilitySet::new([]);

        // ModelAndEffortMenu is not required-strict for Design; default is Degrade.
        assert_eq!(
            reqs.resolve_absence_disposition(Capability::ModelAndEffortMenu, &no_caps),
            Some(AbsenceDisposition::Degrade),
        );
    }

    #[test]
    fn provided_capability_resolves_to_none() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let all_caps = CapabilitySet::new([Capability::StructuredOutput, Capability::ToolUseInterception]);

        assert_eq!(
            reqs.resolve_absence_disposition(Capability::StructuredOutput, &all_caps),
            None,
        );
    }

    #[test]
    fn absence_override_takes_precedence_over_default() {
        let caps =
            CapabilitySet::new([]).with_absence_override(Capability::ToolUseInterception, AbsenceDisposition::Refuse);

        // Default for ToolUseInterception is Degrade; override makes it Refuse.
        assert_eq!(
            caps.absence_disposition(Capability::ToolUseInterception),
            AbsenceDisposition::Refuse,
        );
    }

    #[test]
    fn task_kind_has_no_strict_requirements_by_default() {
        for kind in [
            TaskKind::Chore,
            TaskKind::Investigation,
            TaskKind::ProjectTask,
            TaskKind::Revision,
            TaskKind::Task,
        ] {
            let reqs = KindRequirements::for_kind(kind.clone());
            assert!(
                !reqs.is_required_strict(Capability::StructuredOutput),
                "{kind:?} should not require-strict StructuredOutput",
            );
            assert!(
                !reqs.is_required_strict(Capability::ToolUseInterception),
                "{kind:?} should not require-strict ToolUseInterception",
            );
        }
    }

    #[test]
    fn spawn_and_prompt_composition_refuse_when_absent() {
        assert_eq!(
            Capability::Spawn.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::PromptComposition.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::WorkspaceProvisioning.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::PermissionPolicy.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
    }

    #[test]
    fn progress_and_turn_boundary_synthesize_when_absent() {
        assert_eq!(
            Capability::ProgressObservation.default_absence_disposition(),
            AbsenceDisposition::Synthesize,
        );
        assert_eq!(
            Capability::TurnBoundary.default_absence_disposition(),
            AbsenceDisposition::Synthesize,
        );
    }

    #[test]
    fn awaiting_input_signal_degrades_when_absent_not_synthesizes() {
        // Must never be Synthesize: Boss must not guess WaitingForInput
        // from a lower-fidelity signal when the driver can't back it.
        assert_eq!(
            Capability::AwaitingInputSignal.default_absence_disposition(),
            AbsenceDisposition::Degrade,
        );
    }

    #[test]
    fn post_hoc_interception_action_variants_are_distinct() {
        assert_eq!(PostHocInterceptionAction::Accept, PostHocInterceptionAction::Accept);
        let edit = PostHocInterceptionAction::RequestEdit {
            reason: "bad content".to_owned(),
        };
        assert_ne!(PostHocInterceptionAction::Accept, edit);
    }

    #[test]
    fn driver_default_registers_no_post_hoc_interception_fn() {
        // A driver that never overrides `post_hoc_interception` (every driver
        // today, including Claude) must resolve to `None` — the trait
        // default. Degrade-path dispatch relies on this to mean "no
        // registered fn" rather than a stale/leftover Some.
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
        assert!(driver.post_hoc_interception().is_none());
    }

    #[test]
    fn stub_driver_registers_and_invokes_post_hoc_interception_fn() {
        fn always_request_edit(
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _tool_output: &serde_json::Value,
        ) -> PostHocInterceptionAction {
            PostHocInterceptionAction::RequestEdit {
                reason: "fixture".to_owned(),
            }
        }

        let driver =
            StubDriver::new(stub_descriptor(), CapabilitySet::new([])).with_post_hoc_interception(always_request_edit);
        let f = driver.post_hoc_interception().expect("fn was registered");
        assert_eq!(
            f("Bash", &serde_json::Value::Null, &serde_json::Value::Null),
            PostHocInterceptionAction::RequestEdit {
                reason: "fixture".to_owned(),
            },
        );
    }

    #[test]
    fn tool_use_interception_wiring_default_is_empty() {
        let wiring = ToolUseInterceptionWiring::default();
        assert!(wiring.pre_tool_use_hooks.is_empty());
    }

    #[test]
    fn tool_use_interception_config_fields_are_accessible() {
        let config = ToolUseInterceptionConfig {
            data_dir: Some(PathBuf::from("/Library/Boss")),
            path_guard_script: Some(PathBuf::from("/tmp/boss-path-guard.py")),
            checkleft_guard_script: Some(PathBuf::from("/tmp/boss-checkleft-push-guard.py")),
            is_revision: true,
            is_standard_worker: true,
        };
        assert!(config.is_revision);
        assert!(config.is_standard_worker);
        assert_eq!(config.data_dir.unwrap(), PathBuf::from("/Library/Boss"));
    }

    #[test]
    fn rich_fidelity_reuses_the_passed_in_default_threshold_unchanged() {
        // Claude declares Rich; the sweep must reuse whatever threshold it is
        // configured with (30 min in production) so its behaviour is
        // unchanged by this mapping existing.
        assert_eq!(ProgressFidelity::Rich.stale_threshold_secs(1_800), Some(1_800));
        assert_eq!(ProgressFidelity::Rich.stale_threshold_secs(42), Some(42));
    }

    #[test]
    fn coarse_and_minimal_fidelity_are_exempt_from_cadence_staleness() {
        assert_eq!(ProgressFidelity::Coarse.stale_threshold_secs(1_800), None);
        assert_eq!(ProgressFidelity::Minimal.stale_threshold_secs(1_800), None);
    }

    #[test]
    fn all_capabilities_covers_every_variant() {
        let all: Vec<_> = Capability::all().collect();
        // Every variant must appear exactly once.
        assert_eq!(all.len(), 13, "Capability::all() must cover all 13 variants");
        // Spot-check a few to ensure the enum and all() stay in sync.
        assert!(all.contains(&Capability::Spawn));
        assert!(all.contains(&Capability::StructuredOutput));
        assert!(all.contains(&Capability::PromptComposition));
    }

    #[test]
    fn capability_resolver_returns_ok_plan_when_no_refused_caps() {
        // A driver that provides every capability must always yield Ok for any kind.
        let all_caps = CapabilitySet::new(Capability::all());
        let driver = StubDriver::new(stub_descriptor(), all_caps);
        let resolver = CapabilityResolver::new(&driver);
        let plan = resolver.check_dispatch(&TaskKind::Design).unwrap();
        assert!(plan.is_full_fidelity(), "full-capability driver must be full-fidelity");
        assert_eq!(plan.driver_name, "stub");
    }

    #[test]
    fn capability_resolver_refuses_design_task_without_structured_output() {
        let caps = CapabilitySet::new([Capability::Spawn, Capability::PromptComposition]);
        let driver = StubDriver::new(stub_descriptor(), caps);
        let resolver = CapabilityResolver::new(&driver);
        let err = resolver.check_dispatch(&TaskKind::Design).unwrap_err();
        assert!(
            err.refused.contains(&Capability::StructuredOutput),
            "Design kind must refuse StructuredOutput when absent: {:?}",
            err.refused,
        );
        assert!(
            err.refused.contains(&Capability::ToolUseInterception),
            "Design kind must refuse ToolUseInterception when absent: {:?}",
            err.refused,
        );
    }

    #[test]
    fn capability_resolver_refuses_any_kind_without_spawn() {
        // Spawn has Refuse as its global default; any kind without Spawn fails.
        let caps = CapabilitySet::new(Capability::all().filter(|c| *c != Capability::Spawn));
        let driver = StubDriver::new(stub_descriptor(), caps);
        let resolver = CapabilityResolver::new(&driver);
        let err = resolver.check_dispatch(&TaskKind::Chore).unwrap_err();
        assert!(
            err.refused.contains(&Capability::Spawn),
            "Spawn must be refused when absent: {:?}",
            err.refused,
        );
    }

    #[test]
    fn dispatch_plan_degraded_and_synthesized_populated_for_partial_driver() {
        // ModelAndEffortMenu is Degrade by default; ProgressObservation is Synthesize.
        let caps = CapabilitySet::new(
            Capability::all().filter(|c| *c != Capability::ModelAndEffortMenu && *c != Capability::ProgressObservation),
        );
        let driver = StubDriver::new(stub_descriptor(), caps);
        let resolver = CapabilityResolver::new(&driver);
        let plan = resolver.check_dispatch(&TaskKind::Chore).unwrap();
        assert!(!plan.is_full_fidelity());
        assert!(
            plan.degraded.contains(&Capability::ModelAndEffortMenu),
            "ModelAndEffortMenu must appear in degraded: {:?}",
            plan.degraded,
        );
        assert!(
            plan.synthesized.contains(&Capability::ProgressObservation),
            "ProgressObservation must appear in synthesized: {:?}",
            plan.synthesized,
        );
    }

    #[test]
    fn capability_gate_error_message_names_driver_and_refused_caps() {
        let err = CapabilityGateError {
            driver_name: "copilot",
            driver_label: "GitHub Copilot CLI",
            task_kind: TaskKind::Design,
            refused: vec![Capability::StructuredOutput, Capability::ToolUseInterception],
        };
        let msg = err.to_string();
        assert!(msg.contains("GitHub Copilot CLI"), "error must name the driver label");
        assert!(msg.contains("design"), "error must name the task kind");
        assert!(msg.contains("StructuredOutput"), "error must name refused caps");
        assert!(msg.contains("ToolUseInterception"), "error must name refused caps");
    }

    // ── Test-only stub driver ──────────────────────────────────────────────
    //
    // Shared with other crates' tests via `crate::test_support::StubDriver`;
    // see that module's doc comment.

    use crate::test_support::{StubDriver, stub_descriptor};

    #[test]
    fn a_driver_declaring_no_turn_boundary_reports_none() {
        // The absence case the (deliberately unbuilt) engine-side synthesizer
        // would cover: no boundary from the driver, for any event.
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
        assert!(
            driver
                .turn_boundary(&WorkerEvent::Stop {
                    session_id: "sess-1".to_owned(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Completed,
                })
                .is_none(),
            "a driver without Capability::TurnBoundary must not claim a boundary",
        );
    }

    // ── pr_url_capture_feed / default_pr_url_capture_feed ──────────────────

    #[test]
    fn default_feed_reads_claude_bash_stdout_stderr_shape() {
        let input = serde_json::json!({
            "command": "cube pr create --branch boss/exec_x --title t"
        });
        let response = serde_json::json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
        });
        let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
        assert_eq!(feed.output_text, "https://github.com/spinyfin/mono/pull/458");
        assert_eq!(feed.command, "cube pr create --branch boss/exec_x --title t");
        assert_eq!(
            boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn default_feed_prefers_stdout_url_over_stderr_url() {
        let input = serde_json::json!({ "command": "gh pr create --title t" });
        let response = serde_json::json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "https://github.com/spinyfin/mono/pull/100",
        });
        let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
        assert_eq!(
            boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn default_feed_falls_back_to_stderr_when_stdout_empty() {
        let input = serde_json::json!({ "command": "gh pr create --title t" });
        let response = serde_json::json!({
            "stdout": "",
            "stderr": "Created: https://github.com/spinyfin/mono/pull/458\n",
        });
        let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
        assert_eq!(
            boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn default_feed_reads_codex_aggregated_output_string_shape() {
        // Mirrors what a stdout-JSONL normaliser emits after mapping
        // `item.command` / `item.aggregated_output` onto PostToolUse as
        // bare strings (see stdout-progress Codex-shaped test driver).
        let input = serde_json::json!("/bin/zsh -lc 'cube pr create --branch boss/x --title t'");
        let response = serde_json::json!("https://github.com/spinyfin/mono/pull/99\n");
        let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
        assert_eq!(feed.command, "/bin/zsh -lc 'cube pr create --branch boss/x --title t'");
        assert_eq!(
            boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/99"),
        );
    }

    #[test]
    fn default_feed_rejects_non_bash_tools() {
        let input = serde_json::json!({ "command": "gh pr create" });
        let response = serde_json::json!({
            "stdout": "https://github.com/spinyfin/mono/pull/1",
        });
        assert!(default_pr_url_capture_feed("Read", &input, &response).is_none());
    }

    #[test]
    fn default_feed_rejects_unrecognised_response_shape() {
        let input = serde_json::json!({ "command": "gh pr create" });
        assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!(null)).is_none());
        assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!(42)).is_none());
        assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!({ "other": true })).is_none());
    }

    #[test]
    fn trait_default_pr_url_capture_feed_matches_free_function() {
        // StubDriver uses the trait default; it must match the free function
        // so an un-overridden driver still feeds PR-URL capture.
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
        let input = serde_json::json!("cube pr create --branch b");
        let response = serde_json::json!("see https://github.com/o/r/pull/7\n");
        assert_eq!(
            driver.pr_url_capture_feed("Bash", &input, &response),
            default_pr_url_capture_feed("Bash", &input, &response),
        );
    }

    // ── structured_output_wiring / default_structured_output_wiring ────────

    fn so_request<'a>(
        kind: StructuredOutputKind,
        result_path: &'a Path,
        schema: Option<&'a serde_json::Value>,
    ) -> StructuredOutputRequest<'a> {
        StructuredOutputRequest {
            kind,
            result_path,
            schema,
        }
    }

    #[test]
    fn default_wiring_exports_pr_url_env_and_echoes_result_path() {
        let path = PathBuf::from("/tmp/boss-worker-output/exec_1.pr-url.json");
        let arts = default_structured_output_wiring(&so_request(StructuredOutputKind::PrUrl, &path, None));
        assert_eq!(
            arts.env,
            vec![(
                boss_engine_structured_output::PR_URL_OUTPUT_ENV.to_owned(),
                path.display().to_string(),
            )],
        );
        assert!(arts.extra_args.is_empty(), "env-file contract has no CLI flags");
        assert_eq!(arts.result_path, path);
    }

    #[test]
    fn default_wiring_exports_structured_output_env_for_non_pr_kinds() {
        let path = PathBuf::from("/tmp/boss-worker-output/exec_1.review-result.json");
        for kind in [
            StructuredOutputKind::ReviewResult,
            StructuredOutputKind::TriageDecision,
            StructuredOutputKind::Followups,
            StructuredOutputKind::PostmortemFollowups,
        ] {
            let arts = default_structured_output_wiring(&so_request(kind, &path, None));
            assert_eq!(
                arts.env,
                vec![(
                    boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
                    path.display().to_string(),
                )],
                "{kind:?} must export BOSS_STRUCTURED_OUTPUT",
            );
            assert!(arts.extra_args.is_empty());
            assert_eq!(arts.result_path, path);
        }
    }

    #[test]
    fn default_wiring_ignores_schema() {
        // The common-denominator contract has no native schema enforcement;
        // schema is carried for richer drivers only.
        let path = PathBuf::from("/tmp/out.json");
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "pr_url": { "type": "string" } },
            "required": ["pr_url"],
        });
        let arts = default_structured_output_wiring(&so_request(StructuredOutputKind::PrUrl, &path, Some(&schema)));
        assert!(
            arts.extra_args.is_empty(),
            "default must not materialise schema into CLI flags: {:?}",
            arts.extra_args,
        );
        assert_eq!(arts.env.len(), 1);
        assert_eq!(arts.result_path, path);
    }

    #[test]
    fn claude_wiring_matches_env_file_contract_with_no_behavioural_change() {
        // Claude expresses the existing BOSS_* env-file contract through the
        // trait method. Schema is ignored; no CLI flags; result path echoes.
        let path = PathBuf::from("/tmp/boss-worker-output/exec_x.followups.json");
        let schema = serde_json::json!({ "type": "array" });
        let request = so_request(StructuredOutputKind::Followups, &path, Some(&schema));

        let via_claude = ClaudeDriver
            .structured_output_wiring(&request)
            .expect("claude wiring is infallible");
        let via_default = default_structured_output_wiring(&request);

        assert_eq!(via_claude, via_default, "Claude must be the env-file contract");
        assert_eq!(
            via_claude.env,
            vec![(
                boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
                path.display().to_string(),
            )],
        );
        assert!(via_claude.extra_args.is_empty());
        assert_eq!(via_claude.result_path, path);
    }

    #[test]
    fn trait_default_wiring_matches_free_function() {
        // StubDriver uses the trait default; un-overridden drivers still get
        // the env-file contract without implementing the richer method.
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
        let path = PathBuf::from("/tmp/out.triage.json");
        let request = so_request(StructuredOutputKind::TriageDecision, &path, None);
        assert_eq!(
            driver.structured_output_wiring(&request).unwrap(),
            default_structured_output_wiring(&request),
        );
    }

    #[test]
    fn schema_capable_driver_passes_schema_and_result_path_to_cli() {
        // Shape a richer driver (Codex `--output-schema` /
        // `--output-last-message`) would produce: start from the env-file
        // fallback, materialise the opaque schema next to the result path,
        // and append the native CLI flags. Engine applies
        // `StructuredOutputArtifacts` generically — no further trait change
        // needed when a real Codex driver lands.
        struct SchemaCapableDriver;

        impl SchemaCapableDriver {
            fn wiring(request: &StructuredOutputRequest<'_>) -> anyhow::Result<StructuredOutputArtifacts> {
                let mut arts = default_structured_output_wiring(request);
                if let Some(schema) = request.schema {
                    // `foo.json` → `foo.schema.json` so the schema sits next
                    // to the result without colliding with it.
                    let schema_path = request.result_path.with_extension("schema.json");
                    if let Some(parent) = schema_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&schema_path, serde_json::to_vec_pretty(schema)?)?;
                    arts.extra_args.push("--output-schema".to_owned());
                    arts.extra_args.push(schema_path.display().to_string());
                    arts.extra_args.push("--output-last-message".to_owned());
                    arts.extra_args.push(request.result_path.display().to_string());
                }
                Ok(arts)
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "boss-so-schema-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result_path = dir.join("exec_1.review-result.json");
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "findings": { "type": "array" }
            },
            "required": ["findings"],
        });
        let request = so_request(StructuredOutputKind::ReviewResult, &result_path, Some(&schema));

        let arts = SchemaCapableDriver::wiring(&request).expect("schema wiring");

        // File-contract fallback still present.
        assert_eq!(
            arts.env,
            vec![(
                boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
                result_path.display().to_string(),
            )],
        );
        assert_eq!(arts.result_path, result_path, "engine still reads the designated path");

        // Native flags carry schema path + result path.
        let schema_path = result_path.with_extension("schema.json");
        assert_eq!(
            arts.extra_args,
            vec![
                "--output-schema".to_owned(),
                schema_path.display().to_string(),
                "--output-last-message".to_owned(),
                result_path.display().to_string(),
            ],
        );
        // Schema was materialised as whatever the caller supplied.
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&schema_path).unwrap()).unwrap();
        assert_eq!(written, schema);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_name_helper_splits_pr_url_from_designated_payload() {
        assert_eq!(
            structured_output_env_name(StructuredOutputKind::PrUrl),
            boss_engine_structured_output::PR_URL_OUTPUT_ENV,
        );
        assert_eq!(
            structured_output_env_name(StructuredOutputKind::Followups),
            boss_engine_structured_output::STRUCTURED_OUTPUT_ENV,
        );
    }
}
