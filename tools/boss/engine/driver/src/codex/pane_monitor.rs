//! Codex's `PaneMonitorSpec` — the substrings the app screen-scrapes out of a
//! Codex worker's GhosttyKit viewport.
//!
//! # Why this module exists
//!
//! `CodexDriver` used to declare no spec at all, so the app fell back to
//! `PaneMonitorSpec.claudeDefault` (`app-macos/Sources/Ghostty/TerminalPaneSession.swift`).
//! Its agent markers are `"Claude Code"` / `"auto mode on"` / `"/effort"`,
//! none of which a Codex pane can ever render — so every Codex worker's pane
//! monitor was pinned to `notDetected`, while Claude's busy marker
//! (`"esc to interrupt"`) coincidentally *did* match. That partial match is
//! the confidently-wrong shape the marker discipline exists to avoid.
//!
//! # Measurement, not inference
//!
//! Every literal below was observed on a real `codex` TUI. The verbatim
//! strings come from the GhosttyKit-hosted capture in
//! `tools/boss/docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`
//! (V5); their *stability across polls* — which is what a scrape needs and
//! what a single observation cannot tell you — was measured separately over
//! 910 viewport polls of three live sessions in
//! `tools/boss/docs/investigations/codex-tui-liveness-marker-stability-2026-07-31.md`.
//!
//! Two results from that stability pass changed the spec away from a literal
//! transcription of the V5 table, and both are the reason it was run:
//!
//! * **The startup banner scrolls out of the viewport and never returns.**
//!   Codex spawns with `--no-alt-screen`, so the banner is ordinary
//!   scrollback: `">_ OpenAI Codex (v0.145.0)"` and `"/model to change"` were
//!   present for the first ~15 s of a session and absent from every poll
//!   after (last hit poll 61 of 400). A spec whose agent markers were only
//!   the banner would go `notDetected` mid-run — the same defect, delayed.
//!   The composer prefix `"›"` is what actually holds: 909/910 polls (the
//!   one miss is a poll taken before the TUI had painted at all), including
//!   during heavy tool output.
//! * **`"permissions:"` never rendered** (0/910). The V5 table lists it, but
//!   the boxed header in these captures carries `model:` and `directory:`
//!   rows only. Declaring it would be an unmeasured guess, so it is dropped.
//!
//! # Shape this targets
//!
//! The bare interactive TUI session — the one and only shape `CodexDriver`
//! ships (`codex --strict-config --no-alt-screen -a never …`, see
//! `build_codex_command`). There is no `codex exec` pane to support, so no
//! markers are declared for one.
//!
//! Codex's markers are Codex's own. They are deliberately **not** merged into
//! Claude's or Grok's sets — each driver owns its surface strings (same rule
//! `grok.rs` states).

use boss_protocol::PaneMonitorSpec;

/// Measured marker set for a Codex TUI pane under `--no-alt-screen`.
pub(super) fn spec() -> PaneMonitorSpec {
    PaneMonitorSpec {
        // OR-semantics (the app's `agentMarkers.contains { … }`). The two
        // banner literals are precise but short-lived; `"›"` prefixes the
        // composer and every user-message line in the transcript, so it
        // survives for the life of the session. `">_ OpenAI Codex"` omits
        // the version so a CLI bump does not silently un-detect the pane.
        agent_markers: vec![">_ OpenAI Codex".into(), "/model to change".into(), "›".into()],
        // Rendered inside the working footer, e.g.
        // `• Working (9s • esc to interrupt)`. Perfectly discriminating in
        // the stability pass — present on 112/112 busy polls, 0/288 idle —
        // and it drops the instant the turn ends, in one contiguous span
        // with no flicker. Identical to Claude's literal by coincidence of
        // both CLIs' phrasing, not by sharing Claude's set.
        busy_markers: vec!["esc to interrupt".into()],
        // Transient (~1 s) and only when the session boots an MCP server, so
        // it is measured-real but often missed by a 0.5 s poll. Costless
        // either way: starting and busy both classify as `working`.
        starting_markers: vec!["Booting MCP server:".into()],
        // U+203A. The app scans lines bottom-up, so the live composer wins
        // over the `"› …"` history lines above it — Grok's `❯` collision
        // does not bite here.
        //
        // Caveat worth knowing: Codex renders a rotating *placeholder*
        // ("Improve documentation in @filename") in the empty composer, and
        // a scrape cannot tell placeholder from typed text. So the app's
        // `promptHasInput` reads a parked composer as "has input" and its
        // prompt-just-submitted heuristic never fires for Codex. Turn
        // classification is unaffected: it comes from the busy marker.
        prompt_prefixes: vec!["›".into()],
        // Same as Claude and Grok. Justified here by the busy marker's clean
        // single-span behaviour — two polls of a stable prompt are enough.
        idle_debounce_polls: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_pane_monitor_spec_matches_measured_literals() {
        let spec = spec();
        assert_eq!(spec.agent_markers, vec![">_ OpenAI Codex", "/model to change", "›"]);
        assert_eq!(spec.busy_markers, vec!["esc to interrupt"]);
        assert_eq!(spec.starting_markers, vec!["Booting MCP server:"]);
        assert_eq!(spec.prompt_prefixes, vec!["›"]);
        assert_eq!(spec.idle_debounce_polls, 2);
    }

    #[test]
    fn codex_agent_markers_survive_the_banner_scrolling_out() {
        // The whole point of the stability pass: at least one agent marker
        // must still be present once `--no-alt-screen` has scrolled the
        // startup banner out of the viewport. `"›"` is that marker.
        let spec = spec();
        let after_scroll_out = "• Ran seq 1 200\n  └ …\n\n› Improve documentation in @filename\n\
                                \n  gpt-5.6-terra low · /ws/mono-agent-001";
        assert!(
            spec.agent_markers.iter().any(|m| after_scroll_out.contains(m.as_str())),
            "no agent marker survives banner scroll-out: {:?}",
            spec.agent_markers
        );
    }

    #[test]
    fn codex_declares_no_claude_or_grok_chrome() {
        // Guardrail against the forbidden fix: never detect Codex by making
        // another driver's markers match, and never borrow their chrome.
        let spec = spec();
        let all: Vec<&str> = spec
            .agent_markers
            .iter()
            .chain(spec.busy_markers.iter())
            .chain(spec.starting_markers.iter())
            .map(String::as_str)
            .collect();
        for foreign in [
            "Claude Code",
            "auto mode on",
            "/effort",
            "Shift+Tab:mode",
            "Grok 4",
            "[stop]",
        ] {
            assert!(
                !all.iter().any(|m| m.contains(foreign)),
                "borrowed foreign chrome: {foreign}"
            );
        }
    }
}
