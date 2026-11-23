//! Per-run `agy` session settings: permission posture, trust, and native-file
//! rules, rendered into one JSON file passed as `--settings <file>`.
//!
//! The posture is the one the permission study recommends
//! (`docs/investigations/agy-permission-trust-soft-deny.md`, §§1, 7, 10) and
//! the driver design adopts, identical in shape for every worker kind:
//!
//! - `agentMode: ""` — pinned explicitly. With the key absent both qualified
//!   releases start in Auto mode, which approves workspace edits and sandboxed
//!   commands on its own.
//! - `toolPermission: "request-review"` — the only level under which a hook
//!   verdict decides a call without raising an approval dialog. `strict`
//!   ignores its allowlist and raises a dialog on every call, and
//!   `always-proceed` fails open when the hook is missing. Neither, nor
//!   `--mode plan`, `--sandbox` or `--dangerously-skip-permissions`, is ever
//!   rendered for any kind.
//! - `trustedWorkspaces` — every spelling of the working directory
//!   (`trust_path_variants`: the path as given, its symlink-resolved form,
//!   and the `/tmp`↔`/private/tmp` and `/var`↔`/private/var` pairs). Matching
//!   is exact: a parent directory or a single unresolved spelling does not
//!   count, so a progression scratch under `/var/folders/…` must also list
//!   `/private/var/folders/…` (and the reverse). The control directory is
//!   not trusted.
//! - `permissions.deny` — **native file tools only** (`read_file` /
//!   `write_file`; a `write_file` rule also covers `edit_file`): the Boss data
//!   directory and the run's hook, control and rules directories (write).
//!   Reviewer fences the workspaces root to permit its external artifact;
//!   other non-writing kinds fence all native writes. Command-level denies
//!   (`bossctl`, `sudo`, `rm -rf`, publication) are *not* rendered here: a
//!   command rule is a prefix match on the command as typed, so an absolute
//!   path, a `sh -c` wrapper or a flag between two rule words gets past it, and
//!   next to a `read_file` deny it stopped working altogether (6 of 6 probes).
//!   Those denies belong to the `PreToolUse` hook adapter. [`validate_rule`]
//!   refuses a `command(...)` deny so the combination cannot be reintroduced by
//!   accident.
//! - `permissions.allow` — only for the deny-by-default kinds
//!   ([`permission_mode_for_worker_kind`] is `Some`), and **native read grants
//!   only**: `read_file` of the workspace. The hook answers
//!   `deny_unless_prior_grant` for these kinds, so this list is the whole set
//!   of native file calls that can run once the guards have passed.
//! - **No command allow rules.** A kind's command allowlist is not rendered
//!   here; it goes to the `PreToolUse` hook adapter as [`command_grants`], and
//!   the adapter answers `auto_approve` for a command it covers. The CLI's own
//!   matcher cannot carry it. Observed on 1.5.14 in progression runs (permission
//!   study, section 5, 2026-11-23 addendum): `command("$BOSS_BIN" task list)`
//!   granted the command `"$BOSS_BIN" task list` and refused `"$BOSS_BIN" task
//!   list --product boss`, as it did every `"$BOSS_BIN" task show`, `pr status`
//!   and `propose` call that carried an argument, while `command(gh pr view)`
//!   in the same list granted `gh pr view` with arguments. A rule whose program
//!   is a shell variable is matched against the exact command text, not as a
//!   prefix. The bare `boss` spelling is no alternative: the Boss launch guard
//!   in the hook chain refuses a `PATH` lookup of `boss` before the allowlist
//!   is consulted. So no Boss verb with an argument could run, and a run could
//!   not declare how it ended.
//!   [`validate_rule`] refuses a `command(...)` allow rule so that dependence
//!   on the CLI's matcher cannot be reintroduced by accident.
//! - No `permissions.ask`: an ask rule overrides a hook `auto_approve` and
//!   raises a dialog nobody is there to answer.
//!
//! A malformed rule is dropped by `agy` with only a log warning, and a dropped
//! deny rule fails open. So every rendered rule is validated here before it is
//! written, and [`verify_settings_log`] checks the CLI's own log after spawn.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::home::{
    check_private_entry, cli_settings_path_for_hooks_dir, control_dir_for_run, hooks_dir_for_run,
    session_settings_path_for_run,
};
use crate::scoped_home::trust_path_variants;
use crate::{PermissionArtifacts, PermissionInput, WorkerKind};

