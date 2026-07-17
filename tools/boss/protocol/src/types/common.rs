//! Shared defaults, helpers, and small vocabulary types used across the
//! other `types` submodules: status/effort/actor enums, `created_via`
//! constants, and the serde default helpers.

use serde::{Deserialize, Serialize};

/// Which naming strategy to use for worker branches pushed to this
/// product's repo. The execution-id suffix is always appended for
/// uniqueness; only the leading prefix component varies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BranchNaming {
    /// Engine default: `boss/exec_<id>`. Clearly identifies Boss-authored
    /// branches; unwanted in repos with strict per-developer prefix rules.
    #[default]
    BossExecPrefix,
    /// Replace the leading prefix with a short opaque hash so the branch
    /// name gives no hint of its Boss origin while remaining unique by
    /// construction.
    OpaqueHash,
    /// Use `<prefix>exec_<id>` instead of `boss/exec_<id>`. Satisfies
    /// orgs that enforce per-developer branch prefixes (e.g. `bduff/`).
    CustomPrefix { prefix: String },
}

/// Allowed values for `tasks.effort_level`. Per design §"Naming" /
/// §Q1: `trivial | small | medium | large | max`. Stored as TEXT
/// in SQLite (no `CHECK` constraint), validated in code by
/// [`EffortLevel::from_str`].
///
/// `max` is the human-only escape hatch: the coordinator's
/// heuristic never emits it; humans set it via `--effort max` when
/// they want Claude's maximum reasoning depth regardless of what
/// the scope markers suggest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Trivial,
    Small,
    Medium,
    Large,
    Max,
}

impl EffortLevel {
    pub const ALL: &'static [EffortLevel] = &[
        EffortLevel::Trivial,
        EffortLevel::Small,
        EffortLevel::Medium,
        EffortLevel::Large,
        EffortLevel::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Trivial => "trivial",
            EffortLevel::Small => "small",
            EffortLevel::Medium => "medium",
            EffortLevel::Large => "large",
            EffortLevel::Max => "max",
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EffortLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trivial" => Ok(EffortLevel::Trivial),
            "small" => Ok(EffortLevel::Small),
            "medium" => Ok(EffortLevel::Medium),
            "large" => Ok(EffortLevel::Large),
            "max" => Ok(EffortLevel::Max),
            other => Err(format!(
                "unknown effort level `{other}`; expected one of: trivial, small, medium, large, max"
            )),
        }
    }
}

/// Display-safe GitHub OAuth auth state pushed from the engine to the UI.
/// The token itself is never included — only fields safe to render.
///
/// Matches the state machine in the OAuth device-flow design (§3):
/// `Disconnected → RequestingCode → PendingUserAuth → Authorized/Expired/Denied/Error`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GitHubAuthStateDto {
    /// No stored token; no flow in progress.
    Disconnected,
    /// Device code is being requested from GitHub's `/login/device/code`.
    RequestingCode,
    /// Device code obtained. The user must type `user_code` at
    /// `verification_uri` (or `verification_uri_complete` if present) to
    /// authorize. The engine is polling.
    PendingUserAuth {
        user_code: String,
        verification_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_uri_complete: Option<String>,
        /// Unix epoch seconds when the device code expires.
        expires_at: i64,
        interval_seconds: u32,
    },
    /// Token obtained, validated, and stored. `granted_scopes` is what
    /// GitHub actually granted (may differ from what was requested).
    Authorized {
        login: String,
        granted_scopes: Vec<String>,
        org_state: OrgAuthState,
    },
    /// The device code expired before the user completed authorization.
    /// The user must restart the flow.
    Expired,
    /// The user denied the authorization request in the browser.
    Denied,
    /// A non-recoverable error occurred during the flow.
    Error { message: String },
}

/// Sub-state of `GitHubAuthStateDto::Authorized` that reflects whether the
/// stored token can actually reach private org resources. A valid user token
/// may still be blocked by org approval or SAML SSO requirements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrgAuthState {
    /// Token can read the org's private resources. Sync should work.
    Ok,
    /// The OAuth App has not yet been approved by an org owner. Sync
    /// against private org resources will fail. `request_url` is the
    /// org-owner approval / request page.
    NeedsOrgApproval { request_url: String },
    /// The token requires SAML SSO authorization for the org. `sso_url`
    /// is the SSO authorization URL from GitHub's `X-GitHub-SSO` header.
    NeedsSso { sso_url: String },
    /// Org auth state could not be determined (probe failed for an
    /// unexpected reason). Sync may or may not work.
    Unknown,
}

