//! Automation triage execution: preamble rendering, decision resolution, and
//! the real [`TriageDispatcher`] (Maint task 6).
//!
//! The scheduler (Maint task 5) decides *when* an automation fires and calls
//! [`crate::automation_scheduler::TriageDispatcher::dispatch_triage`] through
//! a seam. This module supplies the real implementation,
//! [`EngineTriageDispatcher`], which creates a `ready`
//! [`boss_protocol::EXECUTION_KIND_AUTOMATION_TRIAGE`] work_execution bound to
//! the automation and kicks the coordinator. From there the execution flows
//! through the *normal* dispatch pipeline (cube lease → worker pane), routed
//! to the automations pool by `kind`, with one difference at each end:
//!
//! - **Spawn:** the runner renders [`render_triage_preamble`] instead of the
//!   ordinary work-item prompt (the worker is a *triage* agent, not an
//!   implementer).
//! - **Stop:** the completion handler resolves the agent's decision with
//!   [`resolve_triage_decision`] and finalises the matching `automation_runs`
//!   row, rather than running PR detection.
//!
//! ## How the decision reaches the engine
//!
//! **Primary:** the agent writes its decision to the engine-owned
//! structured-output artifact
//! ([`StructuredOutputKind::TriageDecision`]) — a plain file, no transcript
//! format involved, so any agent driver can satisfy it.
//!
//! **Fallback:** the driver recovers it from the worker's prose. For the
//! Claude driver that is the historical marker protocol
//! (`automation: task <id>` / `automation: skip — <reason>`), which now lives
//! in `boss_engine_driver::claude::structured_output` rather than here.
//!
//! ## The marker protocol (design Risk #3) — and why it is not the primary signal
//!
//! The decision marker is unreliable in practice: a measurement of 67
//! consecutive `produced_task` finalizations found none carrying a marker
//! the parser accepted — every one was decided by
//! [`crate::work::WorkDb::find_most_recent_open_task_for_automation`]'s
//! DB-state recovery instead. This is not a parser bug (the parser accepts
//! exactly the marker text the preamble instructs, see the
//! `preamble_marker_examples_round_trip_through_the_validator` test below,
//! and every known transcript-reading edge case — multi-turn concatenation,
//! Stop-boundary flush races — already has a fix and regression test in
//! `completion.rs`). It is that a single-shot agent asked to both
//! investigate *and* end its very last line with an exact, bare, no-prose-
//! after-it string essentially never complies literally, even when it
//! reaches the right decision. So the DB-derived decision (a task actually
//! created, or a final message that plainly concludes "nothing to do") is
//! treated as the primary signal by
//! [`crate::completion::WorkerCompletionHandler::finalize_automation_triage`],
//! and the marker — when it *does* parse — as a same-answer fast-path
//! override, not the thing recovery falls back to. The marker parser does
//! not enforce "exactly one": it scans the joined transcript in reverse and
//! takes the last marker-shaped line, so a marker the agent narrated earlier
//! (e.g. quoting the preamble's example) cannot shadow the real final
//! decision. The transactional cap re-check at `boss task create
//! --automation` (see [`crate::work::WorkDb::create_automation_task`])
//! remains the backstop against a misbehaving agent fanning out.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::Automation;

use crate::automation_scheduler::{TriageDispatch, TriageDispatcher};
use crate::driver::AgentDriver;
use crate::structured_output::{StructuredOutputKind, TriageOutcome};
use crate::work::WorkDb;

pub use boss_execution_runner::automation_triage::*;

/// One decision the triage agent can reach, parsed from its final message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// `automation: task <id>` — the agent created a task with the given
    /// (friendly or canonical) id. The detector verifies the id resolves to a
    /// task whose `source_automation_id` is this automation before trusting it.
    ProducedTask(String),
    /// `automation: skip — <reason>` — the agent decided nothing actionable
    /// exists right now. An explicit, agent-authored no-op.
    Skip(String),
    /// No decision at all — the worker errored, was reaped, or simply never
    /// reached one. Treated as a transient/ambiguous failure (the run is left
    /// `failed_will_retry`), never as a skip.
    NoDecision,
}

