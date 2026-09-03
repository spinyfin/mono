//! Coordinator session handoff.
//!
//! When the coordinator session restarts — Claude Code update, app
//! restart, crash, or the supervisor's restart ceiling — the incoming
//! session starts with none of what the operator told the outgoing one.
//! This module is the engine half of the fix:
//!
//! * **Storage.** The outgoing session writes a short handoff through
//!   `boss handoff write` (`FrontendRequest::SetCoordinatorHandoff`).
//!   The engine keeps it as one JSON value in the `metadata` table of its
//!   own database — coordinator-private state, never in a repo — stamped
//!   with the write time and the spawn token of the coordinator session
//!   that was live when it was written. The engine never synthesizes a
//!   handoff from logs or transcripts; the coordinator writes it.
//!
//! * **Delivery.** Every *fresh* coordinator session (`start_new` in
//!   [`crate::coordinator_tmux`]) is launched with a session-start brief
//!   as its initial prompt — the same mechanism worker sessions use for
//!   their initial prompt — so the incoming session consumes the handoff
//!   on its very first turn without anyone typing anything. An adopted
//!   session (engine restart, coordinator survived) keeps its own context
//!   and gets no brief; that path is the existing prompt-change nudge.
//!
//! * **Three distinct states, none silent.** [`HandoffState`] separates
//!   "a handoff is present", "no session ever wrote one", and "one is
//!   stored but cannot be read". The brief spells out which applies, and
//!   for a present handoff, whether the session that just ended wrote it
//!   or whether it is a leftover from an earlier session (an abrupt kill
//!   before the outgoing session refreshed it). A missing or stale
//!   handoff is reported as such, never passed off as "nothing to hand
//!   off".
//!
//! Abrupt termination is handled on the writer side by policy, not by a
//! shutdown hook: the coordinator prompt (`bossSystemPrompt` in
//! `BossPaneModel.swift`) tells the session to refresh the handoff at
//! natural boundaries — whenever the operator states a fact that changed
//! the world, a decision, or a prohibition — so a handoff always exists
//! as of the last such boundary, and the brief's writer/age stamps make
//! any gap visible. See `tools/boss/docs/coordinator-session-handoff.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use boss_engine_utils::iso8601::{format_elapsed_ago, format_epoch_iso8601};
use boss_protocol::{CoordinatorHandoffView, CoordinatorRecreateReason};
use serde::{Deserialize, Serialize};

use crate::work::{CoordinatorTmuxRecord, WorkDb};

/// Metadata key holding the JSON-encoded [`CoordinatorHandoff`].
pub(crate) const HANDOFF_METADATA_KEY: &str = "coordinator.handoff";

/// Upper bound on a handoff body. A handoff is a brief — operator-stated
/// facts, decisions, open threads, prohibitions — not a transcript
/// replay; the cap is what keeps it that way when a session is tempted to
/// dump everything it knows.
pub(crate) const MAX_HANDOFF_BYTES: usize = 16 * 1024;

/// Filename of the session-start brief, written under the coordinator
/// session directory's `.claude/` (next to the app-managed
/// `settings.local.json`) and handed to `claude` as its initial prompt.
pub(crate) const START_BRIEF_FILENAME: &str = "handoff-brief.txt";

/// The stored handoff record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoordinatorHandoff {
    pub(crate) body: String,
    /// Unix epoch seconds.
    pub(crate) written_at: i64,
    /// Spawn token of the coordinator record that was live at write time.
    /// Empty when no coordinator record existed (a handoff written from a
    /// plain terminal before any coordinator ever ran).
    pub(crate) writer_spawn_token: String,
}

/// What the engine found when it looked for a handoff. `Missing` and
/// `Unreadable` are deliberately separate variants: "no session ever wrote
/// one" and "one was written but the engine cannot read it back" call for
/// different operator responses, and collapsing them would hide the
/// second behind the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandoffState {
    Present(CoordinatorHandoff),
    Missing,
    Unreadable(String),
}

