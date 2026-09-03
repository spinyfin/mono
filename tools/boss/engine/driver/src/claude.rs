//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn`, `WorkspaceProvisioning`, and `PromptComposition`
//! capabilities are live; remaining behavioural methods are `unimplemented!()`
//! pending their per-capability extraction tasks (Depth 1–2 in the design).

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_engine_transient_error::ErrorClass;
use boss_protocol::{
    EffortLevel, NormalizeError, PaneMonitorSpec, ReasoningMode, ReviewModelTier, WorkerEvent, normalize_hook_event,
};
use boss_ssh_transport::shell_quote;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, EnvDirective, HookWiringDestination, InterruptDelivery,
    InterruptGesture, InterruptPlan, MidTurnPaneInput, ModelMenu, PermissionArtifacts, PermissionInput, ProbeDelivery,
    ProgressFidelity, ProgressIngress, ProgressObservationConfig, ProgressObservationWiring, ReapDelivery, SpawnPlan,
    SpawnRequest, StopDelivery, StructuredOutputArtifacts, StructuredOutputRequest, ToolUseInterceptionConfig,
    ToolUseInterceptionWiring, TurnEnd, TurnEndEvidence, WorkerErrorClass, default_structured_output_wiring,
};

pub mod structured_output;

// ---------------------------------------------------------------------------
// Claude model / effort menu (design §1.4 / §Mix-and-match)
// ---------------------------------------------------------------------------
//
// These are the per-driver table functions referenced from CLAUDE_DESCRIPTOR.model_menu.
// The same tables lived in `effort.rs` as global functions prior to this move.
// All callers now route through the driver's ModelMenu rather than calling
// these functions directly.

fn claude_effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    })
}

/// Model slug for a given [`ReasoningMode`] — the capability lever.
///
/// The tier table that distinction resolves against for the Claude driver.
/// Neither arm consults [`EffortLevel`], which is the entire point — a small
/// investigate-and-fix chore gets Opus without its effort level being inflated
/// to `large`, and a big-but-mechanical `large` row gets Sonnet.
///
/// Family aliases, not pinned snapshots, for the same reason as
/// [`claude_default_model_for_level`]. Fable is deliberately absent: it is the
/// most expensive model in the menu and is only ever reachable through an
/// explicit per-row `--model fable`, never as a table default.
fn claude_model_for_reasoning(reasoning: ReasoningMode) -> &'static str {
    match reasoning {
        ReasoningMode::Standard => "sonnet",
        ReasoningMode::Investigation => "opus",
    }
}

/// Concrete model mapping for metadata-derived review tiers. This is kept
/// distinct from the task reasoning menu: reviews are sized from the PR,
/// never from the work item's classification.
fn claude_review_model_for_tier(tier: ReviewModelTier) -> &'static str {
    match tier {
        ReviewModelTier::Fast | ReviewModelTier::Balanced => "sonnet",
        ReviewModelTier::Strong => "opus",
    }
}

/// Default model slug for a given effort level.
///
/// **Legacy fall-through only.** Since the `reasoning` column landed, this
/// table is consulted exclusively for rows that carry no [`ReasoningMode`] —
/// rows created before the column existed, and insert paths that do not seed
/// it. Classified rows resolve through [`claude_model_for_reasoning`] instead.
/// Do not extend this table to express capability: effort is a size signal,
/// and deriving the model from it is precisely the conflation the reasoning
/// column exists to undo. It stays because clearing a row's reasoning must
/// restore exactly the dispatch behaviour that row had before.
///
/// Family aliases (`"sonnet"`, `"opus"`, `"fable"`) are used so the engine
/// auto-tracks the latest snapshot per family without requiring a code
/// change on each model release.
///
/// `Trivial` maps to `sonnet`, NOT `haiku`. Per issue #746 ("don't use haiku")
/// Boss must never dispatch a worker on Haiku: on the user's work machine Haiku
/// supports neither auto mode nor `--dangerously-skip-permissions`, so it prompts
/// for every edit. Trivial work still runs at `--effort low`; only the model floor
/// is raised to Sonnet. Do not lower it back to Haiku.
///
/// The table tops out at Opus for every level, including `Max` — Fable is
/// the most expensive model in the menu, so it is never a *default* for any
/// row regardless of kind or effort. Fable is still a valid model slug, but
/// only via an explicit, hand-set `--model fable` / `model_override` on the
/// row (`resolve_spawn_config` precedence step 1), never a table default.
///
/// Tier ordering, highest to lowest:
/// Fable (`fable`, opt-in only) > Opus (`opus`) > Sonnet (`sonnet`) > Haiku.
fn claude_default_model_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial | EffortLevel::Small | EffortLevel::Medium => "sonnet",
        EffortLevel::Large | EffortLevel::Max => "opus",
    }
}

/// Optional per-level worker-prompt addendum prepended to `.claude/initial-prompt.txt`.
/// `None` for levels where the existing task-implementation framing is already correct.
fn claude_prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
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

/// Returns `true` iff the model slug belongs to the Opus or Fable tier (both require
/// `--permission-mode auto` instead of `--dangerously-skip-permissions`).
/// Matching is case-insensitive substring search.
fn claude_model_requires_auto_permissions(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("opus") || lower.contains("fable")
}

/// Returns `true` iff `model` names a Claude model: either a dated slug
/// (contains `"claude"`, e.g. `"claude-opus-4-7"`) or one of the bare family
/// aliases the effort/reasoning tables above hand out (`"opus"`, `"sonnet"`,
/// `"haiku"`, `"fable"`). Case-insensitive.
fn claude_model_belongs_to_driver(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("claude") || matches!(lower.as_str(), "opus" | "sonnet" | "haiku" | "fable")
}

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        // Intentionally stays "opus", not "fable": this is only the step-5
        // fallback in resolve_spawn_config for a row with no model_override,
        // no pool override, no effort_level, and no product default — a
        // conservative mid-tier fallback for untagged/misconfigured rows,
        // not the effort=max dispatch tier. It was introduced post-suspension
        // by the ModelMenu refactor and was never "fable" pre-suspension.
        engine_default: "opus",
        effort_value_for_level: claude_effort_value_for_level,
        default_model_for_level: claude_default_model_for_level,
        model_for_reasoning: claude_model_for_reasoning,
        review_model_for_tier: claude_review_model_for_tier,
        design_investigation_model: Some(|| "fable"),
        prompt_addendum_for_level: claude_prompt_addendum_for_level,
        model_requires_auto_permissions: claude_model_requires_auto_permissions,
        model_belongs_to_driver: claude_model_belongs_to_driver,
    },
};

/// The seven Claude hook events wired to the `boss-event` forwarder for
/// rich-tier ProgressObservation, in lifecycle order. Output key order is
/// independent of this list (the settings file serialises a sorted map); the
/// order here is purely for readers.
const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Notification",
    "SessionEnd",
];

/// Python preamble shared by every inline command-inspecting decision hook.
///
/// Establishes two things every guard body below relies on:
///
/// 1. `cmd` — the shell command string, guaranteed to be a `str`.
/// 2. `_block(msg)` / `_approve()` — the decision emitters.
///
/// **It fails closed.** A payload the guard cannot parse into a command
/// string is *blocked*, not approved: an unrecognised payload shape means the
/// guard cannot prove the call is safe, and a guardrail that approves what it
/// cannot read is worse than no guardrail, because the worker prompt asserts
/// the protection is real. This matters specifically for drivers whose tool
/// vocabulary is not Claude's — Codex reaches these same scripts through its
/// own `PreToolUse` wire (see
/// [`crate::codex::write_hooks_and_attest`]), and a future payload change
/// there must surface as a loud block rather than as silent approval. The
/// empirical basis for the shapes handled here is
/// `tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`.
///
/// Emitted as a `macro_rules!` expansion rather than a `const` because the
/// guard bodies are `concat!`-built string literals, which cannot interpolate
/// a `const`. Every line avoids `"`, `$` and backticks: the whole command is
/// itself embedded in a double-quoted `sh -c` string.
macro_rules! python_command_guard {
    ($($body:expr),+ $(,)?) => {
        concat!(
            "python3 -c \"\n",
            "import json,os,sys,re,shlex\n",
            "def _emit(d):\n",
            "    print(json.dumps(d))\n",
            "    sys.exit(0)\n",
            "def _approve():\n",
            "    _emit({'decision':'approve'})\n",
            "def _block(msg):\n",
            "    _emit({'decision':'block','reason':msg})\n",
            "_SHAPE=('Blocked (fail-closed): a Boss PreToolUse guard could not read this tool ",
            "call as a shell command, so it cannot prove the call is allowed. Guards deny what ",
            "they cannot parse rather than approving by default. Re-issue the work as an ordinary ",
            "shell command, and report this payload shape to the operator -- it means Boss guard ",
            "wiring needs updating for this agent driver. Detail: ')\n",
            "try:\n",
            "    inp=json.load(sys.stdin)\n",
            "except Exception as _e:\n",
            "    _block(_SHAPE+'hook stdin was not JSON ('+str(_e)+')')\n",
            "if not isinstance(inp,dict):\n",
            "    _block(_SHAPE+'hook payload was not a JSON object')\n",
            "_ti=inp.get('tool_input')\n",
            "if not isinstance(_ti,dict):\n",
            "    _block(_SHAPE+'tool_input was '+type(_ti).__name__+', not an object')\n",
            "cmd=_ti.get('command')\n",
            "if not isinstance(cmd,str):\n",
            "    _block(_SHAPE+'tool_input.command was '+type(cmd).__name__+', not a string')\n",
            $($body),+,
            "\""
        )
    };
}

