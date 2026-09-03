//! `CodexDriver` — OpenAI Codex agent driver.
//!
//! Implements spawn + Boss-owned per-run `CODEX_HOME` provisioning and the
//! native rollout-JSONL progress normaliser (`codex exec` writes
//! `CODEX_HOME/sessions/**/rollout-*.jsonl` unconditionally; the driver
//! tails that file rather than reading `--json` stdout — see
//! `progress_observation_wiring`).
//!
//! See `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`
//! (T-11 / capability declaration) and
//! `tools/boss/docs/investigations/ghostty-codex-pane-viability.md` Q2 for
//! the pane-launch buffered-tty footgun this spawn line closes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use boss_codex_auth::{
    AuthSnapshot, adopt_refresh_if_newer, resolve_operator_auth_path, snapshot_auth_into_codex_home,
};
use boss_engine_codex_hook_trust::{
    ArmRequest, CommandHookSpec, HookEvent, arm_and_attest, sha256_hex_prefixed, write_attestation_file,
};
use boss_engine_codex_rollout::flatten_tool_output_text;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::{FallbackCandidate, json_object_candidates};
use boss_protocol::{EffortLevel, NormalizeError, PaneMonitorSpec, ReasoningMode, ReviewModelTier, WorkerEvent};
use boss_ssh_transport::shell_quote;
use serde::{Deserialize, Serialize};

mod decision;
mod guard_chain;
pub mod guard_trace;
mod pane_monitor;
mod progress;
mod rollout_calls;
mod tool_surface_guard;

use guard_trace::{GUARD_TRACE_SHIM_FILENAME, GUARD_TRACE_SHIM_SCRIPT, guard_trace_path, wrapper_body};
use tool_surface_guard::CODEX_TOOL_SURFACE_GUARD_SCRIPT;

use crate::transcript_store::{
    durable_sessions_dir, provision_durable_sessions, transcript_store_root, verified_durable_sessions_dir,
};
use progress::{CodexRolloutProgressSession, CodexTranscriptSession, normalize_rollout, verified_sessions_root};

use super::claude::{
    BOSS_LAUNCH_GUARD_COMMAND, PR_REDIRECT_GUARD_COMMAND, REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND,
    REVISION_PR_GUARD_COMMAND,
};
use super::{
    AgentDriver, AgentJsonlFileIngress, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, EnvDirective,
    InterruptDelivery, InterruptGesture, InterruptPlan, MidTurnPaneInput, ModelMenu, PermissionArtifacts,
    PermissionInput, PrUrlCaptureFeed, ProbeDelivery, ProgressFidelity, ProgressIngress, ProgressObservationConfig,
    ProgressSessionConfig, ProgressSessionNormalizer, ReapDelivery, SpawnPlan, SpawnRequest, StopDelivery,
    StructuredOutputArtifacts, StructuredOutputRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring,
    TranscriptSessionNormalizer, TurnEnd, TurnEndEvidence, WorkerErrorClass, WorkerKind, WorkerProcessLifetime,
    default_structured_output_wiring,
};

/// Marker prefix on a [`WorkerEvent::Notification`] message emitted by this
/// driver's rollout progress session when a `command_execution` item
/// started (`item.started` / a rollout `function_call`) but never observed a
/// completion (`item.completed` / `function_call_output`) before its turn
/// boundary — reproduced in probe 6 of the exit-code investigation: a shell
/// command that outlives the model's chosen `yield_time_ms` with no further
/// polling leaves `turn.completed` firing (and `codex exec` exiting 0) with
/// no completion record for that command anywhere.
///
/// The engine's `codex_unobserved_command` module matches on this literal
/// prefix to stage the signal for `WorkerCompletionHandler`, which files an
/// attention item and refuses the worker's `NO_CHANGES_NEEDED` claim for the
/// rest of the run.
pub const UNOBSERVED_COMMAND_MARKER: &str = "[codex-unobserved-command]";

/// Render the notification message for one abandoned `command_execution`:
/// [`UNOBSERVED_COMMAND_MARKER`], a single space, then the bare command
/// verbatim — nothing else. Deliberately terse (no embedded explanatory
/// prose) so the engine-side consumer can recover the exact command with a
/// plain `strip_prefix` + trim; a command can itself contain colons or other
/// punctuation a more descriptive template might be split on ambiguously.
pub(crate) fn unobserved_command_notification(command: &str) -> String {
    format!("{UNOBSERVED_COMMAND_MARKER} {command}")
}

/// Marker prefix on a [`WorkerEvent::Notification`] carrying this turn's
/// `PreToolUse` guard activity: how many guards ran, what they decided, and
/// the reason head of every block or guard failure.
///
/// This is the answer to "did the guard fire for this execution?", which had
/// no answer at all on the Codex path before — Codex's rollout carries no hook
/// record, so an approved guard left no trace anywhere. Records come from
/// [`guard_trace`]'s per-run JSONL, written by the shim every materialised
/// guard is invoked through.
pub const GUARD_TRACE_MARKER: &str = "[codex-guard-trace]";

/// Marker prefix on a [`WorkerEvent::Notification`] emitted when tool calls
/// have run and **no** guard invocation has been recorded for this run at all.
///
/// That is the observable signature of Codex's documented silent fail-open:
/// an untrusted hook is skipped with no stream event, and a handler that
/// cannot be executed produces no diagnostic. Both leave a run that looks
/// healthy while every guardrail is inert. Boss asserts in the worker prompt
/// that pushes are blocked, so this condition is a defect signal, not
/// bookkeeping.
///
/// Fires on either of two conditions, both meaning "guardrails not enforced":
///
/// - the armed guard chain is no longer on disk with the bytes Boss attested
///   ([`guard_chain`]), checked at every turn boundary. This is the condition a
///   long-lived session needs and a one-turn-per-process run never could:
///   arming happens once, and a session outlives it by hours;
/// - no guard invocation has been recorded for the run at all, while tool calls
///   ran. Kept run-scoped rather than per-turn because a code-mode cell that
///   invokes no inner tool fires no `PreToolUse` and would otherwise alarm on
///   every quiet turn — see `CodexRolloutProgressSession::guard_records_seen`.
pub const GUARDS_SILENT_MARKER: &str = "[codex-guards-silent]";

/// Render the guard-activity notification for one turn.
pub(crate) fn guard_trace_notification(summary: &guard_trace::GuardTraceSummary) -> String {
    format!("{GUARD_TRACE_MARKER} {}", guard_trace::render_summary(summary))
}

/// Render the notification for a run whose tool calls have produced no guard
/// record at all. `observed` is the number of tool calls seen in the rollout.
pub(crate) fn guards_silent_notification(observed: usize) -> String {
    format!(
        "{GUARDS_SILENT_MARKER} {observed} tool call(s) ran this turn and no PreToolUse guard \
         invocation has been recorded for this run; Boss's Codex guardrails may be disarmed (hook trust \
         stale, guard handler unexecutable, or hooks not reached). Treat command guardrails as \
         unenforced until this is explained."
    )
}

/// Render the notification for a run whose armed guard chain is no longer on
/// disk as attested.
///
/// Distinct from [`guards_silent_notification`] in detail only — same marker,
/// so the engine routes both to the same counter and the same `error` line —
/// because the operator action differs: this one names a chain that *was*
/// verified at arming and has since changed underneath a live session, and it
/// is true whether or not the turn happened to run a tool call.
pub(crate) fn guard_chain_broken_notification(detail: &str) -> String {
    format!(
        "{GUARDS_SILENT_MARKER} the armed PreToolUse guard chain is no longer intact on disk: \
         {detail}. Codex skips a hook it cannot execute silently and fails open, so every command \
         guardrail this run believes it has — push/PR redirection, the Boss-launch gate, the \
         checkleft pre-push gate, the data-directory gate — must be treated as unenforced from now on."
    )
}

// ---------------------------------------------------------------------------
// Codex model / effort menu
// ---------------------------------------------------------------------------
//
// Sourced from `codex debug models` on codex-cli 0.145.0 (2026-07-24 design
// spike; re-verified on this host for the skeleton row). Catalog snapshot:
//
//   gpt-5.6-sol          default=low     levels=low,medium,high,xhigh,max,ultra
//   gpt-5.6-terra        default=medium  levels=low,medium,high,xhigh,max,ultra
//   gpt-5.6-luna         default=medium  levels=low,medium,high,xhigh,max
//   gpt-5.5              default=medium  levels=low,medium,high,xhigh
//   gpt-5.4 / gpt-5.4-mini               levels=low,medium,high,xhigh
//   gpt-5.3-codex-spark  default=high    levels=low,medium,high,xhigh
//   codex-auto-review    (hidden)        levels=low,medium,high,xhigh
//
// `ModelMenu` is static function pointers today, so this is a baked snapshot
// rather than a live `codex debug models` parse. Per-model effort filtering
// (only expose rungs the *selected* model supports) is follow-on work under
// the ModelAndEffortMenu gap — Boss's five [`EffortLevel`]s already fit
// inside every listed model's ladder, and `ultra` has no [`EffortLevel`] to
// map from (see [`ModelMenu::effort_value_for_level`]).

/// Map a Boss effort level onto Codex's reasoning-effort vocabulary.
///
/// Mirrors Claude's five-rung ladder so operator-facing effort names stay
/// consistent across drivers. Codex's sixth rung (`ultra` on `gpt-5.6-sol` /
/// `gpt-5.6-terra`) is unreachable through [`EffortLevel`] by design.
fn codex_effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    })
}

/// Capability-lever model choice. `terra` is the well-articulated coding tier;
/// `sol` is the frontier model reserved for investigation/design work —
/// analogous to Claude's sonnet/opus split.
fn codex_model_for_reasoning(reasoning: ReasoningMode) -> &'static str {
    match reasoning {
        ReasoningMode::Standard => "gpt-5.6-terra",
        ReasoningMode::Investigation => "gpt-5.6-sol",
    }
}

/// Concrete model mapping for metadata-derived review tiers. The Luna → Terra
/// → Sol progression is review-only and does not infer anything from task
/// effort or reasoning.
fn codex_review_model_for_tier(tier: ReviewModelTier) -> &'static str {
    match tier {
        ReviewModelTier::Fast => "gpt-5.6-luna",
        ReviewModelTier::Balanced => "gpt-5.6-terra",
        ReviewModelTier::Strong => "gpt-5.6-sol",
    }
}

/// Legacy size-derived table. Consulted only for rows with no
/// [`ReasoningMode`]. Keeps untagged rows on the frontier default rather than
/// inventing a size→model progression Codex has not validated.
fn codex_default_model_for_level(_level: EffortLevel) -> &'static str {
    "gpt-5.6-sol"
}

fn codex_prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(
            "Begin with a written plan. Identify the files you expect to touch and the \
             order you'll touch them in. Confirm the approach against the work item's \
             description before writing code.",
        ),
    }
}

/// Codex has no Claude-style "auto permissions" model family. Always `false`.
fn codex_model_requires_auto_permissions(_model: &str) -> bool {
    false
}

