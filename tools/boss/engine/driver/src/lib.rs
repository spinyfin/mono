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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{
    EffortLevel, ExecutionKind, NormalizeError, PaneMonitorSpec, ReasoningMode, StopReason, TaskKind, WorkerEvent,
};

pub mod transcript_store;

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
    /// triage blanket-write deny, standard implementation no extras).
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
    /// Absolute path to the materialised `boss-path-guard.py` script, when
    /// the local path-guard applies. `None` for remote workers.
    pub path_guard_script: Option<PathBuf>,
    /// Absolute path to the materialised `boss-checkleft-push-guard.py`
    /// script, when the local checkleft push guard applies. `None` for remote.
    pub checkleft_guard_script: Option<PathBuf>,
    /// Codex-only: mirrors the `codex_sandbox_enforced` feature flag. When
    /// `false` (the flag's default), Codex's Standard/Triage/AnswerAgent
    /// workers get `--sandbox danger-full-access` instead of the OS-enforced
    /// `workspace-write` seatbelt, matching the Claude driver's no-OS-sandbox
    /// posture (see `codex::codex_sandbox_for_worker_kind`). Reviewer always
    /// stays `--sandbox read-only` regardless of this value. Ignored by every
    /// other driver.
    #[builder(default)]
    pub codex_sandbox_enforced: bool,
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

