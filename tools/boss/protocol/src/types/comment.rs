//! PR and design-doc comments: resilient anchoring, status vocabulary,
//! resolution records, threads, and the answer-agent runs that reply to them.

use super::common::{EffortLevel, default_true};
use serde::{Deserialize, Serialize};

// ===========================================================================
// Comments in the markdown viewer (design:
// tools/boss/docs/designs/comments-in-markdown-viewer.md). Phase 2 adds the
// engine-backed persistence + W3C-Web-Annotation-style resilient anchoring.
// ===========================================================================

/// W3C Web Annotation Data Model [`TextQuoteSelector`][wadm], serialised
/// inline on each comment row. The three fields are taken from the
/// rendered *plain-text projection* of the markdown (not the raw source)
/// because the user selects on rendered text.
///
/// `prefix`/`suffix` default to 64 chars each at the authoring path; they
/// disambiguate the `exact` quote when it recurs in the doc, and let the
/// fuzzy resolver re-anchor through edits that touch the surrounding text.
///
/// [wadm]: https://www.w3.org/TR/annotation-model/#text-quote-selector
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CommentAnchor {
    /// The verbatim selected text.
    pub exact: String,

    /// Up to ~64 chars of plain text immediately preceding `exact`.
    /// Empty when the selection begins at the start of the doc.
    #[serde(default)]
    pub prefix: String,

    /// Up to ~64 chars of plain text immediately following `exact`.
    /// Empty when the selection ends at the end of the doc.
    #[serde(default)]
    pub suffix: String,
}

impl CommentAnchor {
    /// The full context string the resolver matches against:
    /// `prefix + exact + suffix`.
    pub fn context(&self) -> String {
        format!("{}{}{}", self.prefix, self.exact, self.suffix)
    }
}

/// Comment status values. `active` is the authored state; `resolved` is the
/// soft-dismiss outcome (hidden from the active sidebar but kept for the
/// history surface); `orphaned` is derived — the renderer reports that an
/// anchor could no longer resolve, and the engine records the flip so the
/// sidebar can group it. `dismissed` is reserved for a future hard-dismiss.
pub const COMMENT_STATUS_ACTIVE: &str = "active";
pub const COMMENT_STATUS_RESOLVED: &str = "resolved";
pub const COMMENT_STATUS_ORPHANED: &str = "orphaned";
pub const COMMENT_STATUS_DISMISSED: &str = "dismissed";
/// Phase 2 (buckets 1&3, `comment-triggered-document-revisions.md`): a
/// `directive`/`larger_change` comment addressed by a `CommentsReviseDoc`
/// batch. `revise_task_id` is set for the duration of this status.
/// Transitions `active` → `in_revision` on the guarded batch UPDATE;
/// `in_revision` → `resolved` (task done) or `active` (task
/// abandoned/reopened) via reconciliation.
pub const COMMENT_STATUS_IN_REVISION: &str = "in_revision";
/// Bucket-2 track (P3b): a `question`-classified comment has spawned a
/// read-only answer-agent run that is still in flight. Entered from `active`
/// when the classifier resolves `intent = question`; exits to
/// [`COMMENT_STATUS_ANSWERED`] when the run posts its reply (or fails without
/// one — see `finalize_answer_agent`). Mutually exclusive with the
/// bucket-1&3 track (`active`/`in_revision`/`resolved`) at any instant.
pub const COMMENT_STATUS_ANSWERING: &str = "answering";
/// Bucket-2 track (P3b): the answer agent posted its reply (or the run ended
/// without one). Awaits an operator follow-up (phase 3c reclassifies it back
/// into `answering` for another question, or into `active` for a
/// directive/larger-change bridge into the revision path).
pub const COMMENT_STATUS_ANSWERED: &str = "answered";
/// Bucket-2 track (P3c): an operator has posted an `entry_kind =
/// 'operator_followup'` reply on an `answered` comment; the reply is being
/// (re)classified. Exits back to [`COMMENT_STATUS_ANSWERING`] if the
/// follow-up reclassifies as `question` (the answer agent runs again with
/// the accumulated thread as context), or to [`COMMENT_STATUS_ACTIVE`] if it
/// reclassifies as `directive`/`larger_change` (the bucket-1&3 bridge — the
/// comment re-enters the `[Revise]` candidate pool with the bucket-2
/// thread's context carried into the eventual directive).
pub const COMMENT_STATUS_AWAITING_FOLLOWUP: &str = "awaiting_followup";

