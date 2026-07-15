//! Wire types for Boothby, Boss's autonomous groundskeeper.
//!
//! Boothby is a periodically-woken, coordinator-privileged maintenance
//! agent. Each wake-up is a [`BoothbyPass`]; every mutation it makes during
//! that pass is journalled as a [`BoothbyAction`] carrying pre/post images
//! of the columns it touched, which is what the UI's undo affordance is
//! built on. Observations it mines from logs and transcripts land as
//! [`BoothbyFinding`] rows, deduped by fingerprint. [`BoothbyCursor`]
//! records how far a mining source has been read so the next pass resumes
//! rather than re-scanning.
//!
//! Shapes follow `tools/boss/docs/designs/boothby.md` §"Audit & undo data
//! model" column-for-column.
//!
//! These live in their own module rather than `types.rs` because that file
//! is already ~5.6k lines and carries a standing `file/size` waiver with a
//! "split below 3000" TODO; `host_registry_wire` / `metrics_wire` /
//! `planner` set the precedent for per-feature wire modules.
//!
//! Field ordering follows the `types.rs` convention: identity first, then
//! required fields alphabetically, then `Option` fields alphabetically.

use serde::{Deserialize, Serialize};

/// One Boothby wake-up. Id prefix `bp`.
///
/// At most one pass is open (`finished_at IS NULL`) at a time — a
/// partial-unique index enforces it. That invariant is load-bearing: the
/// mutation layer resolves "the pass this action belongs to" by looking up
/// the open pass in-transaction, rather than threading a pass id through
/// every mutation signature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BoothbyPass {
    pub id: String,

    /// Rows journalled to `boothby_actions` during this pass. Denormalised
    /// so the pane can render a pass list without a per-row COUNT.
    #[builder(default = 0)]
    pub actions_count: i64,

    /// Rows written to `boothby_findings` during this pass.
    #[builder(default = 0)]
    pub findings_count: i64,

    /// Intended actions turned into proposals rather than executed —
    /// either because the pass ran in `propose` mode or because a
    /// blast-radius cap was hit.
    #[builder(default = 0)]
    pub proposals_count: i64,

    pub started_at: String,

    /// [`BOOTHBY_TRIGGER_SCHEDULE`] | [`BOOTHBY_TRIGGER_MANUAL`] |
    /// `event:<name>` (see [`BOOTHBY_TRIGGER_EVENT_PREFIX`]).
    pub trigger: String,

    /// `None` while the pass is in flight; set together with `outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    /// [`BOOTHBY_OUTCOME_COMPLETED`] | `..._NOTHING_TO_DO` | `..._TIMED_OUT`
    /// | `..._FAILED` | `..._CAPPED`. `None` exactly while in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,

    /// Claude session id for the pass, for transcript correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Boothby's own account of the pass, written by the `pass-summary`
    /// verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// One journalled mutation made by Boothby. Id prefix `ba`.