/// Merge [`PermissionArtifacts::extra_args`] into a pane spawn command line.
///
/// Permission args win over defaults already present on the command: each
/// flag in `extra_args` that already appears in `command` is stripped (with
/// its value when the next extra_arg is not a flag) and then re-inserted so
/// Codex's default `--sandbox workspace-write` is replaced by Reviewer's
/// `--sandbox read-only` rather than duplicated.
///
/// Args are shell-quoted for safe insertion into the pane command string.
/// Empty `extra_args` leaves `command` unchanged (Claude path).
pub fn apply_permission_extra_args(command: &str, extra_args: &[String]) -> String {
    if extra_args.is_empty() {
        return command.to_owned();
    }

    let mut cmd = command.to_owned();

    // Strip flags that extra_args will re-introduce so policy values win.
    let mut i = 0usize;
    while i < extra_args.len() {
        let arg = &extra_args[i];
        if arg.starts_with('-') {
            let takes_value = i + 1 < extra_args.len() && !extra_args[i + 1].starts_with('-');
            cmd = strip_cli_flag_token(&cmd, arg, takes_value);
            i += if takes_value { 2 } else { 1 };
        } else {
            i += 1;
        }
    }

    let insert = extra_args
        .iter()
        .map(|a| boss_ssh_transport::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    // Prefer inserting before `-m` / stdin redirect so flags stay together
    // with the rest of the CLI options (Codex and Claude both put model
    // after flags).
    if let Some(pos) = cmd.find(" -m ") {
        format!("{} {}{}", &cmd[..pos], insert, &cmd[pos..])
    } else if let Some(pos) = cmd.find(" < ") {
        format!("{} {}{}", &cmd[..pos], insert, &cmd[pos..])
    } else if let Some(pos) = cmd.rfind('\n') {
        format!("{} {}{}", &cmd[..pos], insert, &cmd[pos..])
    } else {
        format!("{cmd} {insert}")
    }
}

/// Remove ` flag` (and optionally its following value token) from a generated
/// pane command. Only used on engine-built command lines (whitespace-delimited
/// flags); not a general shell parser.
fn strip_cli_flag_token(command: &str, flag: &str, takes_value: bool) -> String {
    let needle = format!(" {flag}");
    let Some(start) = command.find(&needle) else {
        // Flag at the very start of the command (no leading space).
        if command == flag || command.starts_with(&format!("{flag} ")) || command.starts_with(&format!("{flag}\n")) {
            let after = flag.len();
            if takes_value {
                let rest = command[after..].trim_start();
                let skipped = command[after..].len() - rest.len();
                let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                return command[after + skipped + token_end..].trim_start().to_owned();
            }
            return command[after..].trim_start().to_owned();
        }
        return command.to_owned();
    };
    let after_flag = start + needle.len();
    if !takes_value {
        return format!("{}{}", &command[..start], &command[after_flag..]);
    }
    let rest = &command[after_flag..];
    let rest_trimmed = rest.trim_start();
    let ws = rest.len() - rest_trimmed.len();
    // Value may be shell-quoted (`'workspace-write'`) or bare.
    let token_end = if let Some(inside) = rest_trimmed.strip_prefix('\'') {
        // Find closing unescaped single quote (shell_quote form).
        inside
            .find('\'')
            .map(|i| i + 2) // include both quotes
            .unwrap_or(rest_trimmed.len())
    } else {
        rest_trimmed.find(char::is_whitespace).unwrap_or(rest_trimmed.len())
    };
    let end = after_flag + ws + token_end;
    format!("{}{}", &command[..start], &command[end..])
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
    /// The driver's [`Capability::ProgressObservation`] stream can positively
    /// report each command's exit status (success/failure), as distinct from
    /// mere activity (a command started, then finished, with some output).
    /// This is a narrower claim than `ProgressObservation` itself, in the
    /// same way [`Capability::AwaitingInputSignal`] is: a driver can produce
    /// a perfectly good activity stream while never actually carrying a
    /// reliable per-command outcome.
    ///
    /// Codex is the reason this exists as its own capability rather than
    /// being folded into [`ProgressFidelity::Rich`]. Codex's rollout log
    /// carries `exit_code`/`status` fields alongside the `aggregated_output`
    /// text Boss's normaliser reads, but that field set is not a reliable
    /// per-command outcome: the exit code is only sometimes present, can be
    /// dropped by the model's own result-projection layer before the record
    /// is ever emitted, and becomes unparseable once output is truncated.
    /// `Rich` genuinely describes Codex's event *cadence* — it reports a
    /// start/end boundary around every tool call, same resolution as
    /// Claude's hooks — but cadence says nothing about whether the
    /// end-of-command record actually says whether the command succeeded.
    /// Declaring `Rich` alone would let a scheduler assume that guarantee
    /// anyway, so the outcome claim needs its own capability rather than
    /// riding along on the fidelity tier.
    ///
    /// Absence is **Degrade, never Synthesize**, for the same reason as
    /// `AwaitingInputSignal`: Boss must not guess a command's outcome from
    /// activity alone (e.g. "it kept going, so the last command must have
    /// succeeded") when the driver never actually observed it.
    ///
    /// This is also the seam the eventual cross-driver load-balancing work
    /// will need: a normalised per-command outcome has to carry an explicit
    /// "observed" bit rather than assuming every driver's silence means
    /// success, because Codex's unobserved state has no Claude counterpart —
    /// collapsing the two would make an absent signal indistinguishable from
    /// a confirmed pass.
    CommandOutcomeObservation,
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
            Self::CommandOutcomeObservation,
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
            Self::CommandOutcomeObservation => AbsenceDisposition::Degrade,
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
///
/// Carries `#[derive(bon::Builder)]` per the repo's builder convention for
/// structs with more than five named fields, so an additive field doesn't
/// force every construction site to change.
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
    /// Returns `true` iff the given model slug is one this driver's CLI
    /// accepts. Coupled spawn resolution uses it to refuse invalid literal
    /// overrides (and to make an incompatible product default yield to the
    /// driver default); the spawn path checks it again immediately before
    /// launch as a defence-in-depth compatibility gate.
    pub model_belongs_to_driver: fn(&str) -> bool,
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

/// Per-[`TaskKind`] (and, where the [`ExecutionKind`] carries information the
/// `TaskKind` alone does not, per-[`ExecutionKind`]) capability escalations.
/// A kind can mark specific capabilities as *required-strict*, forcing
/// [`AbsenceDisposition::Refuse`] on absence even when the capability's
/// default is Degrade or Synthesize.
///
/// The document-producing / design-family kinds — `Design`, `Investigation`,
/// `DesignPostmortem` (the same grouping `ReasoningMode::default_for` calls
/// `design_family`) — mark `StructuredOutput` and `ToolUseInterception`
/// required-strict: their deliverable is a doc whose task breakdown /
/// followups must round-trip through the file-based structured-output
/// contract, so a driver lacking either capability is refused for these
/// kinds without a bespoke per-kind block (agent-driver design doc,
/// Codex-eligibility Phase 2: "enable the document-producing kinds via
/// `KindRequirements` once the structured-output contract is proven").
///
/// `ExecutionKind::ConflictResolution` and `ExecutionKind::CiRemediation`
/// mark `CommandOutcomeObservation` required-strict: both need to know
/// whether a shell command (a rebase, a merge, a build/test run) actually
/// succeeded, not merely that one started and finished — exactly the
/// "merge-conflict telemetry path" gap the driver design docs (Codex/Grok
/// "review and conflict resolution" phase) name as the reason those two
/// executions are not yet Codex/Grok-eligible. Their underlying `tasks.kind`
/// is whatever the fixed-up task's own kind is (`Chore`, `Task`, `Revision`,
/// ...), so the `TaskKind` dimension alone cannot express this — it has to
/// come from the execution.
pub struct KindRequirements {
    required_strict: HashSet<Capability>,
}

impl KindRequirements {
    /// Required-strict capability set for a given task kind, additionally
    /// escalated by `execution_kind` when the execution itself (not the
    /// underlying task row) carries a capability requirement. `None` when
    /// the caller has no execution context (e.g. a `TaskKind`-only check);
    /// every real dispatch decision has a concrete `ExecutionKind` and should
    /// pass `Some`.
    pub fn for_kind(kind: TaskKind, execution_kind: Option<&ExecutionKind>) -> Self {
        let mut required_strict: HashSet<Capability> = match kind {
            TaskKind::Design | TaskKind::Investigation | TaskKind::DesignPostmortem => {
                [Capability::StructuredOutput, Capability::ToolUseInterception]
                    .into_iter()
                    .collect()
            }
            _ => HashSet::new(),
        };
        if matches!(
            execution_kind,
            Some(ExecutionKind::ConflictResolution) | Some(ExecutionKind::CiRemediation)
        ) {
            required_strict.insert(Capability::CommandOutcomeObservation);
        }
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

    /// Check whether this driver can dispatch a work item of `kind`, with
    /// `execution_kind` supplying the execution-level context
    /// [`KindRequirements::for_kind`] needs for escalations `TaskKind` alone
    /// cannot express (e.g. conflict resolution / CI remediation). Pass
    /// `None` only when no execution context exists yet.
    ///
    /// Iterates every [`Capability`], resolves each one's effective
    /// disposition under `(kind, execution_kind, driver)` using
    /// [`KindRequirements`] and the driver's [`CapabilitySet`], and:
    ///
    /// - Returns [`Ok(DispatchPlan)`] if no capability has `Refuse`
    ///   disposition. The plan lists what is provided, synthesized, and
    ///   degraded for observability.
    /// - Returns [`Err(CapabilityGateError)`] listing every refused
    ///   capability with an actionable message if the driver is ineligible.
    pub fn check_dispatch(
        &self,
        kind: &TaskKind,
        execution_kind: Option<&ExecutionKind>,
    ) -> Result<DispatchPlan, CapabilityGateError> {
        let caps = self.driver.capabilities();
        let kind_reqs = KindRequirements::for_kind(kind.clone(), execution_kind);
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

/// What a driver's foreground process does with pty bytes that arrive while
/// it is **mid-turn** (`WorkerActivity::Working`) — the property that decides
/// whether `probe --urgent` can honour its documented "inject at the next
/// tool-call boundary" promise.
///
/// This is deliberately a *driver* property, not a pane-activity one. The
/// original typed-input guard (see `boss_protocol::WorkerActivity::
/// accepts_typed_input`) refused every mid-turn write on the grounds that
/// injecting there is unsafe. The unsafety it was protecting against is real
/// but specific: `codex exec` runs one turn per process with stdin on
/// `/dev/null`, so bytes written mid-turn are never read by the agent, survive
/// in the tty input buffer, and are then executed by the interactive shell
/// after the agent exits (ghostty-codex-pane-viability, Q2 Layer D). Applying
/// that blanket refusal to every driver made `--urgent` structurally
/// undeliverable for interactive-TUI drivers too, because the activity is
/// `Working` by construction at exactly the `PostToolUse` boundary the urgent
/// path fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidTurnPaneInput {
    /// The foreground process reads stdin continuously and holds mid-turn
    /// input as the next prompt — the same thing a human gets by typing into
    /// the pane while the agent is working. Mid-turn injection is safe: the
    /// bytes are consumed by the agent, not left in the tty for the shell.
    Buffers,
    /// The foreground process does not consume stdin mid-turn (or it is not
    /// established that it does). Mid-turn injection must be refused — the
    /// bytes would linger in the tty and be executed by the shell once the
    /// process exits. The safe default for any driver that has not proven
    /// otherwise.
    Rejects,
}

impl MidTurnPaneInput {
    /// True when mid-turn pane writes are safe for this driver.
    pub fn buffers(self) -> bool {
        matches!(self, Self::Buffers)
    }
}

// ── ControlVerbs delivery plans ──────────────────────────────────────────────
//
// Declarative answers the engine asks a driver when it needs to act on a live
// worker. The methods on [`AgentDriver`] return these plans; the engine owns
// the transport (pane RPCs, process signals). A driver that has not proven a
// verb returns the safe/unsupported arm so the worker is fire-and-forget for
// that verb rather than silently inheriting another driver's mechanism.

/// How the engine should deliver a probe (inject text) into a live worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeDelivery {
    /// Write text into the worker pane as typed input — the interactive-TUI
    /// path used by Claude today (`SendToPane`).
    PaneText,
    /// Driver does not support probing; the worker is fire-and-forget for
    /// this verb. Safe default for any driver that has not established a
    /// delivery mechanism.
    Unsupported,
}

/// How the engine should interrupt an in-flight turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptDelivery {
    /// Deliver an Esc keystroke into the pane (`InterruptWorkerPane`) —
    /// Claude's interactive-TUI path today.
    PaneEsc,
    /// Driver does not support interrupt; the in-flight turn cannot be
    /// cancelled short of a full stop/reap. Safe default.
    Unsupported,
}

/// What evidence tells the engine that an interrupted turn has actually
/// ended, so it is safe to type into the pane.
///
/// Both variants are observed through the same place — the slot's live
/// [`boss_protocol::WorkerActivity`] leaving `Working` — because every turn
/// end the engine knows about arrives as a `WorkerEvent::Stop` on the events
/// socket. What differs per driver is *what produces that event* for a
/// **cancelled** turn, and a driver that gets it wrong would have the engine
/// typing into a still-running turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndEvidence {
    /// The interrupted turn still fires this driver's ordinary turn-boundary
    /// signal, so nothing extra is needed: Claude's Esc-cancelled turn emits
    /// its `Stop` hook, and Codex's rollout emits `turn_aborted`, which
    /// [`crate::codex`]'s progress normalizer turns into a
    /// `Stop { stop_reason: Interrupted }`.
    TurnBoundarySignal,
    /// The interrupted turn **skips** this driver's turn-boundary signal, and
    /// the engine's bounded interrupt-recovery observer
    /// ([`AgentDriver::prepare_interrupt_recovery`]) is what supplies the turn
    /// end — by observing the driver's own cancellation record, or by
    /// synthesizing one when the settle window elapses. Grok is this case:
    /// an Esc-cancelled turn writes only a `turn_ended`/`cancelled` record to
    /// `events.jsonl` and never fires `Stop`.
    ///
    /// A plan declaring this is asserting that the driver also returns `Some`
    /// from `prepare_interrupt_recovery` — without it there is no path from
    /// the interrupt to an observable turn end at all, and every interrupt
    /// would time out. [`crate::registry::DriverRegistry`] refuses to
    /// register a driver whose two declarations disagree.
    RecoveryObserver,
}

/// Per-driver recipe for interrupting **one** in-flight turn and proving it
/// stopped.
///
/// This exists because interrupt semantics are not portable. How many presses
/// of which key cancel a turn, how long the TUI takes to unwind, and what
/// record proves it unwound are properties of each agent's terminal UI, and
/// assuming one driver's answer for another is how "we sent Escape" gets
/// mistaken for "the turn ended". Every field here is a driver's own measured
/// answer, and the engine executes the plan without knowing which agent it is
/// driving.
///
/// The plan describes *one* probe's worth of interrupting: [`Self::presses`]
/// keys per attempt, at most [`Self::max_attempts`] attempts, each attempt
/// waited out for [`Self::confirm_window`]. Once those are exhausted the
/// engine gives up loudly rather than escalating on its own — interrupting
/// discards the worker's in-flight work, so an unbounded retry loop would
/// keep destroying partial work with nothing to show for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptPlan {
    /// What one attempt sends.
    pub gesture: InterruptGesture,
    /// How long one attempt waits for [`Self::turn_end_evidence`] before the
    /// engine considers that attempt to have not taken.
    pub confirm_window: Duration,
    /// How many attempts before the interrupt is declared failed. Bounded by
    /// construction — see the type doc.
    pub max_attempts: u8,
    /// What proves the turn ended for this driver.
    pub turn_end_evidence: TurnEndEvidence,
}