impl HandoffState {
    /// Short label for the engine-audit log.
    pub(crate) fn audit_outcome(&self) -> &'static str {
        match self {
            Self::Present(_) => "present",
            Self::Missing => "missing",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

/// Why a fresh coordinator session is being created. Rendered into the
/// session-start brief so the incoming session can tell the operator what
/// ended the previous one — a crash and an operator reset warrant
/// different first replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoordinatorStartReason {
    /// No coordinator record existed: the first session on this engine.
    FirstCreation,
    /// The recorded tmux session no longer exists (tmux server restart,
    /// session killed, or a crash window between intent and creation).
    SessionMissing,
    /// The tmux session survived but its `claude` process exited — a
    /// crash, `/exit`, or a Claude Code update that ended the process.
    PaneDead,
    /// An explicit UI-confirmed recreate (model mismatch or operator reset,
    /// the latter also being how a `claude` update is picked up).
    Recreate(CoordinatorRecreateReason),
}

impl CoordinatorStartReason {
    fn describe(self) -> &'static str {
        match self {
            Self::FirstCreation => "this is the first coordinator session on this engine",
            Self::SessionMissing => {
                "the previous coordinator's tmux session no longer existed (tmux server restart, session \
                 killed, or an engine crash mid-create)"
            }
            Self::PaneDead => {
                "the previous coordinator's claude process exited (a crash, `/exit`, or a Claude Code update \
                 ending the process) while its tmux session survived"
            }
            Self::Recreate(CoordinatorRecreateReason::ModelMismatch) => {
                "the operator confirmed replacing the previous coordinator to change its model"
            }
            Self::Recreate(CoordinatorRecreateReason::OperatorReset) => {
                "the operator explicitly reset the coordinator (typically to pick up a Claude Code update or \
                 clear context)"
            }
        }
    }

    pub(crate) fn audit_label(self) -> &'static str {
        match self {
            Self::FirstCreation => "first_creation",
            Self::SessionMissing => "session_missing",
            Self::PaneDead => "pane_dead",
            Self::Recreate(CoordinatorRecreateReason::ModelMismatch) => "recreate_model_mismatch",
            Self::Recreate(CoordinatorRecreateReason::OperatorReset) => "recreate_operator_reset",
        }
    }
}

/// Identity of the coordinator session a fresh one replaces, captured
/// from the metadata record *before* the new spawn intent overwrites it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviousSession {
    pub(crate) spawn_token: String,
    pub(crate) spawned_at: Option<i64>,
}

impl From<&CoordinatorTmuxRecord> for PreviousSession {
    fn from(record: &CoordinatorTmuxRecord) -> Self {
        Self {
            spawn_token: record.spawn_token.clone(),
            spawned_at: record.spawned_at,
        }
    }
}

impl WorkDb {
    /// Read the stored handoff. Never errors: every failure mode is a
    /// [`HandoffState`] variant so callers cannot accidentally treat a
    /// read failure as "nothing stored".
    pub(crate) fn coordinator_handoff_state(&self) -> HandoffState {
        match self.get_metadata(HANDOFF_METADATA_KEY) {
            Ok(None) => HandoffState::Missing,
            Ok(Some(raw)) => match serde_json::from_str::<CoordinatorHandoff>(&raw) {
                Ok(handoff) => HandoffState::Present(handoff),
                Err(error) => {
                    HandoffState::Unreadable(format!("stored coordinator handoff is not valid JSON: {error}"))
                }
            },
            Err(error) => HandoffState::Unreadable(format!("could not read stored coordinator handoff: {error:#}")),
        }
    }

    /// Replace the stored handoff. `body` must already have passed
    /// [`validate_handoff_body`].
    pub(crate) fn set_coordinator_handoff(
        &self,
        body: &str,
        writer_spawn_token: &str,
        now_epoch_secs: i64,
    ) -> Result<CoordinatorHandoff> {
        let handoff = CoordinatorHandoff {
            body: body.to_owned(),
            written_at: now_epoch_secs,
            writer_spawn_token: writer_spawn_token.to_owned(),
        };
        let raw = serde_json::to_string(&handoff).context("encoding coordinator handoff")?;
        self.set_metadata(HANDOFF_METADATA_KEY, &raw)
            .context("persisting coordinator handoff")?;
        Ok(handoff)
    }
}

