//! `CodexDriver` — OpenAI Codex agent driver.
//!
//! Implements spawn + Boss-owned per-run `CODEX_HOME` provisioning and the
//! native `codex exec --json` progress normaliser.
//!
//! See `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`
//! (T-11 / capability declaration) and
//! `tools/boss/docs/investigations/ghostty-codex-pane-viability.md` Q2 for
//! the pane-launch buffered-tty footgun this spawn line closes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use boss_codex_auth::{
    AuthSnapshot, adopt_refresh_if_newer, resolve_operator_auth_path, snapshot_auth_into_codex_home,
};
use boss_engine_codex_hook_trust::{ArmRequest, CommandHookSpec, HookEvent, arm_and_attest, write_attestation_file};
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{EffortLevel, NormalizeError, ReasoningMode, WorkerEvent};
use boss_ssh_transport::shell_quote;
use serde::{Deserialize, Serialize};

mod progress;

use progress::{
    CodexProgressSession, CodexRolloutProgressSession, CodexTranscriptSession, normalize_rollout,
    verified_sessions_root,
};

use super::claude::{BOSS_LAUNCH_GUARD_COMMAND, PR_REDIRECT_GUARD_COMMAND, REVISION_PR_GUARD_COMMAND};
use super::{
    AgentDriver, AgentJsonlFileIngress, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, EnvDirective,
    InterruptDelivery, MidTurnPaneInput, ModelMenu, PermissionArtifacts, PermissionInput, PrUrlCaptureFeed,
    ProbeDelivery, ProgressFidelity, ProgressIngress, ProgressObservationConfig, ProgressSessionConfig,
    ProgressSessionNormalizer, ProgressStreamSource, ReapDelivery, SpawnPlan, SpawnRequest, StopDelivery,
    StructuredOutputArtifacts, StructuredOutputRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring,
    TranscriptSessionNormalizer, TurnEnd, WorkerErrorClass, WorkerKind, WorkerProcessLifetime,
    default_structured_output_wiring,
};

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
        prompt_addendum_for_level: codex_prompt_addendum_for_level,
        model_requires_auto_permissions: codex_model_requires_auto_permissions,
        model_belongs_to_driver: codex_model_belongs_to_driver,
    },
};

/// Preamble for the agent-rules file (`AGENTS.md`). Names Codex observability
/// rather than Claude hooks so the shared body below it is not lying about
/// the mechanism this session uses.
const CODEX_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via the Codex rollout JSONL file in this run's isolated CODEX_HOME.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.";

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
    let safe = sanitize_run_id_for_home(run_id)?;
    let home = codex_homes_root().join(safe);
    // Logical containment: a join of a single segment under an absolute root
    // always starts with that root; keep the check so a future root change
    // cannot silently open an escape.
    let root = codex_homes_root();
    if !home.starts_with(&root) || home == root {
        bail!(
            "resolved CODEX_HOME {} is not a strict child of homes root {}",
            home.display(),
            root.display()
        );
    }
    Ok(home)
}

/// Sandbox mode for Codex `exec --sandbox` from Boss's abstract worker kind.
///
/// Reviewer is always OS-enforced read-only, regardless of `sandbox_enforced`
/// — it never runs build gates, and `materialize_guards` wires no reviewer
/// denylist for Codex, so loosening it would drop that protection entirely.
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
        WorkerKind::Reviewer => "read-only",
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
    // TOML basic-string escape for paths that may contain backslashes or quotes.
    let workspace_key = toml_basic_string(&workspace.display().to_string());
    let sandbox_workspace_write = render_sandbox_workspace_write_toml(workspace);
    format!(
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
    )
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
/// This table only takes effect under `--sandbox workspace-write`, i.e. for
/// Standard/Triage/AnswerAgent when the `codex_sandbox_enforced` feature flag
/// is on. Reviewer (`--sandbox read-only`) and the default
/// `danger-full-access` path both ignore it entirely (see
/// [`codex_sandbox_for_worker_kind`]), so no worker-kind branch is needed
/// here.
///
/// `workspace` itself is granted write access by Codex's own cwd default,
/// separate from this function's `writable_roots` list, and does not need a
/// paired `workspace/.git` grant: cube workspaces are non-colocated secondary
/// jj workspaces (`.jj` pointer file, no `.git`) by construction — the same
/// invariant `--skip-git-repo-check` exists to work around (see
/// `build_codex_exec_command`) — so there is no colocated `.git` under
/// `workspace` for the sandbox's auto-exclusion to bite. Only the shared
/// store root resolved by [`cube_repo_store_root`] carries a real `.git`.
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

/// Build the `codex exec …` command body (without the leading `exec `).
///
/// Contract (also enforced by `assert_codex_exec_spawn_contract` in core):
/// - requires `--json`, `--strict-config`, and `--skip-git-repo-check`
///   (cube workspaces are non-colocated jj workspaces with a `.jj` and no
///   `.git`, so the git-repo trust check would refuse every dispatch)
/// - forbids `-a` / `--ask-for-approval` (removed on 0.145.0)
/// - redirects stdin from `/dev/null` so Codex does not block on "Reading
///   additional input from stdin..."
///
/// **Pane launch safety (ghostty-codex-pane-viability Q2, choice (a)):**
/// the full pane line is `exec codex exec … </dev/null`. `exec` replaces the
/// interactive shell, so when codex exits there is no shell left to consume
/// tty-buffered injects that arrived while codex was foreground. Choice (b)
/// (drain pending tty input) is deliberately not used: draining requires a
/// shell still in control after the worker exits, which re-opens the
/// footgun window. See [`pane_launch_spec_uses_exec_not_interactive_return`].
pub fn build_codex_exec_command(request: &SpawnRequest<'_>) -> String {
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
    // always replaces it via [`PermissionArtifacts::extra_args`] (see
    // `codex_sandbox_for_worker_kind`: `--sandbox read-only` for Reviewer,
    // `--sandbox danger-full-access` for every other kind unless the
    // `codex_sandbox_enforced` feature flag is on, in which case
    // `workspace-write`) applied by the spawn flow — do not hardcode a
    // second source of truth here without also applying extra_args.
    let mut cmd = String::from("codex exec --json --strict-config --skip-git-repo-check --sandbox workspace-write");
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
    // Stdin must not be the open tty: otherwise Codex prints
    // "Reading additional input from stdin..." and waits.
    cmd.push_str(" < /dev/null\n");
    cmd
}