/// Tokenizer fragment shared by [`REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND`]
/// and [`BOSS_LAUNCH_GUARD_COMMAND`]: splits a (possibly multi-line) shell
/// command string into independent argv groups at shell delimiters, and
/// strips leading env-assignment / launcher-wrapper / `timeout <n>` tokens to
/// reach the real program.
///
/// `shlex.split` does not tokenize a shell operator written without
/// surrounding spaces (`cd x&&bazel test //y` is a single token), so this
/// uses `shlex.shlex(..., punctuation_chars=";&|")` with `whitespace_split =
/// True`, which splits `a&&b` into `a`, `&&`, `b`. `commenters` is cleared
/// because `shlex.shlex` otherwise treats `#` as a comment introducer
/// anywhere in a word, not just at the start of one, silently discarding the
/// remainder of the line (`shlex.split` does not have this behavior — it
/// sets `commenters=''` itself). This is the same lexer configuration used
/// by `boss_engine::worker_setup`'s `PATH_GUARD_SCRIPT` for the
/// `BOSS_DATA_DIR` path guard, and by
/// [`crate::codex::tool_surface_guard::CODEX_TOOL_SURFACE_GUARD_SCRIPT`]'s
/// `command_groups`.
///
/// A plain string-literal macro (not a `const`) for the same reason
/// [`python_command_guard!`] is one: it is spliced into other `concat!`
/// invocations, which require every argument to be a literal.
macro_rules! shell_command_tokenizer_fragment {
    () => {
        "DELIMS={'&&','||',';','|','&'}\n\
         WRAPPERS={'env','command','exec','nohup','stdbuf','setsid','caffeinate','sudo','time','xargs'}\n\
         ASSIGNMENT_RE=re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')\n\
         def command_groups(command):\n\
         \x20   groups=[]\n\
         \x20   for line in command.split(chr(10)):\n\
         \x20       try:\n\
         \x20           _lex=shlex.shlex(line,posix=True,punctuation_chars=';&|')\n\
         \x20           _lex.whitespace_split=True\n\
         \x20           _lex.commenters=''\n\
         \x20           toks=list(_lex)\n\
         \x20       except Exception:\n\
         \x20           toks=line.split()\n\
         \x20       cur=[]\n\
         \x20       for tok in toks:\n\
         \x20           if tok in DELIMS:\n\
         \x20               if cur:\n\
         \x20                   groups.append(cur)\n\
         \x20               cur=[]\n\
         \x20           else:\n\
         \x20               cur.append(tok)\n\
         \x20       if cur:\n\
         \x20           groups.append(cur)\n\
         \x20   return groups\n\
         def strip_prefixes(group):\n\
         \x20   i=0\n\
         \x20   while i<len(group):\n\
         \x20       tok=group[i]\n\
         \x20       base=os.path.basename(tok)\n\
         \x20       if ASSIGNMENT_RE.match(tok) or base in WRAPPERS:\n\
         \x20           i+=1\n\
         \x20           continue\n\
         \x20       if base=='timeout' and i+1<len(group):\n\
         \x20           i+=2\n\
         \x20           continue\n\
         \x20       break\n\
         \x20   return group[i:]\n"
    };
}

/// Inline Python decision hook that blocks workers from launching the Boss
/// macOS app or a Boss engine. Always applied (matcher `Bash`).
///
/// **This is the advisory layer, not the control.** It fails in-session, with
/// a message the worker can act on immediately, which the engine-side gate
/// (`boss_engine::app::agent_launch_guard`) cannot do — by the time that gate
/// fires the worker has already spent a build. But a text matcher only
/// recognises spellings, and the previous revision of this guard was defeated
/// twice in four hours by ordinary shell idiom: an engine invoked as
/// `./bazel-bin/tools/boss/engine/core/engine` (no bundle shape in the path),
/// and a bundle path assigned to a variable on one line with `open "$APP"` on
/// the next (no single line carrying both). The binary-side gate is what
/// actually holds; treat this as a fast, friendly first failure.
///
/// The matcher works on `shlex`-tokenised commands rather than raw text, so:
///
/// - Single-assignment shell variables are resolved before matching, which
///   catches the `APP=…/Boss.app` + `open "$APP"` split.
/// - Launcher prefixes (`nohup`, `env`, `sudo`, `exec`, `timeout <n>`, …) are
///   skipped so they cannot hide the program being run.
/// - A phrase inside a quoted argument (a commit message, a `--body`) is one
///   token and never matches.
///
/// Blocked: any program whose basename is `Boss`, `engine`, `boss-engine`,
/// or `bossctl` (bossctl is coordinator-only, never worker-facing, even by
/// absolute bundled path), any program path containing `Boss.app` *except*
/// CLI tools under `Contents/Resources/bin/`, `open` of a Boss bundle /
/// `-a Boss` / the bundle id, `bazel run` of an `app-macos` target without
/// an isolating `BOSS_SOCKET_PATH` + `BOSS_ENGINE_AUTOSTART=0`, `bazel run`
/// of an engine target without an isolating `--socket-path`, a `boss engine
/// start` / `boss engine stop` invocation of any `boss` binary (PATH or
/// bundled path — these bounce the engine out from under the worker), and
/// `swift run`.
///
/// Allowed: `bazel build`, `bazel test`, unpacking or inspecting a bundle,
/// absolute paths to bundled CLI tools that are not Boss-tier
/// (`…/Boss.app/Contents/Resources/bin/boss`, `boss-event`, `cube`, … —
/// these are command-line tools that start no GUI; the basename rule above
/// still blocks `bossctl` and the engine binary living in that same
/// directory), `bazel run //tools/boss/engine/core:engine -- --socket-path
/// <non-production>`, and isolated capture launches of the app
/// (`BOSS_SOCKET_PATH=/tmp/boss-shot-<id>.sock BOSS_ENGINE_AUTOSTART=0 bazel
/// run //tools/boss/app-macos:Boss -- --capture-to <path>.png`).
///
/// Distinguishing CLI-under-bundle from app launch is load-bearing: workers
/// must be able to invoke the version-matched `boss` that ships inside the
/// running app (including by absolute path); `bossctl` stays off the worker
/// surface entirely, per the coordinator-only invariant pinned by
/// `boss_engine_worker_bin::launcher_names`. Conflating CLI-under-bundle
/// with app launch would have forbidden the only correct binary while still
/// letting a stale `PATH` copy
/// win if the launcher ever slipped.
pub const BOSS_LAUNCH_GUARD_COMMAND: &str = python_command_guard!(
    shell_command_tokenizer_fragment!(),
    "PROD='/tmp/boss-engine.sock'\n",
    "ASSIGN=re.compile(r'^([A-Za-z_][A-Za-z0-9_]*)=(.*)$', re.S)\n",
    // Resolve single-level shell variables. Assignments are collected as the
    // token stream is walked, so a value defined earlier in the command is
    // substituted into later tokens -- the `APP=...` / `open $APP` split.
    // This is a separate pass over the groups `command_groups` (from the
    // shared tokenizer fragment) already produced, so it applies uniformly
    // whether a group came from before or after a delimiter, and whether it
    // spans one line or several -- `vars` carries across both.
    "vars={}\n",
    "def expand(t):\n",
    "    for k,v in vars.items():\n",
    "        t=t.replace('$'+'{'+k+'}',v).replace('$'+k,v)\n",
    "    return t\n",
    "groups=[]\n",
    "for g in command_groups(cmd):\n",
    "    resolved=[]\n",
    "    for t in g:\n",
    "        m=ASSIGN.match(t)\n",
    "        if m:\n",
    "            vars[m.group(1)]=expand(m.group(2))\n",
    "        resolved.append(expand(t))\n",
    "    groups.append(resolved)\n",
    "def socket_arg(g):\n",
    "    for j,t in enumerate(g):\n",
    "        if t=='--socket-path' and j+1<len(g):\n",
    "            return g[j+1]\n",
    "        if t.startswith('--socket-path='):\n",
    "            return t.split('=',1)[1]\n",
    "    return None\n",
    // CLI tools ship inside the .app at Contents/Resources/bin/. Executing
    // those is not launching the app (Contents/MacOS/<AppName> / open -a).
    // Basename Boss/engine/boss-engine/bossctl is still blocked above this
    // helper. Require the *normalized* parent directory to be exactly
    // Contents/Resources/bin -- a plain substring test on the raw path
    // would let a traversal like '.../Contents/Resources/bin/../MacOS/x'
    // through even though it resolves outside bin/.
    "def is_bundle_cli(p):\n",
    "    d=os.path.dirname(os.path.normpath(p))\n",
    "    return d.endswith('/Contents/Resources/bin')\n",
    "matched=None\n",
    "for g in groups:\n",
    "    rest=strip_prefixes(g)\n",
    "    if not rest:\n",
    "        continue\n",
    "    prog=rest[0]\n",
    "    base=os.path.basename(prog)\n",
    "    if base in ('Boss','engine','boss-engine','bossctl'):\n",
    "        matched=prog\n",
    "        break\n",
    "    if 'Boss.app' in prog and not is_bundle_cli(prog):\n",
    "        matched=prog\n",
    "        break\n",
    // `boss engine start|stop` bounces the engine out from under the
    // worker regardless of whether `boss` was resolved via PATH or an
    // absolute bundled path -- the bare-name deny rules in
    // `worker_setup::deny_rules` only match the literal PATH-invocation
    // text, so the guard closes the absolute-path gap here.
    "    if base=='boss' and len(rest)>=3 and rest[1]=='engine' and rest[2] in ('start','stop'):\n",
    "        matched=prog+' engine '+rest[2]\n",
    "        break\n",
    "    if base=='open':\n",
    "        joined=' '.join(rest)\n",
    "        if 'Boss.app' in joined or 'dev.spinyfin.bossmacapp' in joined or re.search(r'(^| )-a +Boss( |$)',joined):\n",
    "            matched=joined[:120]\n",
    "            break\n",
    "    if base in ('bazel','bazelisk') and len(rest)>1 and rest[1]=='run':\n",
    "        joined=' '.join(rest)\n",
    "        if 'tools/boss/app-macos' in joined:\n",
    "            sock=vars.get('BOSS_SOCKET_PATH')\n",
    "            auto=vars.get('BOSS_ENGINE_AUTOSTART')\n",
    "            if (sock is None or os.path.normpath(sock)==PROD\n",
    "                    or 'Library/Application Support/Boss' in sock or auto!='0'):\n",
    "                matched='bazel run of an app-macos target without an isolating BOSS_SOCKET_PATH and BOSS_ENGINE_AUTOSTART=0'\n",
    "                break\n",
    "        if 'tools/boss/engine' in joined:\n",
    "            sp=socket_arg(rest)\n",
    "            if sp is None or os.path.normpath(sp)==PROD or 'Library/Application Support/Boss' in sp:\n",
    "                matched='bazel run of an engine target without an isolating --socket-path'\n",
    "                break\n",
    "    if base=='swift' and len(rest)>1 and rest[1]=='run':\n",
    "        matched='swift run'\n",
    "        break\n",
    "if matched:\n",
    "    msg=('Blocked: this would start Boss itself (matched: '+matched+'). Workers must not launch ",
    "the installed /Applications/Boss.app or an engine that can reach production state on this ",
    "machine -- the live app seizes the running engine, and an unisolated launch puts a window ",
    "on the operator screen. To exercise a real engine, start an isolated one: env -u ",
    "BOSS_EVENTS_SOCKET ",
    env!("BOSS_ENGINE_BAZEL_RUN_COMMAND"),
    " -- --socket-path /tmp/boss-test-<id>.sock. ",
    "Any --socket-path other than /tmp/boss-engine.sock derives its own db, events socket, pid file ",
    "and control token; unsetting BOSS_EVENTS_SOCKET matters because every worker pane inherits one ",
    "pointing at production. To screenshot the real Boss UI quietly, launch an isolated capture ",
    "instance: BOSS_SOCKET_PATH=/tmp/boss-shot-<id>.sock BOSS_ENGINE_AUTOSTART=0 bazel run ",
    "//tools/boss/app-macos:Boss -- --capture-to <path>.png. The instance renders itself in-process ",
    "via cacheDisplay and exits; it never shows a window, never takes focus, and needs no ",
    "screen-recording permission. Read the PNG back and state in the PR what you verified. ",
    "Building and testing are unaffected (bazel build, bazel test).')\n",
    "    _block(msg)\n",
    "_approve()\n",
);