/// Normalize and bound a handoff body. Returns the trimmed body, or the
/// operator-facing reason it was refused.
pub(crate) fn validate_handoff_body(body: &str) -> std::result::Result<String, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(
            "coordinator handoff body is empty; write the operator-stated facts, decisions, open threads, \
                    and prohibitions the next session must know (see \"Session handoff\" in the coordinator prompt)"
                .to_owned(),
        );
    }
    if trimmed.len() > MAX_HANDOFF_BYTES {
        return Err(format!(
            "coordinator handoff body is {} bytes; the cap is {MAX_HANDOFF_BYTES}. A handoff is a brief, not a \
             transcript: keep operator-stated facts, decisions, open threads, and prohibitions; drop narrative",
            trimmed.len()
        ));
    }
    Ok(trimmed.to_owned())
}

/// Wire view of a stored handoff, judged against the coordinator record
/// that is live *now*.
pub(crate) fn handoff_view(
    handoff: &CoordinatorHandoff,
    current_spawn_token: Option<&str>,
    now_epoch_secs: i64,
) -> CoordinatorHandoffView {
    CoordinatorHandoffView::builder()
        .age_secs(now_epoch_secs.saturating_sub(handoff.written_at))
        .body(handoff.body.clone())
        .writer_spawn_token(handoff.writer_spawn_token.clone())
        .written_at(handoff.written_at)
        .written_at_iso8601(format_epoch_iso8601(handoff.written_at))
        .written_by_current_session(
            !handoff.writer_spawn_token.is_empty() && current_spawn_token == Some(handoff.writer_spawn_token.as_str()),
        )
        .build()
}

/// Everything [`compose_start_brief`] needs; kept as a struct so the
/// composer stays pure and directly testable.
pub(crate) struct StartBriefInputs<'a> {
    pub(crate) state: &'a HandoffState,
    pub(crate) previous: Option<&'a PreviousSession>,
    pub(crate) reason: CoordinatorStartReason,
    pub(crate) now_epoch_secs: i64,
    /// Directory holding prior sessions' Claude Code transcripts, when the
    /// caller verified it exists. Mentioned as a slow last resort only.
    pub(crate) transcript_dir: Option<&'a Path>,
}

fn when(epoch: i64, now: i64) -> String {
    match format_elapsed_ago(epoch, now) {
        Some(ago) => format!("{ago} ({})", format_epoch_iso8601(epoch)),
        None => format_epoch_iso8601(epoch),
    }
}