/// The keystrokes one interrupt attempt delivers.
///
/// Split from the rest of [`InterruptPlan`] because it answers a different
/// question — *what to send* versus *how to know it worked* — and only this
/// half is transport-specific. The engine's pane transports execute this; the
/// confirmation policy around it is transport-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptGesture {
    /// tmux key name delivered to the pane (`send-keys <key>`), e.g.
    /// `"Escape"`. Named rather than a raw byte so the app-hosted transport
    /// (`EngineToAppRequest::InterruptWorkerPane`, which sends `kVK_Escape`)
    /// and the tmux transport agree on the same gesture.
    pub key: &'static str,
    /// How many presses of [`Self::key`] make up one attempt. One is enough
    /// for every driver measured so far; the field exists because a TUI that
    /// needs a double-press (the human gesture some agents use to escape a
    /// nested mode first) must be able to say so instead of the engine
    /// hard-coding one press for everyone.
    pub presses: u8,
    /// Gap between presses inside a single attempt. Only meaningful when
    /// [`Self::presses`] > 1.
    pub press_interval: Duration,
}

impl InterruptPlan {
    /// Total time the engine may spend trying to interrupt one turn: every
    /// attempt's confirm window plus the presses inside it. Callers that need
    /// to bound their own RPC against this (the probe path answers
    /// synchronously) use it instead of re-deriving the arithmetic.
    pub fn worst_case_duration(&self) -> Duration {
        let per_attempt =
            self.confirm_window + self.gesture.press_interval * u32::from(self.gesture.presses.saturating_sub(1));
        per_attempt * u32::from(self.max_attempts.max(1))
    }
}

