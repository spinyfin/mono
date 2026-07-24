//! The engine↔worker **structured-output contract**: a known file, per
//! payload kind, that a worker writes and the engine reads.
//!
//! Boss needs four structured results back from a worker run — the **PR URL**,
//! a reviewer's **ReviewResult**, a triage agent's **decision**, and a
//! worker's **followup** proposals. Historically the engine *scraped* each of
//! these out of the worker's transcript: a fenced-block/balanced-brace ladder
//! for the review result, an `automation: task <id>` marker line for triage, a
//! `FOLLOWUPS:` sentinel for followups. Every one of those is a bet on Claude
//! Code's transcript format and final-message conventions.
//!
//! This crate replaces that with a file contract (design:
//! `agent-driver-abstraction-*.md` §1.6, capability `StructuredOutput`):
//!
//! - The engine designates an absolute path per `(execution, kind)` and hands
//!   it to the worker in its prompt (and, for the kinds that have one, an env
//!   var).
//! - The worker writes JSON there.
//! - The engine reads and schema-validates the file at the Stop boundary.
//!
//! The file channel is **primary** and completely driver-agnostic: writing a
//! file needs no transcript-format knowledge, so any coding-agent CLI can
//! satisfy it. Transcript-sentinel parsing survives only as the *Claude
//! driver's* fallback producer (`boss_engine_driver::claude::structured_output`),
//! consumed through [`fallback::FallbackCandidate`].
//!
//! # Why a temp dir, not the workspace or the Boss data dir
//!
//! The artifact must live **outside the worker's git checkout**: the reviewer
//! is read-only and must not commit a sidecar, and followup manifests must not
//! pollute the PR. It also cannot live under the Boss support dir, because
//! workers are fenced off that directory by both the permission deny-globs and
//! the `PreToolUse` path-guard hook. So it lives in an engine-owned scratch
//! directory under the system temp dir, keyed by execution id and payload
//! kind, and is reaped after the engine reads it. The reviewer's file-write
//! deny is correspondingly scoped to its workspace so it may write this one
//! artifact while still being unable to touch the PR/repo.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod fallback;
pub mod pr_url;

/// Env var carrying the absolute path of the execution's *designated* payload
/// artifact — the one its prompt is built around (a reviewer's ReviewResult, an
/// implementer's followups, a triage agent's decision).
///
/// The operative instruction is the literal absolute path embedded in the
/// worker prompt (a model writes via the `Write` tool, not by expanding env
/// vars); this var surfaces the same value so a worker can also resolve it
/// programmatically and so the convention is self-documenting in the pane env.
pub const STRUCTURED_OUTPUT_ENV: &str = "BOSS_STRUCTURED_OUTPUT";

/// Env var carrying the absolute path of the PR-URL artifact. Separate from
/// [`STRUCTURED_OUTPUT_ENV`] because an implementer execution produces *both*
/// a PR URL and (optionally) followups, so the two cannot share one path.
pub const PR_URL_OUTPUT_ENV: &str = "BOSS_PR_URL_OUTPUT";

/// Engine-side env var overriding the base directory. Set by tests and by
/// non-default installs; unset in production (the temp-dir default applies).
const OUTPUT_DIR_ENV: &str = "BOSS_WORKER_OUTPUT_DIR";

/// Fixed subdirectory created under the resolved base.
const DIR_NAME: &str = "boss-worker-output";

/// A structured payload a worker produces for the engine.
///
/// Each kind is a distinct file, so one execution can emit several (an
/// implementer writes both a PR URL and, optionally, followups). The wire
/// schema of each is documented on its variant; the engine validates against
/// it on read, and the Claude driver's fallback producer emits the *same* wire
/// form so both channels feed one parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredOutputKind {
    /// The URL of the PR the worker opened.
    /// Wire form: `{"pr_url": "https://github.com/owner/repo/pull/123"}`.
    PrUrl,
    /// A reviewer's findings. Wire form: the `ReviewResult` object
    /// (`boss_pr_review::ReviewResult`).
    ReviewResult,
    /// A triage agent's decision. Wire form: [`TriageOutcome`] —
    /// `{"decision":"task","task_id":"T42"}` or
    /// `{"decision":"skip","reason":"…"}`.
    TriageDecision,
    /// Optional follow-on work proposals from an implementer. Wire form: a
    /// JSON array of followup entries (`proposed_name`, `proposed_description`,
    /// `proposed_effort`, `proposed_work_kind`, `rationale`).
    Followups,
    /// Required uncompleted-work findings from a `design_postmortem` review.
    /// Wire form: a JSON array of `{name, description, evidence}` objects.
    /// Distinct from [`Self::Followups`]: these become real tasks, not
    /// proposals, and an absent file is an error rather than "none found".
    PostmortemFollowups,
}