impl TriageDecision {
    /// Interpret a decision delivered over the structured-output contract.
    ///
    /// Both channels produce this same wire form — the artifact the agent
    /// wrote, and the Claude driver's marker-line fallback producer — so this
    /// is the single point where a triage payload becomes a decision.
    ///
    /// Returns `None` for a malformed payload (an unrecognised `decision`, or
    /// a `task` decision with no id): the caller then treats the channel as
    /// having produced nothing, exactly as it does for an absent artifact,
    /// rather than recording a bogus outcome against the automation run.
    pub fn from_outcome(outcome: &TriageOutcome) -> Option<Self> {
        match outcome.decision.as_str() {
            TriageOutcome::DECISION_TASK => {
                let id = outcome.task_id.as_deref().unwrap_or_default().trim();
                (!id.is_empty()).then(|| Self::ProducedTask(id.to_owned()))
            }
            TriageOutcome::DECISION_SKIP => Some(Self::Skip(
                outcome.reason.as_deref().unwrap_or_default().trim().to_owned(),
            )),
            _ => None,
        }
    }
}

/// Resolve the decision a triage agent reached, file first.
///
/// Two channels, in order:
///
/// 1. **Primary — the structured-output artifact.** The agent wrote a
///    [`TriageOutcome`] to the path its prompt named. Driver-agnostic: no
///    transcript format, no final-message convention.
/// 2. **Fallback — the driver's producer.** `driver` recovers the same wire
///    form from `final_message` (for Claude, the `automation:` marker line).
///
/// A present-but-malformed payload on either channel is logged and treated as
/// "this channel produced nothing", so a garbled artifact falls through to the
/// prose rather than recording a bogus outcome. When neither channel yields a
/// decision the result is [`TriageDecision::NoDecision`] and the caller's
/// existing recovery path (look for an open task this automation created)
/// takes over.
pub fn resolve_triage_decision(
    driver: &dyn AgentDriver,
    structured_output_dir: &Path,
    execution_id: &str,
    final_message: Option<&str>,
) -> TriageDecision {
    let kind = StructuredOutputKind::TriageDecision;
    match crate::structured_output::read_json::<TriageOutcome>(structured_output_dir, execution_id, kind) {
        Ok(Some(outcome)) => match TriageDecision::from_outcome(&outcome) {
            Some(decision) => return decision,
            None => tracing::warn!(
                execution_id,
                decision = %outcome.decision,
                "triage: decision artifact present but not a recognised outcome; \
                 falling back to the driver's producer",
            ),
        },
        Ok(None) => {}
        Err(err) => tracing::warn!(
            execution_id,
            error = %err,
            "triage: decision artifact present but did not validate; \
             falling back to the driver's producer",
        ),
    }

    let Some(text) = final_message else {
        return TriageDecision::NoDecision;
    };
    driver
        .structured_output_fallback(kind, text)
        .iter()
        .find_map(|candidate| {
            serde_json::from_str::<TriageOutcome>(&candidate.payload)
                .ok()
                .as_ref()
                .and_then(TriageDecision::from_outcome)
        })
        .unwrap_or(TriageDecision::NoDecision)
}

/// The real [`TriageDispatcher`]: creates an `automation_triage` execution and
/// kicks the coordinator so its normal drain picks the row up.
///
/// `kick` is a thin closure over `ExecutionCoordinator::kick`, mirroring how
/// the other sweepers (`dep_unblock_sweep`) re-enter the scheduler — it keeps
/// this module free of a hard dependency on the coordinator type. `is_paused`
/// is the same pattern for `ExecutionCoordinator::is_automation_paused` — the
/// single seam both the scheduler's fire path and `boss automation run`'s
/// manual fire path go through, so a `bossctl automation pause` blocks new
/// triage passes from either caller without each needing its own check.
pub struct EngineTriageDispatcher {
    work_db: Arc<WorkDb>,
    kick: Arc<dyn Fn() + Send + Sync>,
    is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl EngineTriageDispatcher {
    pub fn new(
        work_db: Arc<WorkDb>,
        kick: Arc<dyn Fn() + Send + Sync>,
        is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            work_db,
            kick,
            is_paused,
        }
    }