/// `agentMode` rendered for every kind: the documented default mode, pinned
/// because an absent key means Auto on 1.5.7 and later.
pub const AGENT_MODE: &str = "";

/// `toolPermission` rendered for every kind.
pub const TOOL_PERMISSION: &str = "request-review";

/// Pin the pane study's buffered input mode. The JSON key and `queue` value
/// were accepted by agy 1.5.10's strict `--settings` parser (invalid values
/// and the typo `queuedMessage` exit 1); "send-immediately" changes mid-turn
/// semantics. See the permission-study queued-messages artifacts.
pub const QUEUED_MESSAGES: &str = "queue";

/// The one per-run CLI flag the posture needs.
const SETTINGS_FLAG: &str = "--settings";

// ---------------------------------------------------------------------------
// Worker-kind posture
// ---------------------------------------------------------------------------

/// The permission mode the engine forces for `worker_kind`, if any.
///
/// Delegates to [`crate::capability_rules::forced_permission_mode`], the one
/// driver-side mirror of `WorkerKind::forced_permission_mode` in
/// `boss-engine-execution-contracts` (this crate cannot depend on it), shared
/// with Grok and pinned by the engine's drift test. `Some("dontAsk")` marks the
/// capability-restricted, deny-by-default kinds. `agy` has no such flag; the
/// value decides whether the kind is rendered with an allowlist and answered
/// with `deny_unless_prior_grant`, so a restricted kind cannot be silently
/// downgraded to an allow-by-default posture. The match is exhaustive: a new
/// kind must choose a posture here.
pub fn permission_mode_for_worker_kind(worker_kind: WorkerKind) -> Option<&'static str> {
    crate::capability_rules::forced_permission_mode(worker_kind)
}

/// How the hook adapter must answer a call for a kind, and so whether the
/// settings carry an allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictPosture {
    /// Allow-by-default behind guards: `auto_approve` once the guards pass.
    GuardedApproval,
    /// Deny-by-default: `deny_unless_prior_grant`, with native read grants in
    /// `permissions.allow` and [`command_grants`] decided by the adapter.
    GuardedAllowlist,
}

/// The verdict posture for `worker_kind`, derived from the forced mode so the
/// two cannot disagree.
pub fn verdict_posture_for_worker_kind(worker_kind: WorkerKind) -> VerdictPosture {
    match permission_mode_for_worker_kind(worker_kind) {
        Some(_) => VerdictPosture::GuardedAllowlist,
        None => VerdictPosture::GuardedApproval,
    }
}

/// Kinds requiring native write fences; only Reviewer may write an external artifact.
fn forbids_file_writes(worker_kind: WorkerKind) -> bool {
    match worker_kind {
        WorkerKind::Standard => false,
        WorkerKind::Reviewer
        | WorkerKind::Triage
        | WorkerKind::AnswerAgent
        | WorkerKind::ReviewGuide
        | WorkerKind::Progression => true,
    }
}