/// Prefix the exec body with shell `exec` so the pane does not return to an
/// interactive prompt after the worker process exits (Q2 choice (a)).
pub fn wrap_codex_command_for_pane(exec_body: &str) -> String {
    // `exec` replaces the shell process. Mid-run SendToPane injects land in
    // the tty line discipline; with no shell after codex exits they are not
    // evaluated as shell commands.
    format!("exec {exec_body}")
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

    let mut out = Vec::new();
    let mut index = 0usize;

    // 1. Path guard — local workers only (script never ships to remotes).
    if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
        let wrapper = guards_dir.join(format!("{index:02}_path_guard.sh"));
        let body = format!(
            "#!/bin/sh\nexport BOSS_DATA_DIR={dir}\nexec python3 {script}\n",
            dir = shell_quote(&data_dir.display().to_string()),
            script = shell_quote(&guard_script.display().to_string()),
        );
        write_executable(&wrapper, &body)?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&wrapper).unwrap_or(wrapper),
            matcher: Some(".*"),
        });
        index += 1;
    }

    // 2. Boss-launch guard — always on. Materialise the Claude inline
    //    `python3 -c` body as a real .py so the trust gate can path-check it.
    {
        let script = guards_dir.join(format!("{index:02}_boss_launch_guard.py"));
        write_executable(&script, &python_c_to_script(BOSS_LAUNCH_GUARD_COMMAND)?)?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&script).unwrap_or(script),
            matcher: Some("Bash"),
        });
        index += 1;
    }

    // 3. PR redirect — Standard workers only.
    if config.is_standard_worker {
        let script = guards_dir.join(format!("{index:02}_pr_redirect_guard.py"));
        write_executable(&script, &python_c_to_script(PR_REDIRECT_GUARD_COMMAND)?)?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&script).unwrap_or(script),
            matcher: Some("Bash"),
        });
        index += 1;
    }

    // 4. Checkleft push guard — local Standard workers only.
    if config.is_standard_worker
        && let Some(checkleft_script) = &config.checkleft_guard_script
    {
        let wrapper = guards_dir.join(format!("{index:02}_checkleft_push_guard.sh"));
        let body = format!(
            "#!/bin/sh\nexec python3 {script}\n",
            script = shell_quote(&checkleft_script.display().to_string()),
        );
        write_executable(&wrapper, &body)?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&wrapper).unwrap_or(wrapper),
            matcher: Some("Bash"),
        });
        index += 1;
    }

    // 5. Revision PR guard.
    if config.is_revision {
        let script = guards_dir.join(format!("{index:02}_revision_pr_guard.py"));
        write_executable(&script, &python_c_to_script(REVISION_PR_GUARD_COMMAND)?)?;
        out.push(MaterializedGuard {
            command_path: fs::canonicalize(&script).unwrap_or(script),
            matcher: Some("Bash"),
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
    workspace: &Path,
    config: &ToolUseInterceptionConfig,
    codex_bin: &Path,
) -> anyhow::Result<()> {
    fs::create_dir_all(codex_home).with_context(|| format!("creating CODEX_HOME {}", codex_home.display()))?;

    let guards = materialize_guards(codex_home, config)?;
    let base = render_base_config_toml(workspace);
    let full = append_hooks_toml(&base, &guards);
    let config_path = codex_home.join("config.toml");
    fs::write(&config_path, full).with_context(|| format!("writing {}", config_path.display()))?;

    // Prefer realpath form for ArmRequest so state keys match Codex on macOS
    // (`/private/var/...`).
    let config_path_abs = fs::canonicalize(&config_path).unwrap_or(config_path.clone());
    let codex_home_abs = fs::canonicalize(codex_home).unwrap_or(codex_home.to_path_buf());
    let cwd_abs = fs::canonicalize(workspace).unwrap_or(workspace.to_path_buf());

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

    let attestation_path = codex_home.join(HOOK_TRUST_ATTESTATION_FILENAME);
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

fn rollout_output_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(rollout_output_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(object) => object.get("text").map(rollout_output_text).unwrap_or_default(),
        _ => String::new(),
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
            // Synthesize). `codex exec` is one turn per process; `turn.completed`
            // means exit is imminent, not "blocked on a human". There is no
            // channel that positively means awaiting-input (agent-driver design
            // §Decision: AwaitingInput derivation; codex-progress-channel-
            // decision investigation).
        ])
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
        let body = build_codex_exec_command(&request);
        let command = wrap_codex_command_for_pane(&body);
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
        fs::create_dir_all(codex_home.join("sessions"))
            .with_context(|| format!("creating Codex sessions directory under {}", codex_home.display()))?;

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
    /// Leaves the Boss-owned `CODEX_HOME` on disk as terminal-run evidence
    /// for the retention policy (`boss-engine-codex-rollout-retention` /
    /// `codex_home_retention_sweep`). Disk reclaim happens later against
    /// **this recorded path only** — never by scanning `~/.codex` or
    /// inferring a home from the engine environment. Idempotent: a missing
    /// home or missing runtime state is a pure no-op.
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
            run_id: Some(input.run_id.clone()),
            workspace_path: Some(input.workspace_path.clone()),
        };

        // When path/checkleft scripts are supplied via PermissionInput they
        // win; otherwise leave those guards off (remote / early unit tests).
        let codex_bin = resolve_codex_bin();
        write_hooks_and_attest(&codex_home, &input.workspace_path, &interception, &codex_bin)?;

        // Sandbox mode is the permission-policy artifact the spawn flow must
        // apply (see pane_spawn apply_permission_extra_args). `--strict-config`
        // stays on the spawn plan's base command (required flag contract).
        Ok(PermissionArtifacts {
            config_files: vec![codex_home.join("config.toml")],
            extra_args: codex_sandbox_extra_args(input.worker_kind, input.codex_sandbox_enforced),
            env: vec![("CODEX_HOME".into(), codex_home.display().to_string())],
        })
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Codex `--json` carries `item.started` / `item.completed` around each
        // tool call — same per-tool resolution as Claude's hooks (Progress-
        // Observation gap / ProgressFidelity docs). Tier is about resolution
        // (cadence), not transport, and it is not a claim about per-command
        // outcome fidelity — Codex correctly leaves
        // `Capability::CommandOutcomeObservation` undeclared above for that.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress {
        // Pane-hosted Codex stdout belongs to Ghostty's pty master; the engine
        // cannot read it from `shell_pid`. Codex independently writes a raw
        // rollout JSONL under the run-private CODEX_HOME, so the engine tails
        // that file and feeds it to the generic JSONL reader. Hooks remain the
        // ToolUseInterception transport only.
        let directory = codex_home_for_run(&config.run_id)
            .unwrap_or_else(|_| codex_homes_root().join("__invalid_run__"))
            .join("sessions");
        ProgressIngress::AgentJsonlFile(AgentJsonlFileIngress {
            directory,
            filename_prefix: "rollout-".to_owned(),
            filename_suffix: ".jsonl".to_owned(),
            workspace_path: config.workspace_path.clone(),
        })
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Stateless compatibility path for direct callers. Stdout ingestion
        // owns a durable per-reader session via `progress_session` below.
        CodexProgressSession::new(None, None, None, None).normalize_progress_event(raw)
    }

    fn progress_session(&self, config: &ProgressSessionConfig) -> Option<Box<dyn ProgressSessionNormalizer>> {
        let homes_root = codex_homes_root();
        let codex_home = config
            .run_id
            .as_deref()
            .and_then(|run_id| match codex_home_for_run(run_id) {
                Ok(home) => Some(home),
                Err(err) => {
                    tracing::warn!(run_id, %err, "codex stdout: invalid run id for progress session");
                    None
                }
            });
        match config.source {
            ProgressStreamSource::StdoutJsonl => Some(Box::new(CodexProgressSession::new(
                codex_home,
                Some(homes_root),
                config.run_id.clone(),
                config.identity_store.clone(),
            ))),
            ProgressStreamSource::AgentJsonlFile => Some(Box::new(CodexRolloutProgressSession::new(
                config.run_id.clone(),
                config.identity_store.clone(),
                config.transcript_path.clone(),
            ))),
        }
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // The progress normaliser maps every terminal stdout envelope
        // (`turn.completed`, `turn.failed`, and unrecoverable top-level
        // `error`) to `WorkerEvent::Stop`, so the boundary is the same shape
        // as Claude's: Stop means the turn ended. `codex exec` does not
        // re-enter via stop-hooks, so continuation is always false.
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
        let command = tool_input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .or_else(|| tool_input.as_str())
            .unwrap_or("")
            .to_owned();
        Some(PrUrlCaptureFeed {
            // Rollout tool completion is
            // `response_item.payload.output`, observed as either a string
            // (`function_call_output`) or text-content array
            // (`custom_tool_call_output`). Keep extraction here, rather than
            // pretending the value is stdout's `aggregated_output`.
            output_text: rollout_output_text(tool_response),
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
        let homes_root = codex_homes_root();
        let codex_home = codex_home_for_run(run_id)?;
        let sessions = verified_sessions_root(&homes_root, &codex_home).ok_or_else(|| {
            anyhow!(
                "Codex transcript root for run {run_id:?} is missing, symlinked, replaced, or outside {}",
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

    /// Existing engine path: probes still go through `SendToPane` at a turn
    /// boundary (or are refused mid-turn via [`Self::mid_turn_pane_input`]).
    /// Resume-as-new-process probing is a follow-on; this declares today's
    /// behaviour so the seam is real without changing delivery.
    fn probe(&self) -> ProbeDelivery {
        ProbeDelivery::PaneText
    }

    /// Existing engine path: Esc via `InterruptWorkerPane`. Esc semantics on
    /// non-interactive `codex exec` are unvalidated; this declares the
    /// transport the engine uses today rather than inventing a signal path.
    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    /// Stop is process-level only — same as Claude today.
    fn stop(&self) -> StopDelivery {
        StopDelivery::ProcessOnly
    }

    /// Reap is the universal SIGTERM→SIGKILL process-group ladder.
    fn reap(&self) -> ReapDelivery {
        ReapDelivery::ProcessGroup
    }

    /// `codex exec` is the driver the mid-turn injection guard exists for.
    /// It runs one turn per process with stdin on `/dev/null`, so bytes
    /// written into the pane mid-turn are never read by the agent, survive in
    /// the tty input buffer, and are executed by the interactive shell once
    /// the process exits (ghostty-codex-pane-viability, Q2 Layer D). Declared
    /// explicitly rather than left to the trait default so that this — the
    /// motivating case — is stated where the driver lives.
    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        MidTurnPaneInput::Rejects
    }

    /// `codex exec` serves exactly one turn and then exits — the same
    /// one-turn-per-process shape the mid-turn guard above exists for, seen
    /// from the other end of the run. `turn.completed` is followed by process
    /// exit within milliseconds, so a reaper that reads "the foreground
    /// process is gone" as "the worker died" reaps a successful run before its
    /// completion handler can finish. Declaring the lifetime here is what lets
    /// the engine pair that exit with the run's own delivered turn boundary
    /// instead of guessing.
    ///
    /// This is not an exemption from the liveness sweeps: an exit with no
    /// delivered turn boundary is still reaped as a death.
    fn worker_process_lifetime(&self) -> WorkerProcessLifetime {
        WorkerProcessLifetime::OneTurnPerProcess
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

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        // No Codex-specific prose-scrape conventions yet. Empty Vec is the
        // honest answer: primary channel is the file contract (+ future
        // --output-schema), not transcript scraping.
        Vec::new()
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
mod tests {
    use super::*;
    use crate::{AbsenceDisposition, Capability};
    use boss_protocol::StopReason;
    use tempfile::TempDir;

    #[test]
    fn codex_model_belongs_to_driver_recognises_codex_vocabulary() {
        for model in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4-mini",
            "codex-auto-review",
            "GPT-5.6-SOL",
        ] {
            assert!(
                codex_model_belongs_to_driver(model),
                "{model:?} should be recognised as a Codex model"
            );
        }
    }

    #[test]
    fn codex_model_belongs_to_driver_rejects_other_drivers_models() {
        // The exact bug this gate exists to catch: a Claude family alias
        // reaching the Codex CLI verbatim.
        for model in ["opus", "sonnet", "claude-opus-4-7", "grok-4.5"] {
            assert!(
                !codex_model_belongs_to_driver(model),
                "{model:?} should not be recognised as a Codex model"
            );
        }
    }

    // Tests that mutate `BOSS_CODEX_*` go through
    // [`crate::test_support::codex_homes_override`] (owns
    // [`CODEX_HOMES_ENV_TEST_LOCK`]). `CODEX_AUTH_SOURCE_ENV` rides on that
    // same lock — set/restore it only while a homes override is held.

    fn sample_auth_json() -> String {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id.token",
                "access_token": "access.token",
                "refresh_token": "refresh.token",
                "account_id": "acct_test"
            },
            "last_refresh": "2026-01-01T00:00:00.000Z"
        })
        .to_string()
    }

    fn spawn_request<'a>(model: &'a str, run_id: &'a str) -> SpawnRequest<'a> {
        SpawnRequest {
            model,
            effort: Some("high"),
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(run_id),
        }
    }

    #[test]
    fn codex_descriptor_matches_design() {
        let driver = CodexDriver::default();
        let d = driver.descriptor();
        assert_eq!(d.name, "codex");
        assert_eq!(d.label, "OpenAI Codex");
        assert_eq!(d.binary, "codex");
        assert_eq!(d.config_dir, ".codex");
        assert_eq!(d.agent_rules_filename, "AGENTS.md");
        assert_eq!(d.initial_prompt_filename, "initial-prompt.txt");
        assert_eq!(d.model_menu.engine_default, "gpt-5.6-sol");
    }

    #[test]
    fn agent_rules_destination_is_codex_home_not_dot_codex() {
        // Codex never reads `.codex/AGENTS.md` (verified with `codex debug
        // prompt-input`). Must route to `$CODEX_HOME/AGENTS.md`, not the
        // trait default (`<workspace>/<config_dir>/<agent_rules_filename>`).
        let driver = CodexDriver::default();
        let workspace = Path::new("/tmp/some-workspace");
        let destination = driver.agent_rules_destination(workspace, "run-agents-md-1");
        assert_eq!(
            destination,
            codex_home_for_run("run-agents-md-1").unwrap().join("AGENTS.md")
        );
        assert!(
            !destination.starts_with(workspace),
            "AGENTS.md must not land inside the workspace tree: {}",
            destination.display()
        );
    }

    #[test]
    fn codex_model_menu_sourced_from_debug_models_vocabulary() {
        let driver = CodexDriver::default();
        let menu = &driver.descriptor().model_menu;
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Trivial), Some("low"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Small), Some("medium"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Medium), Some("high"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Large), Some("xhigh"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Max), Some("max"));
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Standard), "gpt-5.6-terra");
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Investigation), "gpt-5.6-sol");
        assert!(!(menu.model_requires_auto_permissions)("gpt-5.6-sol"));
    }

    #[test]
    fn codex_declares_design_capability_set() {
        let caps = CodexDriver::default().capabilities();
        for cap in [
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
        ] {
            assert!(caps.provides(cap), "CodexDriver must provide {cap:?}");
        }
        assert!(!caps.provides(Capability::ToolProvisioning));
        assert!(!caps.provides(Capability::AwaitingInputSignal));
        assert!(!caps.provides(Capability::CommandOutcomeObservation));
        assert_eq!(
            caps.absence_disposition(Capability::ToolProvisioning),
            AbsenceDisposition::Degrade
        );
        assert_eq!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Degrade
        );
        assert_eq!(
            caps.absence_disposition(Capability::CommandOutcomeObservation),
            AbsenceDisposition::Degrade,
            "Boss must not synthesize a per-command outcome Codex never observed"
        );
    }

    #[test]
    fn codex_declares_rich_progress_fidelity_without_command_outcome_observation() {
        // Rich cadence (per-tool item.started/item.completed boundaries) is
        // not the same claim as reliable per-command exit status: Codex's
        // rollout exit_code/status fields are sometimes absent, can be
        // dropped by the model's own result-projection layer, and become
        // unparseable once output is truncated. A scheduler must not infer
        // outcome observability from the fidelity tier alone.
        let driver = CodexDriver::default();
        assert_eq!(driver.progress_fidelity(), ProgressFidelity::Rich);
        assert!(!driver.capabilities().provides(Capability::CommandOutcomeObservation));
    }

    #[test]
    fn spawn_invocation_meets_codex_exec_contract() {
        let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-terra", "run-spawn-1"));
        assert!(plan.command.contains("--json"), "requires --json: {}", plan.command);
        assert!(
            plan.command.contains("--strict-config"),
            "requires --strict-config: {}",
            plan.command
        );
        assert!(
            plan.command.contains("--skip-git-repo-check"),
            "requires --skip-git-repo-check: cube workspaces are non-colocated jj \
             workspaces with no `.git`, so codex would refuse to run without it: {}",
            plan.command
        );
        assert!(
            !plan.command.contains("--ask-for-approval"),
            "forbids --ask-for-approval: {}",
            plan.command
        );
        let tokens: Vec<&str> = plan.command.split_whitespace().collect();
        assert!(!tokens.contains(&"-a"), "forbids bare -a: {}", plan.command);
        assert!(
            plan.command.contains("< /dev/null"),
            "must redirect stdin from /dev/null: {}",
            plan.command
        );
        assert!(
            plan.command.contains(&format!("-m {}", shell_quote("gpt-5.6-terra"))),
            "must pass shell-quoted model: {}",
            plan.command
        );
        assert!(
            plan.command
                .contains(&format!("model_reasoning_effort={}", shell_quote("high"))),
            "must pass shell-quoted effort: {}",
            plan.command
        );
        assert!(
            plan.env.iter().any(|d| matches!(
                d,
                EnvDirective::Set(k, v) if k == "CODEX_HOME" && v.contains("run-spawn-1")
            )),
            "must export CODEX_HOME for the run: {:?}",
            plan.env
        );
    }

    #[test]
    fn pane_launch_spec_uses_exec_not_interactive_return() {
        // Choice (a): do not return to an interactive prompt after the worker
        // exits. The spawn line must start with `exec` so the shell is
        // replaced by codex; buffered injects cannot be eval'd by zsh after.
        let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-sol", "run-pane-a"));
        let trimmed = plan.command.trim_start();
        assert!(
            trimmed.starts_with("exec "),
            "pane launch must use `exec` (Q2 choice a); got: {}",
            plan.command
        );
        assert!(
            !trimmed.contains("; exit") && !plan.command.contains("\nexit"),
            "must not rely on a trailing shell `exit` after an interactive return: {}",
            plan.command
        );
    }

    /// Integration-style: a shell that runs `exec <long-running>` must not
    /// execute tty-buffered input that arrived while the child was foreground.
    ///
    /// Apparatus (Python `pty`): child runs `exec sleep 1` — same shape as
    /// [`wrap_codex_command_for_pane`]. Mid-run we write a side-effect command
    /// into the master. After the child exits we assert the side-effect file
    /// was **not** created — proving choice (a) closes the Q2 footgun that
    /// Layer D measured against interactive zsh.
    #[test]
    fn exec_launch_discards_buffered_inject_after_exit() {
        let tmp = TempDir::new().unwrap();
        let side_effect = tmp.path().join("injected_side_effect.txt");
        let side_effect_str = side_effect.display().to_string();
        let script = format!(
            r#"
import os, pty, time, pathlib, sys
side = pathlib.Path({side:?})
pid, fd = pty.fork()
if pid == 0:
    # Same shape as wrap_codex_command_for_pane: exec replaces the shell.
    os.execvp("zsh", ["zsh", "-c", "exec sleep 1"])
else:
    time.sleep(0.2)
    os.write(fd, b"echo INJECTED > " + str(side).encode() + b"\n")
    _pid, status = os.waitpid(pid, 0)
    time.sleep(0.1)
    if side.exists():
        sys.stderr.write("side effect file exists: " + side.read_text() + "\n")
        sys.exit(2)
    sys.exit(0)
"#,
            side = side_effect_str,
        );
        let status = Command::new("python3")
            .arg("-c")
            .arg(&script)
            .status()
            .expect("python3 pty harness");
        assert!(
            status.success(),
            "buffered inject must not execute after `exec` worker exits (rc={status:?})"
        );
        assert!(
            !side_effect.exists(),
            "side-effect file must not exist; got {:?}",
            fs::read_to_string(&side_effect).ok()
        );
    }

    /// Driven by a test-owned current-thread runtime rather than
    /// `#[tokio::test]`, because the homes-root override must stay held for
    /// the whole provision → reclaim sequence. Releasing it after the initial
    /// `set_var` left reclaim reading a root any parallel test in this binary
    /// could move (or clear) out from under it — CI saw reclaim refuse a home
    /// under the test temp tree when `codex_homes_root()` had flipped back to
    /// the default `$TMPDIR/boss-codex-homes`. `block_on` keeps the guard
    /// inside one blocking call so we never hold a `MutexGuard` across
    /// `.await` (`clippy::await_holding_lock`).
    #[test]
    fn provision_workspace_creates_owned_home_and_snapshots_auth() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        // Make workspace a git repo so project trust stamps are meaningful.
        let _ = Command::new("git").args(["init"]).current_dir(&workspace).output();

        let auth_src = tmp.path().join("source-auth.json");
        fs::write(&auth_src, sample_auth_json()).unwrap();

        // Point homes + auth source at the temp tree; never touch ~/.codex.
        // `_homes` owns CODEX_HOMES_ENV_TEST_LOCK for its lifetime; AUTH rides
        // on the same lock (see module comment above).
        let _homes = crate::test_support::codex_homes_override(&homes);
        let prior_auth = std::env::var_os(CODEX_AUTH_SOURCE_ENV);
        // SAFETY: lock held by `_homes` for the whole function.
        unsafe {
            std::env::set_var(CODEX_AUTH_SOURCE_ENV, &auth_src);
        }
        // Restore auth before `_homes` drops the lock (field drop order is
        // reverse declaration order, so this guard is dropped first).
        struct RestoreAuth(Option<std::ffi::OsString>);
        impl Drop for RestoreAuth {
            fn drop(&mut self) {
                // SAFETY: still under the homes-override lock.
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var(CODEX_AUTH_SOURCE_ENV, v) },
                    None => unsafe { std::env::remove_var(CODEX_AUTH_SOURCE_ENV) },
                }
            }
        }
        let _restore_auth = RestoreAuth(prior_auth);

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(provision_workspace_creates_owned_home_and_snapshots_auth_body(
                &homes, &workspace,
            ));
    }

    async fn provision_workspace_creates_owned_home_and_snapshots_auth_body(
        homes: &std::path::Path,
        workspace: &std::path::Path,
    ) {
        let driver = CodexDriver::default();
        let state = driver
            .provision_workspace(workspace, "hello prompt", "run-prov-1")
            .await
            .expect("provision")
            .expect("Codex must return runtime state");

        let runtime = CodexRuntimeState::from_driver_runtime_state(&state).unwrap();
        assert!(runtime.codex_home.starts_with(homes));
        assert!(runtime.codex_home.join("auth.json").is_file());
        assert!(runtime.codex_home.join("config.toml").is_file());

        let config = fs::read_to_string(runtime.codex_home.join("config.toml")).unwrap();
        assert!(
            config.contains("[notice.external_config_migration_prompts]") && config.contains("home = true"),
            "must suppress the external-agent config-migration notice via the real \
             nested key (not a nonexistent top-level field): {config}"
        );
        assert!(
            config.contains("[features]") && config.contains("external_agent_memory_import = false"),
            "must pin the memory-import feature off: {config}"
        );
        assert!(
            config.contains("trust_level = \"trusted\""),
            "must stamp project trust: {config}"
        );

        let prompt = workspace.join(".codex/initial-prompt.txt");
        assert_eq!(fs::read_to_string(prompt).unwrap(), "hello prompt");

        // Interactive home must not have been created/scanned/mutated as CODEX_HOME.
        assert_ne!(
            runtime.codex_home,
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex")
        );

        // Teardown adopts auth but retains the home for policy-based reclaim.
        driver
            .teardown_workspace(Some(workspace), "run-prov-1", Some(&state))
            .await
            .unwrap();
        assert!(
            runtime.codex_home.exists(),
            "teardown must retain CODEX_HOME as terminal-run evidence"
        );

        // Explicit reclaim (what the retention sweep does) removes only this root.
        // Must run while the homes-root override is still held so
        // `codex_homes_root()` still matches the path provision recorded.
        reclaim_codex_home(&runtime.codex_home).unwrap();
        assert!(!runtime.codex_home.exists(), "reclaim must remove the recorded home");
        // Idempotent.
        reclaim_codex_home(&runtime.codex_home).unwrap();
    }

    #[test]
    fn base_config_escapes_workspace_paths_with_spaces() {
        let toml = render_base_config_toml(Path::new("/Users/a b/ws"));
        assert!(toml.contains("\"/Users/a b/ws\""), "{toml}");
        assert!(toml.contains("[notice.external_config_migration_prompts]"));
        assert!(toml.contains("[features]"));
        assert!(toml.contains("external_agent_memory_import = false"));
        // No stray unnested/unquoted occurrence of the old, invalid top-level
        // scalar form this config once emitted.
        assert!(!toml.contains("\nexternal_config_migration_prompts = false\n"));
    }

    #[test]
    fn base_config_grants_bazel_sandbox_permissions() {
        let toml = render_base_config_toml(Path::new("/ws"));
        assert!(toml.contains("[sandbox_workspace_write]"), "{toml}");
        assert!(toml.contains("network_access = true"), "{toml}");
        // The table must land before [projects.*] so a duplicate top-level
        // key introduced later in the format string can't silently shadow it.
        let sandbox_pos = toml.find("[sandbox_workspace_write]").unwrap();
        let projects_pos = toml.find("[projects.").unwrap();
        assert!(sandbox_pos < projects_pos, "{toml}");
        // writable_roots is present iff the real environment resolves at
        // least one root (it always does on a dev/CI host with HOME set;
        // this stays non-brittle if some future test host truly lacks one).
        let roots = bazel_writable_roots();
        if roots.is_empty() {
            assert!(!toml.contains("writable_roots"), "{toml}");
        } else {
            let quoted: Vec<String> = roots
                .iter()
                .map(|r| toml_basic_string(&r.display().to_string()))
                .collect();
            assert!(
                toml.contains(&format!("writable_roots = [{}]", quoted.join(", "))),
                "{toml}"
            );
        }
    }

    #[test]
    fn bazel_writable_roots_prefers_test_tmpdir() {
        assert_eq!(
            bazel_writable_roots_impl(Some("/scratch/test-tmp"), Some("/Users/test-home"), None),
            vec![PathBuf::from("/scratch/test-tmp")]
        );
    }

    #[test]
    fn bazel_writable_roots_falls_back_to_platform_cache_dirs() {
        let roots = bazel_writable_roots_impl(None, Some("/Users/test-home"), None);
        let expected = if cfg!(target_os = "macos") {
            vec![
                PathBuf::from("/Users/test-home/Library/Caches/bazel"),
                PathBuf::from("/Users/test-home/.cache"),
            ]
        } else {
            vec![PathBuf::from("/Users/test-home/.cache/bazel")]
        };
        assert_eq!(roots, expected);
    }

    #[test]
    fn bazel_writable_roots_prefers_xdg_cache_home_on_non_macos() {
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(
            bazel_writable_roots_impl(None, Some("/Users/test-home"), Some("/custom/cache")),
            vec![PathBuf::from("/custom/cache/bazel")]
        );
    }

    #[test]
    fn bazel_writable_roots_empty_without_home_or_test_tmpdir() {
        assert_eq!(bazel_writable_roots_impl(None, None, None), Vec::<PathBuf>::new());
        assert_eq!(bazel_writable_roots_impl(Some(""), None, None), Vec::<PathBuf>::new());
    }

    /// Lay out a fake cube secondary jj workspace: `<workspace>/.jj/repo`
    /// pointing at `<repos_root>/<repo>/.jj/repo`, mirroring what `jj` itself
    /// writes for a real cube-leased checkout.
    fn write_cube_jj_pointer(workspace: &Path, repo_root: &Path) {
        fs::create_dir_all(workspace.join(".jj")).unwrap();
        fs::write(
            workspace.join(".jj").join("repo"),
            repo_root.join(".jj").join("repo").display().to_string(),
        )
        .unwrap();
    }

    #[test]
    fn cube_repo_store_root_reads_the_jj_pointer_file() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspaces").join("mono-agent-1");
        let repo_root = tmp.path().join("repos").join("mono");
        write_cube_jj_pointer(&workspace, &repo_root);

        assert_eq!(cube_repo_store_root(&workspace), Some(repo_root));
    }

    #[test]
    fn cube_repo_store_root_none_without_pointer_file() {
        let tmp = TempDir::new().unwrap();
        // Plain checkout: no .jj at all.
        assert_eq!(cube_repo_store_root(tmp.path()), None);
    }

    #[test]
    fn cube_repo_store_root_none_for_unexpected_pointer_shape() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(workspace.join(".jj")).unwrap();
        fs::write(workspace.join(".jj").join("repo"), "/not/a/jj/store/path").unwrap();
        assert_eq!(cube_repo_store_root(&workspace), None);
    }

    #[test]
    fn cube_repo_store_root_resolves_relative_pointer_against_workspace_jj_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspaces").join("mono-agent-1");
        let repo_root = tmp.path().join("repos").join("mono");
        fs::create_dir_all(workspace.join(".jj")).unwrap();
        // jj itself resolves a relative pointer relative to the workspace's
        // own `.jj` directory (tmp/workspaces/mono-agent-1/.jj here), so
        // reaching tmp/repos/mono/.jj/repo takes three `..` hops up to `tmp`.
        let relative_pointer = Path::new("../../../repos/mono/.jj/repo");
        fs::write(
            workspace.join(".jj").join("repo"),
            relative_pointer.display().to_string(),
        )
        .unwrap();

        let resolved = cube_repo_store_root(&workspace).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, repo_root);
    }

    /// The regression this task exists for: a Codex worker in a cube
    /// workspace must be granted write access to the shared jj store, or
    /// every `jj describe`/`jj git fetch` in the sandbox dies with
    /// `Operation not permitted` on the store's lock files.
    #[test]
    fn codex_config_grants_cube_shared_store_as_writable_root() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspaces").join("mono-agent-1");
        let repo_root = tmp.path().join("repos").join("mono");
        write_cube_jj_pointer(&workspace, &repo_root);

        let toml = render_base_config_toml(&workspace);
        let quoted_repo_root = toml_basic_string(&repo_root.display().to_string());
        assert!(
            toml.contains(&quoted_repo_root),
            "writable_roots must include the cube shared repo store: {toml}"
        );
    }

    /// Codex's workspace-write sandbox name-excludes `.git` from every
    /// writable root it is granted, so granting the cube store root alone is
    /// not enough: `jj git fetch`'s `FETCH_HEAD` write and `jj new`'s loose
    /// object writes both land under `<store root>/.git` and get denied with
    /// `Operation not permitted` even though the store root itself is
    /// writable. An explicit `<store root>/.git` entry is its own top-level
    /// writable root and is not subject to that auto-exclusion.
    #[test]
    fn render_sandbox_workspace_write_toml_grants_store_root_git_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspaces").join("mono-agent-1");
        let repo_root = tmp.path().join("repos").join("mono");
        write_cube_jj_pointer(&workspace, &repo_root);

        let toml = render_sandbox_workspace_write_toml(&workspace);
        let quoted_repo_root = toml_basic_string(&repo_root.display().to_string());
        let quoted_git_dir = toml_basic_string(&repo_root.join(".git").display().to_string());
        assert!(
            toml.contains(&quoted_repo_root),
            "writable_roots must include the cube shared repo store root: {toml}"
        );
        assert!(
            toml.contains(&quoted_git_dir),
            "writable_roots must include the store root's .git dir explicitly, since Codex \
             auto-excludes .git from every granted root: {toml}"
        );
    }

    #[test]
    fn codex_progress_ingress_is_run_correlated_rollout_jsonl() {
        let config = ProgressObservationConfig {
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            lease_id: "lease".into(),
            run_id: "run".into(),
            workspace_path: PathBuf::from("/ws"),
            forwarder_binary: PathBuf::from("/bin/boss-event"),
        };
        let driver = CodexDriver::default();
        match driver.progress_observation_wiring(&config) {
            ProgressIngress::AgentJsonlFile(file) => {
                assert_eq!(file.directory, codex_home_for_run("run").unwrap().join("sessions"));
                assert_eq!(file.workspace_path, PathBuf::from("/ws"));
                assert_eq!(file.filename_prefix, "rollout-");
                assert_eq!(file.filename_suffix, ".jsonl");
            }
            ProgressIngress::StdoutJsonl | ProgressIngress::HookCallback(_) => {
                panic!("Codex progress must use the rollout file, not pane stdout or hooks")
            }
        }
        assert_eq!(driver.progress_fidelity(), ProgressFidelity::Rich);
    }

    #[test]
    fn codex_pr_capture_reads_rollout_payload_output_shapes() {
        let driver = CodexDriver::default();
        let input = serde_json::json!({"command":"gh pr create --title rollout"});

        let function_output = driver
            .pr_url_capture_feed(
                "Bash",
                &input,
                &serde_json::json!("https://github.com/example/repo/pull/41\n"),
            )
            .unwrap();
        assert_eq!(function_output.command, "gh pr create --title rollout");
        assert_eq!(function_output.output_text, "https://github.com/example/repo/pull/41\n");

        let custom_output = driver
            .pr_url_capture_feed(
                "Bash",
                &input,
                &serde_json::json!([
                    {"type":"input_text","text":"created"},
                    {"type":"input_text","text":"https://github.com/example/repo/pull/42"}
                ]),
            )
            .unwrap();
        assert_eq!(
            custom_output.output_text,
            "created\nhttps://github.com/example/repo/pull/42"
        );
        assert!(
            driver
                .pr_url_capture_feed("Read", &input, &serde_json::json!("ignored"))
                .is_none()
        );
    }

    #[test]
    fn codex_turn_boundary_on_stop_is_non_continuation() {
        let event = WorkerEvent::Stop {
            session_id: "thread-1".into(),
            stop_hook_active: true,
            stop_reason: StopReason::Completed,
        };
        let driver = CodexDriver::default();
        let boundary = driver.turn_boundary(&event).expect("Stop is a boundary");
        assert_eq!(boundary.session_id, "thread-1");
        assert_eq!(boundary.reason, StopReason::Completed);
        assert!(!boundary.continuation);
        assert!(
            driver
                .turn_boundary(&WorkerEvent::SessionStart {
                    session_id: "thread-1".into(),
                    source: boss_protocol::SessionStartSource::Startup,
                    model: None,
                })
                .is_none()
        );
    }

    #[test]
    fn normalize_progress_event_requires_thread_start_before_turn_events() {
        let raw = serde_json::json!({"type": "turn.completed"});
        assert!(matches!(
            CodexDriver::default().normalize_progress_event(&raw),
            Err(NormalizeError::MissingField("thread_id"))
        ));
    }

    #[test]
    fn append_hooks_toml_emits_pre_tool_use_groups() {
        let base = render_base_config_toml(Path::new("/ws"));
        let guards = vec![MaterializedGuard {
            command_path: PathBuf::from("/tmp/guard.sh"),
            matcher: Some(".*"),
        }];
        let full = append_hooks_toml(&base, &guards);
        assert!(full.contains("[[hooks.PreToolUse]]"));
        assert!(full.contains("matcher = \".*\""));
        assert!(full.contains("command = \"/tmp/guard.sh\""));
    }

    #[test]
    fn python_c_to_script_extracts_body() {
        let body = python_c_to_script(r#"python3 -c "print(1)""#).unwrap();
        assert!(body.contains("#!/usr/bin/env python3"));
        assert!(body.contains("print(1)"));
    }

    #[test]
    fn materialize_guards_writes_executables() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let path_guard = tmp.path().join("path_guard.py");
        fs::write(&path_guard, "print('ok')\n").unwrap();
        let config = ToolUseInterceptionConfig {
            data_dir: Some(tmp.path().to_path_buf()),
            path_guard_script: Some(path_guard),
            checkleft_guard_script: None,
            is_revision: false,
            is_standard_worker: false,
            run_id: Some("r".into()),
            workspace_path: Some(tmp.path().to_path_buf()),
        };
        let guards = materialize_guards(&home, &config).unwrap();
        // path + boss-launch at minimum
        assert!(guards.len() >= 2);
        for g in &guards {
            assert!(g.command_path.is_file(), "{:?}", g.command_path);
        }
    }

    #[test]
    fn empty_run_id_refused_for_codex_home() {
        let err = codex_home_for_run("").expect_err("empty run_id must fail");
        assert!(
            err.to_string().contains("empty"),
            "expected empty-run_id error, got {err:#}"
        );
        assert!(sanitize_run_id_for_home("").is_err());
    }

    #[test]
    fn reviewer_sandbox_extra_args_are_read_only() {
        assert_eq!(codex_sandbox_for_worker_kind(WorkerKind::Reviewer, false), "read-only");
        assert_eq!(codex_sandbox_for_worker_kind(WorkerKind::Reviewer, true), "read-only");
        assert_eq!(
            codex_sandbox_extra_args(WorkerKind::Reviewer, false),
            vec!["--sandbox".to_owned(), "read-only".to_owned()]
        );
        // Final command after permission merge must prefer reviewer read-only
        // over the spawn-plan default workspace-write.
        let plan = CodexDriver::default().spawn_invocation(spawn_request("gpt-5.6-terra", "run-review-sandbox"));
        assert!(
            plan.command.contains("--sandbox workspace-write"),
            "spawn default is workspace-write: {}",
            plan.command
        );
        let merged =
            crate::apply_permission_extra_args(&plan.command, &codex_sandbox_extra_args(WorkerKind::Reviewer, false));
        assert!(
            merged.contains("--sandbox") && merged.contains("read-only"),
            "Reviewer must get --sandbox read-only after extra_args apply: {merged}"
        );
        assert!(
            !merged.contains("workspace-write"),
            "default sandbox must be replaced, not duplicated: {merged}"
        );
    }

    #[test]
    fn standard_worker_sandbox_defaults_to_danger_full_access() {
        // codex_sandbox_enforced=false (the feature-flag default): Standard,
        // Triage, and AnswerAgent all get danger-full-access, matching the
        // Claude driver's no-OS-sandbox posture.
        assert_eq!(
            codex_sandbox_for_worker_kind(WorkerKind::Standard, false),
            "danger-full-access"
        );
        assert_eq!(
            codex_sandbox_for_worker_kind(WorkerKind::Triage, false),
            "danger-full-access"
        );
        assert_eq!(
            codex_sandbox_for_worker_kind(WorkerKind::AnswerAgent, false),
            "danger-full-access"
        );
        // codex_sandbox_enforced=true restores the OS-enforced fence.
        assert_eq!(
            codex_sandbox_for_worker_kind(WorkerKind::Standard, true),
            "workspace-write"
        );
        assert_eq!(
            codex_sandbox_extra_args(WorkerKind::Standard, true),
            vec!["--sandbox".to_owned(), "workspace-write".to_owned()]
        );
        assert_eq!(
            codex_sandbox_extra_args(WorkerKind::Standard, false),
            vec!["--sandbox".to_owned(), "danger-full-access".to_owned()]
        );
    }

    /// Same isolation pattern as
    /// [`provision_workspace_creates_owned_home_and_snapshots_auth`]: hold
    /// the homes-root override for the whole check, not only around
    /// `set_var`. The previous set-then-release shape also always
    /// `remove_var`'d on cleanup (not restore-prior), which could clear a
    /// parallel test's override mid-flight.
    #[test]
    fn teardown_refuses_codex_home_outside_homes_root() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        fs::create_dir_all(&homes).unwrap();
        let outside = tmp.path().join("not-a-boss-home");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("marker"), "keep").unwrap();

        let _homes = crate::test_support::codex_homes_override(&homes);

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(teardown_refuses_codex_home_outside_homes_root_body(
                tmp.path(),
                &homes,
                &outside,
            ));
    }

    async fn teardown_refuses_codex_home_outside_homes_root_body(
        tmp: &std::path::Path,
        homes: &std::path::Path,
        outside: &std::path::Path,
    ) {
        let state = CodexRuntimeState {
            codex_home: outside.to_path_buf(),
            auth_source_path: tmp.join("auth.json"),
            auth_fingerprint: "fp".into(),
            auth_policy: "SnapshotWithRefreshAdoption".into(),
        }
        .to_driver_runtime_state();

        let err = CodexDriver::default()
            .teardown_workspace(None, "run-bad", Some(&state))
            .await
            .expect_err("teardown must refuse out-of-root home");
        assert!(
            err.to_string().contains("outside") || err.to_string().contains("refusing"),
            "expected containment error, got {err:#}"
        );
        assert!(
            outside.join("marker").is_file(),
            "must not delete a path outside homes root"
        );

        // Homes root itself must never be deleted.
        let root_state = CodexRuntimeState {
            codex_home: homes.to_path_buf(),
            auth_source_path: tmp.join("auth.json"),
            auth_fingerprint: "fp".into(),
            auth_policy: "SnapshotWithRefreshAdoption".into(),
        }
        .to_driver_runtime_state();
        let err = CodexDriver::default()
            .teardown_workspace(None, "run-root", Some(&root_state))
            .await
            .expect_err("teardown must refuse homes root");
        assert!(
            err.to_string().contains("equals") || err.to_string().contains("refusing"),
            "expected root-equals error, got {err:#}"
        );
        assert!(homes.is_dir(), "homes root must remain");
    }

    /// `codex exec` is the motivating case for the mid-turn injection guard:
    /// one turn per process with stdin on `/dev/null`, so bytes written
    /// mid-turn are never read and are later executed by the interactive
    /// shell. Declared explicitly rather than inherited from the trait
    /// default, so this stays asserted even if the default ever changes.
    #[test]
    fn codex_rejects_mid_turn_pane_input() {
        let driver = CodexDriver::default();
        assert_eq!(driver.mid_turn_pane_input(), MidTurnPaneInput::Rejects);
        assert!(!driver.mid_turn_pane_input().buffers());
    }

    #[test]
    fn codex_control_verbs_match_existing_engine_paths() {
        let driver = CodexDriver::default();
        assert_eq!(driver.probe(), ProbeDelivery::PaneText);
        assert_eq!(driver.interrupt(), InterruptDelivery::PaneEsc);
        assert_eq!(driver.stop(), StopDelivery::ProcessOnly);
        assert_eq!(driver.reap(), ReapDelivery::ProcessGroup);
    }

    /// The declaration the engine's process-liveness reapers key off. Without
    /// it a clean `codex exec` termination reads as a pane death and the run
    /// is orphaned milliseconds after it succeeded.
    #[test]
    fn codex_declares_one_turn_per_process_lifetime() {
        let driver = CodexDriver::default();
        assert_eq!(
            driver.worker_process_lifetime(),
            WorkerProcessLifetime::OneTurnPerProcess
        );
        assert!(driver.worker_process_lifetime().exits_after_each_turn());
    }
}