    /// Resolve the repo the triage worker should lease: the automation's
    /// explicit `repo_remote_url` override, else the product's primary repo.
    /// `None` when neither is available (a genuinely unrunnable automation).
    fn resolve_repo(&self, automation: &Automation) -> Option<String> {
        if let Some(repo) = automation.repo_remote_url.clone() {
            return Some(repo);
        }
        self.work_db
            .get_product(&automation.product_id)
            .ok()
            .flatten()
            .and_then(|p| p.repo_remote_url)
    }

    /// Shared fire path used by both the scheduler seam and the manual
    /// `boss automation run` verb: resolve repo, create the triage execution,
    /// kick the coordinator.
    pub fn fire(&self, automation: &Automation) -> TriageDispatch {
        // A pause is `Held`, not `TransientFailure`. The two used to share the
        // transient spelling — the only vocabulary this seam had — which meant
        // the scheduler treated a deliberate operator gate exactly like an
        // unreachable VPN: one `failed_will_retry` row per attempt, retried at
        // the loop's sleep floor of once per second, and aged out as "stale"
        // 15 minutes later. See `automation_scheduler::TriageDispatch::Held`.
        if (self.is_paused)() {
            return TriageDispatch::Held {
                reason: "automation is paused (bossctl automation pause); holding new triage \
                         passes until `bossctl automation resume`"
                    .to_owned(),
            };
        }
        let Some(repo) = self.resolve_repo(automation) else {
            return TriageDispatch::TransientFailure {
                detail: format!(
                    "automation {} has no repo and its product has no primary repo; \
                     cannot lease a workspace",
                    automation.id
                ),
            };
        };
        match self.work_db.create_automation_triage_execution(&automation.id, &repo) {
            Ok(execution) => {
                (self.kick)();
                TriageDispatch::Dispatched {
                    execution_id: execution.id,
                }
            }
            Err(err) => TriageDispatch::TransientFailure {
                detail: format!("failed to create triage execution: {err:#}"),
            },
        }
    }
}