/// How the comment's anchor last resolved against the doc's plain-text
/// projection: `exact`, `fuzzy` (drives the ⚠ sidebar glyph), or `orphan`.
pub const RESOLVED_WITH_EXACT: &str = "exact";
pub const RESOLVED_WITH_FUZZY: &str = "fuzzy";
pub const RESOLVED_WITH_ORPHAN: &str = "orphan";

/// Comment intent-classification values (`work_comments.intent`). A
/// `directive`/`larger_change` classification routes to the revision/
/// update-task path; `question` routes to the read-only answer agent. Both
/// routing paths are later phases — see
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`.
pub const INTENT_DIRECTIVE: &str = "directive";
pub const INTENT_QUESTION: &str = "question";
pub const INTENT_LARGER_CHANGE: &str = "larger_change";

pub fn default_comment_status() -> String {
    COMMENT_STATUS_ACTIVE.to_owned()
}

/// The outcome of resolving one comment's anchor against a doc's current
/// plain-text projection. `start`/`length` are character offsets (Unicode
/// scalar count) of the `exact` span within the plain text; both are `None`
/// for an orphan. `score` is the fuzzy match score (only set for `fuzzy`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommentResolution {
    /// `exact` | `fuzzy` | `orphan`.
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
}

/// `comments_create` request body. The renderer supplies `doc_version` (it
/// hashes the plain-text projection) so the engine and renderer agree on the
/// authoring input without the engine having to render markdown itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateCommentInput {
    pub artifact_id: String,
    pub anchor: CommentAnchor,
    pub artifact_kind: String,
    pub author: String,
    pub body: String,
    pub doc_version: String,
    #[serde(default)]
    #[builder(default)]
    pub plain_text_projection_version: i64,
}

/// Input for `boss task create-revision`. Creates a `kind = 'revision'`
/// task bound to an existing parent task whose PR is open and unmerged.
/// The worker's deliverable is a new commit on the *parent's* PR branch —
/// no new PR is opened. The `parent_task_id` field is required; the engine
/// enforces "kind = revision ⇒ parent_task_id IS NOT NULL" in
/// `insert_revision_in_tx` (Phase 2). `product_id` and `project_id` are
/// inherited from the parent at create time; `repo_remote_url` is likewise
/// inherited so the revision always targets the parent's repo.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateRevisionInput {
    /// The task whose PR this revision will commit to. Must refer to a task
    /// (or chain of revisions) with an open, unmerged PR. May itself be a
    /// `revision` — the gate is evaluated against the chain root's PR.
    pub parent_task_id: String,

    /// The operator's verbatim ask. Stored as the task's `description` and
    /// shown in the Review-lane rollup affordance so reviewers can see what
    /// each new commit was for.
    pub description: String,

    /// Canonical ids of work items this revision must wait on, in addition
    /// to the automatic chain-tail gate. See [`CreateChoreInput::depends_on`]
    /// — same atomic-gate semantics: each id becomes a `blocks` prerequisite
    /// edge declared in the same transaction as the row insert, so the
    /// revision is born `blocked` (and never dispatched) while any
    /// prerequisite is unsatisfied. The caller (CLI) resolves selectors
    /// (`T42`) to canonical ids before sending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub depends_on: Vec<String>,

    /// Bypass the recent-duplicate guard. See
    /// [`CreateTaskInput::force_duplicate`].
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    /// Surface that filed this revision — `"operator"` for Source A
    /// (direct boss-operator feedback); `"pr-comment:<repo>#<pr>:<cid>"`
    /// for Source B (deferred comment-triage UI). Stored in
    /// `tasks.created_via`; the `(repo, pr#, comment-id)` pointer is
    /// carried verbatim here rather than mirrored into separate columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    /// Effort estimate. Omitted → defaults to `small` (revisions are
    /// typically narrow), EXCEPT when the chain root is design-family
    /// (kind `design`/`investigation`, transitively through a revision
    /// chain), which defaults to `large` — see
    /// `default_revision_effort_level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Explicit model slug override. `None` → resolve per design §Q3
    /// precedence (same as other task kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// See [`CreateTaskInput::driver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,

    /// Short summary title for the revision card (1–10 words). When the
    /// coordinator supplies this, it is used verbatim as `tasks.name`;
    /// when absent the engine falls back to deriving a name from the first
    /// line of `description` (legacy behaviour, preserved for callers that
    /// pre-date this field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → inherits from the
    /// parent task's priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// When `false`, the revision is created in `todo` but the engine
    /// does NOT auto-dispatch a worker. The caller must explicitly start
    /// it (via `bossctl work start <id>` or a kanban drag-to-Doing).
    /// Defaults to `true` (auto-dispatch immediately), which is the
    /// existing behaviour and the right default for revision serialisation
    /// on a parent PR.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,
}