/// Returns `true` iff `model` names a Codex model — the `gpt-5.*`/`gpt-4.*`
/// SKU family `codex debug models` lists, plus the hidden `codex-auto-review`
/// SKU. Case-insensitive. Guards against a Claude/Grok family alias (e.g.
/// `"opus"`) reaching the Codex CLI verbatim.
fn codex_model_belongs_to_driver(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("gpt-") || lower == "codex-auto-review"
}

/// Session-scoped tmux config sourced into every codex worker's tmux session
/// at spawn — see `codex-tmux.conf` for why codex needs `mouse on`. Applied
/// via `crate::tmux_session_config_for` / `spawn_flow::start_tmux_worker` in
/// `tools/boss/engine/core`.
pub const CODEX_TMUX_SESSION_CONFIG: &str = include_str!("codex-tmux.conf");

static CODEX_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "codex",
    label: "OpenAI Codex",
    binary: "codex",
    config_dir: ".codex",
    agent_rules_filename: "AGENTS.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        // Highest-priority model in `codex debug models` (0.145.0): frontier
        // agentic coding. Step-5 fall-through only — classified rows resolve
        // through `model_for_reasoning`.
        engine_default: "gpt-5.6-sol",
        effort_value_for_level: codex_effort_value_for_level,
        default_model_for_level: codex_default_model_for_level,
        model_for_reasoning: codex_model_for_reasoning,
        review_model_for_tier: codex_review_model_for_tier,
        design_investigation_model: Some(|| "gpt-5.6-sol"),
        prompt_addendum_for_level: codex_prompt_addendum_for_level,
        model_requires_auto_permissions: codex_model_requires_auto_permissions,
        model_belongs_to_driver: codex_model_belongs_to_driver,
    },
};

/// Preamble for the agent-rules file (`AGENTS.md`). Names Codex observability
/// rather than Claude hooks so the shared body below it is not lying about
/// the mechanism this session uses.
///
/// The two Codex-specific tool rules are stated here because they are enforced
/// by a guard that can only refuse, never explain in advance
/// ([`tool_surface_guard`]): both routes are ones Boss's command guardrails
/// cannot observe, so a worker that reaches for them gets a hard block. Saying
/// so up front turns a wasted turn into a known constraint.
///
/// The long-command rule is also Codex-specific. `exec_command`'s initial
/// yield is capped at 30 seconds by codex-cli 0.145.0 and has no config/env/CLI
/// override (`--strict-config` rejects the corresponding candidate keys).
/// Unified exec does, however, return a session id and supports empty
/// `write_stdin` polls up to 300 seconds. The injected example keeps those
/// polls inside the originating JavaScript cell: the rollout correlator
/// observes that cell through its `wait` continuations and ultimately receives
/// the command's real `exit_code`; a separate polling cell has no attributable
/// command.
const CODEX_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via the Codex rollout JSONL file in this run's isolated CODEX_HOME.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.\n\
     \n\
     Two tool routes are blocked in this session, because Boss's command\n\
     guardrails cannot see them:\n\
     \n\
     - **Do not use `mcp__*` app tools** (the Codex GitHub/Gmail/Drive\n\
     connectors). Use the shell instead: `gh` for GitHub reads,\n\
     `cube pr create` / `cube pr update` for pull requests, `jj` for VCS.\n\
     - **Do not start interactive, stdin-driven sessions** (a bare `bash` /\n\
     `sh -s` / `python3` REPL, an editor, a pager). Commands typed into those\n\
     sessions are invisible to the guardrails.\n\
     Run each command as its own shell invocation instead:\n\
     `bash -lc '<command>'`, `python3 -c '<code>'`, `python3 <script.py>`,\n\
     `sqlite3 <db> '<sql>'`.\n\
     \n\
     To diagnose any command, use only that invocation's output and logs;\n\
     never infer ownership or blockage from global process-name matches.\n\
     \n\
     For any ordinary command expected to exceed roughly ten seconds, keep its\n\
     session handle and poll it to completion in the same JavaScript cell.\n\
     `exec_command` yields after at most 30 seconds; a result containing\n\
     `session_id` means the command is still running — never that it passed or\n\
     failed. `text(r.output)` discards that handle, so use this pattern instead:\n\
     \n\
     ```js\n\
     let r = await tools.exec_command({cmd: \"bazel test //backend/blob:blob_test //backend/admintasks:admintasks_test\",\n\
     workdir: \"…\", yield_time_ms: 30000, max_output_tokens: 20000});\n\
     while (!(\"exit_code\" in r)) {\n\
       r = await tools.write_stdin({session_id: r.session_id, chars: \"\",\n\
         yield_time_ms: 300000, max_output_tokens: 20000});\n\
     }\n\
     text(JSON.stringify(r));\n\
     ```\n\
     \n\
     Empty `write_stdin(session_id, chars: \"\")` polling is explicitly\n\
     allowed for a session started by an ordinary command; repeat the poll in\n\
     that same cell with its returned `session_id` until the terminal result\n\
     carries `exit_code`. Writing commands through stdin is not. Do not end the\n\
     turn or claim a gate result without the real `exit_code`. Give a command\n\
     that might hang its own foreground timeout so expiry returns a nonzero\n\
     status instead of polling forever.\n\
     \n\
     **Do not invent a budget.** Nothing in this session meters your runtime,\n\
     your turns, or your tokens, and you are not the accountant for any of\n\
     them. Never infer a budget from your own context usage or from how long\n\
     the run has taken, and never shorten, narrow, defer, or abandon required\n\
     work — a full suite, a validation gate, a verification step — because it\n\
     \"would take too long\" or because the session feels long. Wait for slow\n\
     commands and poll them to their real `exit_code`. Ending the run with the\n\
     work unfinished is a failed run; a partial result reported as a blocker\n\
     is still a failed run. A blocker is reportable only when something\n\
     external forced it: a command that ran and failed, a missing credential,\n\
     a genuinely conflicting instruction. Say so plainly and show the\n\
     evidence when that happens — and complete the work when it has not.";

/// Single-pattern gitignore for the workspace-local `.codex/` config dir
/// (prompt + agent-rules copies). Engine-injected files must not appear in
/// `jj status` / `git status`.
const CODEX_DIR_GITIGNORE: &str = "*\n";

/// Env override for the root under which per-run `CODEX_HOME` directories
/// are created. Tests set this so homes land in a disposable temp tree.
pub const CODEX_HOMES_ROOT_ENV: &str = "BOSS_CODEX_HOMES_DIR";

/// Process-global lock for any test that mutates [`CODEX_HOMES_ROOT_ENV`].
/// Hold across the full set/clear of the env var so parallel crate tests
/// (engine_lib_test + driver_test) cannot race the process environment.
///
/// Prefer [`crate::test_support::codex_homes_override`] over taking this
/// lock by hand: it acquires the lock and sets the variable together, so a
/// call site cannot set the variable while forgetting the lock.
pub static CODEX_HOMES_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default leaf under the system temp when [`CODEX_HOMES_ROOT_ENV`] is unset.
const CODEX_HOMES_DIR_NAME: &str = "boss-codex-homes";

/// Filename of the hook-trust attestation JSON written next to the run home.
const HOOK_TRUST_ATTESTATION_FILENAME: &str = "hook-trust-attestation.json";

// ---------------------------------------------------------------------------
// Per-run CODEX_HOME path + runtime-state payload
// ---------------------------------------------------------------------------

/// Root directory that holds Boss-owned per-run `CODEX_HOME` trees.
/// Everything under each home is temporary except `sessions/`, which is a
/// link into Boss's durable per-execution transcript store.
///
/// Prefer [`CODEX_HOMES_ROOT_ENV`] when set (tests); otherwise
/// `$TMPDIR/boss-codex-homes`. Never the operator interactive `~/.codex`.
pub fn codex_homes_root() -> PathBuf {
    match std::env::var_os(CODEX_HOMES_ROOT_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(CODEX_HOMES_DIR_NAME),
    }
}

/// Sanitize `run_id` to a single path segment under the homes root.
///
/// Refuses empty ids (and ids that sanitize to empty): an empty segment would
/// make [`codex_home_for_run`] resolve to the homes root itself, which teardown
/// must never delete.
pub fn sanitize_run_id_for_home(run_id: &str) -> anyhow::Result<String> {
    if run_id.is_empty() {
        bail!("empty run_id refused for Boss-owned CODEX_HOME");
    }
    // Sanitize path segments: execution ids are already slug-like, but refuse
    // `..` / separators so a malformed id cannot escape the homes root.
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        bail!("run_id {run_id:?} sanitized to empty; refused for Boss-owned CODEX_HOME");
    }
    Ok(safe)
}

/// Absolute path of the Boss-owned per-run `CODEX_HOME` for `run_id`.
///
/// Deterministic so [`CodexDriver::spawn_invocation`] and
/// [`CodexDriver::provision_workspace`] agree without threading the path
/// through [`SpawnRequest`]. Never points at the interactive Codex home.
///
/// # Errors
///
/// Returns an error for empty / unsafe `run_id` values that would resolve to
/// the homes root (see [`sanitize_run_id_for_home`]).
pub fn codex_home_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(codex_homes_root_and_home_for_run(run_id)?.1)
}

/// The homes root and the per-run `CODEX_HOME` beneath it, from a *single*
/// read of [`CODEX_HOMES_ROOT_ENV`].
///
/// Callers that need both values must use this rather than pairing their own
/// [`codex_homes_root`] call with [`codex_home_for_run`]: reading the root
/// twice lets a change to the env between the two reads (only tests mutate it,
/// via `test_support::codex_homes_override`) produce a root and a home that
/// disagree, which makes containment checks — here and in
/// [`verified_sessions_root`] — reject a perfectly valid run id.
///
/// # Errors
///
/// Returns an error for empty / unsafe `run_id` values that would resolve to
/// the homes root (see [`sanitize_run_id_for_home`]).
pub fn codex_homes_root_and_home_for_run(run_id: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    let safe = sanitize_run_id_for_home(run_id)?;
    let root = codex_homes_root();
    let home = root.join(safe);
    // Logical containment: a join of a single segment under an absolute root
    // always starts with that root; keep the check so a future root change
    // cannot silently open an escape.
    if !home.starts_with(&root) || home == root {
        bail!(
            "resolved CODEX_HOME {} is not a strict child of homes root {}",
            home.display(),
            root.display()
        );
    }
    Ok((root, home))
}

/// Sandbox mode for Codex `exec --sandbox` from Boss's abstract worker kind.
///
/// Reviewer uses an OS-enforced workspace-write sandbox whose working root is
/// the engine-owned structured-output directory, never the checkout. This
/// grants the single report-body write required for `boss propose` while the
/// reviewed workspace remains outside every writable root.
///
/// Every other kind is gated by the `codex_sandbox_enforced` feature flag
/// (default off): Codex's seatbelt template hardcodes a mach-service
/// allowlist that excludes LaunchServices, so `xcode-locator` fails under
/// `workspace-write` and every bazel build using `apple_support`'s crosstool
/// breaks with it — see `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`.
/// With the flag off, Standard/Triage/AnswerAgent get `danger-full-access`,
/// the same no-OS-sandbox posture the Claude driver has always run workers
/// at (`claude.rs`'s `--permission-mode auto`); the advisory
/// `PATH_GUARD_SCRIPT` PreToolUse hook remains the Boss-data-dir fence
/// either way. Single source of truth for
/// [`CodexDriver::write_permission_config`]'s `extra_args` — the spawn plan's
/// default is overridden when pane_spawn applies those args.
pub fn codex_sandbox_for_worker_kind(worker_kind: WorkerKind, sandbox_enforced: bool) -> &'static str {
    match worker_kind {
        WorkerKind::Reviewer => "workspace-write",
        WorkerKind::Standard | WorkerKind::Triage | WorkerKind::AnswerAgent => {
            if sandbox_enforced {
                "workspace-write"
            } else {
                "danger-full-access"
            }
        }
    }
}