// ---------------------------------------------------------------------------
// Editorial controls (editorial-controls-for-agent-authored-prs-and-github-comments.md)
// ---------------------------------------------------------------------------

/// What the editorial hook does when a redaction pattern matches: rewrite
/// the matched substring in place, or block the `gh` invocation entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedactionKind {
    /// Replace the match with `RedactionRule::replacement`.
    #[default]
    Rewrite,
    /// Reject the `gh` invocation outright with an actionable message.
    Block,
}

/// One user-configured redaction rule applied to `gh pr|issue` bodies.
/// `pattern` is a regex (Rust `regex` crate syntax); `replacement` is
/// substituted for every match when `kind = Rewrite` and ignored when
/// `kind = Block`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionRule {
    #[serde(default)]
    pub kind: RedactionKind,

    pub pattern: String,
    pub replacement: String,
}

/// Role/origin of a rendered transcript segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentRole {
    User,
    Assistant,
    Thinking,
    Tool,
    System,
}

pub(crate) fn default_true() -> bool {
    true
}

pub fn default_priority() -> String {
    "medium".to_owned()
}

pub fn default_human_actor() -> String {
    "human".to_owned()
}

/// A status change made by a human operator through the CLI or macOS app,
/// or by any peer whose process ancestry doesn't match the Boss-session pid.
pub const LAST_STATUS_ACTOR_HUMAN: &str = "human";
/// A status change whose caller's process ancestry traces back to the
/// registered Boss-coordinator session (the libghostty pane where the
/// Boss Claude Code instance runs as coordinator).
pub const LAST_STATUS_ACTOR_BOSS: &str = "boss";
/// A status change made directly by the engine (auto-block, dep-unblock,
/// merge poller, CI watch, etc.) — never comes from a peer RPC call.
pub const LAST_STATUS_ACTOR_ENGINE: &str = "engine";
/// A status change made by Boothby, the autonomous groundskeeper, during
/// a maintenance pass (closing a stale task, archiving an empty project,
/// unwedging stuck work). Like `'human'` and `'boss'` — and unlike
/// `'engine'` — a Boothby status change is a *deliberate* decision about
/// one row, not a cascade the engine may silently reverse. See
/// [`StatusActor::is_engine_cascade`] for the consumer-facing rule.
///
/// Boothby never arrives over the peer-RPC path (`resolve_status_actor`
/// only ever resolves `'boss'` or `'human'`); it is engine-internal and
/// passes this literal to `update_work_item_as_actor` directly.
pub const LAST_STATUS_ACTOR_BOOTHBY: &str = "boothby";

/// The `last_status_actor` vocabulary as a closed set.
///
/// The column is TEXT and every write path stamps one of the
/// `LAST_STATUS_ACTOR_*` literals, so this enum is a *view* over that
/// vocabulary rather than the storage type. Consumers that have to make
/// a real decision about who touched a row should [`FromStr`]-parse the
/// stored value and `match` exhaustively, so that adding a fifth actor
/// is a compile error at each such site instead of silently falling into
/// whichever branch happened to be the default.
///
/// There is deliberately no `Unknown` variant: a value outside this set
/// means a write site invented an actor, which is a bug to fix there and
/// not a case every consumer should be forced to branch on. `from_str`
/// returns `Err` for those, and callers decide — today the only decision
/// site treats an unparseable actor exactly as the pre-enum `== "engine"`
/// string compare did (not engine-owned, so hands off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusActor {
    Human,
    Boss,
    Engine,
    Boothby,
}