/// An engine-persisted answer-agent run row (`answer_agent_runs` table).
///
/// Tracks one ephemeral, read-only "mini-coordinator" answer-agent execution
/// against a `question`-classified doc comment (P3a of
/// `comment-triggered-document-revisions.md`). Deliberately mirrors the shape
/// of the retired `magic_wand_dispatches` row (comment-keyed, per-run row,
/// status + result) because it solves the analogous problem (track one
/// ephemeral LLM run against a comment) with a different capability profile:
/// the answer agent runs under the capability-restricted `answer_agent`
/// execution kind and produces a thread reply instead of an apply/discard
/// result.
///
/// No `tasks` row backs an answer-agent run (no kanban card); it is tracked
/// purely as an agent run here. 12 fields → builder pattern per project
/// convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct AnswerAgentRun {
    pub id: String,
    pub comment_id: String,
    pub artifact_kind: String,
    pub artifact_id: String,
    /// The `doc_version` (plain-text-projection SHA) the comment was anchored
    /// to when the agent was dispatched — snapshots the doc state the answer
    /// reasoned about.
    pub doc_version: String,
    /// Thread turn this run answers: `0` for the first answer, `1+` for
    /// re-entered follow-ups (each accumulates the prior thread as context).
    #[serde(default)]
    #[builder(default)]
    pub thread_turn: i64,

    /// `running` | `replied` | `failed`
    /// (see [`ANSWER_AGENT_RUN_STATUS_RUNNING`] et al.).
    pub status: String,

    pub created_at: String,

    /// The cube workspace lease the run holds while it checks out code to read.
    /// `None` when the run answered without leasing. Released on completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_lease_id: Option<String>,

    /// The agent's comprehensive reply (may embed proposed edits as prose, but
    /// never a patch the engine applies). `None` until the run replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_body: Option<String>,

    /// Short error classification when `status = 'failed'`. `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

// --- Comment thread entries (P2/P3: engine-authored nudge/answer/follow-up) ---