/// How the engine should stop a worker (graceful path before process kill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDelivery {
    /// No graceful quit string; tear the worker down at the process level
    /// only. Claude and Codex today both use this — `agents stop` cancels
    /// the execution and reaps the pane/process without typing a quit
    /// command into the agent.
    ProcessOnly,
    /// Type `command` into the pane (as a full line), then release. A future
    /// interactive driver may use this for a graceful `/quit`-style exit
    /// before process kill.
    PaneCommand { command: &'static str },
    /// Driver has no stop verb of its own; fall through to reap.
    Unsupported,
}

/// How the engine should reap a worker process.
///
/// Reap is the one ControlVerbs verb that always works: every driver is a
/// process the engine can signal. The plan names the ladder so a future
/// driver with a different cleanup order can diverge without the engine
/// inventing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapDelivery {
    /// SIGTERM, then SIGKILL on the process group after a grace window —
    /// today's universal ladder (`reap_worker_process_tree`).
    ProcessGroup,
}

/// Pre-interrupt snapshot for [`AgentDriver::prepare_interrupt_recovery`] /
/// the engine's bounded interrupt-recovery observer.
///
/// Closes the Esc-cancelled-turn-boundary hazard: most drivers' interrupt
/// path still produces a normal turn boundary through the regular
/// [`Capability::TurnBoundary`] channel (Claude's Esc-cancelled turn still
/// fires a `Stop` hook), but a driver whose interrupt path skips that
/// channel entirely (Grok: Esc-cancelled turns skip the `Stop` hook, see
/// the ControlVerbs design) needs the engine to watch for — or, failing
/// that, synthesize — the turn end itself.
///
/// `offset` is the byte length already on disk at snapshot time (0 if the
/// file does not exist yet), captured by [`AgentDriver::prepare_interrupt_recovery`]
/// **before** the engine delivers the interrupt: a cancellation record
/// written in the race between "Esc reaches the pty" and "the recovery
/// observer starts tailing" would otherwise be missed and wrongly treated
/// as evidence the interrupt never took.
#[derive(Debug, Clone)]
pub struct InterruptRecoverySnapshot {
    /// File to tail for this driver's turn-end evidence.
    pub events_path: PathBuf,
    /// Byte offset to start reading new content from.
    pub offset: u64,
    /// Session identity to stamp on the synthetic `WorkerEvent::Stop` this
    /// recovery produces — threaded through rather than re-read after the
    /// interrupt so the eventual event carries the right identity even if
    /// driver state has moved on by the time the settle window elapses.
    pub session_id: String,
    /// How long to wait for evidence before falling back to a synthesized
    /// turn end. The fallback is sanctioned and expected to fire
    /// occasionally — see [`AgentDriver::prepare_interrupt_recovery`] — but
    /// must be logged distinctly from an observed one so an operator can
    /// tell the two apart.
    pub settle_window: Duration,
}

/// How long a driver's worker **process** is expected to live relative to the
/// turns it serves — the property that decides whether "the OS process is
/// gone" is evidence of a *death*.
///
/// Boss's process-liveness reapers ([`crate`]'s consumers: the engine's
/// dead-pid sweep, the app's pane-death report, and the durable-pid dead-pane
/// sweep) are all written against a long-lived interactive session: it
/// outlives every turn, so its exit can only ever mean the worker died
/// mid-run. That inference was *false* for `codex exec`, whose CLI ran one
/// turn per process and wrote `turn.completed` before exiting by design —
/// reading that exit as a crash reaped a cleanly finished Codex run 160 ms
/// after it succeeded, orphaned it, and redispatched the same work item ~20
/// times. Codex was moved to a persistent interactive session to close that
/// gap (see `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`),
/// and the classification/drain machinery that used to pair a one-shot
/// exit with terminal-result evidence — so a driver could legitimately
/// declare [`Self::OneTurnPerProcess`] — was removed along with the last
/// driver that needed it.
///
/// No registered driver may declare [`Self::OneTurnPerProcess`] today:
/// [`crate::registry::DriverRegistry`] refuses to register one that does
/// (mirroring its `refuse_stdout_jsonl_ingress` guard for a different
/// discontinued topology). A future one-shot driver must rebuild that
/// classification/drain machinery before it can declare this variant — not
/// just flip the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerProcessLifetime {
    /// The foreground process is expected to outlive every turn it serves and
    /// to stay attached until Boss tears it down (Claude Code's interactive
    /// TUI). Its exit is therefore always unexpected: a crash, an OOM, a
    /// kill-9, or the host app dying and taking the pane with it.
    ///
    /// The only variant any registered driver may declare today — see the
    /// type doc above.
    #[default]
    Persistent,
    /// The foreground process serves exactly one turn and then exits.
    /// [`crate::registry::DriverRegistry`] refuses to register a driver that
    /// declares this — see the type doc above for why.
    OneTurnPerProcess,
}