/// Inline Python decision hook that blocks all Standard workers from pushing
/// branches or opening PRs via bare VCS commands (`gh pr create`, `jj git push`,
/// `git push`). Uses `shlex.split()` so push/PR-creation phrases inside quoted
/// arguments do NOT trigger the block.
///
/// Applies to ALL `WorkerKind::Standard` workers (local and remote). The
/// revision-specific guard ([`REVISION_PR_GUARD_COMMAND`]) stacks on top for
/// revision workers and adds additional blocks.
pub const PR_REDIRECT_GUARD_COMMAND: &str = python_command_guard!(
    "DELIMS={'&&','||',';','|','&'}\n",
    "try:\n",
    "    toks=shlex.split(cmd,posix=True)\n",
    "except Exception:\n",
    "    toks=cmd.split()\n",
    "groups=[]\n",
    "cur=[]\n",
    "for t in toks:\n",
    "    if t in DELIMS:\n",
    "        if cur:\n",
    "            groups.append(cur[:])\n",
    "        cur=[]\n",
    "    else:\n",
    "        cur.append(t)\n",
    "if cur:\n",
    "    groups.append(cur)\n",
    "matched=None\n",
    "for g in groups:\n",
    "    i=0\n",
    "    while i<len(g) and re.match(r'^[A-Za-z_][A-Za-z0-9_]*=',g[i]):\n",
    "        i+=1\n",
    "    rest=g[i:]\n",
    "    prog=os.path.basename(rest[0]) if rest else ''\n",
    "    if len(rest)>=3 and prog=='gh' and rest[1]=='pr' and rest[2]=='create':\n",
    "        matched='gh pr create'\n",
    "        break\n",
    "    if len(rest)>=3 and prog=='jj' and rest[1]=='git' and rest[2]=='push':\n",
    "        matched='jj git push'\n",
    "        break\n",
    "    if len(rest)>=2 and prog=='git' and rest[1]=='push':\n",
    "        matched='git push'\n",
    "        break\n",
    "if matched:\n",
    "    msg='Workers must not push branches or open PRs with bare VCS commands (blocked: '+matched+'). Use cube instead: cube pr create --branch <branch> (new PR: pushes the branch and opens the PR in one step, jj-aware, no GIT_DIR) or cube pr update --branch <branch> (existing PR: pushes new commits to it). Never use jj git push, git push, or gh pr create directly.'\n",
    "    _block(msg)\n",
    "_approve()\n",
);

/// Inline Python decision hook that guards revision tasks from opening new PRs.
/// Uses `shlex.split()` to tokenise the Bash command so PR-creation phrases
/// inside quoted arguments do NOT trigger the block. Blocks `gh pr create`,
/// `cube pr create`, and the deprecated `cube pr ensure`; allows `cube pr update`.
pub const REVISION_PR_GUARD_COMMAND: &str = python_command_guard!(
    "DELIMS={'&&','||',';','|','&'}\n",
    "try:\n",
    "    toks=shlex.split(cmd,posix=True)\n",
    "except Exception:\n",
    "    toks=cmd.split()\n",
    "groups=[]\n",
    "cur=[]\n",
    "for t in toks:\n",
    "    if t in DELIMS:\n",
    "        if cur:\n",
    "            groups.append(cur[:])\n",
    "        cur=[]\n",
    "    else:\n",
    "        cur.append(t)\n",
    "if cur:\n",
    "    groups.append(cur)\n",
    "def branch_of(g):\n",
    "    for j,t in enumerate(g):\n",
    "        if t in ('--branch','--head') and j+1<len(g):\n",
    "            return g[j+1]\n",
    "        if t.startswith('--branch=') or t.startswith('--head='):\n",
    "            return t.split('=',1)[1]\n",
    "    return None\n",
    "matched=None\n",
    "br=None\n",
    "for g in groups:\n",
    "    i=0\n",
    "    while i<len(g) and re.match(r'^[A-Za-z_][A-Za-z0-9_]*=',g[i]):\n",
    "        i+=1\n",
    "    rest=g[i:]\n",
    "    if len(rest)>=3 and rest[0]=='gh' and rest[1]=='pr' and rest[2]=='create':\n",
    "        matched='gh pr create'\n",
    "        br=branch_of(rest)\n",
    "        break\n",
    "    if len(rest)>=3 and rest[0]=='cube' and rest[1]=='pr' and rest[2] in ('create','ensure'):\n",
    "        matched='cube pr '+rest[2]\n",
    "        br=branch_of(rest)\n",
    "        break\n",
    "if matched:\n",
    "    sug='cube pr update --branch '+br if br else 'cube pr update --branch <your-pr-bookmark>'\n",
    "    msg='Revision tasks push commits to the existing parent PR; they must not open a new PR (matched command: '+matched+'). Push your commits to the existing PR with: '+sug\n",
    "    _block(msg)\n",
    "_approve()\n",
);

/// Inline Python decision hook for static-analysis-only reviewer sessions.
///
/// Mutation and publication remain fenced by the existing reviewer deny rules
/// and driver sandboxes. This complementary guard closes the command surface
/// those controls intentionally leave open: builds, tests, formatters,
/// generators, language runners, shell interpreters, and direct execution of
/// checked-out artifacts. It is shared unchanged by Claude, Codex, and Grok.
pub const REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND: &str = python_command_guard!(
    shell_command_tokenizer_fragment!(),
    "BLOCKED={'make','just','cmake','ninja','meson','buck','xcodebuild','gradle','gradlew','mvn','mvnw','sbt','dotnet','pytest','tox','nox','rustfmt','gofmt','prettier','black','ruff','protoc','buf','codegen','npx','npm','pnpm','yarn','bun','deno','node','python','python3','ruby','perl','php','lua','java','kotlinc','swift','uv','poetry','pipenv','maturin','checkleft'}\n",
    "matched=None\n",
    "for group in command_groups(cmd):\n",
    "    rest=strip_prefixes(group)\n",
    "    if not rest:\n",
    "        continue\n",
    "    prog=rest[0]\n",
    "    base=os.path.basename(prog)\n",
    "    subcommands=set(rest[1:])\n",
    "    if base in ('bash','sh','zsh','fish') or base in ('source','.'):\n",
    "        matched=base\n",
    "    elif base in ('bazel','bazelisk') and subcommands.intersection({'build','test','run','coverage'}):\n",
    "        matched=base+' execution subcommand'\n",
    "    elif base=='cargo' and subcommands.intersection({'build','test','run','bench','fmt','clippy','install'}):\n",
    "        matched='cargo execution subcommand'\n",
    "    elif base=='go' and subcommands.intersection({'build','test','run','generate','install'}):\n",
    "        matched='go execution subcommand'\n",
    "    elif base in BLOCKED:\n",
    "        matched=base\n",
    "    elif prog.startswith('./') or prog.startswith('../') or '/bazel-bin/' in prog or '/target/' in prog:\n",
    "        matched=prog\n",
    "    if matched:\n",
    "        break\n",
    "if matched:\n",
    "    _block('Blocked: reviewers perform static analysis only (matched: '+matched+'). Do not run builds, tests, formatters, generators, language runners, shell interpreters, or checked-out executables. Read source, diffs, and metadata instead. If a claim depends on execution, record it with needs_runtime_verification: true in the review report.')\n",
    "_approve()\n",
);

/// The driver-specific preamble for the agent-rules file. Names the hook
/// mechanism ("claude hooks") and is injected at the top of `CLAUDE.md` by
/// `boss_engine::worker_setup::render_claude_md`.
const CLAUDE_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via claude hooks.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.";

/// Reference implementation of [`AgentDriver`] for Claude Code.
///
/// Declares all capabilities (Claude is the full-fidelity reference driver).
/// Behavioural methods are extracted from `boss_engine`'s `effort`,
/// `worker_setup`, and `runner` modules, and from
/// [`boss_engine_transient_error`].
pub struct ClaudeDriver;

