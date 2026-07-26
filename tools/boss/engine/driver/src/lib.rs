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
use boss_protocol::{EffortLevel, NormalizeError, TaskKind, WorkerEvent};

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
    /// FOLLOWUPS) via file-based primary contract (T1414).
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
#[derive(Debug, Clone, Copy)]
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
    pub default_model_for_level: fn(EffortLevel) -> &'static str,
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
    /// stream — `PreToolUse`/`PostToolUse` give per-tool granularity.
    Rich,
    /// Turn + lifecycle boundaries only, with no per-tool granularity.
    Coarse,
    /// Process alive/exited only — no in-run signal.
    Minimal,
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
///   → `item.started`/`item.completed` → `turn.completed`) that a
///   driver-owned reader parses and feeds to
///   [`AgentDriver::normalize_progress_event`]. That reader is separate
///   plumbing, built in a later task; this variant is the documented seam it
///   plugs into — a driver that selects it has no settings-file hook wiring
///   to merge.
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
/// Codex). Not yet implemented for any driver: `ClaudeDriver` provides
/// real-time PreToolUse interception and the engine's
/// `dispatch_editorial_on_pretooluse` handles the editorial surface
/// server-side.
pub type PostHocInterceptionFn =
    fn(tool_name: &str, tool_input: &serde_json::Value, tool_output: &serde_json::Value) -> PostHocInterceptionAction;

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

    /// Build the worker invocation string written into the pane as the
    /// spawn command. Replaces `boss_engine::effort::SpawnConfig::claude_invocation`
    /// for the Claude driver.
    ///
    /// `permission_mode_override`, when `Some`, forces `--permission-mode
    /// <mode>` and suppresses the model-derived `auto` /
    /// `--dangerously-skip-permissions` choice. Used by the capability-restricted
    /// answer agent to guarantee `dontAsk` (deny-by-default allowlist), which
    /// must not be downgradable. `None` keeps the default per-model behaviour.
    fn spawn_invocation(
        &self,
        model: &str,
        effort: Option<&str>,
        settings_path: Option<&Path>,
        non_opus_auto_mode: bool,
        permission_mode_override: Option<&str>,
    ) -> String;

    // ── WorkspaceProvisioning capability ────────────────────────────────────

    /// Write per-session workspace files (prompt file, agent-rules, gitignore)
    /// and suppress the backend's first-run trust prompt.
    async fn provision_workspace(&self, workspace: &Path, prompt_text: &str, run_id: &str) -> anyhow::Result<()>;

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
        AgentDriver, CapabilitySet, DriverDescriptor, PermissionArtifacts, PermissionInput, ProgressFidelity,
        ProgressIngress, ProgressObservationConfig, ToolUseInterceptionConfig, ToolUseInterceptionWiring,
        WorkerErrorClass,
    };

    /// Configurable [`AgentDriver`] stub. Every method beyond
    /// `descriptor`/`capabilities` is unimplemented (or a harmless no-op for
    /// the methods that can't panic on the hot paths, like
    /// `normalize_transcript_entry`) — callers that need this fixture only
    /// ever exercise capability declaration and menu resolution against it.
    pub struct StubDriver {
        pub descriptor: DriverDescriptor,
        pub caps: CapabilitySet,
    }

    impl StubDriver {
        pub fn new(descriptor: DriverDescriptor, caps: CapabilitySet) -> Self {
            Self { descriptor, caps }
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
        fn spawn_invocation(&self, _: &str, _: Option<&str>, _: Option<&Path>, _: bool, _: Option<&str>) -> String {
            unimplemented!()
        }
        async fn provision_workspace(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
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
    fn post_hoc_interception_action_variants_are_distinct() {
        assert_eq!(PostHocInterceptionAction::Accept, PostHocInterceptionAction::Accept);
        let edit = PostHocInterceptionAction::RequestEdit {
            reason: "bad content".to_owned(),
        };
        assert_ne!(PostHocInterceptionAction::Accept, edit);
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
    fn all_capabilities_covers_every_variant() {
        let all: Vec<_> = Capability::all().collect();
        // Every variant must appear exactly once.
        assert_eq!(all.len(), 12, "Capability::all() must cover all 12 variants");
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

    use crate::test_support::StubDriver;

    fn stub_descriptor() -> DriverDescriptor {
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
                prompt_addendum_for_level: |_| None,
                model_requires_auto_permissions: |_| false,
            },
        }
    }
}