/// The Claude-grammar allowlist the kind's posture is translated from. Each
/// table is the same one Claude (and, for progression, Grok) renders, so there
/// is exactly one allowlist per posture. Empty for allow-by-default kinds.
fn claude_allowlist(worker_kind: WorkerKind) -> Vec<String> {
    match worker_kind {
        WorkerKind::AnswerAgent => crate::capability_rules::answer_agent_allow_rules(),
        WorkerKind::ReviewGuide => crate::codex::review_guide_allow_rules(),
        WorkerKind::Progression => crate::capability_rules::progression_allow_rules(),
        WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Settings document
// ---------------------------------------------------------------------------

/// The `--settings` document. `deny_unknown_fields` on both levels is the
/// same strictness the CLI applies through `--settings`, so a typo cannot
/// survive a round trip through [`parse_rendered`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSettings {
    agent_mode: String,
    tool_permission: String,
    queued_messages: String,
    trusted_workspaces: Vec<String>,
    permissions: Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Vec<String>,
}

impl SessionSettings {
    /// Render the document, refusing anything that is not exactly the posture
    /// this module renders.
    pub fn to_json(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self).context("serialising agy session settings")? + "\n")
    }

    /// Exactly the posture this module renders — nothing looser.
    fn validate(&self) -> anyhow::Result<()> {
        if self.queued_messages != QUEUED_MESSAGES {
            bail!("agy queuedMessages must be {QUEUED_MESSAGES:?}");
        }
        if self.agent_mode != AGENT_MODE {
            bail!(
                "agy agentMode must be pinned to {AGENT_MODE:?}, got {:?}",
                self.agent_mode
            );
        }
        if self.tool_permission != TOOL_PERMISSION {
            bail!(
                "agy toolPermission must be {TOOL_PERMISSION:?}, got {:?}",
                self.tool_permission
            );
        }
        if self.trusted_workspaces.is_empty() {
            bail!("agy trustedWorkspaces must not be empty");
        }
        let mut seen = BTreeSet::new();
        for workspace in &self.trusted_workspaces {
            if !Path::new(workspace).is_absolute() {
                bail!("agy trustedWorkspaces must hold absolute workspace paths, got {workspace:?}");
            }
            if !seen.insert(workspace.clone()) {
                bail!("agy trustedWorkspaces must not contain duplicate paths, got {workspace:?}");
            }
        }
        // Every spelling of the first path, and nothing else: a parent, the
        // control directory, or a missing `/var`↔`/private/var` form is how
        // the folder-trust dialog still appears.
        let required: BTreeSet<String> = trust_path_variants(Path::new(&self.trusted_workspaces[0]))
            .into_iter()
            .collect();
        if seen != required {
            bail!(
                "agy trustedWorkspaces must list every spelling of the working directory \
                 and nothing else (matching is exact, so omitting /var or /tmp vs /private/… \
                 shows the folder-trust dialog); need {required:?}, got {seen:?}"
            );
        }
        if self.permissions.deny.is_empty() {
            bail!(
                "agy permissions.deny is empty; every posture fences the run's own wiring out of the native file tools"
            );
        }
        validate_rules(RuleList::Deny, &self.permissions.deny)?;
        if let Some(allow) = &self.permissions.allow {
            validate_rules(RuleList::Allow, allow)?;
        }
        Ok(())
    }

    /// The workspaces the session trusts.
    pub fn trusted_workspaces(&self) -> &[String] {
        &self.trusted_workspaces
    }

    /// The rendered `permissions.allow` list (empty for allow-by-default kinds).
    pub fn allow_rules(&self) -> &[String] {
        self.permissions.allow.as_deref().unwrap_or_default()
    }

    /// The rendered `permissions.deny` list.
    pub fn deny_rules(&self) -> &[String] {
        &self.permissions.deny
    }
}

/// Inputs the document is derived from, already resolved. Separated from
/// [`PermissionInput`] so rendering is a pure function of paths.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct SettingsPlan<'a> {
    pub worker_kind: WorkerKind,
    /// Absolute, symlink-resolved workspace path.
    pub workspace: &'a Path,
    /// The Boss engine data directory, `None` for remote workers (their
    /// events socket is a forwarded `/tmp` socket, not a data directory).
    pub boss_data_dir: Option<&'a Path>,
    /// The run's global-root hook directory (`$HOME/.gemini/config`).
    pub hooks_dir: &'a Path,
    /// The run's Boss-owned control directory (session settings, launcher, log).
    pub control_dir: &'a Path,
    /// Boss-owned rules exposed through --add-dir.
    pub rules_root: &'a Path,
}