///
/// `pre_image` / `post_image` are JSON objects holding **only the columns
/// the mutation actually touched** (`{"status": "todo"}` → `{"status":
/// "archived"}`), the shape borrowed from `project_property_audit`.
/// Restricting them to touched columns keeps undo honest: replaying
/// `pre_image` reverts exactly what Boothby changed and cannot clobber a
/// column some other writer has moved since. `post_image` doubles as the
/// undo conflict check — undo compares the row's current state against it
/// and refuses rather than silently overwriting a later change.
///
/// Rows are append-only. An undo does not delete the row; it moves
/// `undo_state` to `undone`, so the pane can show that a decision was made
/// *and* reversed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BoothbyAction {
    pub id: String,

    /// The owning [`BoothbyPass`]. Every action belongs to a pass.
    pub pass_id: String,

    pub created_at: String,

    /// Agent-supplied one-liner explaining the call. Required: an
    /// unexplained autonomous mutation is what the journal exists to
    /// prevent.
    pub rationale: String,

    /// [`BOOTHBY_REVERSIBILITY_REVERSIBLE`] | `..._SEMI` |
    /// `..._IRREVERSIBLE`, from the executor's verb catalogue.
    pub reversibility: String,

    /// Ordinal within the pass; `(pass_id, seq)` is the read order.
    pub seq: i64,

    pub target_id: String,

    /// [`BOOTHBY_TARGET_TASK`] and friends.
    pub target_kind: String,

    /// [`BOOTHBY_UNDO_STATE_NONE`] | `..._UNDOABLE` | `..._UNDONE` |
    /// `..._EXPIRED` | `..._CONFLICTED`.
    #[builder(default = BOOTHBY_UNDO_STATE_NONE.to_string())]
    pub undo_state: String,

    /// Catalogue slug, e.g. `close_stale_task`.
    pub verb: String,

    /// JSON: the verb's inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,

    /// JSON object of touched columns *after* the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_image: Option<String>,

    /// JSON object of touched columns *before* the write — the undo
    /// payload. `None` for irreversible actions, which journal `params`
    /// and evidence instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_image: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,

    /// Always `"human"` — undo is human-only, so Boothby cannot launder
    /// its own mistakes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_by: Option<String>,
}

/// Something Boothby noticed while mining logs and transcripts.
/// Id prefix `bf`.
///
/// `fingerprint` is the dedup key: a finding recurring across passes bumps
/// `occurrences` and `last_seen` on the existing row instead of inserting a
/// duplicate. It is also the unit of human veto — suppressing a fingerprint
/// is what stops Boothby re-doing something a human undid.
///
/// Findings deliberately carry no `pass_id`: they outlive the pass that
/// first saw them (they are the dedup memory), and the design retains them
/// indefinitely while passes and actions are pruned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BoothbyFinding {
    pub id: String,

    /// Stable content-derived dedup key.
    pub fingerprint: String,

    pub first_seen: String,

    /// [`BOOTHBY_FINDING_ERROR`] | `..._ANOMALY` | `..._PERF` |
    /// `..._FRICTION` | `..._TAXONOMY`.
    pub kind: String,

    pub last_seen: String,

    /// Times seen across all passes. Starts at 1.
    #[builder(default = 1)]
    pub occurrences: i64,

    /// [`BOOTHBY_FINDING_STATUS_OPEN`] | `..._FILED` | `..._RESOLVED` |
    /// `..._SUPPRESSED`.
    #[builder(default = BOOTHBY_FINDING_STATUS_OPEN.to_string())]
    pub status: String,

    /// JSON refs: log span / transcript span / row ids.
    pub subject: String,

    /// [`BOOTHBY_FILED_KIND_CHORE`] | [`BOOTHBY_FILED_KIND_GITHUB_ISSUE`];
    /// `None` until filed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filed_kind: Option<String>,

    /// Task id or issue URL, per `filed_kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filed_ref: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_reason: Option<String>,
}

/// How far Boothby has read a given mining source, so the next pass resumes
/// instead of re-scanning from the beginning.
///
/// Keyed by `source` (there is exactly one cursor per source), so this has
/// no surrogate id and — at three fields — no builder: the repo's
/// `rust/giant-structs` check requires one only past five fields, the point
/// at which additive changes start churning every construction site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoothbyCursor {
    /// [`BOOTHBY_CURSOR_ENGINE_TRACE`], [`BOOTHBY_CURSOR_DISPATCH_EVENTS`],
    /// or `transcript:<session>`. Primary key.
    pub source: String,

    /// Opaque resume token, interpreted by that source's reader: a
    /// segment/offset or a timestamp high-water mark, as JSON.
    pub position: String,

    pub updated_at: String,
}

// --- boothby_passes.trigger ------------------------------------------