#[async_trait]
impl AgentDriver for ClaudeDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &CLAUDE_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Claude provides all capabilities. ToolProvisioning is declared
        // provided even though it is unused in v1 — the driver could in
        // principle inject MCP servers; it currently does not.
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
            Capability::ToolProvisioning,
            Capability::PromptComposition,
            Capability::AwaitingInputSignal,
            Capability::CommandOutcomeObservation,
        ])
    }

    fn remote_spawn_host_independent(&self) -> bool {
        true
    }

    fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
        // Exact reproduction of the pre-spec literals in
        // `GhosttyTerminalView.makeClaudeSnapshot` / the tracker's
        // two-poll idle debounce. Shipping them on the wire keeps the
        // app's Claude-default fallback and the engine-supplied path
        // behaviour-identical for Claude workers.
        Some(PaneMonitorSpec {
            agent_markers: vec!["Claude Code".into(), "auto mode on".into(), "/effort".into()],
            busy_markers: vec!["esc to interrupt".into()],
            starting_markers: vec!["Accessing workspace:".into(), "Quick safety check:".into()],
            prompt_prefixes: vec!["❯".into()],
            idle_debounce_polls: 2,
        })
    }

    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        let SpawnRequest {
            model,
            effort,
            settings_path,
            non_opus_auto_mode,
            permission_mode_override,
            run_id: _,
        } = request;
        let mut cmd = format!("claude --model {model}");
        if let Some(e) = effort {
            cmd.push_str(" --effort ");
            cmd.push_str(e);
        }
        if let Some(mode) = permission_mode_override {
            // Forced mode (e.g. `dontAsk` for the capability-restricted answer
            // agent). Suppresses BOTH the `auto` and `--dangerously-skip-permissions`
            // branches — the latter would bypass the settings allow/deny rules
            // entirely, defeating the allowlist. The CLI flag wins over the
            // settings-file `defaultMode`, so this is the authoritative mode.
            cmd.push_str(" --permission-mode ");
            cmd.push_str(mode);
        } else if claude_model_requires_auto_permissions(model) || non_opus_auto_mode {
            cmd.push_str(" --permission-mode auto");
        } else {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if let Some(settings) = settings_path {
            // Single-quote the path so a `$TMPDIR` with spaces survives
            // the pane's shell. Worker settings paths never contain a
            // single quote, so naive single-quoting is sufficient.
            cmd.push_str(&format!(" --settings '{}'", settings.display()));
        }
        // Use the descriptor's config_dir and initial_prompt_filename so this
        // stays in sync with provision_workspace's write location.
        cmd.push_str(&format!(
            " \"$(cat {}/{})\"\n",
            CLAUDE_DESCRIPTOR.config_dir, CLAUDE_DESCRIPTOR.initial_prompt_filename,
        ));
        SpawnPlan {
            // Claude must authenticate via OAuth credentials
            // (~/.claude/.credentials.json), not a stray ANTHROPIC_API_KEY
            // inherited from the user's shell profile (or `launchctl
            // setenv`) — that produces "Auth conflict: Using
            // ANTHROPIC_API_KEY instead of Anthropic Console key." The
            // engine needs the var in its own process for pane-summary LLM
            // calls, so this unset is scoped to the worker pane shell only.
            env: vec![EnvDirective::Unset("ANTHROPIC_API_KEY".to_owned())],
            command: cmd,
        }
    }

    /// Write per-session workspace files and suppress the first-run trust
    /// prompt. Specifically:
    ///
    /// - Creates `<workspace>/<config_dir>/` (`.claude/`)
    /// - Writes the initial prompt to `<config_dir>/<initial_prompt_filename>`
    /// - Writes a catch-all `.gitignore` so engine-injected files never appear
    ///   in `jj status` / `git status`
    /// - Pre-seeds `~/.claude.json` so the folder-trust dialog does not block
    ///   the headless worker session
    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<Option<super::DriverRuntimeState>> {
        let config_dir = workspace.join(CLAUDE_DESCRIPTOR.config_dir);
        std::fs::create_dir_all(&config_dir).with_context(|| format!("creating {}", config_dir.display()))?;

        let prompt_path = config_dir.join(CLAUDE_DESCRIPTOR.initial_prompt_filename);
        std::fs::write(&prompt_path, prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;

        let gitignore_path = config_dir.join(".gitignore");
        std::fs::write(&gitignore_path, CLAUDE_DIR_GITIGNORE)
            .with_context(|| format!("writing gitignore to {}", gitignore_path.display()))?;

        // Pre-seed the Claude global config so the folder-trust dialog does
        // not block the headless worker. Best-effort: failure is logged and
        // swallowed by pre_trust_workspace.
        pre_trust_workspace(workspace);

        // Claude creates no per-run state outside the cube workspace, so
        // there is nothing for teardown (or a future retention sweep) to
        // clean up. A Codex driver would return its Boss-owned CODEX_HOME
        // (or archive root) here instead.
        Ok(None)
    }

    /// No-op: Claude needs no teardown. It creates no per-run state outside
    /// the cube workspace — the `.claude/` config dir `provision_workspace`
    /// writes lives inside the workspace, which cube owns and tears down
    /// itself. `runtime_state` is expected to be `None` for Claude and is
    /// ignored; we deliberately do not invent a cleanup target from the
    /// engine environment or by scanning a shared provider home.
    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        _runtime_state: Option<&super::DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Override: Claude's trust record lives in the user-global
    /// `~/.claude.json`, not in any per-run home `provision_workspace`
    /// creates, so it needs this second pre-trust seam (see
    /// [`pre_trust_workspace`] for the full rationale).
    fn pre_trust_workspace(&self, workspace: &Path) {
        pre_trust_workspace(workspace);
    }

    /// Override: same value as the trait default, spelled out via the
    /// public [`CLAUDE_DIR_GITIGNORE`] constant so other call sites (and
    /// tests) that need this exact content have a single source of truth.
    fn config_dir_gitignore(&self) -> &'static str {
        CLAUDE_DIR_GITIGNORE
    }

    async fn write_permission_config(
        &self,
        _input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        // Claude's permission/hooks rendering still lives in
        // `boss_engine::worker_setup` (settings.json + deny rules + hooks).
        // The spawn flow continues to use that path for Claude; this method
        // returns empty artifacts so call sites that invoke it generically
        // (Codex + Claude) do not panic. Porting the full settings renderer
        // into this crate remains follow-on work.
        Ok(PermissionArtifacts::default())
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Claude's hooks deliver per-tool PreToolUse/PostToolUse events — the
        // richest tier (design §Capabilities, ProgressObservation).
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress {
        // Inline-prefix every env var the `boss-event` shim needs. `BOSS_RUN_ID`
        // is load-bearing: without it the shim can't splice `_boss_run_id` and
        // the engine drops the event, pinning the worker at `Spawning`.
        // `BOSS_WORKSPACE` tells the shim where to buffer events when the
        // engine is unreachable. Setting them here (rather than relying on env
        // inheritance from the worker pane through Claude into the hook
        // subprocess) guarantees the shim sees them regardless of how Claude
        // propagates env.
        let command = format!(
            "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} BOSS_WORKSPACE={workspace} {shim}",
            socket = shell_quote(&config.events_socket_path.display().to_string()),
            lease = shell_quote(&config.lease_id),
            run_id = shell_quote(&config.run_id),
            workspace = shell_quote(&config.workspace_path.display().to_string()),
            shim = shell_quote(&config.forwarder_binary.display().to_string()),
        );

        // Every hook event fires this same forwarder hook (matcher `*`). The
        // caller may extend the `PreToolUse` array with interception guards —
        // a separate capability — without disturbing the forwarder, which
        // stays the first entry.
        let forward_hook = serde_json::json!({
            "matcher": "*",
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                }
            ],
        });

        let mut hooks = serde_json::Map::new();
        for event in CLAUDE_HOOK_EVENTS {
            hooks.insert((*event).to_owned(), serde_json::json!([forward_hook.clone()]));
        }
        ProgressIngress::HookCallback(ProgressObservationWiring {
            hooks,
            // Claude reads hooks from the engine-rendered `--settings` file;
            // the spawn flow merges this map (and interception guards) there.
            destination: HookWiringDestination::WorkerSettingsFile,
        })
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        normalize_hook_event(raw)
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // Claude's turn boundary is its `Stop` hook, and only that: every
        // other hook fires mid-turn. `stop_hook_active` is Claude's name for
        // "a stop hook pulled the agent back into another turn", which is
        // exactly `TurnEnd::continuation`; `stop_reason` is already the
        // sequencer-refined value.
        match event {
            WorkerEvent::Stop {
                session_id,
                stop_hook_active,
                stop_reason,
            } => Some(TurnEnd {
                session_id: session_id.clone(),
                reason: *stop_reason,
                continuation: *stop_hook_active,
            }),
            _ => None,
        }
    }

    fn tool_use_interception_wiring(&self, config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        let mut hooks: Vec<serde_json::Value> = Vec::new();

        // 1. Path guard (data-dir sandbox). Canonicalises every candidate path
        //    and blocks any tool call that resolves inside the Boss data dir.
        //    Matcher `*` covers all tools. Local workers only — the script is
        //    never shipped to remote hosts.
        if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
            let guard_command = format!(
                "BOSS_DATA_DIR={dir} python3 {script}",
                dir = shell_quote(&data_dir.display().to_string()),
                script = shell_quote(&guard_script.display().to_string()),
            );
            hooks.push(serde_json::json!({
                "matcher": "*",
                "hooks": [{"type": "command", "command": guard_command}],
            }));
        }

        // 2. Boss-launch guard (always on, all workers). Blocks the worker from
        //    starting the Boss macOS app or its bundled engine binary.
        hooks.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": BOSS_LAUNCH_GUARD_COMMAND}],
        }));

        // 3. PR redirect guard (Standard workers only, local AND remote). Blocks
        //    bare VCS push and `gh pr create`; redirects to cube pr create/update.
        //    Reviewer and triage workers skip this — their deny rules already block
        //    push operations.
        if config.is_standard_worker {
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": PR_REDIRECT_GUARD_COMMAND}],
            }));
        }

        // 4. Checkleft push guard (local Standard workers only). Blocks jj/git push
        //    when the repo's checkleft reports errors. Remote workers skip it — the
        //    script is materialised locally and never shipped.
        if config.is_standard_worker
            && let Some(checkleft_script) = &config.checkleft_guard_script
        {
            let guard_command = format!(
                "python3 {script}",
                script = shell_quote(&checkleft_script.display().to_string()),
            );
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": guard_command}],
            }));
        }

        // 5. Static-analysis-only reviewer guard. Existing reviewer deny
        // rules continue to own mutation and publish fences; this adds the
        // independent no-execution restriction.
        if config.is_reviewer {
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND}],
            }));
        }

        // 6. Revision PR guard. Blocks PR creation (`gh pr create`, `cube pr
        //    create`, `cube pr ensure`) for revision workers, which must push
        //    commits to the existing parent PR, never open a new one.
        if config.is_revision {
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": REVISION_PR_GUARD_COMMAND}],
            }));
        }

        ToolUseInterceptionWiring {
            pre_tool_use_hooks: hooks,
        }
    }

    /// The Claude-specific preamble injected at the top of `CLAUDE.md`.
    /// Names "claude hooks" as the observability mechanism and is distinct
    /// from the driver-agnostic body that follows it.
    fn agent_rules_preamble(&self) -> &'static str {
        CLAUDE_AGENT_RULES_PREAMBLE
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        // Claude stamps the absolute path to the session's JSONL transcript
        // on every hook payload it emits —
        // `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Empty
        // strings are treated as missing so callers never end up trying to
        // open a path `tokio::fs::File::open` would reject anyway.
        let s = raw.get("transcript_path")?.as_str()?;
        if s.is_empty() { None } else { Some(s.to_owned()) }
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        // Claude's transcript JSONL is already in the canonical redactable
        // field shape (tool_name / tool_input / tool_response at the top level,
        // content[].type == "tool_use" blocks with name + input sub-fields).
        // No remapping is needed; return the entry as-is (moved, not cloned —
        // this runs on every polled transcript line on the hot live-status
        // path).
        raw
    }

    fn extract_error_from_transcript(&self, lines: &[serde_json::Value]) -> Option<String> {
        boss_engine_transient_error::extract_worker_error(lines)
    }

    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass {
        match boss_engine_transient_error::classify_claude_error(raw_output) {
            ErrorClass::Transient => WorkerErrorClass::Transient,
            ErrorClass::Permanent => WorkerErrorClass::Permanent,
            ErrorClass::Indeterminate => WorkerErrorClass::Indeterminate,
        }
    }

    /// Probe is typed pane input (`SendToPane`) — Claude's interactive TUI
    /// reads stdin as the next user message.
    fn probe(&self) -> ProbeDelivery {
        ProbeDelivery::PaneText
    }

    /// Interrupt is Esc into the pane (`InterruptWorkerPane`) — cancels the
    /// in-flight turn; the process survives.
    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    /// One Escape, and the cancelled turn still fires the `Stop` hook.
    ///
    /// Claude Code's TUI advertises the gesture itself — `"esc to interrupt"`
    /// is this driver's own [`Self::pane_monitor_spec`] busy marker, so the
    /// single press is the documented interactive contract, not an inference
    /// from another driver. Because the cancelled turn still reaches the
    /// ordinary `Stop` hook, the engine needs no recovery observer here
    /// ([`Self::prepare_interrupt_recovery`] stays at the trait default
    /// `None`) and the turn end arrives on the same channel a completed
    /// turn's does — [`TurnEndEvidence::TurnBoundarySignal`].
    ///
    /// A second press is deliberately *not* part of one attempt: in Claude
    /// Code a double Escape at the prompt opens the rewind/history picker,
    /// which would leave the pane in a modal state that swallows the text the
    /// engine is about to type. Retrying is therefore a whole attempt later
    /// (after the confirm window has proven the first press did not take),
    /// never two presses back to back.
    ///
    /// `confirm_window` is sized against the hook round trip rather than the
    /// model: cancelling unwinds the current tool call and fires `Stop`
    /// locally, so six seconds is generous. Two attempts, because the failure
    /// this guards against (a press swallowed by a transient modal) is not
    /// made likelier by repetition.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        Some(InterruptPlan {
            gesture: InterruptGesture {
                key: "Escape",
                presses: 1,
                press_interval: Duration::from_millis(120),
            },
            confirm_window: Duration::from_secs(6),
            max_attempts: 2,
            turn_end_evidence: TurnEndEvidence::TurnBoundarySignal,
        })
    }

    /// Stop is process-level only: `agents stop` cancels the execution and
    /// reaps the pane without typing a quit command into Claude.
    fn stop(&self) -> StopDelivery {
        StopDelivery::ProcessOnly
    }

    /// Reap is the universal SIGTERM→SIGKILL process-group ladder.
    fn reap(&self) -> ReapDelivery {
        ReapDelivery::ProcessGroup
    }

    /// Claude Code runs as a long-lived interactive TUI that reads stdin for
    /// the whole session, including while a turn is in flight: text arriving
    /// mid-turn is held in its composer and submitted as the next prompt once
    /// the turn ends. That is precisely what a human gets by typing into a
    /// worker pane while the agent is working, and it is what makes
    /// `probe --urgent` deliverable at a `PostToolUse` boundary. Unlike
    /// `codex exec`, the process never exits between turns leaving unread
    /// bytes behind for the shell to execute.
    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        MidTurnPaneInput::Buffers
    }

    fn structured_output_wiring(
        &self,
        request: &StructuredOutputRequest<'_>,
    ) -> anyhow::Result<StructuredOutputArtifacts> {
        // Env-file contract: export the designated path via the BOSS_* env
        // vars, point the engine's reader at the same path. Claude has no
        // native schema-enforcement flag (`--output-schema` is a Codex
        // mechanism), so `request.schema` is ignored — the prompt carries
        // the shape and the worker Write-tools to `result_path`. Behaviour
        // is identical to the pre-trait env-var export in the spawn flow.
        Ok(default_structured_output_wiring(request))
    }

    fn structured_output_fallback(&self, kind: StructuredOutputKind, text: &str) -> Vec<FallbackCandidate> {
        structured_output::fallback_candidates(kind, text)
    }
}