/// CLI `extra_args` that encode sandbox policy for the spawn flow.
pub fn codex_sandbox_extra_args(worker_kind: WorkerKind, sandbox_enforced: bool) -> Vec<String> {
    vec![
        "--sandbox".into(),
        codex_sandbox_for_worker_kind(worker_kind, sandbox_enforced).into(),
    ]
}

/// Extra Codex CLI arguments for a reviewer's output-only sandbox.
///
/// The pane itself still starts in the leased checkout, but Codex's `--cd`
/// changes its sandbox working root to engine scratch before it processes any
/// tool calls. Consequently `workspace-write` permits the report body file
/// while the checkout stays OS read-only. The reviewer prompt already names
/// the checkout explicitly for all source inspection.
fn reviewer_output_sandbox_extra_args(output_dir: &Path) -> Vec<String> {
    vec!["--cd".to_owned(), output_dir.display().to_string()]
}

/// Reclaim a Boss-owned per-run `CODEX_HOME` after retention policy says it
/// is eligible. Refuses anything outside [`codex_homes_root`]. Idempotent
/// when the path is already gone. Used by the engine retention sweep —
/// **not** by interactive `~/.codex` scanning and not by cwd heuristics.
pub fn reclaim_codex_home(codex_home: &Path) -> anyhow::Result<()> {
    assert_codex_home_safe_to_delete(codex_home)?;
    if !codex_home.exists() {
        return Ok(());
    }
    // Re-check after exists: race with another reclaim is fine (NotFound).
    assert_codex_home_safe_to_delete(codex_home)?;
    match fs::remove_dir_all(codex_home) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing Boss-owned CODEX_HOME {}", codex_home.display())),
    }
}