/// The periodic wake-up.
pub const BOOTHBY_TRIGGER_SCHEDULE: &str = "schedule";
/// An operator asked for a pass now.
pub const BOOTHBY_TRIGGER_MANUAL: &str = "manual";
/// Prefix for an engine-event-driven wake-up: `event:<name>`. The name is
/// open-ended, which is why the column carries no `CHECK`.
pub const BOOTHBY_TRIGGER_EVENT_PREFIX: &str = "event:";

// --- boothby_passes.outcome ------------------------------------------

pub const BOOTHBY_OUTCOME_COMPLETED: &str = "completed";
/// The pre-spawn short-circuit found no work, so no session was spawned.
pub const BOOTHBY_OUTCOME_NOTHING_TO_DO: &str = "nothing_to_do";
pub const BOOTHBY_OUTCOME_TIMED_OUT: &str = "timed_out";
pub const BOOTHBY_OUTCOME_FAILED: &str = "failed";
/// A blast-radius cap ended the pass; the remainder became proposals.
pub const BOOTHBY_OUTCOME_CAPPED: &str = "capped";

// --- boothby_actions.target_kind -------------------------------------

pub const BOOTHBY_TARGET_TASK: &str = "task";
pub const BOOTHBY_TARGET_PROJECT: &str = "project";
pub const BOOTHBY_TARGET_ATTENTION: &str = "attention";
pub const BOOTHBY_TARGET_ATTENTION_ITEM: &str = "attention_item";
pub const BOOTHBY_TARGET_EXECUTION: &str = "execution";
pub const BOOTHBY_TARGET_LEASE: &str = "lease";
pub const BOOTHBY_TARGET_WORKSPACE: &str = "workspace";
pub const BOOTHBY_TARGET_FILE: &str = "file";
pub const BOOTHBY_TARGET_ISSUE: &str = "issue";

// --- boothby_actions.reversibility -----------------------------------

/// Fully restorable from `pre_image`.
pub const BOOTHBY_REVERSIBILITY_REVERSIBLE: &str = "reversible";
/// Restorable in the taxonomy, but with a side effect that is not (e.g. a
/// posted comment).
pub const BOOTHBY_REVERSIBILITY_SEMI: &str = "semi";
/// No `pre_image`; journals `params` and evidence instead.
pub const BOOTHBY_REVERSIBILITY_IRREVERSIBLE: &str = "irreversible";

// --- boothby_actions.undo_state --------------------------------------

/// Not undoable and not undone — the resting state for I-class actions.
pub const BOOTHBY_UNDO_STATE_NONE: &str = "none";
pub const BOOTHBY_UNDO_STATE_UNDOABLE: &str = "undoable";
pub const BOOTHBY_UNDO_STATE_UNDONE: &str = "undone";
/// The target row is gone, or retention lapsed.
pub const BOOTHBY_UNDO_STATE_EXPIRED: &str = "expired";
/// The row moved since the action: current state no longer matches
/// `post_image`, so undo refuses and the UI shows both images.
pub const BOOTHBY_UNDO_STATE_CONFLICTED: &str = "conflicted";

/// `undone_by` is always this — undo is human-only.
pub const BOOTHBY_UNDONE_BY_HUMAN: &str = "human";

// --- boothby_findings.kind -------------------------------------------

pub const BOOTHBY_FINDING_ERROR: &str = "error";
pub const BOOTHBY_FINDING_ANOMALY: &str = "anomaly";
pub const BOOTHBY_FINDING_PERF: &str = "perf";
pub const BOOTHBY_FINDING_FRICTION: &str = "friction";
pub const BOOTHBY_FINDING_TAXONOMY: &str = "taxonomy";

// --- boothby_findings.status -----------------------------------------

pub const BOOTHBY_FINDING_STATUS_OPEN: &str = "open";
/// Turned into a chore / issue; `filed_kind` + `filed_ref` are set.
pub const BOOTHBY_FINDING_STATUS_FILED: &str = "filed";
pub const BOOTHBY_FINDING_STATUS_RESOLVED: &str = "resolved";
/// Vetoed by a human; Boothby must not re-act on this fingerprint.
pub const BOOTHBY_FINDING_STATUS_SUPPRESSED: &str = "suppressed";