// ---------------------------------------------------------------------------
// WorkspaceProvisioning helpers (Claude-specific)
// ---------------------------------------------------------------------------
//
// These moved here from `boss_engine::worker_setup` when the driver became its
// own crate: they are all Claude-specific workspace provisioning (the `.claude`
// gitignore body and the `~/.claude.json` folder-trust pre-seed), which is
// exactly what the `WorkspaceProvisioning` capability owns. Leaving them in
// `worker_setup` would have made the engine -> driver edge circular, since
// `provision_workspace` below calls them.

/// Single-pattern gitignore body. `*` matches every entry in
/// the driver's config dir — including dotfiles and the `.gitignore`
/// itself, since gitignore globs apply to leading-dot names. Both git
/// and jj (with a git backend) honor this in-tree gitignore, so worker
/// setup files stop appearing in `jj status` / `git status`.
pub const CLAUDE_DIR_GITIGNORE: &str = "*\n";

/// Absolute path to Claude Code's user-global config file
/// (`~/.claude.json`). This is the store Claude consults for the
/// first-run folder-trust dialog (the per-project `hasTrustDialogAccepted`
/// flag); it is *separate* from the `--settings` file the engine passes.
///
/// Resolved from `$HOME` (the convention used elsewhere in the engine,
/// e.g. `boss_engine::config`). Returns `None` if `HOME` is unset, in which
/// case pre-trust is skipped and the worker falls back to today's
/// behaviour (it may block on the dialog).
///
/// Public so callers that assert on the pre-trust side effect (e.g.
/// `boss_engine::worker_setup`'s tests) resolve the config through this
/// single source of truth rather than re-deriving `$HOME/.claude.json`.
pub fn claude_global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude.json"))
}

/// Pre-accept Claude Code's first-run folder-trust dialog for
/// `workspace_path` so a headless Boss worker never blocks on it.
///
/// Boss/cube materialises the workspace itself, specifically for the
/// agent to work in, so it is trusted by construction — there is no
/// untrusted third-party content. But the folder-trust dialog ("Is this
/// a project you created or one you trust?") is a *separate* first-run
/// gate that `--permission-mode auto` and the `--settings` file do not
/// cover: it is keyed off the per-project `hasTrustDialogAccepted` flag
/// in Claude's user-global `~/.claude.json`, and is evaluated before any
/// repo- or `--settings`-supplied config. A headless worker has no human
/// to press "1", so it wedges here. We carry the trust intent through by
/// seeding that flag for the workspace path before `claude` launches.
///
/// Best-effort: failure to pre-trust is logged and swallowed (it only
/// costs the worker today's behaviour, not correctness), so it never
/// aborts worker setup.
pub fn pre_trust_workspace(workspace_path: &Path) {
    let Some(config_path) = claude_global_config_path() else {
        tracing::warn!(
            workspace = %workspace_path.display(),
            "worker setup: HOME unset, cannot pre-trust workspace in ~/.claude.json; worker may block on the folder-trust dialog",
        );
        return;
    };
    if let Err(err) = pre_trust_workspace_in(&config_path, workspace_path) {
        tracing::warn!(
            config = %config_path.display(),
            workspace = %workspace_path.display(),
            ?err,
            "worker setup: failed to pre-trust workspace in ~/.claude.json; worker may block on the folder-trust dialog",
        );
    }
}