/// Refuse to delete a path unless it is a strict, canonicalized child of the
/// Boss-owned homes root. Prevents an empty/malicious `codex_home` in
/// persisted runtime state from wiping the shared root or an unrelated tree.
pub fn assert_codex_home_safe_to_delete(codex_home: &Path) -> anyhow::Result<()> {
    if codex_home.as_os_str().is_empty() {
        bail!("refusing teardown with empty codex_home path");
    }
    let root = codex_homes_root();
    if root.as_os_str().is_empty() {
        bail!("refusing teardown: Boss codex homes root is empty");
    }

    // Canonicalize the root when it exists so macOS `/var` → `/private/var`
    // does not false-negative `starts_with`. If the root has never been
    // created, fall back to the logical path.
    let root_canon = match fs::canonicalize(&root) {
        Ok(p) => p,
        Err(_) => root.clone(),
    };

    if !codex_home.exists() {
        // Nothing to delete; still require logical containment so a bad
        // payload is reported rather than silently no-op'd forever.
        if codex_home == root || codex_home == root_canon {
            bail!(
                "refusing teardown: codex_home {} equals homes root {}",
                codex_home.display(),
                root_canon.display()
            );
        }
        if !(codex_home.starts_with(&root) || codex_home.starts_with(&root_canon)) {
            bail!(
                "refusing teardown: codex_home {} is outside homes root {}",
                codex_home.display(),
                root_canon.display()
            );
        }
        return Ok(());
    }

    let home_canon =
        fs::canonicalize(codex_home).with_context(|| format!("canonicalize CODEX_HOME {}", codex_home.display()))?;
    if home_canon == root_canon {
        bail!(
            "refusing to delete CODEX_HOME {} — equals Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    if !home_canon.starts_with(&root_canon) {
        bail!(
            "refusing to delete CODEX_HOME {} — outside Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    Ok(())
}

/// Opaque payload persisted on the execution as [`DriverRuntimeState`].
///
/// Carries everything teardown needs without scanning a shared provider home:
/// the Boss-owned home path, the auth snapshot identity, and the policy name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRuntimeState {
    pub codex_home: PathBuf,
    pub auth_source_path: PathBuf,
    pub auth_fingerprint: String,
    pub auth_policy: String,
}

impl CodexRuntimeState {
    pub fn from_snapshot(codex_home: PathBuf, snapshot: &AuthSnapshot) -> Self {
        Self {
            codex_home,
            auth_source_path: snapshot.source_path.clone(),
            auth_fingerprint: snapshot.fingerprint.as_str().to_owned(),
            auth_policy: snapshot.policy.as_str().to_owned(),
        }
    }

    pub fn to_driver_runtime_state(&self) -> DriverRuntimeState {
        DriverRuntimeState::new(serde_json::to_value(self).expect("CodexRuntimeState is serializable"))
    }

    pub fn from_driver_runtime_state(state: &DriverRuntimeState) -> anyhow::Result<Self> {
        serde_json::from_value(state.as_value().clone()).context("decoding CodexRuntimeState from DriverRuntimeState")
    }
}

// ---------------------------------------------------------------------------
// Base config.toml (trust + migration suppress; hooks appended later)
// ---------------------------------------------------------------------------

/// Render the non-hook portion of the per-run user `config.toml`.
///
/// - Stamps the cube workspace as trusted so the first-run trust dialog never
///   blocks a headless worker.
/// - Suppresses the external-agent (Claude Code) config-migration notice for
///   this home and this project, and pins the underlying memory-import
///   feature off, so a co-located `.claude/` from a prior Claude worker is
///   never imported into this home.
/// - Does **not** write hooks; those are appended by
///   [`write_hooks_and_attest`] after guard scripts are materialised, so the
///   trust gate hashes the exact final handler identity.
///
/// # Key provenance (codex-cli 0.145.0 / `openai/codex`)
///
/// `external_config_migration_prompts` is **not** a top-level field — it is
/// nested under `[notice]` as `notice.external_config_migration_prompts`, a
/// table with `home: Option<bool>` / `home_last_prompted_at` /
/// `projects: BTreeMap<String, bool>` / `project_last_prompted_at`
/// (`codex-rs/config/src/types.rs::ExternalConfigMigrationPrompts`). We set
/// both `home` and this project's entry in `projects` to `true` ("suppress
/// the prompt for this scope").
///
/// That struct only ever gates a **notice** shown by the interactive TUI /
/// app-server client (`codex-rs/tui/src/external_agent_config_migration*.rs`)
/// asking the user whether to import another agent's config — it does not
/// gate the import itself. The actual import path
/// (`codex-rs/app-server/src/external_agent_migration/processor.rs`) only
/// runs in response to an explicit `externalAgentConfig/detect` or `/import`
/// app-server request, which `codex exec` never sends, and is additionally
/// gated by the `external_agent_memory_import` feature flag — confirmed
/// `false` (disabled) by default via `codex features list` on 0.145.0. We
/// pin it `false` explicitly under `[features]` anyway: belt-and-suspenders
/// against a future default flip silently re-enabling import into a
/// Boss-owned home.
///
/// Verified behaviourally (not just "it parses"): a workspace carrying a
/// `.claude/CLAUDE.md` marker does not surface that content anywhere in
/// `codex debug prompt-input`'s model-visible output, with or without these
/// keys set — the import path is structurally unreachable from `codex exec`.
pub fn render_base_config_toml(workspace: &Path) -> String {
    render_config_toml(workspace, None, render_sandbox_workspace_write_toml(workspace))
}

/// Render Codex configuration for a reviewer whose sandbox root is engine
/// scratch rather than the checkout. The checkout remains trusted for source
/// inspection, while the scratch root is trusted because Boss creates it.
fn render_reviewer_base_config_toml(workspace: &Path, output_dir: &Path) -> String {
    render_config_toml(
        workspace,
        Some(output_dir),
        render_reviewer_sandbox_workspace_write_toml(output_dir),
    )
}

fn render_config_toml(workspace: &Path, sandbox_root: Option<&Path>, sandbox_workspace_write: String) -> String {
    // TOML basic-string escape for paths that may contain backslashes or quotes.
    let workspace_key = toml_basic_string(&workspace.display().to_string());
    let mut config = format!(
        "# Boss-owned per-run Codex config. Do not hand-edit; regenerated every dispatch.\n\
         \n\
         # Suppress the external-agent (Claude Code) config-migration notice\n\
         # for this home and project, and pin the memory-import feature off.\n\
         # Boss workspaces routinely contain a co-located `.claude/` from the\n\
         # Claude driver path; see render_base_config_toml's doc comment for\n\
         # why this is belt-and-suspenders rather than the actual gate.\n\
         [notice.external_config_migration_prompts]\n\
         home = true\n\
         \n\
         [notice.external_config_migration_prompts.projects]\n\
         {workspace_key} = true\n\
         \n\
         [features]\n\
         external_agent_memory_import = false\n\
         \n\
         {sandbox_workspace_write}\
         [projects.{workspace_key}]\n\
         trust_level = \"trusted\"\n\
         \n"
    );
    if let Some(sandbox_root) = sandbox_root.filter(|root| *root != workspace) {
        let sandbox_root_key = toml_basic_string(&sandbox_root.display().to_string());
        config.push_str(&format!("[projects.{sandbox_root_key}]\ntrust_level = \"trusted\"\n\n"));
    }
    config
}

/// `[sandbox_workspace_write]` table for the per-run `config.toml`.
///
/// Codex's `--sandbox workspace-write` default renders with no
/// `[sandbox_workspace_write]` table at all, so `network_access` and
/// `writable_roots` take Codex's own binary defaults: `false` and `[]`. That
/// denies the localhost TCP bind Bazel's client/server handshake needs
/// (`bazel build` aborts with a `java.net.SocketException`, and Bazel's own
/// shutdown path then hits `sysctl kern.proc.all` outside the seatbelt
/// allowlist, which is what actually surfaces as `FATAL: bazel crashed due to
/// an internal error` — a consequence of the socket failure, not an
/// independent gap) and denies writes to Bazel's cache directories, which sit
/// outside the workspace by default. See "Bazel under the Codex sandbox" in
/// `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md` for the
/// full repro.
///
/// `network_access = true` grants full outbound network, not a
/// localhost-only tier — Codex's `sandbox_workspace_write` schema is
/// two-valued (`restricted` / `enabled`) with no such tier. Bazel itself also
/// needs real egress here: bzlmod/module-registry fetches and (absent a
/// pinned `.bazelversion`) bazelisk's own version-resolution call both go out
/// over the network on a cold cache.
///
/// This table takes effect under `--sandbox workspace-write`, i.e. for
/// Standard/Triage/AnswerAgent when the `codex_sandbox_enforced` feature flag
/// is on. Reviewer has a separate minimal workspace-write configuration that
/// permits only its engine-owned output root. The default
/// `danger-full-access` path ignores this table (see
/// [`codex_sandbox_for_worker_kind`]).
///
/// `workspace` itself is granted write access by Codex's own cwd default,
/// separate from this function's `writable_roots` list, and does not need a
/// paired `workspace/.git` grant: cube workspaces are non-colocated secondary
/// jj workspaces (`.jj` pointer file, no `.git`) by construction, so there is
/// no colocated `.git` under `workspace` for the sandbox's auto-exclusion to
/// bite. (The retired `codex exec` shape needed `--skip-git-repo-check` to
/// dispatch into a `.git`-less workspace at all; that check — and the flag
/// that bypassed it — is `exec`-specific and does not exist on the bare TUI,
/// which never performs it.) Only the shared store root resolved by
/// [`cube_repo_store_root`] carries a real `.git`.
fn render_sandbox_workspace_write_toml(workspace: &Path) -> String {
    let mut out = String::from(
        "[sandbox_workspace_write]\n\
         network_access = true\n",
    );
    let mut roots = bazel_writable_roots();
    match cube_repo_store_root(workspace) {
        Some(root) => {
            // Codex's workspace-write sandbox name-excludes `.git` from every
            // writable root it renders (verified against codex-cli 0.145.0's
            // seatbelt template: each granted root gets a paired
            // `require-not (subpath ..._EXCLUDED_...)` clause covering its own
            // `.git`). Granting `root` alone lets `jj`/git-backend writes
            // under `.jj` succeed but leaves `root/.git/FETCH_HEAD` and
            // `root/.git/objects/*` denied with `Operation not permitted`,
            // which is exactly where `jj git fetch` and `jj new` write. An
            // explicit `root/.git` entry is its own top-level writable root,
            // so it is not subject to the auto-exclusion applied to `root`.
            let git_dir = root.join(".git");
            roots.push(root);
            roots.push(git_dir);
        }
        None if workspace.join(".jj").join("repo").is_file() => {
            tracing::warn!(
                workspace = %workspace.display(),
                "workspace has a .jj/repo pointer file but it did not resolve to a cube \
                 store root; the sandbox writable-roots grant will omit the shared jj/git \
                 store, which can reproduce 'Operation not permitted' failures on jj/git \
                 commands"
            );
        }
        None => {}
    }
    if !roots.is_empty() {
        let quoted: Vec<String> = roots
            .iter()
            .map(|r| toml_basic_string(&r.display().to_string()))
            .collect();
        out.push_str(&format!("writable_roots = [{}]\n", quoted.join(", ")));
    }
    out.push('\n');
    out
}

/// Reviewer sandbox configuration grants only `output_dir` for writes.
/// `network_access = true` is required despite the reviewer's read-only
/// intent: the reviewer's only report-delivery channel is `boss
/// propose review-report`, a Unix-domain-socket connect to the engine
/// control socket (`BossClient::connect_socket`), and Codex's seatbelt
/// classifies an `AF_UNIX connect()` as a `network-outbound` operation that
/// `network_access = false` denies — the same class of local-socket denial
/// this repo already documented for Bazel's TCP handshake (see
/// [`render_sandbox_workspace_write_toml`]'s doc comment). Granting it here
/// widens outbound network reachability, not filesystem writes: it does not
/// add to `writable_roots`, so the checkout stays outside every writable
/// root. `exclude_tmpdir_env_var` and `exclude_slash_tmp` narrow the
/// filesystem grant instead — without them Codex's workspace-write profile
/// additionally grants all of `$TMPDIR` and `/tmp`, which (since
/// `boss_engine_structured_output::default_dir()` is a `boss-worker-output`
/// subdirectory of `std::env::temp_dir()`, keyed by filename) would let the
/// reviewer overwrite concurrent executions' artifacts and reach the engine
/// control socket at `/tmp/boss-engine.sock` by filesystem write, not just
/// the sanctioned `boss propose` connect.
///
/// `output_dir` (the `--cd` target, see [`reviewer_output_sandbox_extra_args`])
/// is listed explicitly in `writable_roots` in addition to relying on
/// Codex's own cwd grant. This repo has not verified, against the installed
/// Codex build, whether `exclude_tmpdir_env_var`/`exclude_slash_tmp` are
/// applied as deny rules that could subsume a bare cwd grant nested under
/// `$TMPDIR`; an explicit `writable_roots` entry is the same defense already
/// relied on elsewhere in this file to keep a root out from under a
/// surrounding exclusion (see the `root.join(".git")` entry in
/// [`render_sandbox_workspace_write_toml`]). If report delivery ever starts
/// failing with no report and no diagnosis, check this assumption first.
fn render_reviewer_sandbox_workspace_write_toml(output_dir: &Path) -> String {
    format!(
        "[sandbox_workspace_write]\nnetwork_access = true\nexclude_tmpdir_env_var = true\nexclude_slash_tmp = true\nwritable_roots = [{}]\n\n",
        toml_basic_string(&output_dir.display().to_string())
    )
}

/// Resolve the writable roots Bazel needs outside the workspace.
///
/// Always includes Bazel's default `output_user_root` (the parent of the
/// per-workspace `output_base` holding the local Bazel server's state, action
/// cache, and sandboxed execroots) — mirroring Bazel's own client resolution
/// order rather than hardcoding a path: `TEST_TMPDIR` first (Bazel's
/// convention for a bazel-in-bazel test invocation providing its own scratch
/// root — the same convention Boss's own bazel-gated test suite for this
/// function relies on), then the platform cache-dir default Bazel falls back
/// to when no `--output_user_root` flag applies.
///
/// On macOS that default alone (`~/Library/Caches/bazel`) is not enough:
/// live verification against this repo showed non-fatal but noisy
/// `Operation not permitted` disk-cache write failures, because mono's own
/// root `.bazelrc` points `--disk_cache` at `~/.cache/bazelcache` — an
/// XDG-style path outside `~/Library/Caches` even on macOS. That convention
/// (shared dotfiles across Linux/macOS hosts pointing bazel cache flags at
/// `~/.cache`) is common enough that granting it isn't a one-repo special
/// case, so macOS additionally grants `~/.cache` outright, covering wherever
/// a repo's `.bazelrc` points `--disk_cache` / `--repository_cache` under it.
/// Non-macOS already resolves under `${XDG_CACHE_HOME:-~/.cache}` natively,
/// so no second root is needed there.
///
/// Returns an empty `Vec` when `HOME` is unset/empty, leaving `writable_roots`
/// unset so Codex falls back to its own `[]` default rather than a guessed
/// path.
fn bazel_writable_roots() -> Vec<PathBuf> {
    bazel_writable_roots_impl(
        std::env::var("TEST_TMPDIR").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
    )
}

/// Env-injected core of [`bazel_writable_roots`], so tests can exercise every
/// resolution branch without mutating process-global env (`HOME` in
/// particular is read by far too much shared test-process state — tempfile,
/// other threads' tests — to remove safely, even under the crate's
/// `ENV_LOCK` convention for its own Boss-owned env vars).
fn bazel_writable_roots_impl(
    test_tmpdir: Option<&str>,
    home: Option<&str>,
    xdg_cache_home: Option<&str>,
) -> Vec<PathBuf> {
    if let Some(dir) = test_tmpdir.filter(|d| !d.is_empty()) {
        return vec![PathBuf::from(dir)];
    }
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        return vec![home.join("Library/Caches/bazel"), home.join(".cache")];
    }
    match xdg_cache_home {
        Some(xdg) if !xdg.is_empty() => vec![PathBuf::from(xdg).join("bazel")],
        _ => vec![home.join(".cache/bazel")],
    }
}

/// Resolve the shared cube jj store root for `workspace`, if it is a cube
/// secondary jj workspace.
///
/// Every cube-leased workspace's `.jj/repo` is not a directory but jj's own
/// *pointer file* for a secondary workspace: its entire contents are the
/// path (jj writes it absolute) to the shared store, e.g.
/// `~/.local/share/cube/repos/<repo>/.jj/repo`. The pointer is written by
/// `jj workspace add` when cube attaches the workspace to the canonical
/// store, so the path is read from cube's actual layout rather than
/// assembled from a fixed prefix.
///
/// `jj commit`/`describe`/`bookmark create`/`git fetch` all write into this
/// shared store (table-store locks, refs, `FETCH_HEAD`) even though the
/// command runs from the leased workspace directory, which is a different
/// path entirely — see "Bazel under the Codex sandbox" in
/// `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`. This
/// returns the checkout root that owns the store (`<repos>/<repo>`, i.e.
/// `.jj`'s parent), not just `.jj/repo` itself, because a colocated `.git/`
/// sits alongside `.jj/` at that same level and needs the same write access
/// (e.g. `.git/FETCH_HEAD` on `jj git fetch`).
///
/// Returns `None` when `workspace` is not a cube secondary jj workspace: no
/// `.jj/repo` pointer file, or its contents don't have the expected
/// `.jj/repo` shape (plain/colocated dev checkouts, most test fixtures).
fn cube_repo_store_root(workspace: &Path) -> Option<PathBuf> {
    let jj_dir = workspace.join(".jj");
    let pointer = fs::read_to_string(jj_dir.join("repo")).ok()?;
    let pointer_path = PathBuf::from(pointer.trim());
    // jj resolves a relative `.jj/repo` pointer relative to the workspace's
    // own `.jj` directory, not the workspace root — mirror that here so a
    // relative pointer still yields an absolute, sandbox-usable root.
    let store_repo_dir = if pointer_path.is_absolute() {
        pointer_path
    } else {
        jj_dir.join(pointer_path)
    };
    if store_repo_dir.file_name()?.to_str()? != "repo" {
        return None;
    }
    let store_jj_dir = store_repo_dir.parent()?;
    if store_jj_dir.file_name()?.to_str()? != ".jj" {
        return None;
    }
    Some(normalize_lexically(store_jj_dir.parent()?))
}

/// Lexically collapse `.`/`..` components without touching the filesystem
/// (no symlink resolution, unlike [`Path::canonicalize`]), so a writable
/// root derived from a relative `.jj/repo` pointer comes out as a clean
/// absolute path rather than one still carrying `..` segments.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Spawn command (pane-safe)
// ---------------------------------------------------------------------------

/// Build the bare `codex …` command body — the interactive TUI, no
/// subcommand. Retired `codex exec` for the one-shape decision recorded in
/// `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`: two of the
/// three flags the old `exec` contract required are hard argument errors on
/// this shape, and the flag this shape needs is a hard argument error on
/// `exec` — "support both" is a spawn-line contract conflict, not a
/// configuration choice.
///
/// Contract (also enforced by `assert_codex_spawn_contract` in core):
/// - requires `--strict-config`, `--no-alt-screen`, and `-a never`
///   (`--no-alt-screen` disables the alternate screen so the viewport and a
///   full-screen read diverge and scrollback accumulates across turns,
///   instead of being capped at one screenful under the default alt-screen
///   mode — measured in the pivot spike (V2); `-a never` pins the approval
///   policy so a long-lived session never blocks on a human approval
///   prompt Boss cannot answer — Boss's own `--sandbox` policy is the real
///   authorization boundary)
/// - forbids `--color`, `--skip-git-repo-check`, and `--json` — each is a
///   hard argument error on the bare TUI (measured against `codex-cli
///   0.145.0` in the pivot spike): `--color`/`--skip-git-repo-check` were
///   required on the retired `exec` shape, and `--json` never existed on
///   either shape as anything but a hard error here.
///
/// No `< /dev/null` stdin redirect: that existed so `codex exec` would not
/// block reading stdin, and a TUI needs the tty to read typed input,
/// including the pane's own initial-prompt line.
pub fn build_codex_command(request: &SpawnRequest<'_>) -> String {
    let SpawnRequest {
        model,
        effort,
        // Codex does not take a Claude `--settings` file; settings live in
        // CODEX_HOME/config.toml. The path is ignored here.
        settings_path: _,
        // Claude-only model-family concept; inert for Codex.
        non_opus_auto_mode: _,
        permission_mode_override: _,
        run_id: _,
    } = request;

    let prompt_cat = format!(
        "\"$(cat {}/{})\"",
        CODEX_DESCRIPTOR.config_dir, CODEX_DESCRIPTOR.initial_prompt_filename,
    );

    // Baked-in fallback sandbox is workspace-write, but permission policy
    // confirms or replaces it via [`PermissionArtifacts::extra_args`] (see
    // `codex_sandbox_for_worker_kind`: Reviewer keeps workspace-write but
    // relocates its root to engine-owned output; every other kind gets
    // `--sandbox danger-full-access` unless the
    // `codex_sandbox_enforced` feature flag is on, in which case
    // `workspace-write`) applied by the spawn flow — do not hardcode a
    // second source of truth here without also applying extra_args.
    let mut cmd = String::from("codex --strict-config --no-alt-screen -a never --sandbox workspace-write");
    cmd.push_str(" -m ");
    // Model / effort tokens come from operator config and work-item metadata;
    // shell-quote so a future slug with spaces/metacharacters cannot break
    // the pane command line.
    cmd.push_str(&shell_quote(model));
    if let Some(e) = effort {
        // Per-model effort: `-c model_reasoning_effort=<level>`.
        cmd.push_str(" -c model_reasoning_effort=");
        cmd.push_str(&shell_quote(e));
    }
    cmd.push(' ');
    cmd.push_str(&prompt_cat);
    cmd.push('\n');
    cmd
}

// ---------------------------------------------------------------------------
// Hook wiring into CODEX_HOME (deny-only PreToolUse + trust attest)
// ---------------------------------------------------------------------------

/// One materialised PreToolUse guard as an absolute executable path.
///
/// The trust gate requires a real filesystem path (not an inline
/// `python3 -c` string): `arm_and_attest` content-binds and path-checks the
/// command. Wrappers live under `$CODEX_HOME/guards/`.
///
/// `pub` (fields included) so the config-schema conformance check in
/// `engine/core`'s `version_pin` can build the exact `append_hooks_toml`
/// input production uses, without re-implementing guard materialisation.
#[derive(Debug, Clone)]
pub struct MaterializedGuard {
    /// Absolute path written into `config.toml` `command = "…"`.
    pub command_path: PathBuf,
    /// Matcher for PreToolUse (`".*"` covers all tools; Bash-only where the
    /// Claude path used a Bash matcher).
    pub matcher: Option<&'static str>,
}

/// Materialise Boss guard scripts under `codex_home/guards/` and return the
/// absolute paths Codex will invoke.
fn materialize_guards(codex_home: &Path, config: &ToolUseInterceptionConfig) -> anyhow::Result<Vec<MaterializedGuard>> {
    let guards_dir = codex_home.join("guards");
    fs::create_dir_all(&guards_dir).with_context(|| format!("creating {}", guards_dir.display()))?;

    // Every guard below is invoked through this shim rather than directly, so
    // each decision lands in the run's guard trace and a guard that cannot
    // answer becomes a block instead of Codex's silent approval. See
    // [`guard_trace`].
    let shim = guards_dir.join(GUARD_TRACE_SHIM_FILENAME);
    write_executable(&shim, GUARD_TRACE_SHIM_SCRIPT)?;
    // The shim is the one file in the chain the attestation cannot bind (it
    // hashes each wrapper, the `command` path). Each wrapper — which *is*
    // bound — re-checks this digest before exec'ing the shim, so replacing the
    // shim can no longer neutralise every guard with the hashes still valid.
    let shim_sha256 = sha256_hex_prefixed(GUARD_TRACE_SHIM_SCRIPT.as_bytes());
    let trace_path = guard_trace_path(codex_home);

    let mut out = Vec::new();

    /// One guard to materialise: the Python that decides, plus how it is armed.
    struct Planned {
        name: &'static str,
        source: GuardSource,
        matcher: &'static str,
        extra_env: Vec<(&'static str, String)>,
    }
    /// Where a guard's executable comes from.
    enum GuardSource {
        /// Inline Python that must be written into `CODEX_HOME` first.
        Inline(String),
        /// A script the engine already wrote outside `CODEX_HOME` (the shared
        /// path / checkleft gates, which are not Codex-specific).
        Existing(PathBuf),
    }

    let mut planned: Vec<Planned> = Vec::new();

    // 1. Path guard — local workers only (script never ships to remotes).
    //    `.*` because it must also see Codex's `apply_patch` file edits, whose
    //    target paths live in the patch body rather than a `file_path` key.
    if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
        planned.push(Planned {
            name: "path_guard",
            source: GuardSource::Existing(guard_script.clone()),
            matcher: ".*",
            extra_env: vec![("BOSS_DATA_DIR", data_dir.display().to_string())],
        });
    }

    // 2. Boss-launch guard — always on. Materialise the Claude inline
    //    `python3 -c` body as a real .py so the trust gate can path-check it.
    planned.push(Planned {
        name: "boss_launch_guard",
        source: GuardSource::Inline(python_c_to_script(BOSS_LAUNCH_GUARD_COMMAND)?),
        matcher: "Bash",
        extra_env: Vec::new(),
    });

    // 3. Codex tool-surface guard — always on, and `.*` by necessity: it is
    //    the only guard that sees non-`Bash` tool names. Closes the two routes
    //    a command matcher structurally cannot reach — `mcp__*` app tools, and
    //    interactive sessions that `write_stdin` would drive with no hook of
    //    its own. See [`tool_surface_guard`].
    planned.push(Planned {
        name: "codex_tool_surface_guard",
        source: GuardSource::Inline(CODEX_TOOL_SURFACE_GUARD_SCRIPT.to_owned()),
        matcher: ".*",
        extra_env: Vec::new(),
    });

    // 4. PR redirect — Standard workers only.
    if config.is_standard_worker {
        planned.push(Planned {
            name: "pr_redirect_guard",
            source: GuardSource::Inline(python_c_to_script(PR_REDIRECT_GUARD_COMMAND)?),
            matcher: "Bash",
            extra_env: Vec::new(),
        });
    }

    // 5. Checkleft push guard — local Standard workers only.
    if config.is_standard_worker
        && let Some(checkleft_script) = &config.checkleft_guard_script
    {
        planned.push(Planned {
            name: "checkleft_push_guard",
            source: GuardSource::Existing(checkleft_script.clone()),
            matcher: "Bash",
            extra_env: Vec::new(),
        });
    }

    // 5. Reviewer static-analysis guard. The output-only sandbox preserves
    // checkout immutability; this independent guard blocks
    // build/test/format/generate and executable-code commands.
    if config.is_reviewer {
        planned.push(Planned {
            name: "reviewer_static_analysis_guard",
            source: GuardSource::Inline(python_c_to_script(REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND)?),
            matcher: "Bash",
            extra_env: Vec::new(),
        });
    }

    // 6. Revision PR guard.
    if config.is_revision {
        planned.push(Planned {
            name: "revision_pr_guard",
            source: GuardSource::Inline(python_c_to_script(REVISION_PR_GUARD_COMMAND)?),
            matcher: "Bash",
            extra_env: Vec::new(),
        });
    }

    for (index, guard) in planned.into_iter().enumerate() {
        let guard_name = format!("{index:02}_{}", guard.name);
        let guard_path = match guard.source {
            GuardSource::Inline(source) => {
                let script = guards_dir.join(format!("{guard_name}.py"));
                write_executable(&script, &source)?;
                script
            }
            GuardSource::Existing(path) => path,
        };
        // Content-bind the guard the shim will run. The trust gate hashes the
        // wrapper (the `command` path); this is what keeps the guard itself
        // bound, and the shim re-checks it on every invocation.
        let guard_sha256 = sha256_hex_prefixed(
            &fs::read(&guard_path).with_context(|| format!("reading guard {}", guard_path.display()))?,
        );
        let wrapper = guards_dir.join(format!("{guard_name}.sh"));
        write_executable(
            &wrapper,
            &wrapper_body(
                &shim,
                &shim_sha256,
                &guard_path,
                &guard_name,
                &guard_sha256,
                &trace_path,
                &guard.extra_env,
            ),
        )?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&wrapper).unwrap_or(wrapper),
            matcher: Some(guard.matcher),
        });
    }

    if out.is_empty() {
        bail!("CodexDriver refuses to arm zero PreToolUse guards (ToolUseInterception declared)");
    }
    Ok(out)
}