impl StatusActor {
    pub const ALL: &'static [StatusActor] = &[
        StatusActor::Human,
        StatusActor::Boss,
        StatusActor::Engine,
        StatusActor::Boothby,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StatusActor::Human => LAST_STATUS_ACTOR_HUMAN,
            StatusActor::Boss => LAST_STATUS_ACTOR_BOSS,
            StatusActor::Engine => LAST_STATUS_ACTOR_ENGINE,
            StatusActor::Boothby => LAST_STATUS_ACTOR_BOOTHBY,
        }
    }

    /// True only for `'engine'`: the row's current status was written by
    /// one of the engine's own cascades (dependency auto-block, merge
    /// poller, CI watch), which therefore also owns reversing it.
    ///
    /// This is the single rule behind every actor check in the engine and
    /// the macOS app, stated once so the next actor added has to answer
    /// the question explicitly rather than inherit an answer.
    ///
    /// `Boothby` sits on the `false` side with `Human` and `Boss`. Boothby
    /// is autonomous, but its writes are per-row judgements ("this task is
    /// stale, close it"), not cascade bookkeeping — so the dep-unblock
    /// sweep must leave a Boothby-touched row alone exactly as it leaves a
    /// human's alone. Note this branch is unreachable for the auto-block
    /// path regardless: `write_engine_status` hardcodes `'engine'`, so a
    /// cascade-owned block can never carry any other actor.
    pub fn is_engine_cascade(self) -> bool {
        match self {
            StatusActor::Engine => true,
            StatusActor::Human | StatusActor::Boss | StatusActor::Boothby => false,
        }
    }
}

impl std::fmt::Display for StatusActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for StatusActor {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            LAST_STATUS_ACTOR_HUMAN => Ok(StatusActor::Human),
            LAST_STATUS_ACTOR_BOSS => Ok(StatusActor::Boss),
            LAST_STATUS_ACTOR_ENGINE => Ok(StatusActor::Engine),
            LAST_STATUS_ACTOR_BOOTHBY => Ok(StatusActor::Boothby),
            other => Err(format!(
                "unknown status actor `{other}`; expected one of: human, boss, engine, boothby"
            )),
        }
    }
}

/// Canonical "I don't know where this came from" stamp. Applied by
/// the migration to existing rows and by the engine's last-resort
/// fallback when a caller omits the field. Fresh writes from any
/// documented surface (`cli`, `bossctl`, `mac_app`, `engine_auto`)
/// must carry their own value.
pub fn default_unknown_created_via() -> String {
    CREATED_VIA_UNKNOWN.to_owned()
}

pub const CREATED_VIA_CLI: &str = "cli";
pub const CREATED_VIA_BOSSCTL: &str = "bossctl";
pub const CREATED_VIA_MAC_APP: &str = "mac_app";
pub const CREATED_VIA_ENGINE_AUTO: &str = "engine_auto";
pub const CREATED_VIA_EXTERNAL_TRACKER_SYNC: &str = "external_tracker_sync";
pub const CREATED_VIA_UNKNOWN: &str = "unknown";
/// Prefix for engine-triggered revisions spawned by the merge-conflict
/// watcher: `merge-conflict:<conflict_resolutions.id>`. The attempt id is
/// the back-pointer; `(repo, pr#)` is recoverable from the chain root.
/// Design: `tools/boss/docs/designs/unify-pr-remediation-on-revisions.md` Q2.
pub const CREATED_VIA_MERGE_CONFLICT_PREFIX: &str = "merge-conflict:";
/// Prefix for engine-triggered revisions spawned by the CI-failure watcher:
/// `ci-fix:<ci_remediations.id>`. Mirrors `CREATED_VIA_MERGE_CONFLICT_PREFIX`.
pub const CREATED_VIA_CI_FIX_PREFIX: &str = "ci-fix:";
/// Prefix for engine-triggered revisions spawned by the automated PR reviewer
/// (P992): `pr_review:<pr_review_execution_id>`.
pub const CREATED_VIA_PR_REVIEW_PREFIX: &str = "pr_review:";
/// Engine-triggered work spawned by actioning an attention group
/// (`ActionAttentionGroup`): the revision / design task produced from a
/// question group, or the batch of tasks/chores produced from a followup
/// group. Design: `tools/boss/docs/designs/attentions.md`.
pub const CREATED_VIA_ATTENTION: &str = "attention";
/// Prefix for the revision/chore spawned by `CommentsReviseDoc`:
/// `doc-comment:<artifact_kind>:<artifact_id>`. Design:
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// §"Association model".
pub const CREATED_VIA_DOC_COMMENT_PREFIX: &str = "doc-comment:";
/// Prefix for work Boothby files during a maintenance pass:
/// `boothby:<boothby_passes.id>`. The pass id is the back-pointer — every
/// row Boothby touched in that pass is recoverable from `boothby_actions`,
/// which carries the pre/post images needed to undo it.
pub const CREATED_VIA_BOOTHBY_PREFIX: &str = "boothby:";