/// Set `projects[<workspace_path>].hasTrustDialogAccepted = true` in the
/// Claude config at `config_path`, preserving every other key.
///
/// - A missing or empty config file is treated as an empty object (fresh
///   install) and created.
/// - A config that already records this workspace as trusted is a no-op:
///   we do not rewrite the file. This matters because `~/.claude.json` is
///   a *shared* file that live `claude` sessions in other workspaces
///   rewrite frequently; cube re-uses a fixed pool of workspaces, so
///   after each is trusted once the engine never touches the file again,
///   keeping the read-modify-write race window to first-spawn-per-workspace.
/// - A config that exists but does not parse as JSON is left **untouched**
///   (we return the parse error rather than clobber the user's file).
/// - The write is atomic (temp file in the same dir + rename) so a
///   concurrent reader never observes a half-written config.
fn pre_trust_workspace_in(config_path: &Path, workspace_path: &Path) -> io::Result<()> {
    let key = workspace_path.display().to_string();

    let mut root: serde_json::Value = match std::fs::read_to_string(config_path) {
        Ok(s) if s.trim().is_empty() => serde_json::json!({}),
        Ok(s) => serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "~/.claude.json is not a JSON object"))?;
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "~/.claude.json `projects` is not an object"))?;
    let entry = projects
        .entry(key)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "~/.claude.json project entry is not an object",
            )
        })?;

    // Already trusted → no-op. Don't rewrite the shared file.
    if entry.get("hasTrustDialogAccepted").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    entry.insert("hasTrustDialogAccepted".to_owned(), serde_json::Value::Bool(true));
    // Claude pairs the trust flag with this counter; seed it if absent so
    // the onboarding flow doesn't re-prompt either. Leave any existing
    // value untouched.
    entry
        .entry("projectOnboardingSeenCount")
        .or_insert_with(|| serde_json::Value::from(0));

    let serialized = serde_json::to_string_pretty(&root).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(config_path, serialized.as_bytes())
}