/// Extract the Python source from a Claude-style `python3 -c "…"` command
/// constant so it can live as a real `.py` file under CODEX_HOME.
fn python_c_to_script(command: &str) -> anyhow::Result<String> {
    // Constants are `python3 -c "\n…\n"` (possibly multi-line). Find the
    // opening quote after `-c` and take the rest minus the trailing quote.
    let Some(c_pos) = command.find("-c") else {
        bail!(
            "guard command is not python3 -c form: {}",
            &command[..command.len().min(40)]
        );
    };
    let after_c = command[c_pos + 2..].trim_start();
    let body = after_c
        .strip_prefix('"')
        .or_else(|| after_c.strip_prefix('\''))
        .ok_or_else(|| anyhow!("guard -c payload is not quoted"))?;
    let body = body
        .strip_suffix('"')
        .or_else(|| body.strip_suffix('\''))
        .unwrap_or(body);
    // Ensure the file is a proper script (shebang optional — we invoke via path).
    Ok(format!("#!/usr/bin/env python3\n{body}\n"))
}

fn write_executable(path: &Path, body: &str) -> anyhow::Result<()> {
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(())
}

/// Append `[[hooks.PreToolUse]]` entries for the materialised guards.
///
/// `pub`: the config-schema conformance check in `engine/core` calls this
/// directly (with a synthetic guard list) so it validates the exact same
/// hooks-appended document production writes, not a hand-rolled stand-in.
pub fn append_hooks_toml(base: &str, guards: &[MaterializedGuard]) -> String {
    let mut out = base.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Boss deny-only PreToolUse guardrails (ToolUseInterception).\n");
    for guard in guards {
        out.push_str("[[hooks.PreToolUse]]\n");
        if let Some(matcher) = guard.matcher {
            out.push_str(&format!("matcher = \"{matcher}\"\n"));
        }
        out.push_str("[[hooks.PreToolUse.hooks]]\n");
        out.push_str("type = \"command\"\n");
        out.push_str(&format!(
            "command = {}\n\n",
            toml_basic_string(&guard.command_path.display().to_string())
        ));
    }
    out
}