/// An engine-authored (or operator-authored) turn in a comment's thread —
/// `comment_thread_entries` table. Shared by both the bucket-1&3 nudge and
/// the bucket-2 answer/follow-up paths (comment-triggered-document-revisions.md
/// §"Reply/link mechanics"): the base comment model is single-level (P529
/// non-goal), so this table is the minimal "conversation" shape layered on
/// top of one `work_comments` row — every entry is a child of exactly one
/// comment, never a sibling top-level comment.
///
/// P3b wires only [`THREAD_ENTRY_KIND_ANSWER`] (the answer-agent's reply).
/// `nudge` (bucket 1&3, phase 2b) and `operator_followup` (phase 3c) are
/// declared here because the table is shared, but nothing yet writes them.
///
/// 8 fields → builder pattern per project convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CommentThreadEntry {
    pub id: String,
    pub comment_id: String,
    /// `nudge` | `answer` | `operator_followup` — see the `THREAD_ENTRY_KIND_*`
    /// constants.
    pub entry_kind: String,
    /// `engine` for an engine-authored entry (nudge, answer, or the
    /// no-reply-posted apology), or the operator's identity for a follow-up.
    pub author: String,
    pub body: String,
    /// Set on a `nudge` entry once a `[Revise]` batch actually claims the
    /// comment (phase 2). Always `None` for `answer` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revise_task_id: Option<String>,
    /// Set on an `answer` entry — the [`AnswerAgentRun`] that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_agent_run_id: Option<String>,
    pub created_at: String,
}

/// `comment_thread_entries.entry_kind` values.
pub const THREAD_ENTRY_KIND_NUDGE: &str = "nudge";
pub const THREAD_ENTRY_KIND_ANSWER: &str = "answer";
pub const THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP: &str = "operator_followup";

/// `comment_thread_entries.author` for engine-authored entries (nudge, answer).
pub const THREAD_ENTRY_AUTHOR_ENGINE: &str = "engine";

/// A comment paired with its resolution against the supplied plain text.
/// Returned by `comments_resolve`. The comment carries any side-effects the
/// resolve persisted (a fuzzy re-anchor, or a flip to `orphaned`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedComment {
    pub comment: WorkComment,
    pub resolution: CommentResolution,
}

/// A comment paired with its thread — the `CommentsList` read-path shape the
/// design specifies (`comment-triggered-document-revisions.md` §"UI / thread
/// behavior"): `{ ..., thread_entries: [...], answer_agent_running: bool }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommentWithThread {
    pub comment: WorkComment,
    /// This comment's [`CommentThreadEntry`] rows, oldest first.
    pub thread_entries: Vec<CommentThreadEntry>,
    /// True iff an `answer_agent_runs` row for this comment is currently
    /// `running` — lets the UI show a "thinking" indicator without a
    /// separate poll.
    pub answer_agent_running: bool,
    /// True iff this comment's *latest* `answer_agent_runs` row is
    /// `failed` — the spawn never made it to `running` (e.g. the doc
    /// owner's repo couldn't be resolved) or the run errored out. Lets the
    /// UI show a distinct failure state instead of leaving an indefinite
    /// "thinking" indicator with nothing behind it. Cleared automatically
    /// once a fresh run supersedes the failed one (a follow-up respawn, or
    /// a manual retry).
    #[serde(default)]
    pub answer_agent_failed: bool,
}

// --- Answer-agent runs (P3a: read-only mini-coordinator answer agent) ---

/// `answer_agent_runs.status` values. The run is created `running`, then flips
/// exactly once to a terminal `replied` (posted its thread reply), `failed`, or
/// [`ANSWER_AGENT_RUN_STATUS_SUPERSEDED`].
pub const ANSWER_AGENT_RUN_STATUS_RUNNING: &str = "running";
pub const ANSWER_AGENT_RUN_STATUS_REPLIED: &str = "replied";
pub const ANSWER_AGENT_RUN_STATUS_FAILED: &str = "failed";
/// Terminal: the operator reclassified the comment away from `question` while
/// this run was still in flight, so the engine stood the run down (see
/// `handle_comments_set_intent`). Deliberately distinct from `failed` — nothing
/// went wrong, the question was retracted — and load-bearing in two places:
/// the run is no longer `running`, so a late `CommentsPostAnswer` from the
/// stood-down agent is rejected instead of resurrecting `answering`; and it
/// carries no `reply_body`, so `compose_doc_comment_directive` never feeds a
/// retracted question's answer into a revision.
pub const ANSWER_AGENT_RUN_STATUS_SUPERSEDED: &str = "superseded";