/// Render the session-start brief handed to a fresh coordinator session
/// as its initial prompt. Pure over its inputs.
pub(crate) fn compose_start_brief(inputs: StartBriefInputs<'_>) -> String {
    let StartBriefInputs {
        state,
        previous,
        reason,
        now_epoch_secs: now,
        transcript_dir,
    } = inputs;
    let mut out = String::new();
    out.push_str("[Boss coordinator session start: automatic handoff brief from the engine]\n\n");
    out.push_str(&format!(
        "You are a fresh coordinator session; {}. Everything the operator told the previous session is gone from \
         your context. What follows is the engine's report on the session handoff.\n\n",
        reason.describe()
    ));

    let previous_started = previous.and_then(|p| p.spawned_at).map(|at| when(at, now));
    let previous_phrase = match (&previous_started, previous) {
        (Some(started), _) => format!("the session that just ended (started {started})"),
        (None, Some(_)) => "the session that just ended".to_owned(),
        (None, None) => "the previous session".to_owned(),
    };

    match state {
        HandoffState::Present(handoff) => {
            let written = when(handoff.written_at, now);
            let by_previous =
                previous.is_some_and(|p| !p.spawn_token.is_empty() && p.spawn_token == handoff.writer_spawn_token);
            if by_previous {
                out.push_str(&format!(
                    "HANDOFF PRESENT: written by {previous_phrase}, {written}. Facts in it are current as of that \
                     time, not now.\n"
                ));
            } else if previous.is_some() {
                out.push_str(&format!(
                    "HANDOFF STALE: {previous_phrase} never wrote a handoff. The newest handoff, below, was written \
                     by an EARLIER session, {written}. Everything the operator told the session that just ended is \
                     NOT in it. Say so in your first reply and ask the operator what changed since then before \
                     relying on any of it.\n"
                ));
            } else {
                out.push_str(&format!(
                    "HANDOFF PRESENT (writer unknown): a handoff was written {written}, but the engine has no record \
                     of the session that wrote it. Treat it as possibly stale and confirm its facts with the \
                     operator.\n"
                ));
            }
            out.push_str("--- handoff begins ---\n");
            out.push_str(handoff.body.trim_end());
            out.push_str("\n--- handoff ends ---\n");
        }
        HandoffState::Missing => {
            if reason == CoordinatorStartReason::FirstCreation && previous.is_none() {
                out.push_str(
                    "NO HANDOFF: none is expected. This is the first coordinator session on this engine, so there \
                     is no previous session to hand anything off.\n",
                );
            } else {
                out.push_str(&format!(
                    "NO HANDOFF AVAILABLE: no coordinator session has ever written one, so {previous_phrase} left \
                     nothing behind. This is NOT the same as \"nothing to hand off\": anything the operator said \
                     in that session (infrastructure taken down or brought back, flags flipped, decisions, things \
                     not to do) is lost unless they repeat it. Say so in your first reply and ask the operator \
                     what you should know before you act on anything.\n"
                ));
            }
        }
        HandoffState::Unreadable(error) => {
            out.push_str(&format!(
                "HANDOFF UNREADABLE: a handoff is stored but the engine could not read it: {error}. Treat this \
                 exactly like a missing handoff ({previous_phrase} may have written facts you cannot see), tell \
                 the operator the stored handoff is unreadable so it can be investigated, and ask them what you \
                 should know before you act on anything.\n"
            ));
        }
    }

    if let Some(dir) = transcript_dir {
        out.push_str(&format!(
            "\nLast resort only: prior sessions' Claude Code transcripts are under {} (newest *.jsonl by mtime). \
             Grepping them is slow and unreliable; use it only when the handoff is missing or stale and the \
             operator is unavailable, never as a substitute for asking.\n",
            dir.display()
        ));
    }

    out.push_str(
        "\nDo this now, before anything else:\n\
         1. Your first reply states which handoff state applies (present / stale / missing / unreadable) in one \
         line, then summarizes the handoff in a few lines if there is one.\n\
         2. Treat every infrastructure, host, flag, tmux, or pause fact from the handoff as \"as of the time it was \
         written\". Before filing work, briefing an agent, or raising an alarm that depends on such a fact, \
         confirm it is still true (a fresh read, or ask the operator).\n\
         3. Once absorbed, write a fresh handoff via `boss handoff write` carrying forward what is still true, so \
         the next restart is handed THIS session's knowledge. Then keep it current: refresh it whenever the \
         operator states a fact that changed the world, makes a decision, or says not to do something. The \
         \"Session handoff\" section of your instructions has the format.\n",
    );
    out
}

/// Claude Code stores a project's transcripts under
/// `~/.claude/projects/<encoded cwd>/`, where the encoding replaces every
/// character outside `[A-Za-z0-9]` with `-`. Pure; the caller decides
/// whether the directory actually exists.
pub(crate) fn transcript_projects_dir(home: &Path, working_directory: &Path) -> PathBuf {
    let encoded: String = working_directory
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude").join("projects").join(encoded)
}

/// [`transcript_projects_dir`] for the real `$HOME`, only when it exists.
pub(crate) fn existing_transcript_dir(working_directory: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = transcript_projects_dir(Path::new(&home), working_directory);
    dir.is_dir().then_some(dir)
}