/// Write hook definitions into `codex_home/config.toml`, stamp trust, and
/// live-attest via `codex app-server` `hooks/list`. Refuses on silence.
///
/// Hooks are regenerated every call so the attested identity matches the
/// exact handlers Boss is about to arm. Guard scripts are materialised as
/// real executables under `$CODEX_HOME/guards/` (the trust gate path-checks
/// them; inline `python3 -c` is not accepted).
pub fn write_hooks_and_attest(
    codex_home: &Path,
    hook_cwd: &Path,
    base_config: &str,
    config: &ToolUseInterceptionConfig,
    codex_bin: &Path,
) -> anyhow::Result<()> {
    fs::create_dir_all(codex_home).with_context(|| format!("creating CODEX_HOME {}", codex_home.display()))?;

    let guards = materialize_guards(codex_home, config)?;
    let full = append_hooks_toml(base_config, &guards);
    let config_path = codex_home.join("config.toml");
    fs::write(&config_path, full).with_context(|| format!("writing {}", config_path.display()))?;

    // Prefer realpath form for ArmRequest so state keys match Codex on macOS
    // (`/private/var/...`).
    let config_path_abs = fs::canonicalize(&config_path).unwrap_or(config_path.clone());
    let codex_home_abs = fs::canonicalize(codex_home).unwrap_or(codex_home.to_path_buf());
    let cwd_abs = fs::canonicalize(hook_cwd).unwrap_or(hook_cwd.to_path_buf());

    let hook_specs: Vec<CommandHookSpec> = guards
        .iter()
        .enumerate()
        .map(|(group_index, guard)| {
            CommandHookSpec::builder()
                .event(HookEvent::PreToolUse)
                .maybe_matcher(guard.matcher.map(str::to_owned))
                .command(guard.command_path.clone())
                .group_index(group_index)
                .handler_index(0usize)
                .require_guard_executable(true)
                .build()
        })
        .collect();

    let request = ArmRequest {
        codex_home: codex_home_abs,
        config_path: config_path_abs,
        cwd: cwd_abs,
        hooks: hook_specs,
        codex_bin: codex_bin.to_path_buf(),
    };

    let attestation = arm_and_attest(&request)
        .map_err(|err| anyhow!("Codex hook-trust gate refused to arm PreToolUse guards: {err}"))?;

    // One derivation, shared with the per-turn re-check that reads it back: a
    // writer and reader that each build this path would drift into reporting
    // every turn boundary as a broken chain.
    let attestation_path = guard_chain::attestation_path(codex_home);
    write_attestation_file(&attestation_path, &attestation)
        .map_err(|err| anyhow!("writing hook-trust attestation: {err}"))?;

    tracing::info!(
        codex_home = %codex_home.display(),
        guards = guards.len(),
        "codex: armed and attested PreToolUse guardrails"
    );
    Ok(())
}

/// Resolve the `codex` binary used for live hook-trust observation.
fn resolve_codex_bin() -> PathBuf {
    which_codex().unwrap_or_else(|| PathBuf::from("codex"))
}