impl StructuredOutputKind {
    /// Filename infix identifying this kind's artifact, e.g. `pr-url` in
    /// `exec_abc_1.pr-url.json`. Stable — it is part of the worker-facing
    /// contract and appears in prompts.
    pub fn slug(self) -> &'static str {
        match self {
            Self::PrUrl => "pr-url",
            Self::ReviewResult => "review-result",
            Self::TriageDecision => "triage",
            Self::Followups => "followups",
            Self::PostmortemFollowups => "postmortem-followups",
        }
    }

    /// Whether this kind was, before the per-kind contract, written to the
    /// shared [`legacy_path`] artifact. Only these kinds fall back to that
    /// path on read, which keeps an execution spawned by a pre-contract engine
    /// readable across an engine upgrade.
    fn reads_legacy_path(self) -> bool {
        matches!(self, Self::ReviewResult | Self::Followups | Self::PostmortemFollowups)
    }

    /// All kinds, in a stable order. Used by [`clear_all`] to reap every
    /// artifact an execution may have produced.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::PrUrl,
            Self::ReviewResult,
            Self::TriageDecision,
            Self::Followups,
            Self::PostmortemFollowups,
        ]
        .into_iter()
    }
}

/// Wire form of [`StructuredOutputKind::PrUrl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrUrlPayload {
    pub pr_url: String,
}

/// Wire form of [`StructuredOutputKind::TriageDecision`].
///
/// `decision` is `"task"` (the agent created a task, id in `task_id`) or
/// `"skip"` (it deliberately created nothing, why in `reason`). A payload
/// whose `decision` is neither, or whose `task_id` is empty on a `task`
/// decision, is malformed and the reader treats it as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageOutcome {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TriageOutcome {
    pub const DECISION_TASK: &'static str = "task";
    pub const DECISION_SKIP: &'static str = "skip";

    /// The agent produced task `task_id`.
    pub fn task(task_id: impl Into<String>) -> Self {
        Self {
            decision: Self::DECISION_TASK.to_owned(),
            task_id: Some(task_id.into()),
            reason: None,
        }
    }

    /// The agent deliberately created nothing.
    pub fn skip(reason: impl Into<String>) -> Self {
        Self {
            decision: Self::DECISION_SKIP.to_owned(),
            task_id: None,
            reason: Some(reason.into()),
        }
    }
}

/// Resolve the engine-owned base directory for structured-output artifacts:
/// `$BOSS_WORKER_OUTPUT_DIR` when set and non-empty, else
/// `<system temp>/boss-worker-output`.
///
/// Read identically by the spawn path (which creates the dir + embeds the
/// path in the prompt) and the completion path (which reads + reaps the
/// file). Both run inside the single engine process, so the resolution is
/// consistent within a run.
pub fn default_dir() -> PathBuf {
    match std::env::var_os(OUTPUT_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(DIR_NAME),
    }
}

/// Absolute artifact path for `(execution_id, kind)` under `dir`.
pub fn path_for(dir: &Path, execution_id: &str, kind: StructuredOutputKind) -> PathBuf {
    dir.join(format!("{execution_id}.{}.json", kind.slug()))
}

/// Pre-per-kind artifact path: one file per execution, carrying whichever
/// payload that execution's prompt designated. Retained as a **read-only
/// compatibility** path so a worker spawned by an older engine — whose prompt
/// named this file — is still readable after an engine upgrade mid-run. Never
/// written or advertised any more.
pub fn legacy_path(dir: &Path, execution_id: &str) -> PathBuf {
    dir.join(format!("{execution_id}.json"))
}

/// The default-dir artifact path for `(execution_id, kind)`, as a string — the
/// form embedded in worker prompts and exported to the worker env. A thin
/// wrapper over [`default_dir`] + [`path_for`] so the spawn-side callers
/// (prompt text + env var) stay in lockstep with the completion-side reader,
/// which resolves the same [`default_dir`].
pub fn default_path_string(execution_id: &str, kind: StructuredOutputKind) -> String {
    path_for(&default_dir(), execution_id, kind).display().to_string()
}