/// Write `contents` to `path` atomically: write a sibling temp file and
/// rename it over `path`. The rename is atomic on POSIX, so a concurrent
/// reader sees either the old or the new file, never a partial write.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Tag the temp name with the pid so concurrent engine writes (should
    // not happen — one engine — but cheap insurance) don't collide.
    let tmp = dir.join(format!(".claude.json.boss-tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;
    use crate::test_support::home_override;
    use boss_protocol::ReviewModelTier;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    #[test]
    fn claude_model_belongs_to_driver_recognises_claude_vocabulary() {
        for model in [
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "OPUS",
        ] {
            assert!(
                claude_model_belongs_to_driver(model),
                "{model:?} should be recognised as a Claude model"
            );
        }
    }

    #[test]
    fn claude_model_belongs_to_driver_rejects_other_drivers_models() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "grok-4.6", "codex-auto-review"] {
            assert!(
                !claude_model_belongs_to_driver(model),
                "{model:?} should not be recognised as a Claude model"
            );
        }
    }

    #[test]
    fn claude_review_model_tiers_use_sonnet_then_opus() {
        let menu = &ClaudeDriver.descriptor().model_menu;
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Fast), "sonnet");
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Balanced), "sonnet");
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Strong), "opus");
    }

    #[test]
    fn claude_driver_provides_all_capabilities() {
        let driver = ClaudeDriver;
        let caps = driver.capabilities();

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
            Capability::ToolProvisioning,
            Capability::PromptComposition,
            Capability::AwaitingInputSignal,
            Capability::CommandOutcomeObservation,
        ] {
            assert!(caps.provides(cap), "ClaudeDriver must provide {cap:?}",);
        }
    }

    #[test]
    fn claude_descriptor_slug_is_claude() {
        let driver = ClaudeDriver;
        assert_eq!(driver.descriptor().name, "claude");
        assert_eq!(driver.descriptor().config_dir, ".claude");
        assert_eq!(driver.descriptor().agent_rules_filename, "CLAUDE.md");
        assert_eq!(driver.descriptor().binary, "claude");
    }

    #[test]
    fn claude_pane_monitor_spec_reproduces_historical_literals() {
        let spec = ClaudeDriver
            .pane_monitor_spec()
            .expect("ClaudeDriver supplies pane-monitor markers");
        assert_eq!(spec.agent_markers, vec!["Claude Code", "auto mode on", "/effort"]);
        assert_eq!(spec.busy_markers, vec!["esc to interrupt"]);
        assert_eq!(
            spec.starting_markers,
            vec!["Accessing workspace:", "Quick safety check:"]
        );
        assert_eq!(spec.prompt_prefixes, vec!["❯"]);
        assert_eq!(spec.idle_debounce_polls, 2);
    }

    fn sample_config() -> ProgressObservationConfig {
        ProgressObservationConfig {
            events_socket_path: PathBuf::from("/Users/x/Library/Application Support/Boss/events.sock"),
            lease_id: "lease-uuid-abc".into(),
            run_id: "run-sample".into(),
            workspace_path: PathBuf::from("/ws/mono-agent-007"),
            forwarder_binary: PathBuf::from("/Users/x/Library/Application Support/Boss/bin/boss-event"),
        }
    }

    #[test]
    fn claude_progress_fidelity_is_rich() {
        assert_eq!(ClaudeDriver.progress_fidelity(), ProgressFidelity::Rich);
    }

    /// Unwrap a [`ProgressIngress`] as the [`ProgressIngress::HookCallback`]
    /// wiring it must be for `ClaudeDriver`, panicking with a clear message
    /// if the driver ever regresses to `StdoutJsonl`.
    fn expect_hook_callback(ingress: ProgressIngress) -> ProgressObservationWiring {
        match ingress {
            ProgressIngress::HookCallback(wiring) => wiring,
            ProgressIngress::StdoutJsonl | ProgressIngress::AgentJsonlFile(_) => {
                panic!("ClaudeDriver must produce HookCallback wiring")
            }
        }
    }

    #[test]
    fn observation_wiring_covers_all_seven_lifecycle_events() {
        let wiring = expect_hook_callback(ClaudeDriver.progress_observation_wiring(&sample_config()));
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "Notification",
            "SessionEnd",
        ] {
            let entries = wiring.hooks[name].as_array().unwrap();
            // Exactly the forwarder hook; interception guards are layered on
            // by the caller, not by the ProgressObservation producer.
            assert_eq!(entries.len(), 1, "{name} should wire only the forwarder");
            assert_eq!(entries[0]["matcher"], "*");
        }
    }

    #[test]
    fn observation_wiring_threads_socket_lease_run_and_workspace_into_command() {
        let wiring = expect_hook_callback(ClaudeDriver.progress_observation_wiring(&sample_config()));
        let command = wiring.hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        // Single-quote escaping must survive the space in "Application Support".
        assert!(command.contains("BOSS_EVENTS_SOCKET='/Users/x/Library/Application Support/Boss/events.sock'"));
        assert!(command.contains("BOSS_LEASE_ID='lease-uuid-abc'"));
        assert!(command.contains("BOSS_RUN_ID='run-sample'"));
        assert!(command.contains("BOSS_WORKSPACE='/ws/mono-agent-007'"));
        assert!(command.starts_with("BOSS_EVENTS_SOCKET="));
        assert!(command.trim_end().ends_with("/boss-event'"));
    }

    #[test]
    fn normalize_progress_event_decodes_a_stop_hook() {
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
        });
        let event = ClaudeDriver.normalize_progress_event(&raw).unwrap();
        assert!(matches!(event, WorkerEvent::Stop { .. }));
    }

    // ── TurnBoundary ─────────────────────────────────────────────────────────

    #[test]
    fn turn_boundary_reports_a_stop_event() {
        let end = ClaudeDriver
            .turn_boundary(&WorkerEvent::Stop {
                session_id: "sess-1".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            })
            .expect("Stop is Claude's turn boundary");
        assert_eq!(end.session_id, "sess-1");
        assert_eq!(end.reason, boss_protocol::StopReason::Completed);
        assert!(!end.continuation);
    }

    #[test]
    fn turn_boundary_carries_stop_hook_active_as_continuation() {
        // `stop_hook_active` is the only Claude-specific field in the boundary;
        // it must survive the mapping or a re-entrant stop reads as a fresh one.
        let end = ClaudeDriver
            .turn_boundary(&WorkerEvent::Stop {
                session_id: "sess-1".to_owned(),
                stop_hook_active: true,
                stop_reason: boss_protocol::StopReason::AwaitingInput,
            })
            .expect("Stop is Claude's turn boundary");
        assert!(end.continuation);
        assert_eq!(end.reason, boss_protocol::StopReason::AwaitingInput);
    }

    #[test]
    fn turn_boundary_rejects_every_non_stop_event() {
        // Mid-turn hooks must not be mistaken for a boundary — completion
        // detection and probe injection both fire off this predicate.
        let mid_turn = [
            WorkerEvent::SessionStart {
                session_id: "sess-1".to_owned(),
                source: boss_protocol::SessionStartSource::Startup,
                model: None,
            },
            WorkerEvent::UserPromptSubmit {
                session_id: "sess-1".to_owned(),
                prompt: "go".to_owned(),
            },
            WorkerEvent::PreToolUse {
                session_id: "sess-1".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
            WorkerEvent::PostToolUse {
                session_id: "sess-1".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
            WorkerEvent::Notification {
                session_id: "sess-1".to_owned(),
                message: "permission?".to_owned(),
            },
            // SessionEnd is a *process* boundary, not a turn boundary: the
            // worker is gone, so there is no turn to complete or probe into.
            WorkerEvent::SessionEnd {
                session_id: "sess-1".to_owned(),
                reason: "exit".to_owned(),
            },
        ];
        for event in mid_turn {
            assert!(
                ClaudeDriver.turn_boundary(&event).is_none(),
                "{event:?} must not report a turn boundary",
            );
        }
    }

    #[test]
    fn normalize_progress_event_surfaces_unknown_hook_error() {
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "hook_event_name": "WeirdNewHook",
        });
        assert!(ClaudeDriver.normalize_progress_event(&raw).is_err());
    }

    fn local_standard_config() -> ToolUseInterceptionConfig {
        ToolUseInterceptionConfig {
            data_dir: Some(PathBuf::from("/Library/Application Support/Boss")),
            path_guard_script: Some(PathBuf::from("/tmp/boss-settings/boss-path-guard.py")),
            checkleft_guard_script: Some(PathBuf::from("/tmp/boss-settings/boss-checkleft-push-guard.py")),
            is_revision: false,
            is_standard_worker: true,
            is_reviewer: false,
            run_id: None,
            workspace_path: None,
        }
    }

    fn remote_standard_config() -> ToolUseInterceptionConfig {
        ToolUseInterceptionConfig {
            data_dir: None,
            path_guard_script: None,
            checkleft_guard_script: None,
            is_revision: false,
            is_standard_worker: true,
            is_reviewer: false,
            run_id: None,
            workspace_path: None,
        }
    }

    #[test]
    fn local_standard_worker_gets_all_five_guards() {
        let wiring = ClaudeDriver.tool_use_interception_wiring(&local_standard_config());
        // path guard + boss-launch guard + PR redirect guard + checkleft guard = 4
        // (no revision guard since is_revision: false)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            4,
            "local standard non-revision worker must get exactly 4 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
    }

    #[test]
    fn local_revision_worker_gets_all_five_guards() {
        let mut config = local_standard_config();
        config.is_revision = true;
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        // path guard + boss-launch guard + PR redirect guard + checkleft guard + revision guard = 5
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            5,
            "local standard revision worker must get exactly 5 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            cmds.iter().any(|c| c.contains("ensure")),
            "revision guard must block cube pr ensure: {cmds:?}",
        );
    }

    #[test]
    fn remote_worker_skips_path_guard_and_checkleft() {
        let wiring = ClaudeDriver.tool_use_interception_wiring(&remote_standard_config());
        // boss-launch guard + PR redirect guard = 2 (no path guard, no checkleft)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            2,
            "remote standard worker must get exactly 2 guards (boss-launch + PR redirect): {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            !cmds.iter().any(|c| c.contains("BOSS_DATA_DIR")),
            "remote worker must not have the path guard: {cmds:?}",
        );
        assert!(
            !cmds.iter().any(|c| c.contains("checkleft")),
            "remote worker must not have the checkleft guard: {cmds:?}",
        );
    }

    #[test]
    fn path_guard_command_names_data_dir_and_script() {
        let config = local_standard_config();
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        let path_guard = wiring
            .pre_tool_use_hooks
            .iter()
            .find(|e| {
                e["hooks"][0]["command"]
                    .as_str()
                    .unwrap_or("")
                    .contains("BOSS_DATA_DIR")
            })
            .expect("path guard must be present for local workers");
        let cmd = path_guard["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("BOSS_DATA_DIR="), "must set BOSS_DATA_DIR: {cmd}");
        assert!(cmd.contains("boss-path-guard.py"), "must reference script: {cmd}");
        assert_eq!(path_guard["matcher"], "*", "path guard matcher must be '*'");
    }

    #[test]
    fn boss_launch_guard_is_always_present() {
        for config in [local_standard_config(), remote_standard_config()] {
            let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
            assert!(
                wiring.pre_tool_use_hooks.iter().any(|e| {
                    e["hooks"][0]["command"]
                        .as_str()
                        .unwrap_or("")
                        .contains("this would start Boss itself")
                }),
                "boss-launch guard must be present in every config",
            );
        }
    }

    #[test]
    fn reviewer_worker_skips_pr_redirect_and_checkleft() {
        let config = ToolUseInterceptionConfig {
            data_dir: Some(PathBuf::from("/Library/Boss")),
            path_guard_script: Some(PathBuf::from("/tmp/boss-path-guard.py")),
            checkleft_guard_script: Some(PathBuf::from("/tmp/boss-checkleft-push-guard.py")),
            is_revision: false,
            is_standard_worker: false,
            is_reviewer: true,
            run_id: None,
            workspace_path: None,
        };
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        // path guard + boss-launch guard + static-analysis guard (no PR redirect, no checkleft)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            3,
            "reviewer worker must get exactly 3 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            !cmds.iter().any(|c| c.contains("jj git push")),
            "non-standard worker must not have the PR redirect guard: {cmds:?}",
        );
        assert!(
            cmds.iter().any(|c| c.contains("static analysis only")),
            "reviewer must have the static-analysis guard: {cmds:?}",
        );
    }

    fn reviewer_static_guard_decision(command: &str) -> serde_json::Value {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start reviewer guard");
        let payload = serde_json::json!({"tool_input": {"command": command}}).to_string();
        child
            .stdin
            .as_mut()
            .expect("guard stdin")
            .write_all(payload.as_bytes())
            .expect("write hook payload");
        let output = child.wait_with_output().expect("wait for reviewer guard");
        assert!(output.status.success(), "guard process failed: {output:?}");
        serde_json::from_slice(&output.stdout).expect("guard decision JSON")
    }

    #[test]
    fn reviewer_static_analysis_guard_blocks_execution_and_allows_reads() {
        for command in [
            "bazel test //tools/boss/engine/core:engine_lib_test",
            "bazel --color=no test //tools/boss/engine/core:engine_lib_test",
            "cargo fmt",
            "python3 scripts/check.py",
            "./bazel-bin/tools/boss/engine/core/engine",
            "checkleft fix",
            // Operators glued to the preceding word must still split the
            // group, so the blocked program after `&&` is seen.
            "cd tools/boss && bazel test //x",
            "cd tools/boss&&bazel test //x",
            // `#` inside a word or after a delimiter must not truncate the
            // command: the blocked program following it must still be seen.
            "echo a#b && bazel test //x",
        ] {
            assert_eq!(
                reviewer_static_guard_decision(command)["decision"],
                "block",
                "reviewer guard must block {command}",
            );
        }
        assert_eq!(reviewer_static_guard_decision("jj diff --stat")["decision"], "approve");
    }

    // ── TranscriptAccess ─────────────────────────────────────────────────────

    #[test]
    fn transcript_path_for_session_reads_field_from_payload() {
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "hook_event_name": "Stop",
            "transcript_path": "/home/u/.claude/projects/foo/sess-1.jsonl",
        });
        assert_eq!(
            ClaudeDriver.transcript_path_for_session(&raw).as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        );
    }

    #[test]
    fn transcript_path_for_session_is_none_when_missing_or_empty() {
        let missing = serde_json::json!({"session_id": "sess-1"});
        assert_eq!(ClaudeDriver.transcript_path_for_session(&missing), None);

        let empty = serde_json::json!({"transcript_path": ""});
        assert_eq!(ClaudeDriver.transcript_path_for_session(&empty), None);
    }

    #[test]
    fn normalize_transcript_entry_is_identity_for_claude_format() {
        // Claude's transcript is already in the canonical shape; the
        // normaliser must return the value unchanged.
        let raw = serde_json::json!({
            "type": "assistant",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_response": "file.txt\n",
        });
        assert_eq!(ClaudeDriver.normalize_transcript_entry(raw.clone()), raw);
    }

    #[test]
    fn normalize_transcript_entry_passes_through_non_tool_entries() {
        let raw = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "working on it"}],
            }
        });
        assert_eq!(ClaudeDriver.normalize_transcript_entry(raw.clone()), raw);
    }

    #[test]
    fn extract_error_from_transcript_returns_trailing_api_error() {
        let lines = vec![
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "running"}]},
            }),
            serde_json::json!({
                "type": "assistant",
                "isApiErrorMessage": true,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "API Error: overloaded_error"}]},
            }),
        ];
        assert_eq!(
            ClaudeDriver.extract_error_from_transcript(&lines).as_deref(),
            Some("API Error: overloaded_error"),
        );
    }

    #[test]
    fn extract_error_from_transcript_yields_none_when_worker_recovered() {
        let lines = vec![
            serde_json::json!({
                "type": "assistant",
                "isApiErrorMessage": true,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "API Error: overloaded_error"}]},
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "retrying now"}]},
            }),
        ];
        assert_eq!(ClaudeDriver.extract_error_from_transcript(&lines), None);
    }

    #[test]
    fn extract_error_from_transcript_yields_none_for_clean_transcript() {
        let lines = vec![serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
        })];
        assert_eq!(ClaudeDriver.extract_error_from_transcript(&lines), None);
    }

    // ── ControlVerbs ─────────────────────────────────────────────────────────

    #[test]
    fn classify_error_maps_transient_errors() {
        use crate::WorkerErrorClass;
        assert_eq!(
            ClaudeDriver.classify_error("API Error: The socket connection was closed unexpectedly."),
            WorkerErrorClass::Transient,
        );
        assert_eq!(
            ClaudeDriver.classify_error("overloaded_error: Overloaded"),
            WorkerErrorClass::Transient,
        );
        assert_eq!(
            ClaudeDriver.classify_error("rate_limit_error: Too Many Requests"),
            WorkerErrorClass::Transient,
        );
        assert_eq!(
            ClaudeDriver.classify_error("Error code: 503 - service unavailable"),
            WorkerErrorClass::Transient,
        );
    }

    #[test]
    fn classify_error_maps_permanent_errors() {
        use crate::WorkerErrorClass;
        assert_eq!(
            ClaudeDriver.classify_error("authentication_error: invalid x-api-key"),
            WorkerErrorClass::Permanent,
        );
        assert_eq!(
            ClaudeDriver.classify_error("billing_error: credit balance too low"),
            WorkerErrorClass::Permanent,
        );
        assert_eq!(
            ClaudeDriver.classify_error("prompt is too long: 250000 tokens > 200000 maximum"),
            WorkerErrorClass::Permanent,
        );
    }

    #[test]
    fn classify_error_maps_unknown_errors_to_indeterminate() {
        use crate::WorkerErrorClass;
        assert_eq!(
            ClaudeDriver.classify_error("something we have never seen before"),
            WorkerErrorClass::Indeterminate,
        );
        assert_eq!(ClaudeDriver.classify_error(""), WorkerErrorClass::Indeterminate);
    }

    #[test]
    fn classify_error_permanent_wins_over_transient_on_overlap() {
        use crate::WorkerErrorClass;
        assert_eq!(
            ClaudeDriver.classify_error("authentication_error after request timed out"),
            WorkerErrorClass::Permanent,
        );
    }

    #[test]
    fn agent_rules_preamble_names_claude_hooks() {
        let preamble = ClaudeDriver.agent_rules_preamble();
        assert!(
            preamble.contains("claude hooks"),
            "preamble must name 'claude hooks': {preamble}"
        );
        assert!(
            preamble.contains("Boss-managed"),
            "preamble must describe Boss session: {preamble}"
        );
        assert!(
            preamble.contains("checkleft run") && preamble.contains("checkleft --all"),
            "preamble must direct ordinary validation to scoped checkleft: {preamble}"
        );
    }

    #[tokio::test]
    async fn provision_workspace_writes_prompt_gitignore_and_pretrust() {
        // HOME must be redirected so pre_trust_workspace doesn't write to the
        // developer's real ~/.claude.json.
        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let _home = home_override(fake_home.path());

        let driver = ClaudeDriver;
        let runtime_state = driver
            .provision_workspace(workspace.path(), "hello prompt", "run-1")
            .await
            .unwrap();
        assert!(
            runtime_state.is_none(),
            "Claude creates no out-of-workspace runtime state"
        );

        // Prompt file at the descriptor-derived path.
        let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
        assert!(
            prompt_path.exists(),
            "prompt file must exist at {}",
            prompt_path.display()
        );
        assert_eq!(std::fs::read_to_string(&prompt_path).unwrap(), "hello prompt");

        // Gitignore must exist and catch all files.
        let gitignore_path = workspace.path().join(".claude").join(".gitignore");
        assert!(gitignore_path.exists(), ".gitignore must exist");
        assert_eq!(std::fs::read_to_string(&gitignore_path).unwrap(), "*\n");

        // Pre-trust must have seeded ~/.claude.json.
        let claude_json = fake_home.path().join(".claude.json");
        assert!(claude_json.exists(), "~/.claude.json must have been written");
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        let key = workspace.path().display().to_string();
        assert_eq!(val["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[tokio::test]
    async fn teardown_workspace_is_a_no_op() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("marker.txt"), "untouched").unwrap();

        ClaudeDriver
            .teardown_workspace(Some(workspace.path()), "run-1", None)
            .await
            .unwrap();

        // Bit-identical to before: nothing in the workspace changed.
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("marker.txt")).unwrap(),
            "untouched",
        );
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            1,
            "teardown must not create or remove any files in the workspace",
        );
    }

    #[tokio::test]
    async fn teardown_workspace_succeeds_with_no_workspace_path() {
        // Callers pass `None` when the workspace path was never recorded or
        // was already cleared by a racing teardown; the no-op impl must not
        // require a path to succeed.
        ClaudeDriver.teardown_workspace(None, "run-1", None).await.unwrap();
    }

    fn spawn_request(model: &str) -> SpawnRequest<'_> {
        SpawnRequest {
            model,
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: None,
        }
    }

    #[test]
    fn spawn_invocation_uses_descriptor_paths() {
        let plan = ClaudeDriver.spawn_invocation(spawn_request("sonnet"));
        let expected_cat = format!(
            "\"$(cat {}/{})\"\n",
            CLAUDE_DESCRIPTOR.config_dir, CLAUDE_DESCRIPTOR.initial_prompt_filename,
        );
        assert!(
            plan.command.contains(&expected_cat),
            "spawn invocation must read from descriptor paths; got: {}",
            plan.command,
        );
    }

    #[test]
    fn spawn_invocation_unsets_anthropic_api_key() {
        // Claude must authenticate via OAuth credentials, not a stray
        // ANTHROPIC_API_KEY inherited from the worker pane's shell profile.
        let plan = ClaudeDriver.spawn_invocation(spawn_request("sonnet"));
        assert_eq!(
            plan.env,
            vec![EnvDirective::Unset("ANTHROPIC_API_KEY".to_owned())],
            "spawn plan must unset ANTHROPIC_API_KEY; got: {:?}",
            plan.env,
        );
    }

    #[test]
    fn permission_mode_override_forces_mode_and_suppresses_skip_permissions() {
        // A non-auto model + non_opus_auto_mode=false would normally spawn with
        // `--dangerously-skip-permissions` (which bypasses the settings
        // allow/deny rules). The override must win and suppress that entirely,
        // so the capability-restricted answer agent always runs deny-by-default.
        let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
            permission_mode_override: Some("dontAsk"),
            ..spawn_request("sonnet")
        });
        let cmd = plan.command;
        assert!(
            cmd.contains("--permission-mode dontAsk"),
            "expected forced dontAsk; got: {cmd}"
        );
        assert!(
            !cmd.contains("--dangerously-skip-permissions"),
            "override must suppress --dangerously-skip-permissions; got: {cmd}",
        );
        assert!(
            !cmd.contains("--permission-mode auto"),
            "override must suppress auto; got: {cmd}"
        );

        // Without an override, the default per-model behaviour is unchanged.
        let default_cmd = ClaudeDriver.spawn_invocation(spawn_request("sonnet")).command;
        assert!(
            default_cmd.contains("--dangerously-skip-permissions"),
            "got: {default_cmd}"
        );
    }

    // ── pre-trust (~/.claude.json folder-trust seeding) ────────────────────
    //
    // Moved here with `pre_trust_workspace_in` from `worker_setup_tests.rs`.

    #[test]
    fn pre_trust_creates_config_when_absent() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join(".claude.json");
        let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-001");

        pre_trust_workspace_in(&config, &workspace).unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        let key = workspace.display().to_string();
        assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
        // The onboarding counter is seeded so onboarding doesn't re-prompt.
        assert_eq!(value["projects"][&key]["projectOnboardingSeenCount"], 0);
    }

    #[test]
    fn pre_trust_preserves_other_projects_and_top_level_keys() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join(".claude.json");
        // A realistic config: a top-level key plus another project with
        // its own state. Pre-trust must leave both untouched.
        let existing = serde_json::json!({
            "numStartups": 42,
            "projects": {
                "/some/other/project": {
                    "hasTrustDialogAccepted": true,
                    "lastCost": 1.23,
                },
            },
        });
        std::fs::write(&config, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-002");
        pre_trust_workspace_in(&config, &workspace).unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        // Top-level key and the pre-existing project survive verbatim.
        assert_eq!(value["numStartups"], 42);
        assert_eq!(value["projects"]["/some/other/project"]["lastCost"], 1.23);
        assert_eq!(value["projects"]["/some/other/project"]["hasTrustDialogAccepted"], true);
        // The new workspace is now trusted.
        let key = workspace.display().to_string();
        assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn pre_trust_is_a_noop_when_already_trusted() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join(".claude.json");
        let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-003");
        let key = workspace.display().to_string();
        // Existing entry already trusted, with an extra field a live
        // claude session would have written.
        let existing = serde_json::json!({
            "projects": {
                &key: { "hasTrustDialogAccepted": true, "lastSessionId": "abc" },
            },
        });
        let serialized = serde_json::to_string_pretty(&existing).unwrap();
        std::fs::write(&config, &serialized).unwrap();

        pre_trust_workspace_in(&config, &workspace).unwrap();

        // The file is left byte-for-byte unchanged: no rewrite of the
        // shared config when the workspace is already trusted.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), serialized);
    }

    #[test]
    fn pre_trust_leaves_corrupt_config_untouched() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join(".claude.json");
        let garbage = "{ this is not valid json";
        std::fs::write(&config, garbage).unwrap();

        let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-004");
        let err = pre_trust_workspace_in(&config, &workspace).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // The corrupt file must NOT be clobbered — we'd rather skip
        // pre-trust than destroy the user's config.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), garbage);
    }

    #[test]
    fn pre_trust_treats_empty_config_as_fresh() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join(".claude.json");
        std::fs::write(&config, "   \n").unwrap();

        let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-005");
        pre_trust_workspace_in(&config, &workspace).unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        let key = workspace.display().to_string();
        assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    /// Claude Code is a long-lived interactive TUI that reads stdin for the
    /// whole session, so mid-turn pane input lands in its composer and is
    /// submitted as the next prompt. This is what makes `probe --urgent`
    /// deliverable at a tool boundary; a regression here silently makes every
    /// urgent probe undeliverable again.
    #[test]
    fn claude_buffers_mid_turn_pane_input() {
        assert_eq!(ClaudeDriver.mid_turn_pane_input(), MidTurnPaneInput::Buffers);
        assert!(ClaudeDriver.mid_turn_pane_input().buffers());
    }

    #[test]
    fn claude_control_verbs_match_existing_pane_behaviour() {
        assert_eq!(ClaudeDriver.probe(), ProbeDelivery::PaneText);
        assert_eq!(ClaudeDriver.interrupt(), InterruptDelivery::PaneEsc);
        assert_eq!(ClaudeDriver.stop(), StopDelivery::ProcessOnly);
        assert_eq!(ClaudeDriver.reap(), ReapDelivery::ProcessGroup);
    }

    #[test]
    fn claude_hook_wiring_destination_is_worker_settings_file() {
        let wiring = expect_hook_callback(ClaudeDriver.progress_observation_wiring(&sample_config()));
        assert_eq!(wiring.destination, HookWiringDestination::WorkerSettingsFile);
    }

    /// Claude's process outlives every turn, so its exit is *always* a death
    /// and every process-liveness reaper must keep firing on it unchanged.
    /// Pinned here rather than left implicit in the trait default: the
    /// one-turn-per-process exemption exists for `codex exec`, and Claude must
    /// never drift into it.
    #[test]
    fn claude_worker_process_is_persistent() {
        use super::super::WorkerProcessLifetime;
        assert_eq!(
            ClaudeDriver.worker_process_lifetime(),
            WorkerProcessLifetime::Persistent
        );
    }
}