/// An engine-persisted comment row (`work_comments` table). Anchored to an
/// artifact (`work_item:<id>` or `pr_doc:<repo>:<branch>:<path>`) via a
/// [`CommentAnchor`]. 17 fields → uses the builder pattern per the project's
/// `boss-protocol` convention.
///
/// `Eq` is deliberately not derived (only `PartialEq`) — `intent_confidence`
/// is an `Option<f64>`, and `f64` has no total order / `Eq` impl.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkComment {
    pub id: String,
    /// The work-item id, or the synthetic `pr_doc:<repo>:<branch>:<path>`
    /// composite key.
    pub artifact_id: String,

    /// The W3C `TextQuoteSelector` anchor. Stored as `anchor_json` in the DB.
    pub anchor: CommentAnchor,

    /// `work_item` (engine-owned description) or `pr_doc` (markdown on a
    /// PR branch).
    pub artifact_kind: String,

    /// `user:<email>` for human-authored comments.
    pub author: String,

    pub body: String,
    pub created_at: String,
    /// SHA-256 (or other opaque digest) of the plain-text projection the
    /// comment was authored against. Used only for equality; never parsed.
    pub doc_version: String,

    /// Version of the renderer's plain-text-projection algorithm the anchor
    /// was authored against. A future projection upgrade can mass re-anchor
    /// every comment whose value is stale (design § Risks mitigation).
    #[serde(default)]
    #[builder(default)]
    pub plain_text_projection_version: i64,

    /// `active` | `resolved` | `orphaned` | `dismissed` | `in_revision` |
    /// `answering` | `answered` | `awaiting_followup`.
    #[serde(default = "default_comment_status")]
    #[builder(default = default_comment_status())]
    pub status: String,

    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,

    /// `exact` | `fuzzy` | `orphan` — how the anchor last resolved. `None`
    /// until the renderer reports a resolution. Drives the ⚠ glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resolved_with: Option<String>,

    /// Who flipped status last (`user:<email>`, `engine_design_detector`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_actor: Option<String>,

    /// `directive` | `question` | `larger_change` — the async classifier's
    /// output. `NULL` while classification is in flight; this doubles as
    /// the transient `classifying` state (no separate `status` value).
    /// Comment-intent-classification design § "The classifier".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,

    /// 0.0–1.0 engine-reported confidence for `intent`. `NULL` until
    /// classified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_confidence: Option<f64>,

    /// When the classifier call completed (or a manual override was
    /// applied). `NULL` while `intent` is `NULL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_classified_at: Option<String>,

    /// `NULL` for an engine classification; `"user"` once a human manually
    /// reclassifies via the (later-phase) `CommentsSetIntent` RPC —
    /// preserved permanently as an audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_overridden_by: Option<String>,

    /// Soft FK → `tasks.id`: the revision or chore that this comment's
    /// `[Revise]` batch was dispatched to. `NULL` unless `status =
    /// 'in_revision'`, with one exception: a `resolved` comment keeps it
    /// as provenance of which batch addressed it (reconciliation on merge
    /// leaves it in place). A `reopened` comment (its batch was abandoned)
    /// clears it back to `NULL`. Design:
    /// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
    /// §"Association model" / §"Reconciliation".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revise_task_id: Option<String>,

    /// When the async classifier gave up after exhausting its retries.
    /// `NULL` while classification is in flight/pending, and cleared back to
    /// `NULL` the moment a classification (or manual override) succeeds.
    /// Lets the UI distinguish "still classifying" from "classification
    /// failed" instead of showing an indefinite spinner for both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_classification_failed_at: Option<String>,

    /// The last classifier error, set alongside
    /// `intent_classification_failed_at`. `NULL` whenever that is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_classification_error: Option<String>,
}
