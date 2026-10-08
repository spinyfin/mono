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
//! `tools/boss/docs/investigations/codex-tui-liveness-marker-stability-2026-07-31.md`,
//! and re-measured against codex-cli 0.160.1 (whose 0.159/0.160 releases
//! redrew the welcome screen, session header and turn footers) in
//! `tools/boss/docs/investigations/codex-0.160.1-qualification-2026-10-06.md`.
//!
//! Three results from those passes changed the spec away from a literal
//! transcription of the V5 table, and they are the reason it was run:
//!
//! * **The startup banner scrolls out of the viewport and never returns.**
//!   Codex spawns with `--no-alt-screen`, so the banner is ordinary
//!   scrollback: `">_ OpenAI Codex (v0.145.0)"` and `"/model to change"` were
//!   present for the first ~15 s of a session and absent from every poll
//!   after (last hit poll 61 of 400). A spec whose agent markers were only
//!   the banner would go `notDetected` mid-run — the same defect, delayed.
//!   The composer prefix `"›"` is what actually holds: 909/910 polls (the
//!   one miss is a poll taken before the TUI had painted at all), including
//!   during heavy tool output. On 0.160.1 the same holds: 334/335 polls, the
//!   one miss again the pre-paint poll.
//! * **`"/model to change"` no longer renders at all on 0.160.1.** The
//!   0.159 welcome-screen refresh (openai/codex#48513) replaced the banner's
//!   hint line with a rotating one-liner (`"A fresh prompt. An open
//!   question."`, `"Welcome to the command line, Neo."`, …), none of them
//!   stable. 0/335 polls on 0.160.1 versus 58/327 on 0.153.4 under the same
//!   harness; the literal is also gone from the 0.160.1 binary. A marker
//!   that can never match is dead weight, so it is dropped rather than
//!   left to imply coverage it does not provide.
//! * **`"permissions:"` is unreliable as a marker.** It never rendered on
//!   0.145.0 (0/910). 0.160.1 does render a `permissions: YOLO mode` header
//!   row, but its value is the sandbox mode and the row scrolls out with the
//!   banner, so it is still not declared.
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
        // OR-semantics (the app's `agentMarkers.contains { … }`). The banner
        // literal is precise but short-lived; `"›"` prefixes the composer
        // and every user-message line in the transcript, so it survives for
        // the life of the session. `">_ OpenAI Codex"` omits the version so
        // a CLI bump does not silently un-detect the pane (0.160.1 renders
        // `>_ OpenAI Codex (v0.160.1)`, matched by the same prefix).
        agent_markers: vec![">_ OpenAI Codex".into(), "›".into()],
        // Rendered inside the working footer, e.g.
        // `• Working (9s • esc to interrupt)`. Perfectly discriminating in
        // the stability pass — present on 112/112 busy polls, 0/288 idle —
        // and it drops the instant the turn ends, in one contiguous span
        // with no flicker. Unchanged on 0.160.1, which appends background
        // terminal chrome after it (`• Working (6s • esc to interrupt) · 1
        // background terminal running · /ps to view · /stop to close`) and
        // replaces it with `Worked for 9s • 8:59 PM` the poll the turn ends.
        // Identical to Claude's literal by coincidence of both CLIs'
        // phrasing, not by sharing Claude's set.
        busy_markers: vec!["esc to interrupt".into()],
        // Transient (~1 s) and only when the session boots an MCP server, so
        // it is measured-real but often missed by a 0.5 s poll (2/335 polls
        // on 0.160.1, rendered as `• Booting MCP server: codex_apps (0s •
        // esc to interrupt)`). Costless either way: starting and busy both
        // classify as `working`.
        starting_markers: vec!["Booting MCP server:".into()],
        // U+203A. The app scans lines bottom-up, so the live composer wins
        // over the `"› …"` history lines above it — Grok's `❯` collision
        // does not bite here.
        //
        // Caveat worth knowing: Codex renders a rotating *placeholder*
        // ("Improve documentation in @filename" on 0.145.0, "Ask Codex to do
        // anything" on 0.160.1) in the empty composer, and a scrape cannot
        // tell placeholder from typed text. So the app's `promptHasInput`
        // reads a parked composer as "has input" and its
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
        assert_eq!(spec.agent_markers, vec![">_ OpenAI Codex", "›"]);
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

    /// Abbreviated excerpts (placeholder paths, trimmed scrollback) of viewport
    /// polls measured on codex-cli 0.160.1 under the driver's own spawn line
    /// in a 140×45 tmux pane (2026-10-06); the raw captures are not checked in. The
    /// 0.159/0.160 TUI overhaul changed the welcome screen and the footers;
    /// these pin that the declared markers still classify each state.
    #[test]
    fn codex_0_160_1_viewports_classify_as_measured() {
        let spec = spec();
        let agent = |screen: &str| spec.agent_markers.iter().any(|m| screen.contains(m.as_str()));
        let busy = |screen: &str| spec.busy_markers.iter().any(|m| screen.contains(m.as_str()));
        let starting = |screen: &str| spec.starting_markers.iter().any(|m| screen.contains(m.as_str()));

        let fresh = "  >_ OpenAI Codex (v0.160.1)\n     /ws/mono-agent-001\n  permissions: YOLO mode\n\
                       A fresh prompt. An open question.\n› Ask Codex to do anything\n";
        assert!(agent(fresh) && !busy(fresh) && !starting(fresh));

        let booting = "› Run the shell command `echo hello-boss-qual`\n\
                       • Booting MCP server: codex_apps (0s • esc to interrupt)\n› Ask Codex to do anything\n";
        assert!(agent(booting) && busy(booting) && starting(booting));

        let working = "› Run the shell command `sleep 45 && echo late-output`\n\
                       • I’ll run the command and wait for it to finish.\n\
                       • Working (6s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close\n\
                       › Ask Codex to do anything\n  GPT-6-Astra low · /ws/mono-agent-001  ⚠ 1 warning · f2 to view\n";
        assert!(agent(working) && busy(working) && !starting(working));

        let finished = "• Ran echo hello-boss-qual\n  └ hello-boss-qual\n• DONE-ONE\n  Worked for 8s • 8:59 PM\n\
                        › Ask Codex to do anything\n  GPT-6-Astra low · /ws/mono-agent-001\n";
        assert!(agent(finished) && !busy(finished));

        let interrupted = "• I’ll run the command and wait for it to finish.\n\
                           ■ Conversation interrupted - use /feedback if something went wrong\n\
                             1 background terminal running · /ps to view · /stop to close\n› Ask Codex to do anything\n";
        assert!(agent(interrupted) && !busy(interrupted));

        // Mid-turn input is queued, not submitted; the turn stays busy.
        let queued = "• Working (11s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close\n\
                      • Messages to be submitted after next tool call (press esc to interrupt and send immediately)\n\
                        ↳ Reply with exactly: DONE-FIVE\n› Ask Codex to do anything\n";
        assert!(agent(queued) && busy(queued));
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