/// Fidelity tier of the [`WorkerEvent`] stream a driver's
/// [`Capability::ProgressObservation`] produces (design §Capabilities).
///
/// The activity machine downstream consumes the same `WorkerEvent` type at
/// every tier; the tier records how much resolution the driver's event source
/// actually carries, so degrade decisions (and the staleness sweep) can
/// account for a driver that observes less than Claude.
///
/// This tier is about event **cadence** only — how often the driver reports
/// a start/end boundary — not about what those boundaries can tell Boss once
/// a command has run. Whether a driver's stream reliably says *whether a
/// given command succeeded* is a separate, narrower claim tracked by
/// [`Capability::CommandOutcomeObservation`]. A driver can legitimately
/// declare `Rich` here (dense per-tool boundaries) while leaving that
/// capability undeclared (no reliable per-command outcome) — see that
/// capability's doc for why Codex is exactly this case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressFidelity {
    /// Per-tool events plus lifecycle. Claude provides this from its hook
    /// stream — `PreToolUse`/`PostToolUse` give per-tool granularity. A
    /// stdout-JSONL driver whose stream carries the same per-tool-call
    /// boundary (e.g. `item.started`/`item.completed`) also declares this
    /// tier — the tier is about resolution, not transport. It is *not* a
    /// claim about per-command outcome fidelity; see
    /// [`Capability::CommandOutcomeObservation`].
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

/// Where a driver's hook-callback wiring is written so the agent actually
/// reads it.
///
/// [`ProgressIngress::HookCallback`] carries a hooks map but does not, by
/// itself, say *where* that map goes. The engine's settings-file renderer
/// used to merge every hook-callback map into the Claude worker settings
/// file — correct for Claude, silently wrong for a driver whose agent
/// reads hooks from its own home (the forwarder and the interception
/// guards would both land in a file the agent never opens). The
/// destination makes the engine's merge conditional on a declared property
/// rather than on the variant alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookWiringDestination {
    /// Merge the hooks map (and any interception guards layered onto
    /// `PreToolUse`) into the worker settings file the engine renders
    /// (Claude's `settings.json` via `--settings`).
    #[default]
    WorkerSettingsFile,
    /// The driver writes the wiring itself (e.g. into a per-run home the
    /// agent reads). The engine must not merge hooks or interception
    /// guards into the settings file.
    DriverOwned,
}

/// A driver's event-source wiring for [`Capability::ProgressObservation`]
/// when that source is [`ProgressIngress::HookCallback`].
///
/// `hooks` is the hooks map that routes every lifecycle + tool hook event
/// to the `boss-event` shim, which forwards each payload to the engine
/// events socket. [`Self::destination`] declares where that map is written
/// so the engine's settings-file merge is conditional on the declaration
/// rather than on the variant alone.
#[derive(Debug, Clone, Default)]
pub struct ProgressObservationWiring {
    /// Hook-event name → array of hook entries. Claude wires all seven
    /// lifecycle events to the forwarder; the caller may extend the
    /// `PreToolUse` entry with interception guards (a separate capability)
    /// when [`Self::destination`] is [`HookWiringDestination::WorkerSettingsFile`].
    pub hooks: serde_json::Map<String, serde_json::Value>,
    /// Where this wiring is written so the agent reads it.
    pub destination: HookWiringDestination,
}

/// A run-correlated JSONL file source owned by the engine.
///
/// `directory` is a driver-resolved, run-private root. The engine snapshots
/// matching files before pane spawn, then accepts exactly one new file whose
/// `session_meta.payload.cwd` matches `workspace_path`. That two-part
/// correlation prevents a stale transcript from an earlier process from
/// being attached to the current execution.
///
/// Serializable because the engine persists the resolved descriptor with the
/// run's ingress checkpoint: an engine that restarts mid-run re-establishes
/// the tail from the descriptor the *spawn* resolved rather than re-deriving
/// one from a config it would have to reconstruct after the fact.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentJsonlFileIngress {
    pub directory: PathBuf,
    pub filename_prefix: String,
    pub filename_suffix: String,
    pub workspace_path: PathBuf,
}

/// The transport a driver's [`Capability::ProgressObservation`] event source
/// rides. Three disjoint transports exist (see
/// `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`):
///
/// - Claude wires a hooks map ([`ProgressObservationWiring`]) that fans every
///   lifecycle/tool event out to the `boss-event` shim, which forwards each
///   payload to the engine's events-socket ingress. The wiring's
///   [`HookWiringDestination`] declares whether the engine merges that map
///   into the worker settings file or the driver writes it itself.
/// - Codex has no equally robust hook signal for *progress*: its hook trust
///   model fails open and silently on an untrusted/misconfigured hook (no
///   error, no log line — reproduction 2 in the decision doc), which is
///   disqualifying for a liveness signal specifically. Engine-spawned
///   processes can use their raw stdout JSONL. Pane-hosted Codex workers use
///   the run-private rollout JSONL file instead, because the engine does not
///   own Ghostty's pty master. Both byte streams feed the same generic JSONL
///   reader and shared event fan-out; only their driver normalizers differ.
///
/// A driver without hook-callback wiring returns the appropriate byte-stream
/// arm here rather than an empty [`ProgressObservationWiring`] — the absence
/// of hooks is a distinct, named transport, not a degenerate hook case.
#[derive(Debug, Clone)]
pub enum ProgressIngress {
    /// Hook-callback transport. Where the hooks map is written is declared
    /// by [`ProgressObservationWiring::destination`] — the engine merges
    /// into the worker settings file only when that is
    /// [`HookWiringDestination::WorkerSettingsFile`].
    HookCallback(ProgressObservationWiring),
    /// Engine-owned process: parse the worker's stdout JSONL stream; no
    /// settings-file wiring is produced.
    StdoutJsonl,
    /// Pane-hosted agent: the engine tails one run-correlated JSONL file and
    /// feeds its raw bytes to the same reader used for stdout JSONL.
    AgentJsonlFile(AgentJsonlFileIngress),
}

/// Which provider dialect the generic JSONL reader is consuming.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgressStreamSource {
    #[default]
    StdoutJsonl,
    AgentJsonlFile,
}