// --- boothby_findings.filed_kind -------------------------------------

pub const BOOTHBY_FILED_KIND_CHORE: &str = "chore";
pub const BOOTHBY_FILED_KIND_GITHUB_ISSUE: &str = "github_issue";

// --- boothby_cursors.source ------------------------------------------

pub const BOOTHBY_CURSOR_ENGINE_TRACE: &str = "engine-trace";
pub const BOOTHBY_CURSOR_ENGINE_AUDIT: &str = "engine-audit";
pub const BOOTHBY_CURSOR_DISPATCH_EVENTS: &str = "dispatch-events";
/// Prefix for a per-session transcript cursor: `transcript:<session>`.
pub const BOOTHBY_CURSOR_TRANSCRIPT_PREFIX: &str = "transcript:";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_builder_defaults_to_an_in_flight_pass_with_zero_counts() {
        let pass = BoothbyPass::builder()
            .id("bp_1")
            .started_at("1700000000")
            .trigger(BOOTHBY_TRIGGER_SCHEDULE)
            .build();

        // In flight: no outcome and no end time, together.
        assert_eq!(pass.outcome, None);
        assert_eq!(pass.finished_at, None);
        assert_eq!(pass.actions_count, 0);
        assert_eq!(pass.proposals_count, 0);
        assert_eq!(pass.findings_count, 0);
    }

    #[test]
    fn action_builder_defaults_undo_state_to_none() {
        let action = BoothbyAction::builder()
            .id("ba_1")
            .pass_id("bp_1")
            .seq(1)
            .created_at("1700000000")
            .verb("close_stale_task")
            .target_kind(BOOTHBY_TARGET_TASK)
            .target_id("task_1")
            .rationale("no activity in 90 days and no PR")
            .reversibility(BOOTHBY_REVERSIBILITY_REVERSIBLE)
            .pre_image(r#"{"status":"todo"}"#)
            .post_image(r#"{"status":"archived"}"#)
            .build();

        assert_eq!(action.undo_state, BOOTHBY_UNDO_STATE_NONE);
        assert_eq!(action.undone_at, None);
        assert_eq!(action.undone_by, None);
    }

    #[test]
    fn finding_builder_defaults_to_one_open_occurrence() {
        let finding = BoothbyFinding::builder()
            .id("bf_1")
            .fingerprint("fp")
            .kind(BOOTHBY_FINDING_ERROR)
            .subject(r#"{"log":"engine-trace","span":[1,2]}"#)
            .first_seen("1700000000")
            .last_seen("1700000000")
            .build();

        assert_eq!(finding.occurrences, 1);
        assert_eq!(finding.status, BOOTHBY_FINDING_STATUS_OPEN);
        assert_eq!(finding.filed_kind, None);
        assert_eq!(finding.filed_ref, None);
    }

    /// `maybe_*` is the bon escape hatch for a dynamic `Option` under
    /// `on(String, into)` — pinned here because passing `Some(..)` to the
    /// plain setter is a compile error the CLAUDE.md convention calls out.
    #[test]
    fn action_builder_accepts_a_dynamic_optional_pre_image() {
        let irreversible: Option<String> = None;
        let action = BoothbyAction::builder()
            .id("ba_1")
            .pass_id("bp_1")
            .seq(1)
            .created_at("1700000000")
            .verb("force_release_lease")
            .target_kind(BOOTHBY_TARGET_LEASE)
            .target_id("lease_1")
            .rationale("holder pid is dead")
            .reversibility(BOOTHBY_REVERSIBILITY_IRREVERSIBLE)
            .maybe_pre_image(irreversible)
            .build();

        assert_eq!(action.pre_image, None);
    }
}