/// Documented `created_via` values. The engine canonicalises caller-
/// supplied strings against this set; values outside it are stored
/// as-is but logged so we can spot undocumented sources sneaking in.
pub const KNOWN_CREATED_VIA: &[&str] = &[
    CREATED_VIA_CLI,
    CREATED_VIA_BOSSCTL,
    CREATED_VIA_MAC_APP,
    CREATED_VIA_ENGINE_AUTO,
    CREATED_VIA_EXTERNAL_TRACKER_SYNC,
    CREATED_VIA_ATTENTION,
    CREATED_VIA_UNKNOWN,
];

/// `true` when `value` is one of the documented `created_via` strings
/// or matches a documented prefix pattern (`merge-conflict:*`,
/// `ci-fix:*`, `pr-comment:*`, `boothby:*`). Engine writes for unknown
/// values still go through, but a warning is logged at the insert site.
pub fn is_known_created_via(value: &str) -> bool {
    KNOWN_CREATED_VIA.contains(&value)
        || value.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
        || value.starts_with(CREATED_VIA_CI_FIX_PREFIX)
        || value.starts_with(CREATED_VIA_PR_REVIEW_PREFIX)
        || value.starts_with(CREATED_VIA_DOC_COMMENT_PREFIX)
        || value.starts_with(CREATED_VIA_BOOTHBY_PREFIX)
        || value.starts_with("pr-comment:")
}

/// Operator-facing short-id string (`"T2344"`) for a task/chore short id —
/// never the canonical `task_*` id, which is an internal implementation
/// detail. Returns `None` when the row lacks a short id so each caller can
/// apply its own fallback (a generic label, the canonical id, an empty
/// string, …); use [`Task::short_label`] for the "a task" fallback.
pub fn short_id_label(short_id: Option<i64>) -> Option<String> {
    short_id.map(|n| format!("T{n}"))
}

pub(crate) fn is_false(b: &bool) -> bool {
    !b
}

/// How strictly the product's `.github/PULL_REQUEST_TEMPLATE.md`
/// conformance is enforced on PR bodies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplatePolicy {
    /// No template enforcement — worker writes whatever it likes (today's
    /// default behaviour).
    #[default]
    Off,
    /// Inject the template as guidance in the worker prompt, but do not
    /// block a non-conforming PR body.
    Advise,
    /// Block `gh pr create` / `gh pr edit` calls whose body does not
    /// contain the mandatory template sections.
    Enforce,
}

/// Whether the worker should append an AI co-author trailer to commit
/// messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrailerPolicy {
    /// Engine default: the worker follows whatever its CLAUDE.md says
    /// (today that means appending `Co-Authored-By: Claude …`).
    #[default]
    Default,
    /// Strip any AI co-author trailer before the worker calls `git
    /// commit`. The worker is also instructed not to add it.
    NoAiTrailer,
}

/// One rendered transcript segment, as returned by `executions.transcript`.
///
/// Structured for lazy per-segment rendering in the UI: each segment maps to
/// one JSONL event (user turn, assistant turn, tool call, tool result, …) and
/// carries its own markdown body so the renderer builds ASTs one at a time.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TranscriptSegment {
    #[builder(default)]
    pub collapsible: bool,

    #[builder(default)]
    pub default_collapsed: bool,

    /// Short human-readable label (e.g. `"User"`, `"⚙ Bash"`, `"↳ result"`).
    pub label: String,

    /// Rendered markdown body for this segment.
    pub markdown: String,

    pub role: SegmentRole,
    pub seq: u64,
    pub model: Option<String>,
    pub timestamp: Option<String>,
    pub truncated: Option<TruncationInfo>,
}

/// Set when a tool result was truncated by the renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationInfo {
    pub shown_bytes: usize,
    pub total_bytes: usize,
}