/// Build and validate the document for one run.
pub fn render_settings(plan: &SettingsPlan<'_>) -> anyhow::Result<SessionSettings> {
    let deny = deny_rules(plan);
    let allow = match verdict_posture_for_worker_kind(plan.worker_kind) {
        VerdictPosture::GuardedApproval => None,
        VerdictPosture::GuardedAllowlist => Some(allow_rules(plan.worker_kind, plan.workspace)?),
    };
    reject_self_fencing(plan, &deny)?;
    let settings = SessionSettings {
        agent_mode: AGENT_MODE.to_owned(),
        tool_permission: TOOL_PERMISSION.to_owned(),
        queued_messages: QUEUED_MESSAGES.to_owned(),
        trusted_workspaces: trust_path_variants(plan.workspace),
        permissions: Permissions { allow, deny },
    };
    settings.validate()?;
    Ok(settings)
}

/// Parse a settings file this module wrote and re-validate it.
pub fn parse_rendered(text: &str) -> anyhow::Result<SessionSettings> {
    let settings: SessionSettings = serde_json::from_str(text).context("parsing rendered agy session settings")?;
    settings.validate()?;
    Ok(settings)
}

// ---------------------------------------------------------------------------
// Native-file deny rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Read,
    Write,
}

impl FileAction {
    fn rule_action(self) -> &'static str {
        match self {
            FileAction::Read => "read_file",
            FileAction::Write => "write_file",
        }
    }
}

/// Directories the native file tools are fenced out of, per kind.
///
/// - The Boss data directory (state database, events socket, dispatch log) is
///   unreadable and unwritable for every kind, as the permission study renders
///   it. Absent for remote workers.
/// - The run's own hook directory and control directory are unwritable. A
///   native write there could rewrite the hook wiring that guards the run, or
///   the settings and launcher that configure it. Only writes are fenced: the
///   CLI keeps its own conversation artifacts under the same `HOME`, and
///   reading them back is ordinary. Protecting the rest of the private state
///   is the hook adapter's job, by canonical path.
/// - Kinds that must not write files get a write fence on the workspaces
///   root — the parent of the workspace, which also keeps one worker out of
///   its siblings' checkouts. It is scoped rather than blanket, as the
///   Reviewer's is on Claude, so a result file written outside the checkout
///   stays possible; Claude's blanket `Edit(**)` has no measured `agy`
///   spelling. The hook denies Reviewer and Triage native writes, except a
///   Reviewer write_file to its engine-owned ReviewResult artifact; the other
///   non-writing kinds rely on deny_unless_prior_grant and this fence.
fn file_fences(plan: &SettingsPlan<'_>) -> Vec<(FileAction, PathBuf)> {
    let mut fences = Vec::new();
    if let Some(dir) = plan.boss_data_dir {
        fences.push((FileAction::Read, dir.to_path_buf()));
        fences.push((FileAction::Write, dir.to_path_buf()));
    }
    fences.push((FileAction::Write, plan.hooks_dir.to_path_buf()));
    fences.push((FileAction::Write, plan.control_dir.to_path_buf()));
    fences.push((FileAction::Write, plan.rules_root.to_path_buf()));
    // The private CLI settings.json is where the CLI stores its own grants
    // (it saves always-allow rules there) and it is read at every session
    // start in this HOME, so a native write must not be able to plant one.
    fences.push((FileAction::Write, cli_settings_path_for_hooks_dir(plan.hooks_dir)));
    if forbids_file_writes(plan.worker_kind) {
        let root = if plan.worker_kind == WorkerKind::Reviewer {
            plan.workspace.parent().unwrap_or(plan.workspace)
        } else {
            Path::new("/")
        };
        fences.push((FileAction::Write, root.to_path_buf()));
    }
    fences
}