/// Write the brief where the coordinator's launch command reads it from.
/// Returns the absolute path written.
pub(crate) fn write_start_brief(working_directory: &Path, brief: &str) -> Result<PathBuf> {
    let dir = working_directory.join(".claude");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(START_BRIEF_FILENAME);
    std::fs::write(&path, brief).with_context(|| format!("writing session-start brief to {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_756_800_000;

    fn present(written_at: i64, writer: &str) -> HandoffState {
        HandoffState::Present(CoordinatorHandoff {
            body: "- greyarea is shut down (operator, 19:22 PDT)\n- tmux hosting re-enabled\n".to_owned(),
            written_at,
            writer_spawn_token: writer.to_owned(),
        })
    }

    fn previous(token: &str, spawned_at: Option<i64>) -> PreviousSession {
        PreviousSession {
            spawn_token: token.to_owned(),
            spawned_at,
        }
    }

    fn brief(state: &HandoffState, previous: Option<&PreviousSession>, reason: CoordinatorStartReason) -> String {
        compose_start_brief(StartBriefInputs {
            state,
            previous,
            reason,
            now_epoch_secs: NOW,
            transcript_dir: None,
        })
    }

    #[test]
    fn validate_trims_and_rejects_blank_bodies() {
        assert_eq!(validate_handoff_body("  - fact \n\n").unwrap(), "- fact");
        let err = validate_handoff_body(" \n\t").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn validate_rejects_oversize_bodies_with_the_cap_named() {
        let big = "x".repeat(MAX_HANDOFF_BYTES + 1);
        let err = validate_handoff_body(&big).unwrap_err();
        assert!(err.contains(&MAX_HANDOFF_BYTES.to_string()), "got: {err}");
        assert!(validate_handoff_body(&"x".repeat(MAX_HANDOFF_BYTES)).is_ok());
    }

    #[test]
    fn state_distinguishes_missing_present_and_unreadable() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        assert_eq!(db.coordinator_handoff_state(), HandoffState::Missing);

        let written = db.set_coordinator_handoff("- fact", "token-a", NOW).unwrap();
        assert_eq!(db.coordinator_handoff_state(), HandoffState::Present(written));

        db.set_metadata(HANDOFF_METADATA_KEY, "{not json").unwrap();
        match db.coordinator_handoff_state() {
            HandoffState::Unreadable(reason) => assert!(reason.contains("not valid JSON"), "got: {reason}"),
            other => panic!("corrupt value must read as Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn view_marks_current_session_only_on_exact_token_match() {
        let handoff = CoordinatorHandoff {
            body: "- fact".to_owned(),
            written_at: NOW - 240,
            writer_spawn_token: "token-a".to_owned(),
        };
        let view = handoff_view(&handoff, Some("token-a"), NOW);
        assert!(view.written_by_current_session);
        assert_eq!(view.age_secs, 240);
        assert_eq!(view.written_at_iso8601, format_epoch_iso8601(NOW - 240));
        assert!(!handoff_view(&handoff, Some("token-b"), NOW).written_by_current_session);
        assert!(!handoff_view(&handoff, None, NOW).written_by_current_session);

        let anonymous = CoordinatorHandoff {
            writer_spawn_token: String::new(),
            ..handoff
        };
        assert!(
            !handoff_view(&anonymous, Some(""), NOW).written_by_current_session,
            "an empty writer token must never match, even against an empty current token"
        );
    }

    #[test]
    fn brief_reports_a_handoff_written_by_the_session_that_just_ended() {
        let state = present(NOW - 240, "token-a");
        let prev = previous("token-a", Some(NOW - 3600));
        let text = brief(&state, Some(&prev), CoordinatorStartReason::PaneDead);
        assert!(
            text.contains("HANDOFF PRESENT: written by the session that just ended"),
            "{text}"
        );
        assert!(text.contains("4 minutes ago"), "{text}");
        assert!(
            text.contains("--- handoff begins ---\n- greyarea is shut down"),
            "{text}"
        );
        assert!(text.contains("claude process exited"), "{text}");
        assert!(!text.contains("STALE"), "{text}");
    }

    #[test]
    fn brief_flags_a_handoff_left_by_an_earlier_session_as_stale() {
        // The ended session (token-b) never refreshed the handoff token-a
        // wrote: an abrupt kill must read as a gap, not as a handoff.
        let state = present(NOW - 3 * 86_400, "token-a");
        let prev = previous("token-b", Some(NOW - 7200));
        let text = brief(&state, Some(&prev), CoordinatorStartReason::SessionMissing);
        assert!(text.contains("HANDOFF STALE"), "{text}");
        assert!(text.contains("never wrote a handoff"), "{text}");
        assert!(text.contains("3 days ago"), "{text}");
        assert!(text.contains("started 2 hours ago"), "{text}");
        assert!(text.contains("--- handoff begins ---"), "{text}");
    }

    #[test]
    fn brief_with_no_previous_record_does_not_claim_a_writer() {
        let state = present(NOW - 60, "token-a");
        let text = brief(&state, None, CoordinatorStartReason::FirstCreation);
        assert!(text.contains("HANDOFF PRESENT (writer unknown)"), "{text}");
    }

    #[test]
    fn brief_is_loud_when_nothing_was_ever_written() {
        let prev = previous("token-b", Some(NOW - 600));
        let text = brief(&HandoffState::Missing, Some(&prev), CoordinatorStartReason::PaneDead);
        assert!(text.contains("NO HANDOFF AVAILABLE"), "{text}");
        assert!(text.contains("NOT the same as \"nothing to hand off\""), "{text}");
        assert!(text.contains("started 10 minutes ago"), "{text}");
    }

    #[test]
    fn brief_does_not_alarm_on_the_very_first_session() {
        let text = brief(&HandoffState::Missing, None, CoordinatorStartReason::FirstCreation);
        assert!(text.contains("NO HANDOFF: none is expected"), "{text}");
        assert!(!text.contains("NO HANDOFF AVAILABLE"), "{text}");
    }

    #[test]
    fn brief_separates_unreadable_from_missing() {
        let state = HandoffState::Unreadable("stored coordinator handoff is not valid JSON: eof".to_owned());
        let prev = previous("token-b", None);
        let text = brief(
            &state,
            Some(&prev),
            CoordinatorStartReason::Recreate(CoordinatorRecreateReason::OperatorReset),
        );
        assert!(text.contains("HANDOFF UNREADABLE"), "{text}");
        assert!(text.contains("not valid JSON: eof"), "{text}");
        assert!(text.contains("operator explicitly reset"), "{text}");
        assert!(!text.contains("NO HANDOFF AVAILABLE"), "{text}");
    }

    #[test]
    fn brief_always_ends_with_the_consumption_steps_and_names_the_transcript_fallback() {
        let dir = PathBuf::from("/home/op/.claude/projects/-x-boss-session");
        let text = compose_start_brief(StartBriefInputs {
            state: &HandoffState::Missing,
            previous: None,
            reason: CoordinatorStartReason::SessionMissing,
            now_epoch_secs: NOW,
            transcript_dir: Some(&dir),
        });
        assert!(text.contains("boss handoff write"), "{text}");
        assert!(text.contains("Last resort only"), "{text}");
        assert!(text.contains(&dir.display().to_string()), "{text}");
        let without = brief(&HandoffState::Missing, None, CoordinatorStartReason::SessionMissing);
        assert!(!without.contains("Last resort only"), "{without}");
    }

    #[test]
    fn transcript_dir_uses_claude_codes_cwd_encoding() {
        let dir = transcript_projects_dir(
            Path::new("/Users/op"),
            Path::new("/Users/op/Some Dir/acme/boss-session"),
        );
        assert_eq!(
            dir,
            PathBuf::from("/Users/op/.claude/projects/-Users-op-Some-Dir-acme-boss-session")
        );
    }

    #[test]
    fn write_start_brief_creates_the_claude_dir_and_returns_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_start_brief(dir.path(), "brief").unwrap();
        assert_eq!(path, dir.path().join(".claude").join(START_BRIEF_FILENAME));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "brief");
    }
}