fn which_codex() -> Option<PathBuf> {
    let output = Command::new("which").arg("codex").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

// ---------------------------------------------------------------------------
// CodexDriver
// ---------------------------------------------------------------------------

/// OpenAI Codex CLI driver.
///
/// Registered under the `"codex"` slug. Declares the v1 capability set from
/// the Codex driver design; spawn / provision / permission / interception are
/// implemented here.
#[derive(Default)]
pub struct CodexDriver {
    // Keep this type non-unit so callers can use `Default` uniformly with
    // stateful drivers without tripping clippy's unit-default lint.
    _private: (),
}

#[async_trait]
impl AgentDriver for CodexDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &CODEX_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Capability declaration for CodexDriver (v1) — design doc §Capability
        // declaration. Every omission below is deliberate; each notes its
        // absence disposition and why.
        //
        // Provided (all except ToolProvisioning + AwaitingInputSignal +
        // CommandOutcomeObservation):
        //   Spawn, WorkspaceProvisioning, PermissionPolicy, ModelAndEffortMenu,
        //   ProgressObservation, ToolUseInterception (deny-only), TurnBoundary,
        //   StructuredOutput, TranscriptAccess, ControlVerbs, PromptComposition.
        //
        // ToolUseInterception is **deny-only**: Codex PreToolUse accepts
        // `permissionDecision: deny` but rejects `allow` / `ask` / `updatedInput`
        // (verified codex-cli 0.145.0). The trait rewrite path is unreachable;
        // inline-`--body` editorial cases become Deny-with-reason.
        //
        // CommandOutcomeObservation — omitted → default Degrade (never
        // Synthesize). `progress_fidelity()` below declares `Rich` because
        // Codex's rollout carries a start/end boundary around every tool
        // call, same cadence as Claude's hooks — but that says nothing about
        // whether the end-of-command record reliably says the command
        // succeeded. The rollout's `exit_code`/`status` fields are only
        // sometimes present, can be dropped by the model's own
        // result-projection layer before the record is emitted, and become
        // unparseable once output is truncated. Declaring `Rich` alone would
        // let a scheduler assume a per-command success/failure guarantee
        // Codex does not actually carry; this omission is what keeps that
        // assumption from being made silently.
        CapabilitySet::new([
            Capability::Spawn,
            Capability::WorkspaceProvisioning,
            Capability::PermissionPolicy,
            Capability::ModelAndEffortMenu,
            Capability::ProgressObservation,
            Capability::ToolUseInterception,
            Capability::TurnBoundary,
            Capability::StructuredOutput,
            Capability::TranscriptAccess,
            Capability::ControlVerbs,
            Capability::PromptComposition,
            // ToolProvisioning — omitted → default Degrade. Unused in v1 for
            // every driver (including Claude, which *declares* it but injects
            // nothing). Codex has MCP/plugins/skills but Boss injects none;
            // declaring it would overclaim. Degrade is correct: no dispatch
            // refusal, no synthesised tooling.
            //
            // AwaitingInputSignal — omitted → default Degrade (never
            // Synthesize). The *old* reason for this omission has been
            // retired and must not be cited again: it argued that
            // `task_complete` means process exit is imminent rather than
            // "blocked on a human". That argument inverts under the bare
            // TUI, which is a persistent session — a Codex worker parked at
            // its composer genuinely *is* waiting for someone to type.
            //
            // The omission survives the inversion, for a different and
            // measured reason. This capability is a claim about the
            // `ProgressObservation` *stream*: that some record in it
            // positively means "blocked on a human", which
            // `LiveWorkerStateRegistry::apply_event` may then promote to
            // `WorkerActivity::WaitingForInput` on a `Notification`. Codex's
            // `Notification` vocabulary is fully enumerated by this driver's
            // own normaliser (`codex/progress.rs`), and every member of it
            // means something else:
            //
            //   * `UNOBSERVED_COMMAND_MARKER` — a tool call whose output
            //     record never arrived (engine-synthesised, not Codex's);
            //   * guard-trace notifications — this run's own PreToolUse
            //     decisions, replayed from the guard log;
            //   * a command-denial notice — a guard said no;
            //   * `turn aborted: <reason>` — an Esc/interrupt;
            //   * a fatal `task_complete.error` diagnostic — the provider
            //     failed and the turn is over.
            //
            // Not one of those means "waiting for a human". Binding the
            // capability to `Notification` would therefore promote a denied
            // command or an aborted turn into a fabricated WaitingForInput —
            // precisely what Grok's precedent refuses to do on the same
            // grounds (`grok.rs`, `AwaitingInputSignal` omission).
            //
            // The TUI liveness literals captured for the pivot (the `›`
            // composer prefix, the absence of `esc to interrupt`) do not
            // earn it either: those are *pane-render* strings for
            // `pane_monitor_spec`, read by scraping the terminal surface.
            // They are not records in the progress stream this capability
            // describes, so declaring it on their strength would be a
            // structural argument with no measured stream signal behind it.
            //
            // Nothing is lost by the omission. A parked composer already
            // reaches the engine as `WorkerActivity::Idle` via the turn
            // boundary, and `WorkerActivity::accepts_typed_input` treats
            // Idle and WaitingForInput alike, so pane delivery to a parked
            // Codex worker is unaffected. What stays unavailable is the
            // narrower claim "this worker is blocked *specifically* on a
            // human", which Codex does not report.
            //
            // This becomes earnable the moment Codex's stream carries a
            // record that means it — an interactive approval request, say —
            // at which point that record is the measured mapping.
        ])
    }

    fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
        // Measured Codex TUI chrome; see `codex/pane_monitor.rs` for the
        // literals, their stability evidence, and why the startup banner
        // alone cannot carry agent detection.
        Some(pane_monitor::spec())
    }

    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        // Empty/missing run_id: fall back to a non-empty leaf so CODEX_HOME
        // never resolves to the shared homes root. Production always passes
        // the execution id; fixtures may omit it.
        let run_id = request.run_id.filter(|id| !id.is_empty()).unwrap_or("unknown-run");
        let codex_home = codex_home_for_run(run_id).unwrap_or_else(|_| {
            // sanitize_run_id_for_home only fails on empty; unknown-run is safe.
            codex_homes_root().join("unknown-run")
        });
        let command = build_codex_command(&request);
        SpawnPlan {
            env: vec![EnvDirective::Set(
                "CODEX_HOME".to_owned(),
                codex_home.display().to_string(),
            )],
            command,
        }
    }

    /// Create the Boss-owned per-run `CODEX_HOME`, snapshot auth into it,
    /// write base `config.toml` (project trust + migration suppress), and
    /// write the initial prompt under the workspace-local config dir.
    ///
    /// Does **not** write PreToolUse hooks or stamp trust — that happens in
    /// [`Self::write_permission_config`] once guard scripts are materialised,
    /// so the trust gate hashes the exact final handler identity.
    ///
    /// Never points `CODEX_HOME` at the operator interactive `~/.codex`, and
    /// never scans or rewrites that tree except via the auth snapshot/adopt
    /// policy (byte-copy in, optional refresh adoption out).
    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        let codex_home = codex_home_for_run(run_id)
            .with_context(|| format!("resolving Boss-owned CODEX_HOME for run_id {run_id:?}"))?;
        fs::create_dir_all(&codex_home)
            .with_context(|| format!("creating Boss-owned CODEX_HOME {}", codex_home.display()))?;
        provision_durable_sessions(&codex_home, "codex", run_id)?;

        // Auth: SnapshotWithRefreshAdoption. Source is the operator auth
        // discovery path (or BOSS_CODEX_AUTH_SOURCE when set for tests);
        // refuse to use a symlink source (enforced inside the auth crate).
        let source_auth = resolve_auth_source_path();
        let snapshot = snapshot_auth_into_codex_home(&source_auth, &codex_home).with_context(|| {
            format!(
                "snapshotting codex auth from {} into {}",
                source_auth.display(),
                codex_home.display()
            )
        })?;

        // Base config (hooks filled in by write_permission_config).
        let config_path = codex_home.join("config.toml");
        fs::write(&config_path, render_base_config_toml(workspace))
            .with_context(|| format!("writing {}", config_path.display()))?;

        // Workspace-local config dir: initial prompt + gitignore. AGENTS.md
        // is written by `write_workspace_files` via the shared agent-rules
        // path (descriptor.agent_rules_filename) so the body stays in lockstep
        // with the Claude path's shared template.
        let config_dir = workspace.join(CODEX_DESCRIPTOR.config_dir);
        fs::create_dir_all(&config_dir).with_context(|| format!("creating {}", config_dir.display()))?;
        let prompt_path = config_dir.join(CODEX_DESCRIPTOR.initial_prompt_filename);
        fs::write(&prompt_path, prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;
        let gitignore_path = config_dir.join(".gitignore");
        fs::write(&gitignore_path, CODEX_DIR_GITIGNORE)
            .with_context(|| format!("writing gitignore to {}", gitignore_path.display()))?;

        let runtime = CodexRuntimeState::from_snapshot(codex_home, &snapshot);
        Ok(Some(runtime.to_driver_runtime_state()))
    }

    /// Adopt any mid-run auth refresh back into the source.
    ///
    /// Leaves the temporary Boss-owned `CODEX_HOME` for its existing reclaim
    /// policy. Its `sessions/` link already writes the full rollout into
    /// Boss-owned transcript storage, while auth and every other home file
    /// remain temporary. Disk reclaim later uses the recorded path only;
    /// it never scans `~/.codex` or infers a home from the engine environment.
    /// `boss-engine-codex-rollout-retention` and `codex_home_retention_sweep`
    /// own that reclaim policy. Idempotent: a missing home or missing runtime
    /// state is a pure no-op.
    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        let Some(state) = runtime_state else {
            // No payload → no-op. Do not invent a cleanup target.
            return Ok(());
        };
        let runtime = CodexRuntimeState::from_driver_runtime_state(state)?;
        let codex_home = &runtime.codex_home;

        // Containment check even though we do not delete here: a tampered
        // payload must surface loudly rather than quietly becoming a
        // retention candidate outside the Boss homes root.
        assert_codex_home_safe_to_delete(codex_home)?;

        // Rebuild the AuthSnapshot handle the auth crate expects for adopt.
        let snapshot = AuthSnapshot {
            auth_path: codex_home.join(boss_codex_auth::AUTH_JSON_NAME),
            fingerprint: boss_codex_auth::AuthFingerprint::from_stored(&runtime.auth_fingerprint),
            source_path: runtime.auth_source_path.clone(),
            policy: boss_codex_auth::AuthIsolationPolicy::SnapshotWithRefreshAdoption,
        };

        match adopt_refresh_if_newer(&snapshot, codex_home) {
            Ok(outcome) => {
                tracing::info!(
                    codex_home = %codex_home.display(),
                    ?outcome,
                    "codex auth: teardown adopt finished (home retained for policy reclaim)"
                );
            }
            Err(err) => {
                // Best-effort: log and leave the home for retention rather
                // than failing the caller's termination path.
                tracing::warn!(
                    codex_home = %codex_home.display(),
                    error = %err,
                    "codex auth: adopt_refresh_if_newer failed (home retained; non-fatal)"
                );
            }
        }

        Ok(())
    }

    /// Write sandbox/hook artifacts into the per-run `CODEX_HOME` and return
    /// the env/argv the spawn flow must apply.
    ///
    /// `dest_dir` is ignored for Codex: the authoritative home is the
    /// Boss-owned per-run path derived from `input.run_id`. Hooks are written
    /// and trust-attested here (not in `tool_use_interception_wiring`) so a
    /// refuse from the gate fails the spawn with a real error.
    async fn write_permission_config(
        &self,
        input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        let codex_home = codex_home_for_run(&input.run_id).with_context(|| {
            format!(
                "CodexDriver::write_permission_config: resolving CODEX_HOME for run_id {:?}",
                input.run_id
            )
        })?;
        if !codex_home.exists() {
            bail!(
                "CodexDriver::write_permission_config: CODEX_HOME {} does not exist; \
                 call provision_workspace first",
                codex_home.display()
            );
        }

        let interception = ToolUseInterceptionConfig {
            data_dir: if input.is_remote {
                None
            } else {
                input.events_socket_path.parent().map(|p| p.to_path_buf())
            },
            path_guard_script: if input.is_remote {
                None
            } else {
                input.path_guard_script.clone()
            },
            checkleft_guard_script: if input.is_remote {
                None
            } else {
                input.checkleft_guard_script.clone()
            },
            is_revision: input.execution_kind == "revision_implementation"
                || input.task_kind.as_deref() == Some("revision"),
            is_standard_worker: input.worker_kind == WorkerKind::Standard,
            is_reviewer: input.worker_kind == WorkerKind::Reviewer,
            run_id: Some(input.run_id.clone()),
            workspace_path: Some(input.workspace_path.clone()),
        };

        // When path/checkleft scripts are supplied via PermissionInput they
        // win; otherwise leave those guards off (remote / early unit tests).
        let reviewer_output_dir = boss_engine_structured_output::default_dir();
        let base_config = if input.worker_kind == WorkerKind::Reviewer {
            render_reviewer_base_config_toml(&input.workspace_path, &reviewer_output_dir)
        } else {
            render_base_config_toml(&input.workspace_path)
        };
        let hook_cwd = if input.worker_kind == WorkerKind::Reviewer {
            reviewer_output_dir.as_path()
        } else {
            input.workspace_path.as_path()
        };
        let codex_bin = resolve_codex_bin();
        write_hooks_and_attest(&codex_home, hook_cwd, &base_config, &interception, &codex_bin)?;

        let mut extra_args = codex_sandbox_extra_args(input.worker_kind, input.codex_sandbox_enforced);
        if input.worker_kind == WorkerKind::Reviewer {
            extra_args.extend(reviewer_output_sandbox_extra_args(&reviewer_output_dir));
        }

        // Sandbox mode is the permission-policy artifact the spawn flow must
        // apply (see pane_spawn apply_permission_extra_args). `--strict-config`
        // stays on the spawn plan's base command (required flag contract).
        Ok(PermissionArtifacts {
            config_files: vec![codex_home.join("config.toml")],
            extra_args,
            env: vec![("CODEX_HOME".into(), codex_home.display().to_string())],
        })
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // The rollout JSONL (`CodexRolloutProgressSession::normalize_rollout`)
        // carries a `response_item` function_call / function_call_output pair
        // around each tool call — same per-tool resolution as Claude's hooks
        // (Progress-Observation gap / ProgressFidelity docs). Tier is about
        // resolution (cadence), not transport, and it is not a claim about
        // per-command outcome fidelity — Codex correctly leaves
        // `Capability::CommandOutcomeObservation` undeclared above for that.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress {
        // Pane-hosted Codex stdout belongs to Ghostty's pty master; the engine
        // cannot read it from `shell_pid`. Codex independently writes a raw
        // rollout JSONL under the run-private CODEX_HOME, so the engine tails
        // that file and feeds it to the generic JSONL reader. Hooks remain the
        // ToolUseInterception transport only.
        //
        // `CODEX_HOME/sessions` is itself a symlink into Boss's durable
        // per-execution transcript store (`provision_durable_sessions`,
        // called from `provision_workspace` before this wiring runs), so it
        // fails `agent_jsonl_progress`'s `VerifiedRoot` symlink-root check —
        // that check exists to stop a compromised worker from swapping its
        // own watched root out from under the engine, and CODEX_HOME sits
        // inside the worker's write surface. Watch the resolved durable
        // directory directly instead: it is the real directory Boss itself
        // created, Codex still reaches it (and only it) through the
        // `sessions` link, and `VerifiedRoot` can hold it to a stable
        // identity across the run.
        let directory = transcript_store_root()
            .and_then(|root| durable_sessions_dir(&root, "codex", &config.run_id))
            .unwrap_or_else(|_| codex_homes_root().join("__invalid_run__").join("sessions"));
        ProgressIngress::AgentJsonlFile(AgentJsonlFileIngress {
            directory,
            filename_prefix: "rollout-".to_owned(),
            filename_suffix: ".jsonl".to_owned(),
            workspace_path: config.workspace_path.clone(),
        })
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Stateless compatibility path for direct callers. Real ingestion
        // owns a durable per-reader session via `progress_session` below.
        CodexRolloutProgressSession::new(None, None, None).normalize_progress_event(raw)
    }

    fn progress_session(&self, config: &ProgressSessionConfig) -> Option<Box<dyn ProgressSessionNormalizer>> {
        let codex_home = config
            .run_id
            .as_deref()
            .and_then(|run_id| match codex_home_for_run(run_id) {
                Ok(home) => Some(home),
                Err(err) => {
                    tracing::warn!(run_id, %err, "codex: invalid run id for progress session");
                    None
                }
            });
        Some(Box::new(
            CodexRolloutProgressSession::new(
                config.run_id.clone(),
                config.identity_store.clone(),
                config.transcript_path.clone(),
            )
            // The guard trace and the arming attestation both live under
            // the same run-private CODEX_HOME the guards were armed in, so
            // the reader can only ever see its own run's decisions and can
            // only ever re-check its own run's chain.
            .with_codex_home(codex_home),
        ))
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // The progress normaliser maps every terminal rollout envelope
        // (`task_complete`, `turn_aborted`, and a fatal `task_complete.error`)
        // to `WorkerEvent::Stop`, so the boundary is the same shape as
        // Claude's: Stop means the turn ended.
        //
        // `continuation: false` — re-reasoned for the persistent TUI, because
        // the premise it used to rest on ("`codex exec` does not re-enter
        // after a boundary") is gone: the session now continues, and the
        // process outlives every boundary it reports.
        //
        // The conclusion survives, on the field's actual definition rather
        // than on process lifetime. [`TurnEnd::continuation`] means "the
        // agent was already stopping and something pulled it back into
        // another turn" — Claude's `stop_hook_active`, and nothing else. It
        // is a property of *this* boundary, not of what happens afterwards.
        // Codex has no stop-hook mechanism at all: its hook surface is
        // PreToolUse-only, there is no record in the rollout that can mark a
        // `task_complete` as re-entrant, and a fresh prompt after a boundary
        // starts a new `task_started`/`task_complete` pair — a fresh idle
        // followed by a fresh turn, which is exactly `false`.
        //
        // A mid-turn prompt buffered by [`Self::mid_turn_pane_input`] does
        // not weaken this either. Such a prompt folds into the *running*
        // turn and produces no boundary of its own (measured; see that
        // method), so there is no extra `Stop` here that could need
        // distinguishing as a continuation — the risk runs the other way,
        // toward one boundary for two prompts, which the engine's
        // boundary-waiting paths handle rather than this declaration.
        match event {
            WorkerEvent::Stop {
                session_id,
                stop_reason,
                ..
            } => Some(TurnEnd {
                session_id: session_id.clone(),
                reason: *stop_reason,
                continuation: false,
            }),
            _ => None,
        }
    }

    fn tool_use_interception_wiring(&self, config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        // Real guardrails live in `$CODEX_HOME/config.toml` as
        // `[[hooks.PreToolUse]]` and are armed by
        // [`Self::write_permission_config`] (trust-attested). Returning Claude
        // settings-file shaped hooks here would put them into a JSON file
        // Codex never reads — a silent no-op of every guardrail.
        //
        // Empty return is honest *only because* write_permission_config is
        // the arming path and is required to succeed before spawn. If that
        // path is skipped, the worker must not run.
        let _ = config;
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn pr_url_capture_feed(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        tool_response: &serde_json::Value,
    ) -> Option<PrUrlCaptureFeed> {
        if tool_name != "Bash" {
            return None;
        }
        let command = crate::command_from_tool_input(tool_input);
        Some(PrUrlCaptureFeed {
            // Rollout tool completion is
            // `response_item.payload.output`, observed as either a string
            // (`function_call_output`) or text-content array
            // (`custom_tool_call_output`) — flattened by the same shared
            // helper cell classification uses, so capture and classification
            // can never see two different texts for one value. Not pretended
            // to be stdout's `aggregated_output`.
            output_text: flatten_tool_output_text(tool_response),
            command,
        })
    }

    fn agent_rules_preamble(&self) -> &'static str {
        CODEX_AGENT_RULES_PREAMBLE
    }

    /// Codex does not read `.codex/AGENTS.md` at all (verified with `codex
    /// debug prompt-input`: a root or `$CODEX_HOME` `AGENTS.md` marker
    /// appears in the model-visible prompt input; a `.codex/AGENTS.md`
    /// marker does not). Route it to `$CODEX_HOME/AGENTS.md` instead — the
    /// same per-run home `provision_workspace` already creates, read as
    /// Codex's "user-level" instructions and concatenated ahead of any
    /// project-level `AGENTS.md` (confirmed both surface, separated by
    /// Codex's own `--- project-doc ---` marker). Writing there, rather than
    /// the workspace root, also means this file never touches the jj-tracked
    /// tree.
    fn agent_rules_destination(&self, _workspace: &Path, run_id: &str) -> PathBuf {
        codex_home_for_run(run_id)
            .unwrap_or_else(|_| codex_homes_root().join("unknown-run"))
            .join("AGENTS.md")
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        // Transcript lookup requires the exact run home supplied when the
        // stdout reader creates its per-ingress progress session.
        let _ = raw;
        None
    }

    fn transcript_session(&self) -> Option<Box<dyn TranscriptSessionNormalizer>> {
        Some(Box::new(CodexTranscriptSession::default()))
    }

    fn transcript_containment_root(&self, run_id: &str) -> anyhow::Result<Option<PathBuf>> {
        let (homes_root, codex_home) = codex_homes_root_and_home_for_run(run_id)?;
        let sessions_path = codex_home.join("sessions");
        match fs::symlink_metadata(&sessions_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let store_root = transcript_store_root()?;
                return Ok(Some(verified_durable_sessions_dir(
                    &codex_home,
                    &store_root,
                    "codex",
                    run_id,
                )?));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let store_root = transcript_store_root()?;
                let durable = durable_sessions_dir(&store_root, "codex", run_id)?;
                if let Ok(canonical) = fs::canonicalize(&durable)
                    && canonical.is_dir()
                {
                    return Ok(Some(canonical));
                }
            }
            Err(err) => tracing::debug!(
                ?err,
                path = %sessions_path.display(),
                "stat of temporary sessions path failed; falling back to legacy containment"
            ),
            _ => {}
        }
        let sessions = verified_sessions_root(&homes_root, &codex_home).ok_or_else(|| {
            anyhow!(
                "Codex transcript root for run {run_id:?} is missing, replaced, or outside {}",
                homes_root.display()
            )
        })?;
        Ok(Some(sessions))
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        normalize_rollout(raw)
    }

    fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
        // Codex-specific API-error shapes are not extracted yet (ControlVerbs
        // hardening). None is the honest "no recognised halting error" answer,
        // not a claim that the run was clean.
        None
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // Must not route through `classify_claude_error`. Real Codex
        // classification (rate limits, quota, auth) is ControlVerbs follow-on.
        // Indeterminate is the documented "recognised as an error but not
        // confidently bucketed; treat as Permanent" class — explicit, not a
        // silent Transient that would auto-resume a permanent failure.
        WorkerErrorClass::Indeterminate
    }

    /// Existing engine path: probes go through `SendToPane`, either at a turn
    /// boundary or into a live composer mid-turn — the TUI buffers the latter
    /// (see [`Self::mid_turn_pane_input`]), so there is no longer a refusal
    /// case for this driver. This declares today's transport so the seam is
    /// real without changing delivery.
    fn probe(&self) -> ProbeDelivery {
        ProbeDelivery::PaneText
    }

    /// Existing engine path: Esc via `InterruptWorkerPane`. Esc semantics on
    /// non-interactive `codex exec` are unvalidated; this declares the
    /// transport the engine uses today rather than inventing a signal path.
    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    /// One Escape; the abort is observable and arrives on the ordinary
    /// turn-boundary channel.
    ///
    /// Measured in `docs/investigations/ghostty-codex-pane-viability.md` Q5:
    /// a single Esc into the interactive TUI mid-turn produces a rollout
    /// `turn_aborted` (`reason: "interrupted"`), the process survives, and a
    /// follow-up turn submitted afterwards completes normally. This driver's
    /// own rollout normalizer ([`crate::codex::progress`]) turns that record
    /// into `Notification` + `Stop { stop_reason: Interrupted }`, so the turn
    /// end reaches the engine on the same channel a completed turn's does —
    /// [`TurnEndEvidence::TurnBoundarySignal`], with no recovery observer
    /// needed ([`Self::prepare_interrupt_recovery`] stays `None`).
    ///
    /// `confirm_window` is wider than Claude's because the evidence path is
    /// longer: the abort has to be appended to the rollout file and read back
    /// by the engine's rollout tail before the slot's activity moves, whereas
    /// Claude's `Stop` hook posts straight to the events socket.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        Some(InterruptPlan {
            gesture: InterruptGesture {
                key: "Escape",
                presses: 1,
                press_interval: Duration::from_millis(120),
            },
            confirm_window: Duration::from_secs(8),
            max_attempts: 2,
            turn_end_evidence: TurnEndEvidence::TurnBoundarySignal,
        })
    }

    /// Stop is process-level only — same as Claude today.
    fn stop(&self) -> StopDelivery {
        StopDelivery::ProcessOnly
    }

    /// Reap is the universal SIGTERM→SIGKILL process-group ladder.
    fn reap(&self) -> ReapDelivery {
        ReapDelivery::ProcessGroup
    }

    /// The bare TUI buffers mid-turn pane input, **measured**, and the
    /// measurement carries a caveat that changes how the engine must treat
    /// the buffered prompt.
    ///
    /// **Evidence.** Text was injected into a live Codex TUI turn through the
    /// exact pane `submitText` path the engine uses, under a
    /// GhosttyKit-embedded surface
    /// (`docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`, V4).
    /// Codex rendered its own first-class affordance —
    /// `Messages to be submitted after next tool call (press esc to interrupt
    /// and send immediately)` — queued the message, delivered it at the next
    /// tool-call boundary and answered it. Nothing landed in a tty line
    /// discipline and nothing was executed by a shell, which is the tty-leak
    /// hazard the `Rejects` default exists to prevent. The predecessor
    /// declaration was `Rejects` because `codex exec` ran one turn per
    /// process with stdin on `/dev/null` (ghostty-codex-pane-viability, Q2
    /// Layer D); that shape is retired, and this is the measurement it was
    /// standing in for.
    ///
    /// **Caveat — the buffered prompt folds into the running turn.** It is
    /// not deferred into a new one. The rollout for the measured session
    /// carried two `user_message` records but only one `task_started` and one
    /// `task_complete`, so the normaliser emits one `UserPromptSubmit` and
    /// one `Stop` for two prompts, and `event_msg/user_message` is an
    /// unmapped record in the progress dialect. A prompt delivered mid-turn
    /// is acted on but **produces no turn boundary of its own**: any engine
    /// path that waits for a boundary per delivered prompt would wait
    /// forever, and any path that counts turns undercounts. The measured run
    /// also showed the model answering the *newer* instruction and never
    /// emitting the original turn's answer.
    ///
    /// The transcript dialect is the exception worth knowing: the *transcript*
    /// normaliser does map `event_msg/user_message`, so a folded prompt is
    /// visible to a post-hoc transcript read even though it is invisible to
    /// the progress stream. That is what
    /// `ServerState::inject_pane_text_verified`'s transcript fallback and the
    /// probe-reply read have to lean on, since no `UserPromptSubmit` will
    /// ever arrive for the folded prompt.
    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        MidTurnPaneInput::Buffers
    }

    /// The bare TUI is a long-lived, multi-turn session — the retired
    /// `codex exec` shape was the one-turn-per-process outlier among Boss's
    /// drivers, not the norm: Claude asserts `Persistent` explicitly
    /// (`claude.rs`), and Grok takes it as the trait default. Flipping this
    /// from `OneTurnPerProcess` closes that gap; a foreground process exiting
    /// is now always a death for Codex too, exactly as it already is for
    /// Claude and Grok — see
    /// `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`.
    fn worker_process_lifetime(&self) -> WorkerProcessLifetime {
        WorkerProcessLifetime::Persistent
    }

    fn structured_output_wiring(
        &self,
        request: &StructuredOutputRequest<'_>,
    ) -> anyhow::Result<StructuredOutputArtifacts> {
        // Common-denominator env-file contract works for Codex today. Spawn /
        // StructuredOutput follow-on can extend this with `--output-schema` /
        // `--output-last-message` on top of the same path (prefer starting
        // from the default).
        Ok(default_structured_output_wiring(request))
    }

    fn structured_output_fallback(&self, kind: StructuredOutputKind, text: &str) -> Vec<FallbackCandidate> {
        match kind {
            // The probe in `finalize_pr_review_pass` tells a Codex reviewer
            // whose artifact write is denied by `--sandbox read-only` to end
            // its reply with the JSON in a fenced ```json block — that is
            // the *only* actionable channel it has (see
            // `docs/investigations/codex-review-eligibility-sandbox-and-structured-output-2026-07-31.md`).
            // `json_object_candidates` is driver-neutral (bare
            // fenced/balanced-object scraping, no Claude-specific
            // convention), so Codex rides the same extraction Claude uses
            // for this kind.
            StructuredOutputKind::ReviewResult => json_object_candidates(text),
            // No Codex-specific prose-scrape conventions for these kinds
            // yet. Empty Vec is the honest answer: primary channel is the
            // file contract (+ future --output-schema), not transcript
            // scraping.
            StructuredOutputKind::PrUrl
            | StructuredOutputKind::TriageDecision
            | StructuredOutputKind::Followups
            | StructuredOutputKind::PostmortemFollowups => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth source resolution (tests override via env)
// ---------------------------------------------------------------------------

/// Env override for the auth snapshot *source* (regular file). Tests point
/// this at a synthetic `auth.json` so the interactive home is never read.
pub const CODEX_AUTH_SOURCE_ENV: &str = "BOSS_CODEX_AUTH_SOURCE";

fn resolve_auth_source_path() -> PathBuf {
    if let Ok(path) = std::env::var(CODEX_AUTH_SOURCE_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    resolve_operator_auth_path()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