/// Ensure `dir` exists and no stale artifact from a prior run of the same
/// execution id remains, then return the artifact path. Leftovers from an
/// earlier run of this exact execution id — at both this kind's path and the
/// legacy shared path, which [`read`] would otherwise fall back to — are
/// removed so a re-prompted worker that fails to re-write cannot have the
/// engine read a stale result. Failure to remove a stale file is logged and
/// ignored — the worker overwrites it anyway.
pub fn prepare(dir: &Path, execution_id: &str, kind: StructuredOutputKind) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = path_for(dir, execution_id, kind);
    remove_quietly(&path);
    if kind.reads_legacy_path() {
        remove_quietly(&legacy_path(dir, execution_id));
    }
    Ok(path)
}

/// Ensure `dir` exists and remove every artifact left over from a prior run of
/// this exact execution id, across all kinds. Called once at spawn: an
/// execution can produce several payloads (an implementer writes a PR URL and
/// may write followups), and a stale file of *any* kind would be read as this
/// run's result.
pub fn prepare_all(dir: &Path, execution_id: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    clear_all(dir, execution_id);
    Ok(())
}

/// Read the artifact for `(execution_id, kind)` under `dir`, returning `None`
/// when it is absent or empty. An empty/whitespace-only file is treated as
/// absent — the worker never wrote a real payload.
///
/// Falls back to the [`legacy_path`] artifact for the kinds that predate the
/// per-kind contract, so an execution spawned by an older engine still
/// resolves after an upgrade.
pub fn read(dir: &Path, execution_id: &str, kind: StructuredOutputKind) -> Option<String> {
    if let Some(content) = read_path(&path_for(dir, execution_id, kind)) {
        return Some(content);
    }
    if kind.reads_legacy_path() {
        return read_path(&legacy_path(dir, execution_id));
    }
    None
}

fn read_path(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                ?err,
                "structured-output: failed to read artifact"
            );
            None
        }
    }
}

/// Read + deserialize the artifact for `(execution_id, kind)`.
///
/// Returns `Ok(None)` when the artifact is absent (the common case), and
/// `Err(message)` when it is present but does not validate — the caller
/// decides whether that is fatal, worth a re-prompt, or a cue to try the
/// driver's fallback producer.
pub fn read_json<T: serde::de::DeserializeOwned>(
    dir: &Path,
    execution_id: &str,
    kind: StructuredOutputKind,
) -> Result<Option<T>, String> {
    let Some(raw) = read(dir, execution_id, kind) else {
        return Ok(None);
    };
    serde_json::from_str::<T>(&raw).map(Some).map_err(|err| err.to_string())
}

/// Best-effort reap of every artifact this execution may have produced. Used
/// at the finalisation boundary, where all of the execution's structured
/// output has been consumed.
pub fn clear_all(dir: &Path, execution_id: &str) {
    for kind in StructuredOutputKind::all() {
        remove_quietly(&path_for(dir, execution_id, kind));
    }
    remove_quietly(&legacy_path(dir, execution_id));
}

fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                ?err,
                "structured-output: could not remove artifact (continuing)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boss-so-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_for_uses_kind_slug() {
        assert_eq!(
            path_for(Path::new("/base"), "exec_abc_1", StructuredOutputKind::PrUrl),
            PathBuf::from("/base/exec_abc_1.pr-url.json"),
        );
        assert_eq!(
            path_for(Path::new("/base"), "exec_abc_1", StructuredOutputKind::ReviewResult),
            PathBuf::from("/base/exec_abc_1.review-result.json"),
        );
        assert_eq!(
            legacy_path(Path::new("/base"), "exec_abc_1"),
            PathBuf::from("/base/exec_abc_1.json"),
        );
    }

    #[test]
    fn every_kind_has_a_distinct_slug() {
        let slugs: Vec<_> = StructuredOutputKind::all().map(|k| k.slug()).collect();
        assert_eq!(slugs.len(), 5, "all() must cover every variant");
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), slugs.len(), "slugs must be distinct: {slugs:?}");
    }

    #[test]
    fn prepare_creates_dir_and_clears_stale_kind_and_legacy() {
        let root = scratch("prepare");
        let dir = root.join("nested");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = path_for(&dir, "exec_1", StructuredOutputKind::Followups);
        std::fs::write(&stale, "STALE").unwrap();
        let stale_legacy = legacy_path(&dir, "exec_1");
        std::fs::write(&stale_legacy, "STALE").unwrap();

        let path = prepare(&dir, "exec_1", StructuredOutputKind::Followups).unwrap();
        assert_eq!(path, stale);
        assert!(!stale.exists(), "prepare must remove the stale artifact");
        assert!(
            !stale_legacy.exists(),
            "prepare must remove the stale legacy artifact the reader falls back to",
        );
        assert!(dir.is_dir(), "prepare must ensure the dir exists");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prepare_leaves_legacy_alone_for_kinds_that_never_used_it() {
        let dir = scratch("prepare-pr-url");
        let legacy = legacy_path(&dir, "exec_1");
        std::fs::write(&legacy, "followups from this same run").unwrap();

        prepare(&dir, "exec_1", StructuredOutputKind::PrUrl).unwrap();
        assert!(
            legacy.exists(),
            "preparing the PR-URL artifact must not delete another kind's legacy payload",
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_roundtrips_written_content_and_treats_empty_as_absent() {
        let dir = scratch("read");
        let kind = StructuredOutputKind::ReviewResult;

        assert_eq!(read(&dir, "missing", kind), None, "absent file → None");

        let path = path_for(&dir, "e1", kind);
        std::fs::write(&path, "   \n  ").unwrap();
        assert_eq!(read(&dir, "e1", kind), None, "empty/whitespace file → None");

        std::fs::write(&path, r#"{"k":"v"}"#).unwrap();
        assert_eq!(read(&dir, "e1", kind).as_deref(), Some(r#"{"k":"v"}"#));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_falls_back_to_the_legacy_path_for_pre_contract_kinds() {
        let dir = scratch("legacy");
        std::fs::write(legacy_path(&dir, "e1"), "[]").unwrap();

        assert_eq!(
            read(&dir, "e1", StructuredOutputKind::Followups).as_deref(),
            Some("[]"),
            "followups must still resolve from an old engine's shared artifact",
        );
        assert_eq!(
            read(&dir, "e1", StructuredOutputKind::PrUrl),
            None,
            "a kind that never used the shared artifact must not read it",
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kind_specific_path_wins_over_legacy() {
        let dir = scratch("precedence");
        std::fs::write(legacy_path(&dir, "e1"), "LEGACY").unwrap();
        std::fs::write(path_for(&dir, "e1", StructuredOutputKind::Followups), "CURRENT").unwrap();

        assert_eq!(
            read(&dir, "e1", StructuredOutputKind::Followups).as_deref(),
            Some("CURRENT")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_json_distinguishes_absent_from_malformed() {
        let dir = scratch("read-json");
        let kind = StructuredOutputKind::PrUrl;

        assert_eq!(read_json::<PrUrlPayload>(&dir, "e1", kind), Ok(None));

        std::fs::write(path_for(&dir, "e1", kind), "{not json}").unwrap();
        assert!(
            read_json::<PrUrlPayload>(&dir, "e1", kind).is_err(),
            "a present-but-invalid artifact must be distinguishable from an absent one",
        );

        std::fs::write(path_for(&dir, "e1", kind), r#"{"pr_url":"https://x/y/pull/1"}"#).unwrap();
        assert_eq!(
            read_json::<PrUrlPayload>(&dir, "e1", kind),
            Ok(Some(PrUrlPayload {
                pr_url: "https://x/y/pull/1".to_owned()
            })),
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clear_all_removes_every_kind_and_is_idempotent() {
        let dir = scratch("clear");
        for kind in StructuredOutputKind::all() {
            std::fs::write(path_for(&dir, "e1", kind), "x").unwrap();
        }
        std::fs::write(legacy_path(&dir, "e1"), "x").unwrap();

        clear_all(&dir, "e1");
        for kind in StructuredOutputKind::all() {
            assert!(!path_for(&dir, "e1", kind).exists(), "{kind:?} artifact must be reaped");
        }
        assert!(!legacy_path(&dir, "e1").exists());
        // Idempotent: clearing already-absent artifacts must not panic.
        clear_all(&dir, "e1");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn triage_outcome_roundtrips_both_decisions() {
        let task = serde_json::to_string(&TriageOutcome::task("T42")).unwrap();
        assert_eq!(task, r#"{"decision":"task","task_id":"T42"}"#);
        assert_eq!(
            serde_json::from_str::<TriageOutcome>(&task).unwrap(),
            TriageOutcome::task("T42")
        );

        let skip = serde_json::to_string(&TriageOutcome::skip("nothing to do")).unwrap();
        assert_eq!(skip, r#"{"decision":"skip","reason":"nothing to do"}"#);
        assert_eq!(
            serde_json::from_str::<TriageOutcome>(&skip).unwrap(),
            TriageOutcome::skip("nothing to do"),
        );
    }
}