#[async_trait]
impl TriageDispatcher for EngineTriageDispatcher {
    async fn dispatch_triage(&self, automation: &Automation, _scheduled_for_epoch: i64) -> TriageDispatch {
        self.fire(automation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the engine-owned decision artifact path in preamble tests.
    const ARTIFACT_PATH: &str = "/tmp/boss-worker-output/exec_1.triage.json";

    /// Resolve a decision from worker prose alone, with no artifact present —
    /// i.e. exercising the Claude driver's marker-line fallback producer
    /// through the same entry point the completion handler uses.
    fn decision_from(text: &str) -> TriageDecision {
        let empty = std::env::temp_dir().join(format!("boss-triage-no-artifact-{}", std::process::id()));
        resolve_triage_decision(&crate::driver::ClaudeDriver, &empty, "exec_absent", Some(text))
    }

    /// Scratch structured-output dir seeded with `contents` as this
    /// execution's triage artifact.
    fn artifact_dir(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("boss-triage-artifact-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::structured_output::path_for(&dir, "exec_1", StructuredOutputKind::TriageDecision),
            contents,
        )
        .unwrap();
        dir
    }

    #[test]
    fn artifact_is_the_primary_channel_and_beats_the_marker() {
        // Artifact and marker disagree: the file wins.
        let dir = artifact_dir("primary", r#"{"decision":"task","task_id":"T42"}"#);
        let decision = resolve_triage_decision(
            &crate::driver::ClaudeDriver,
            &dir,
            "exec_1",
            Some("automation: skip — nothing to do"),
        );
        assert_eq!(decision, TriageDecision::ProducedTask("T42".to_owned()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn artifact_skip_carries_its_reason() {
        let dir = artifact_dir("skip", r#"{"decision":"skip","reason":"no clippy warnings today"}"#);
        assert_eq!(
            resolve_triage_decision(&crate::driver::ClaudeDriver, &dir, "exec_1", None),
            TriageDecision::Skip("no clippy warnings today".to_owned()),
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_artifact_falls_back_to_the_driver_producer() {
        for bad in [r#"{not json}"#, r#"{"decision":"maybe"}"#, r#"{"decision":"task"}"#] {
            let dir = artifact_dir("malformed", bad);
            assert_eq!(
                resolve_triage_decision(
                    &crate::driver::ClaudeDriver,
                    &dir,
                    "exec_1",
                    Some("automation: task T7"),
                ),
                TriageDecision::ProducedTask("T7".to_owned()),
                "a {bad} artifact must not shadow a usable marker",
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn no_artifact_and_no_message_is_no_decision() {
        let dir = std::env::temp_dir().join(format!("boss-triage-empty-{}", std::process::id()));
        assert_eq!(
            resolve_triage_decision(&crate::driver::ClaudeDriver, &dir, "exec_absent", None),
            TriageDecision::NoDecision,
        );
    }

    #[test]
    fn triage_preamble_names_the_decision_artifact_and_keeps_the_marker() {
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        assert!(
            preamble.contains(ARTIFACT_PATH),
            "preamble must name the artifact path the engine reads first"
        );
        assert!(
            preamble.contains(r#"{"decision": "task", "task_id": "T42"}"#),
            "preamble must show the artifact's wire form"
        );
        assert!(
            preamble.contains("automation: task T42"),
            "the marker fallback must survive alongside the file contract"
        );
    }

    #[test]
    fn triage_claude_md_restates_marker_contract_and_omits_pr_mandate() {
        let md = render_triage_claude_md("lease_abc", false);
        // The lease id is surfaced so a confused worker can describe itself.
        assert!(md.contains("lease_abc"));
        // Restates the marker contract (the whole point of the triage run).
        assert!(md.contains("automation: task"));
        assert!(md.contains("automation: skip"));
        assert!(
            md.contains("exactly one"),
            "triage CLAUDE.md must restate the exactly-one-marker contract",
        );
        // Must NOT carry the implementation worker's PR-delivery mandate — that
        // contradiction is the root cause of "triage ended without a decision
        // marker" (the worker chases a PR and never emits the marker).
        assert!(
            !md.contains("Pull requests are the deliverable"),
            "triage CLAUDE.md must not include the standard PR-required reminder",
        );
        assert!(
            !md.contains("A task is not complete until a PR exists"),
            "triage CLAUDE.md must not include the implementation PR mandate",
        );
        assert!(
            !md.contains("PR creation is your terminal act"),
            "triage CLAUDE.md must not tell the worker its terminal act is a PR",
        );
        assert!(
            !md.contains("Print the PR URL"),
            "triage CLAUDE.md must not instruct the worker to print a PR URL",
        );
        // States the no-PR posture explicitly.
        assert!(md.contains("no pull-request deliverable") || md.contains("NO pull-request deliverable"));
    }

    #[test]
    fn triage_claude_md_forbids_sub_agents_and_deferral() {
        let md = render_triage_claude_md("lease_xyz", false);
        // Must explicitly name the Agent tool and explain why it is forbidden
        // (the hang mode: no resume mechanism once a sub-agent is spawned).
        assert!(
            md.contains("sub-agent") || md.contains("sub agent"),
            "triage CLAUDE.md must forbid spawning a sub-agent",
        );
        // Must warn against deferring intent to a later turn.
        assert!(
            md.contains("defer") || md.contains("deferral") || md.contains("later turn"),
            "triage CLAUDE.md must warn against deferring intent to a later turn",
        );
        // Must tell the worker to emit the marker in the same response as the
        // task-create tool call.
        assert!(
            md.contains("same response") || md.contains("same turn"),
            "triage CLAUDE.md must instruct the worker to emit the marker in the same response as the tool call",
        );
    }

    #[test]
    fn triage_claude_md_requires_early_decision_and_forbids_heavy_sweeps() {
        let md = render_triage_claude_md("lease_lw", false);
        let lower = md.to_lowercase();
        // Decide-then-stop: emit the marker the moment the verdict is known.
        assert!(
            lower.contains("decide the moment you have enough signal"),
            "triage CLAUDE.md must tell the worker to decide as soon as it has a verdict",
        );
        assert!(
            md.contains("one more confirming check"),
            "triage CLAUDE.md must forbid re-proving a verdict with \"one more confirming check\"",
        );
        // The budget-burn anti-pattern: repo-wide sweeps.
        assert!(
            lower.contains("repo-wide"),
            "triage CLAUDE.md must forbid repo-wide build/lint sweeps",
        );
        // Bias to a decision under uncertainty rather than escalating verification.
        assert!(
            md.contains("`skip`"),
            "triage CLAUDE.md must tell the worker to skip when it cannot cheaply confirm work",
        );
    }

    // -----------------------------------------------------------
    // Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md,
    // implementation task 11): `automation_outcome_proposals_seam` teaches
    // `boss propose automation-outcome` instead of the legacy `automation:
    // task`/`automation: skip` markers, in both the triage preamble and
    // CLAUDE.md. Mirrors the `deferred_scope_proposals_seam` /
    // `followup_proposals_seam` prompt tests for the earlier seams.
    // -----------------------------------------------------------

    #[test]
    fn triage_claude_md_teaches_legacy_markers_when_seam_is_off() {
        let md = render_triage_claude_md("lease_seam_off", false);
        assert!(md.contains("automation: task <id>"));
        assert!(md.contains("automation: skip — <one-line reason>"));
        assert!(
            !md.contains("boss propose automation-outcome"),
            "seam off: must not teach a verb the engine won't yet read proposals-first for:\n{md}",
        );
    }

    #[test]
    fn triage_claude_md_teaches_boss_propose_verb_when_seam_is_on() {
        let md = render_triage_claude_md("lease_seam_on", true);
        assert!(
            md.contains("\"$BOSS_BIN\" propose automation-outcome --produced-task <id>"),
            "seam on: must teach the produced-task verb form:\n{md}",
        );
        assert!(
            md.contains("\"$BOSS_BIN\" propose automation-outcome --skip --reason \"<one-line reason>\""),
            "seam on: must teach the skip verb form with a worked example:\n{md}",
        );
        assert!(
            !md.contains("automation: task <id>"),
            "seam on: must not also teach the legacy marker grammar:\n{md}",
        );
        assert!(
            !md.contains("automation: skip — <one-line reason>"),
            "seam on: must not also teach the legacy marker grammar:\n{md}",
        );
        // The lease id and no-PR posture still surface regardless of the seam.
        assert!(md.contains("lease_seam_on"));
        assert!(md.contains("no pull-request deliverable") || md.contains("NO pull-request deliverable"));
    }

    #[test]
    fn preamble_requires_early_decision_and_forbids_heavy_sweeps() {
        let automation = Automation::builder()
            .id("auto_lw")
            .short_id(2i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        let lower = preamble.to_lowercase();
        // Decide-then-stop.
        assert!(
            lower.contains("decide the moment you have enough signal"),
            "preamble must tell the worker to decide as soon as it has a verdict",
        );
        assert!(
            preamble.contains("one more confirming check"),
            "preamble must forbid re-proving a verdict with \"one more confirming check\"",
        );
        // No repo-wide sweeps (the field-evidence budget-burn shape).
        assert!(lower.contains("repo-wide"), "preamble must forbid repo-wide sweeps");
        // Bias to a decision under uncertainty.
        assert!(
            preamble.contains("`skip`"),
            "preamble must tell the worker to skip when it cannot cheaply confirm work",
        );
        assert!(
            lower.contains("heavier sweep"),
            "preamble must forbid escalating an inconclusive check to a heavier sweep",
        );
    }

    /// Every triage prompt variant carries the same background-work rule as the
    /// main worker prompt — foreground preferred, a backgrounded command
    /// declared with `propose wait` — and none carries a blanket ban on
    /// backgrounding.
    #[test]
    fn triage_prompts_use_the_propose_wait_rule_not_a_background_ban() {
        let automation = Automation::builder()
            .id("auto_bg")
            .short_id(3i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let declaration =
            crate::prompt_fragments::background_wait_declaration(boss_engine_worker_bin::WORKER_BOSS_INVOCATION);
        for seam_enabled in [false, true] {
            let preamble = render_triage_preamble(
                &automation,
                "My Product",
                &[],
                &TriageContext::default(),
                ARTIFACT_PATH,
                seam_enabled,
            );
            let claude_md = render_triage_claude_md("lease_bg", seam_enabled);
            for (label, text) in [("preamble", preamble), ("CLAUDE.md", claude_md)] {
                // The markdown renderer hard-wraps; compare on collapsed whitespace.
                let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    flat.contains(&declaration),
                    "triage {label} (seam_enabled={seam_enabled}) must carry the shared propose-wait rule:\n{text}",
                );
                let lower = flat.to_lowercase();
                assert!(
                    !lower.contains("wait for any external process"),
                    "triage {label} (seam_enabled={seam_enabled}) must not carry the unconditional no-wait rule:\n{text}",
                );
                assert!(
                    !lower.contains("idle-poll") && !lower.contains("do not run a long build in the background"),
                    "triage {label} (seam_enabled={seam_enabled}) must not ban backgrounding outright:\n{text}",
                );
            }
        }
    }

    #[test]
    fn preamble_forbids_sub_agents_and_deferral() {
        let automation = Automation::builder()
            .id("auto_abc")
            .short_id(1i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        // Must explicitly name the Agent tool and explain the hang risk.
        assert!(
            preamble.contains("Agent"),
            "preamble must name the Agent tool to tell the worker not to use it",
        );
        // Must name the failure mode (sub-agent hang) so the worker understands why.
        assert!(
            preamble.contains("sub-agent") || preamble.contains("sub agent"),
            "preamble must forbid spawning a sub-agent",
        );
        // Must require the marker to be emitted in the same response as the task
        // creation — the premature-end failure mode in the field evidence.
        assert!(
            preamble.contains("same response") || preamble.contains("same turn"),
            "preamble must instruct the worker to emit the marker in the same response as the tool call",
        );
        // Must warn against deferred intent.
        assert!(
            preamble.to_lowercase().contains("defer") || preamble.contains("later turn"),
            "preamble must warn against deferring intent to a later turn",
        );
    }

    /// Every literal `automation: task`/`automation: skip` example line the
    /// preamble shows the agent must itself parse cleanly, so an edit to
    /// either the preamble's wording or the validator's matching rules
    /// cannot silently drift the two apart. The preamble shows three such
    /// examples: the task marker, the duplicate-suspect skip marker, and
    /// the generic skip marker.
    #[test]
    fn preamble_marker_examples_round_trip_through_the_validator() {
        let automation = Automation::builder()
            .id("auto_rt")
            .short_id(9i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );

        let marker_lines: Vec<&str> = preamble
            .lines()
            .map(str::trim)
            .filter(|line| line.to_lowercase().starts_with("automation:"))
            .collect();
        assert_eq!(
            marker_lines.len(),
            3,
            "preamble must show exactly one task-marker example, one duplicate-skip-marker \
             example, and one generic-skip-marker example, found: {marker_lines:?}",
        );

        let parse = |line: &str| {
            let dir = std::env::temp_dir().join(format!("boss-triage-rt-{}-{}", std::process::id(), line.len()));
            let decision = resolve_triage_decision(&crate::driver::ClaudeDriver, &dir, "exec_rt", Some(line));
            let _ = std::fs::remove_dir_all(&dir);
            decision
        };
        assert_eq!(
            parse(marker_lines[0]),
            TriageDecision::ProducedTask("T42".to_owned()),
            "the preamble's own task-marker example must parse as the validator expects",
        );
        assert_eq!(
            parse(marker_lines[1]),
            TriageDecision::Skip("duplicate of Txxxx".to_owned()),
            "the preamble's own duplicate-skip-marker example must parse as the validator expects",
        );
        assert_eq!(
            parse(marker_lines[2]),
            TriageDecision::Skip("<one-line reason>".to_owned()),
            "the preamble's own generic-skip-marker example must parse as the validator expects",
        );
    }

    #[test]
    fn preamble_teaches_boss_propose_verb_when_seam_is_on() {
        let automation = Automation::builder()
            .id("auto_rt_seam")
            .short_id(10i64)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            true,
        );
        assert!(
            preamble.contains("\"$BOSS_BIN\" propose automation-outcome --produced-task T42"),
            "seam on: preamble must teach the produced-task verb with a worked example:\n{preamble}",
        );
        assert!(
            preamble.contains("\"$BOSS_BIN\" propose automation-outcome --skip --reason \"duplicate of Txxxx\""),
            "seam on: preamble must teach the duplicate-skip verb form:\n{preamble}",
        );
        assert!(
            preamble.contains("\"$BOSS_BIN\" propose automation-outcome --skip --reason \"<one-line reason>\""),
            "seam on: preamble must teach the generic-skip verb form:\n{preamble}",
        );
        assert!(
            !preamble
                .lines()
                .any(|line| line.trim().to_lowercase().starts_with("automation:")),
            "seam on: preamble must not also teach the legacy marker grammar:\n{preamble}",
        );
    }

    #[test]
    fn preamble_includes_contract_and_canonical_selector() {
        let automation = Automation::builder()
            .id("auto_123")
            .short_id(3)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        assert!(preamble.contains("triage"));
        assert!(preamble.contains("A3"));
        assert!(preamble.contains("My Product"));
        assert!(preamble.contains("fix any clippy warnings"));
        // Canonical selector so the agent's create resolves without --product.
        assert!(preamble.contains("--automation auto_123"));
        assert!(preamble.contains("automation: task"));
        assert!(preamble.contains("automation: skip"));
        assert!(preamble.contains("Do NOT do the work"));
    }

    /// Layer 1 (pre-file dedup gate): the preamble must require declaring
    /// target files on the create command, explain why (the gate's key),
    /// and tell the agent how to react to a `duplicate-suspect` rejection
    /// instead of retrying.
    #[test]
    fn preamble_requires_declaring_target_files() {
        let automation = Automation::builder()
            .id("auto_dedup")
            .short_id(4)
            .product_id("prod_1")
            .name("dedup sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("look for duplicated code and extract a helper")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(
            &automation,
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        assert!(
            preamble.contains("--target-file"),
            "preamble must include --target-file on the create command example",
        );
        assert!(
            preamble.contains("Declare every file"),
            "preamble must instruct the agent to declare every file it expects to touch",
        );
        assert!(
            preamble.contains("duplicate-suspect"),
            "preamble must explain the duplicate-suspect rejection",
        );
        assert!(
            preamble.contains("automation: skip — duplicate of"),
            "preamble must tell the agent to skip with a duplicate-of reason on a gate hit",
        );
    }

    /// Regression test: when the triage agent calls `boss task create` the
    /// decision marker appears in the SECOND assistant turn (after the tool
    /// result). The previous `iter().rev().find_map(AssistantText)` approach
    /// returned only the last AssistantText event; if the Stop hook fires
    /// before that post-tool turn is fully flushed to disk, the engine read
    /// the pre-tool analysis text (no marker) instead, recording
    /// `failed_will_retry`. The fix concatenates ALL AssistantText turns so
    /// the marker is detected regardless of which turn contains it.
    #[test]
    fn marker_detected_from_concatenated_multi_turn_transcript() {
        use boss_transcript_markdown::{TranscriptEventKind, parse_transcript};

        // Simulate the JSONL transcript for a task-creating triage run:
        //   Turn 1: analysis prose + boss task create tool call
        //   (tool result from the tool)
        //   Turn 2: post-tool summary with the decision marker
        let jsonl = concat!(
            // Turn 1: analysis + tool_use
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I found work to do. Let me create a task."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"boss task create --automation auto_xxx --name \"Fix tests\""}}]}}"#,
            "\n",
            // Tool result
            r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"Created task T1330"}],"isError":false}"#,
            "\n",
            // Turn 2: post-tool marker (the critical turn)
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Created task T1330.\n\nautomation: task T1330"}]}}"#,
            "\n",
        );

        let events = parse_transcript(jsonl);

        // Collect all AssistantText events (the fix's approach).
        let all_text: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::AssistantText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();

        // There are two assistant text turns.
        assert_eq!(all_text.len(), 2, "should have two assistant text turns");

        // The OLD code: find the last AssistantText.
        // When the post-tool turn IS present, even the old code would work.
        // The bug manifested when the post-tool turn was MISSING from the
        // transcript (timing race). Simulate that by taking only the first turn:
        let only_pre_tool = &all_text[..1];
        let pre_tool_decision = decision_from(&only_pre_tool[0]);
        assert_eq!(
            pre_tool_decision,
            TriageDecision::NoDecision,
            "pre-tool analysis text has no marker — this is what the old code saw when Turn 2 was missing"
        );

        // The NEW code: join all turns and parse the combined text.
        let combined = all_text.join("\n");
        let decision = decision_from(&combined);
        assert_eq!(
            decision,
            TriageDecision::ProducedTask("T1330".to_owned()),
            "concatenating all turns finds the marker in the post-tool turn"
        );
    }

    fn sibling(short_id: i64, name: &str, status: &str, pr_url: Option<&str>) -> crate::work::AutomationSiblingTask {
        crate::work::AutomationSiblingTask {
            short_id,
            name: name.to_owned(),
            status: status.to_owned(),
            pr_url: pr_url.map(str::to_owned),
        }
    }

    fn dedup_automation() -> Automation {
        Automation::builder()
            .id("auto_dd")
            .short_id(7i64)
            .product_id("prod_1")
            .name("file size sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("split any file over the size limit")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build()
    }

    /// The whole point of the section: a memory-less triage agent must be
    /// able to see what it already filed, by id, status, and PR.
    #[test]
    fn preamble_lists_already_tracked_tasks() {
        let siblings = [
            sibling(101, "Split engine/core/src/app.rs", "active", None),
            sibling(
                102,
                "Extract pr_review into its own crate",
                "in_review",
                Some("https://github.com/spinyfin/mono/pull/9001"),
            ),
        ];
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &siblings,
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );

        assert!(preamble.contains("Already tracked"), "section heading missing");
        assert!(preamble.contains("T101"), "must cite each sibling by friendly id");
        assert!(preamble.contains("T102"));
        assert!(
            preamble.contains("Split engine/core/src/app.rs"),
            "titles must be shown"
        );
        assert!(preamble.contains("[active]"), "status tells the agent someone is on it");
        assert!(
            preamble.contains("https://github.com/spinyfin/mono/pull/9001"),
            "a PR url is the strongest in-hand signal and must be shown",
        );
        // The instruction that turns the list into a decision.
        assert!(
            preamble.contains("do NOT file it again"),
            "the list is useless without the instruction not to re-file",
        );
        assert!(
            preamble.contains("automation: skip — already tracked as T<n>"),
            "must show the exact marker to emit instead of re-filing",
        );
    }

    /// Companion to `preamble_lists_already_tracked_tasks`: with the seam on,
    /// the already-tracked dedup skip example must teach `boss propose
    /// automation-outcome --skip`, not the legacy marker.
    #[test]
    fn preamble_already_tracked_section_teaches_boss_propose_verb_when_seam_is_on() {
        let siblings = [sibling(101, "Split engine/core/src/app.rs", "active", None)];
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &siblings,
            &TriageContext::default(),
            ARTIFACT_PATH,
            true,
        );
        assert!(
            preamble.contains("\"$BOSS_BIN\" propose automation-outcome --skip --reason \"already tracked as T<n>\""),
            "seam on: must show the boss propose form of the already-tracked skip:\n{preamble}",
        );
        assert!(
            !preamble.contains("automation: skip — already tracked as T<n>"),
            "seam on: must not also show the legacy marker form:\n{preamble}",
        );
    }

    /// The section must still leave room for genuinely new findings — this
    /// is a dedup nudge, not a cap.
    #[test]
    fn preamble_still_permits_distinct_findings() {
        let siblings = [sibling(101, "Split engine/core/src/app.rs", "active", None)];
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &siblings,
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        assert!(
            preamble.contains("Genuinely different target"),
            "must tell the agent that a different file/crate is still fileable",
        );
        assert!(
            preamble.contains("not a cap"),
            "must say explicitly that the list does not cap what can be filed",
        );
    }

    /// No siblings means no section at all. "Already tracked: (none)" would
    /// read as licence to file, which is the opposite of the intent.
    #[test]
    fn preamble_omits_the_section_when_nothing_is_tracked() {
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
            false,
        );
        assert!(
            !preamble.contains("Already tracked"),
            "an empty sibling list must render no section",
        );
        // The rest of the contract is unaffected.
        assert!(preamble.contains("automation: skip"));
        assert!(preamble.contains("A7"));
    }
}