/// Run-scoped context supplied when a JSONL progress reader starts.
///
/// Stream protocols commonly omit session identity after their first
/// envelope. Mutable correlation therefore belongs to the reader session,
/// not the registry's shared [`AgentDriver`] instance. `run_id` also lets a
/// driver resolve run-owned artifacts without scanning another run's state.
#[derive(Clone, Default)]
pub struct ProgressSessionConfig {
    pub run_id: Option<String>,
    pub identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    pub source: ProgressStreamSource,
    pub transcript_path: Option<PathBuf>,
    /// A snapshot previously produced by [`ProgressSessionNormalizer::resume_state`]
    /// for this same run, when the reader is being re-established over a
    /// stream the engine was already part-way through.
    ///
    /// `None` is the ordinary spawn-time case: a brand new session starting at
    /// the first byte. `Some` only ever appears on the readoption path, where
    /// starting fresh would be wrong in both directions — a zeroed
    /// guard-trace cursor re-announces decisions the run already reported, and
    /// an empty call tracker orphans every tool call whose output has not
    /// landed yet.
    pub resume_state: Option<serde_json::Value>,
}

impl std::fmt::Debug for ProgressSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressSessionConfig")
            .field("run_id", &self.run_id)
            .field("has_identity_store", &self.identity_store.is_some())
            .field("source", &self.source)
            .field("transcript_path", &self.transcript_path)
            .field("resuming", &self.resume_state.is_some())
            .finish()
    }
}

/// Engine-owned durable storage for one run's current provider session id.
///
/// The driver must not persist this identity inside an agent-writable home.
/// Implementations atomically compare and replace the one bounded identity
/// for `run_id`; `Ok(true)` means the same id was already present (resume).
pub trait ProgressIdentityStore: Send + Sync {
    fn claim_progress_identity(&self, run_id: &str, session_id: &str) -> Result<bool, String>;
}

/// Mutable normalizer owned by one progress ingress.
///
/// A registry driver may be shared by concurrent readers; implementations of
/// this trait are not shared. The reader invokes both methods in stream order,
/// so sticky session identity and transcript discovery cannot cross streams.
pub trait ProgressSessionNormalizer: Send {
    fn normalize_progress_event(&mut self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError>;

    /// Normalize one provider envelope into its ordered worker-event fanout.
    ///
    /// Most envelopes map one-to-one and use this default. A provider may
    /// override the batch form when one wire event carries two distinct parts
    /// of the shared contract—for example, a fatal error message followed by
    /// the authoritative turn-ending [`WorkerEvent::Stop`].
    fn normalize_progress_events(&mut self, raw: &serde_json::Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        self.normalize_progress_event(raw).map(|event| vec![event])
    }

    fn transcript_path_for_session(&mut self, raw: &serde_json::Value) -> Option<String>;

    /// Everything this session would lose if the engine process died and the
    /// same stream had to be picked up again mid-run.
    ///
    /// The engine snapshots this next to the stream's durable byte offset,
    /// event by event, so the two can never disagree about which record the
    /// state describes. A driver whose session carries no cross-record
    /// correlation returns `None` and is resumed by offset alone.
    fn resume_state(&self) -> Option<serde_json::Value> {
        None
    }

    /// Re-seed this session from a snapshot [`Self::resume_state`] produced
    /// for the same run.
    ///
    /// Only ever called with a state the same driver emitted, so the default
    /// is an error rather than a silent no-op: a driver that snapshots state
    /// and cannot restore it would resume with a zeroed session, which is the
    /// exact failure this seam exists to prevent. Loud is correct here.
    fn restore_resume_state(&mut self, _state: &serde_json::Value) -> Result<(), String> {
        Err("this progress session declares no resumable state".to_owned())
    }
}

/// Mutable normalizer owned by one transcript consumer.
///
/// Some transcript dialects put tool input and output in separate records.
/// Per-tail state correlates those records without leaking call ids across
/// concurrently running slots.
pub trait TranscriptSessionNormalizer: Send {
    fn normalize_transcript_entry(&mut self, raw: serde_json::Value) -> serde_json::Value;
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
/// - **Codex** emits native, typed terminal envelopes on its `--json` stdout
///   stream (`turn.completed`, `turn.failed`, and unrecoverable `error`;
///   verified against codex-cli 0.145.0; see
///   `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`).
///   Its driver decodes that to [`WorkerEvent::Stop`] in
///   [`AgentDriver::normalize_progress_event`] and reports the boundary
///   here, preserving a fatal diagnostic immediately before the `Stop`.
///   Routing a native turn event through this method is the first-class path;
///   what a driver must *not* do is manufacture a Claude-shaped hook payload
///   to satisfy Claude-shaped plumbing behind the engine's back.
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
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
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
    /// Execution / run id — used by Codex to locate the Boss-owned per-run
    /// `CODEX_HOME` when arming hooks. Claude ignores this.
    pub run_id: Option<String>,
    /// Workspace path (Codex project trust / hooks observation cwd). Claude
    /// ignores this.
    pub workspace_path: Option<PathBuf>,
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
    /// `is_pr_url_binding_command` / `is_pr_url_finalization_command_str` /
    /// `is_revision_push_command` gates. Empty when the observation carries
    /// no command surface (the gates then reject).
    pub command: String,
}

/// Extract the shell command string from a `pr_url_capture_feed` `tool_input`:
/// Claude/Codex/Grok's `{ "command": "…" }` object shape, or a bare command
/// string for a normaliser that puts free text directly on `tool_input`.
/// Shared so the `command` key and bare-string fallback stay a single
/// cross-driver convention rather than three copies that can drift.
pub(crate) fn command_from_tool_input(tool_input: &serde_json::Value) -> String {
    tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .or_else(|| tool_input.as_str())
        .unwrap_or("")
        .to_owned()
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

    let command = command_from_tool_input(tool_input);

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
#[derive(Debug, Clone, Copy, bon::Builder)]
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
    /// Execution / run id. Codex uses this to resolve the Boss-owned per-run
    /// `CODEX_HOME` (see [`codex::codex_home_for_run`]). Claude ignores it.
    /// `None` only for fixtures that do not provision a real home.
    pub run_id: Option<&'a str>,
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

    /// Explicit remote-spawn declaration. Drivers default to host-local until
    /// they prove their per-run state can be provisioned remotely.
    fn remote_spawn_host_independent(&self) -> bool {
        false
    }

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

    /// Substrings the app uses to screen-scrape this driver's GhosttyKit
    /// pane for a fallback status pill until the first hook-driven
    /// `LiveWorkerState` arrives. Populated onto
    /// [`boss_protocol::SpawnWorkerPaneInput::pane_monitor`] at spawn.
    ///
    /// Default `None` — the app falls back to Claude's historical
    /// literals, so an older driver (or a headless one with no TUI
    /// chrome) keeps today's behaviour. Interactive drivers that own
    /// a distinctive surface override this with their own markers.
    fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
        None
    }