/// Every spelling of every fence, in a stable order. Spelling variants
/// (`/tmp` and `/private/tmp`, `/var` and `/private/var`) are all denied: a
/// deny that misses a spelling fails open.
fn deny_rules(plan: &SettingsPlan<'_>) -> Vec<String> {
    let mut rules: Vec<String> = Vec::new();
    for (action, dir) in file_fences(plan) {
        for spelling in trust_path_variants(&dir) {
            let rule = format!("{}({spelling})", action.rule_action());
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    rules
}

/// Refuse a posture whose own fences take the workspace away from the worker:
/// no kind may have its workspace read-denied, and a kind that writes
/// (Standard) may not have it write-denied. A misconfigured layout (for
/// example a homes root that contains the checkout) should fail the spawn with
/// a reason, not produce a worker that can do nothing.
fn reject_self_fencing(plan: &SettingsPlan<'_>, deny: &[String]) -> anyhow::Result<()> {
    let workspace_spellings = trust_path_variants(plan.workspace);
    for rule in deny {
        let Ok((action, target)) = split_rule(rule) else {
            continue;
        };
        let covers_workspace = workspace_spellings.iter().any(|ws| Path::new(ws).starts_with(target));
        let blocks_needed_access = match action {
            "read_file" => true,
            "write_file" => !forbids_file_writes(plan.worker_kind),
            _ => false,
        };
        if covers_workspace && blocks_needed_access {
            bail!(
                "agy posture for {:?} would deny the worker its own workspace {} ({rule}); \
                 refusing to render settings for an overlapping layout",
                plan.worker_kind,
                plan.workspace.display()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Allowlist translation
// ---------------------------------------------------------------------------

/// Where one entry of a shared Claude-grammar allowlist is enforced for `agy`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AllowEntry {
    /// `Read` / `Grep` / `Glob`: `read_file(<workspace>)` rules in
    /// `permissions.allow`, one per spelling of the workspace (`/var` and
    /// `/private/var`, `/tmp` and `/private/tmp`), mirroring the deny side:
    /// allow-path matching is not known to be spelling-insensitive, and every
    /// spelling names the same directory, so this does not widen the grant.
    /// Narrower than Claude's unscoped tool grant: nothing outside the
    /// workspace is allowed.
    NativeRead(Vec<String>),
    /// `Bash(<prefix>:*)`: the command prefix, decided by the hook adapter.
    Command(String),
}

/// Translate one shared allowlist entry.
///
/// Anything unrecognised is an error rather than a silent drop or a widening:
/// the tables are static, so a new entry shape fails the tests that translate
/// every table instead of quietly changing a worker's capabilities.
fn translate_claude_allow_rule(rule: &str, workspace: &Path) -> anyhow::Result<AllowEntry> {
    if matches!(rule, "Read" | "Grep" | "Glob") {
        return Ok(AllowEntry::NativeRead(
            trust_path_variants(workspace)
                .into_iter()
                .map(|spelling| format!("read_file({spelling})"))
                .collect(),
        ));
    }
    if let Some(prefix) = rule.strip_prefix("Bash(").and_then(|rest| rest.strip_suffix(":*)")) {
        validate_command_grant(prefix)?;
        return Ok(AllowEntry::Command(prefix.to_owned()));
    }
    bail!("no agy equivalent for allow rule {rule:?}; extend the translation deliberately")
}

/// Every entry of the kind's shared allowlist, translated in table order.
fn allow_entries(worker_kind: WorkerKind, workspace: &Path) -> anyhow::Result<Vec<AllowEntry>> {
    claude_allowlist(worker_kind)
        .iter()
        .map(|claude_rule| {
            translate_claude_allow_rule(claude_rule, workspace)
                .with_context(|| format!("translating allow rule {claude_rule:?} for {worker_kind:?}"))
        })
        .collect()
}

/// The `permissions.allow` list for a deny-by-default kind: its native read
/// grants. The kind's command grants are [`command_grants`].
fn allow_rules(worker_kind: WorkerKind, workspace: &Path) -> anyhow::Result<Vec<String>> {
    let mut rules: Vec<String> = Vec::new();
    for entry in allow_entries(worker_kind, workspace)? {
        let AllowEntry::NativeRead(translated) = entry else {
            continue;
        };
        for rule in translated {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    Ok(rules)
}

/// The command prefixes a deny-by-default kind may run, for the hook adapter
/// to decide: the `Bash(<prefix>:*)` entries of the kind's shared allowlist,
/// in table order. Empty for allow-by-default kinds.
///
/// A prefix is granted word by word, which is what the `:*` form means: a
/// command is covered when it is one simple command whose leading words are
/// the prefix. The adapter matches after resolving the engine-provided path
/// variables on both sides, so `"$BOSS_BIN" propose` names the program
/// `BOSS_BIN` holds and not a spelling of it. A grant is not a write barrier:
/// the adapter's structural checks and the guard chain decide first, and a
/// grant only ever applies to a call they have already passed.
pub fn command_grants(worker_kind: WorkerKind) -> anyhow::Result<Vec<String>> {
    let mut grants: Vec<String> = Vec::new();
    // No path enters a command grant; the workspace only shapes read grants.
    for entry in allow_entries(worker_kind, Path::new("/"))? {
        if let AllowEntry::Command(prefix) = entry
            && !grants.contains(&prefix)
        {
            grants.push(prefix);
        }
    }
    Ok(grants)
}

/// Check one command prefix before it is handed to the adapter. The adapter
/// tokenises it with the same lexer it reads commands with, so the prefix must
/// be a single line of words with nothing that could make it more than one
/// simple command.
fn validate_command_grant(prefix: &str) -> anyhow::Result<()> {
    if prefix.is_empty() || prefix.trim() != prefix {
        bail!("agy command grant {prefix:?}: empty or surrounded by whitespace");
    }
    if let Some(found) = prefix
        .chars()
        .find(|c| c.is_control() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '\\' | '\''))
    {
        bail!("agy command grant {prefix:?}: {found:?} cannot appear in a command prefix");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rule validation
// ---------------------------------------------------------------------------

/// Which `permissions` list a rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleList {
    Allow,
    Deny,
}

impl RuleList {
    fn label(self) -> &'static str {
        match self {
            RuleList::Allow => "allow",
            RuleList::Deny => "deny",
        }
    }
}

fn validate_rules(list: RuleList, rules: &[String]) -> anyhow::Result<()> {
    for (index, rule) in rules.iter().enumerate() {
        validate_rule(list, rule)?;
        if rules[..index].contains(rule) {
            bail!("duplicate agy {} rule {rule:?}", list.label());
        }
    }
    Ok(())
}

/// Split `action(target)` with no space before the parenthesis.
fn split_rule(rule: &str) -> anyhow::Result<(&str, &str)> {
    let Some((action, rest)) = rule.split_once('(') else {
        bail!("invalid grant string {rule:?}: expected action(target)");
    };
    let Some(target) = rest.strip_suffix(')') else {
        bail!("invalid grant string {rule:?}: missing closing parenthesis");
    };
    Ok((action, target))
}

/// Check one rule against the grammar `agy` accepts, mirroring the failures it
/// would otherwise only log: an unknown action, a missing parenthesis, a space
/// before it, or a bare action name. Stricter than the CLI where its behaviour
/// is unmeasured or measured not to hold: only native file actions are
/// rendered, and a path rule needs an absolute path.
///
/// Both lists are file rules only. A `command(...)` deny is evaded by
/// spelling, and a `command(...)` allow is matched as a prefix only when its
/// program is a plain word; see the module documentation. Command policy
/// belongs to the hook adapter in both directions.
pub fn validate_rule(list: RuleList, rule: &str) -> anyhow::Result<()> {
    let (action, target) = split_rule(rule)?;
    if !matches!(action, "read_file" | "write_file") {
        bail!(
            "agy {} rule {rule:?}: action {action:?} is not permitted here ({} rules are native file rules only; \
             command policy is decided by the hook adapter)",
            list.label(),
            list.label()
        );
    }
    if target.is_empty() || target.trim() != target {
        bail!(
            "agy {} rule {rule:?}: empty target or surrounding whitespace",
            list.label()
        );
    }
    if target.chars().any(|c| c.is_control() || c == '(' || c == ')') {
        bail!(
            "agy {} rule {rule:?}: target contains a control character or parenthesis",
            list.label()
        );
    }
    let path = Path::new(target);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!(
            "agy {} rule {rule:?}: path must be absolute with no `..` component",
            list.label()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Resolve the workspace path trust and allow rules are keyed on. Spellings
/// of this path (`/var`↔`/private/var`, `/tmp`↔`/private/tmp`) are added at
/// render time; matching is still exact per spelling.
fn resolved_workspace(workspace: &Path) -> anyhow::Result<PathBuf> {
    fs::canonicalize(workspace).with_context(|| {
        format!(
            "resolving agy workspace {} (trust matches each exact spelling of this path)",
            workspace.display()
        )
    })
}

/// Render the per-kind session document for `input` without writing it.
/// Startup verification compares the installed file to this result.
pub fn render_settings_for_input(input: &PermissionInput) -> anyhow::Result<SessionSettings> {
    let control_dir = control_dir_for_run(&input.run_id)
        .with_context(|| format!("resolving agy control dir for run_id {:?}", input.run_id))?;
    let workspace = resolved_workspace(&input.workspace_path)?;
    let boss_data_dir = if input.is_remote {
        None
    } else {
        input
            .events_socket_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    let hooks_dir = hooks_dir_for_run(&input.run_id)?;
    let rules_path = super::home::agent_rules_path_for_run(&input.run_id)?;
    let rules_root = rules_path.parent().context("rules root")?;
    render_settings(
        &SettingsPlan::builder()
            .worker_kind(input.worker_kind)
            .workspace(&workspace)
            .maybe_boss_data_dir(boss_data_dir.as_deref())
            .hooks_dir(&hooks_dir)
            .control_dir(&control_dir)
            .rules_root(rules_root)
            .build(),
    )
}

/// Render the run's session settings, write them into the run's control
/// directory, and return the artifacts the spawn flow applies.
///
/// `extra_args` is the only genuinely per-run flag the posture needs:
/// `--settings <file>`. The flag overrides the private `settings.json`, is
/// never saved, and exits non-zero on an unknown key, which is why posture and
/// trust travel there and not in the file the CLI itself rewrites.
pub fn write_session_settings(input: &PermissionInput) -> anyhow::Result<PermissionArtifacts> {
    let control_dir = control_dir_for_run(&input.run_id)
        .with_context(|| format!("resolving agy control dir for run_id {:?}", input.run_id))?;
    if !control_dir.is_dir() {
        bail!(
            "agy control dir {} does not exist; call provision_workspace first",
            control_dir.display()
        );
    }
    let settings_path = session_settings_path_for_run(&input.run_id)?;
    if !settings_path.is_absolute() {
        bail!("agy session settings path {} is not absolute", settings_path.display());
    }

    let settings = render_settings_for_input(input)?;
    let body = settings.to_json()?;

    // Refuse a symlink or shared hard link planted where the file goes.
    check_private_entry(&control_dir)?;
    check_private_entry(&settings_path)?;
    // The PTY relay gates the initial brief using the same expectation as the
    // host-side checker, even before the host observes the pane.
    crate::initial_prompt::write_private(
        &control_dir.join("settings-log-check.json"),
        serde_json::to_string(&serde_json::json!({
            "log_path": super::home::log_file_for_run(&input.run_id)?,
            "expected": expected_initialized_fragment(&settings),
            "require": [HOOKS_LOADED_LINE],
            "forbid": [HOOKS_UNPARSEABLE_LINE, HOOKS_NONE_LOADED_LINE],
        }))?
        .as_bytes(),
    )?;
    fs::write(&settings_path, body)
        .with_context(|| format!("writing agy session settings {}", settings_path.display()))?;

    Ok(PermissionArtifacts {
        config_files: vec![settings_path.clone()],
        extra_args: vec![SETTINGS_FLAG.to_owned(), settings_path.display().to_string()],
        env: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Post-spawn log check
// ---------------------------------------------------------------------------

pub use crate::SettingsLogCheck;

/// The CLI's startup line once it has read the run's global-root `hooks.json`:
/// one hook group (`boss`) from one file. A missing file logs `0 named hooks
/// from 0 hooks.json file(s)` and a malformed one `0 from 1`; either way the
/// session still runs, with no authorization hook and no progress ingress.
/// The launcher forbids [`HOOKS_NONE_LOADED_LINE`] so that case fails
/// immediately rather than waiting out the ready timeout. Absence of this
/// line with no `0 named hooks` line is still a wait: native `SessionStart`
/// fires with the first prompt, too late to gate the brief on.
pub const HOOKS_LOADED_LINE: &str = "loaded 1 named hooks from 1 hooks.json file(s)";

/// Logged when the CLI cannot parse `hooks.json` at all.
pub const HOOKS_UNPARSEABLE_LINE: &str = "failed to parse hooks.json";

/// Logged when the CLI started without loading any named hook group: a
/// missing file (`from 0`) or a file that parsed to nothing (`from N`).
/// Either way the session carries on with no authorization hook.
pub const HOOKS_NONE_LOADED_LINE: &str = "loaded 0 named hooks";

/// The text the CLI logs after `CLI settings initialized: ` for `settings`,
/// as Go formats the struct: space-separated slices, `Ask` empty.
fn expected_initialized_fragment(settings: &SessionSettings) -> String {
    format!(
        "permissions=&{{Allow:[{}] Deny:[{}] Ask:[]}}, toolPermission={}",
        settings.allow_rules().join(" "),
        settings.deny_rules().join(" "),
        settings.tool_permission
    )
}

/// Check the `agy` log of a spawned run against the settings rendered for it.
///
/// Fails when the log reports any `ignoring invalid` entry (a dropped deny
/// rule fails open), or when any `CLI settings initialized` line differs from
/// what was rendered — which also catches a settings source other than
/// `--settings` (or a later rewrite) changing the effective posture.
/// `rendered_settings` is the file written by [`write_session_settings`]; it is
/// re-parsed and re-validated, so a tampered file fails here too.
pub fn verify_settings_log(rendered_settings: &str, log: &str) -> anyhow::Result<SettingsLogCheck> {
    let settings = parse_rendered(rendered_settings)?;
    let invalid: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("ignoring invalid"))
        .map(str::trim)
        .collect();
    if !invalid.is_empty() {
        bail!(
            "agy dropped rendered permission rules as invalid; a dropped deny rule fails open: {}",
            invalid.join(" | ")
        );
    }

    if let Some(line) = log.lines().find(|line| line.contains(HOOKS_UNPARSEABLE_LINE)) {
        bail!(
            "agy could not read its hooks file, so no authorization hook is armed: {}",
            line.trim()
        );
    }

    if let Some(line) = log.lines().find(|line| line.contains(HOOKS_NONE_LOADED_LINE)) {
        bail!(
            "agy loaded no named hooks, so no authorization hook is armed: {}",
            line.trim()
        );
    }

    let expected = expected_initialized_fragment(&settings);
    let mut initialised = false;
    for line in log.lines().filter(|line| line.contains("CLI settings initialized")) {
        initialised = true;
        if !line.contains(&expected) {
            bail!(
                "agy initialised settings that differ from the rendered posture; \
                 expected `{expected}`, log line `{}`",
                line.trim()
            );
        }
    }
    // Settings alone are not admission: the guard chain rides on the hooks.
    let hooks_loaded = log.lines().any(|line| line.contains(HOOKS_LOADED_LINE));
    Ok(if initialised && hooks_loaded {
        SettingsLogCheck::ConsistentSoFar
    } else {
        SettingsLogCheck::NotYetInitialized
    })
}

/// [`verify_settings_log`] over the run's files. A log that does not exist yet
/// is [`SettingsLogCheck::NotYetInitialized`].
pub fn verify_settings_log_files(settings_path: &Path, log_path: &Path) -> anyhow::Result<SettingsLogCheck> {
    let rendered = fs::read_to_string(settings_path)
        .with_context(|| format!("reading rendered agy settings {}", settings_path.display()))?;
    let log = match fs::read_to_string(log_path) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(SettingsLogCheck::NotYetInitialized),
        Err(error) => return Err(error).with_context(|| format!("reading agy log {}", log_path.display())),
    };
    verify_settings_log(&rendered, &log)
}

/// [`verify_settings_log_files`] over the paths recorded in this driver's
/// runtime state. Used as [`crate::SettingsLogCheckSpec::verify`].
pub fn verify_runtime_state(state: &crate::DriverRuntimeState) -> anyhow::Result<SettingsLogCheck> {
    let runtime = super::home::AgyRuntimeState::from_driver_runtime_state(state)?;
    verify_settings_log_files(&runtime.session_settings_path, &runtime.log_file)
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "permissions_relay_tests.rs"]
mod relay_tests;