    // ── WorkspaceProvisioning capability ────────────────────────────────────

    /// Write per-session workspace files (prompt file, agent-rules, gitignore)
    /// and suppress the backend's first-run trust prompt.
    ///
    /// Returns optional [`DriverRuntimeState`] describing any per-run state
    /// the driver created *outside* the cube workspace (e.g. a future Codex
    /// driver's Boss-owned per-run `CODEX_HOME` or archive root). The engine
    /// persists that opaque payload on the execution and hands it back to
    /// [`AgentDriver::teardown_workspace`] on every termination path. Claude
    /// returns `None` — it creates no state outside the workspace. Drivers
    /// **must not** expect the engine to infer a home from the process
    /// environment or by scanning a shared provider directory; if cleanup
    /// needs a path, return it here.
    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>>;

    /// Tear down whatever per-run state the driver created *outside* the cube
    /// workspace — a per-worker config/cache dir, a socket, a temp credential
    /// file. Paired with [`AgentDriver::provision_workspace`], but not its
    /// mirror: this must NOT touch anything under `workspace` itself, since
    /// cube owns that checkout's lifecycle.
    ///
    /// `workspace` is informational only (some implementations may use it to
    /// namespace their own state). `run_id` identifies the execution. The
    /// authoritative cleanup handle is `runtime_state` — the opaque payload
    /// this driver returned from a prior [`Self::provision_workspace`] call
    /// for the same execution, reloaded from the execution row. When
    /// `runtime_state` is `None` (Claude, pre-migration rows, or a provision
    /// that returned no state), the driver must no-op rather than invent a
    /// cleanup target by scanning a shared provider home or reading the
    /// engine environment. Callers pass `workspace = None` when the path is
    /// unknown (never recorded, or already cleared by a racing teardown)
    /// rather than skipping the call.
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
    async fn teardown_workspace(
        &self,
        workspace: Option<&Path>,
        run_id: &str,
        runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()>;

    /// Pre-accept this driver's first-run trust/folder-approval dialog for
    /// `workspace`, so a headless worker never blocks on it. Called by
    /// `write_workspace_files` on every spawn, after [`Self::provision_workspace`]
    /// has already run.
    ///
    /// Default: no-op. Most drivers stamp their own trust as part of
    /// `provision_workspace` and need nothing here — Codex writes
    /// `trust_level = "trusted"` into its own `config.toml`, Grok stamps
    /// `trusted_folders.toml` in its per-run `GROK_HOME`. `ClaudeDriver` is
    /// the one override: its trust record lives in the user-global
    /// `~/.claude.json`, outside any per-run home `provision_workspace`
    /// creates, so it needs this second seam.
    fn pre_trust_workspace(&self, _workspace: &Path) {}

    /// Content for the catch-all `.gitignore` `write_workspace_files` drops
    /// into this driver's `config_dir`, hiding every engine-written
    /// per-worker file there (agent-rules file, this `.gitignore` itself)
    /// from `jj status` / `git status`.
    ///
    /// Default: a single self-excluding `*` pattern, sufficient for every
    /// driver today. Override only if a driver ever needs a narrower
    /// pattern.
    fn config_dir_gitignore(&self) -> &'static str {
        "*\n"
    }

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
    /// returns the byte-stream ingress appropriate to its topology
    /// (`StdoutJsonl` or `AgentJsonlFile`) instead.
    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress;

    /// Decode one raw event-source payload into a typed [`WorkerEvent`] that
    /// drives the (driver-agnostic) activity machine. For the Claude driver
    /// the raw payload is a hook JSON object; this delegates to
    /// [`boss_protocol::normalize_hook_event`].
    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError>;

    /// Create mutable correlation state for one stdout ingress.
    ///
    /// Hook-callback drivers return `None` and keep using the stateless
    /// methods above. A stdout driver whose wire omits session identity after
    /// its first record returns a fresh normalizer here; the generic reader
    /// owns it for exactly one stream.
    fn progress_session(&self, _config: &ProgressSessionConfig) -> Option<Box<dyn ProgressSessionNormalizer>> {
        None
    }

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

    /// Absolute path [`crate::worker_setup::write_workspace_files`] (in
    /// `engine/core`) must write the rendered agent-rules body to, so the
    /// driver's own agent actually reads it.
    ///
    /// Default: `<workspace>/<descriptor.config_dir>/<descriptor.agent_rules_filename>`
    /// (e.g. Claude's `.claude/CLAUDE.md`) — correct whenever the driver reads
    /// its rules file from its workspace-local config dir. A driver that reads
    /// rules from somewhere else entirely (e.g. Codex, which reads
    /// `AGENTS.md` from the workspace root or `$CODEX_HOME`, never from
    /// `.codex/`) must override this rather than silently writing a file its
    /// own agent never opens.
    fn agent_rules_destination(&self, workspace: &Path, _run_id: &str) -> PathBuf {
        let descriptor = self.descriptor();
        workspace
            .join(descriptor.config_dir)
            .join(descriptor.agent_rules_filename)
    }

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

    /// Create mutable correlation state for one transcript tail.
    fn transcript_session(&self) -> Option<Box<dyn TranscriptSessionNormalizer>> {
        None
    }

    /// Canonical root a local transcript must remain beneath at every read.
    ///
    /// `Ok(None)` is the legacy unrestricted path used by drivers whose
    /// transcript path is itself authoritative. A contained driver returns a
    /// root; an invalid or replaced run home returns `Err` so callers reject
    /// the transcript rather than falling back to an unrestricted open.
    fn transcript_containment_root(&self, _run_id: &str) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }

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
    //
    // probe / interrupt / stop / reap / classify-error. Each verb is a
    // declarative plan the engine executes over its own transport (pane
    // RPCs, process signals). Defaults are the safe answers: unsupported
    // for probe/interrupt (fire-and-forget), process-only stop, process-
    // group reap. Override only with evidence about the actual process.

    /// Classify a raw error string from the worker's output for
    /// transient-recovery decisions. Provider-specific: must not route
    /// through another driver's classifier.
    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass;

    /// How the engine should deliver a probe (inject text) into a live
    /// worker of this driver. Defaults to [`ProbeDelivery::Unsupported`].
    fn probe(&self) -> ProbeDelivery {
        ProbeDelivery::Unsupported
    }

    /// How the engine should interrupt an in-flight turn. Defaults to
    /// [`InterruptDelivery::Unsupported`].
    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::Unsupported
    }

    /// The recipe for interrupting one in-flight turn on this driver: which
    /// key, how many presses, how long to wait, how many attempts, and what
    /// proves the turn ended.
    ///
    /// `None` — the default — is the declared property "this driver cannot be
    /// interrupted", and it must agree with [`Self::interrupt`]: a driver
    /// answering [`InterruptDelivery::Unsupported`] there must answer `None`
    /// here, and one naming a transport must supply a plan.
    /// [`crate::registry::DriverRegistry`] refuses to register a driver whose
    /// two answers disagree, so a caller may branch on either and get the
    /// same verdict.
    ///
    /// The point of separating this from `interrupt()` is that the two answer
    /// different questions. `interrupt()` names the *transport* the engine
    /// uses (pane keystroke vs. nothing); this names the driver-specific
    /// *timing and evidence* that make the transport actually work. Callers
    /// that only need "can this be interrupted at all" read `interrupt()`;
    /// the one that has to drive the gesture reads this.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        None
    }

    /// How the engine should stop a worker before process kill. Defaults to
    /// [`StopDelivery::ProcessOnly`] — process-level teardown always works.
    fn stop(&self) -> StopDelivery {
        StopDelivery::ProcessOnly
    }

    /// How the engine should reap a worker process. Defaults to
    /// [`ReapDelivery::ProcessGroup`] — every driver is a process the
    /// engine can signal.
    fn reap(&self) -> ReapDelivery {
        ReapDelivery::ProcessGroup
    }

    /// What this driver's foreground process does with pty bytes that arrive
    /// while it is **mid-turn** — the `probe --urgent` / `SendToPane`
    /// injection point.
    ///
    /// Defaults to [`MidTurnPaneInput::Rejects`], the safe answer: a driver
    /// that has not established what its foreground process does with
    /// mid-turn stdin must not have bytes written into it (see the type docs
    /// for the tty-leak this prevents). Override only with evidence about the
    /// actual process.
    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        MidTurnPaneInput::Rejects
    }

    /// Pre-interrupt snapshot for the bounded turn-end recovery the engine
    /// runs immediately after delivering an interrupt to this driver's
    /// worker — see [`InterruptRecoverySnapshot`] for why this must be
    /// captured *before* the interrupt is sent.
    ///
    /// Returns `None` when this driver's interrupt already produces a
    /// normal turn boundary through the regular
    /// [`Capability::TurnBoundary`] channel and needs no recovery — the
    /// default, correct for Claude and Codex, whose Esc-cancelled turn
    /// (or equivalent) still fires the driver's ordinary turn-ended
    /// signal. `run_id` is the engine's run/execution id, from which a
    /// driver that needs recovery resolves its own per-run state (Grok:
    /// `GROK_HOME`, session id, workspace) without the engine knowing
    /// anything about that shape.
    fn prepare_interrupt_recovery(&self, _run_id: &str) -> Option<InterruptRecoverySnapshot> {
        None
    }

    /// Whether one raw JSONL record from
    /// [`InterruptRecoverySnapshot::events_path`] is this driver's
    /// cancelled-turn-end evidence. Called once per new complete line the
    /// engine's bounded tail reads; a line this driver's recovery format
    /// does not recognise (noise, an unrelated event type) must return
    /// `false` rather than be treated as a parse failure, so the tail
    /// keeps reading up to the settle window.
    ///
    /// Only meaningful for a driver that returned `Some` from
    /// [`Self::prepare_interrupt_recovery`] — the default `false` is
    /// never consulted for a driver that returns `None` there.
    fn is_interrupt_recovery_turn_end(&self, _raw: &serde_json::Value) -> bool {
        false
    }

    /// How long this driver's foreground worker process lives relative to the
    /// turns it serves — consulted by every engine reaper that keys on process
    /// liveness before it concludes a vanished process means a dead worker.
    ///
    /// Defaults to [`WorkerProcessLifetime::Persistent`], which reproduces the
    /// pre-existing behaviour exactly: any exit is a death and is reaped. See
    /// [`WorkerProcessLifetime::OneTurnPerProcess`] for why no driver may
    /// declare otherwise today.
    fn worker_process_lifetime(&self) -> WorkerProcessLifetime {
        WorkerProcessLifetime::Persistent
    }

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
pub mod codex;
pub mod grok;
pub mod registry;

// Also available in-module for trait signatures (pub use is an import + re-export).
pub use boss_protocol::DriverRuntimeState;
pub use claude::ClaudeDriver;
pub use codex::CodexDriver;
pub use grok::GrokDriver;
pub use registry::{DriverRegistry, UnknownDriverSlug};

pub mod test_support;

#[cfg(test)]
mod tests;
