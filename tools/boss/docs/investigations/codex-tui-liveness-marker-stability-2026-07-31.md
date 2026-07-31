# Codex TUI liveness marker stability, and the declared `PaneMonitorSpec`

- **Date:** 2026-07-31
- **Kind:** empirical marker-stability capture + the resulting production declaration
- **Pins:** `codex-cli 0.145.0`; pane mode `--no-alt-screen` (the flag `CodexDriver::spawn_invocation` already ships)
- **Isolation:** scratch `CODEX_HOME` under the session scratchpad, seeded with an independent writable byte copy of `auth.json` per [`codex-auth-isolation-2026-07-26.md`](codex-auth-isolation-2026-07-26.md) F4. The operator's `~/.codex` was never used as a runtime home and was not mutated.
- **Related:** [`codex-tui-pivot-pricing-2026-07-30.md`](codex-tui-pivot-pricing-2026-07-30.md) V5 (the verbatim literals), [`grok-tui-liveness-markers-under-ghosttykit.md`](grok-tui-liveness-markers-under-ghosttykit.md) (the method this follows), [`ghostty-codex-pane-viability.md`](ghostty-codex-pane-viability.md)
- **Artifacts:** [`codex-tui-liveness-marker-stability-artifacts/`](codex-tui-liveness-marker-stability-artifacts/)

## The defect this closes

`CodexDriver` declared no `pane_monitor_spec()`, so the spawn RPC carried `pane_monitor: null` and the app fell back to `PaneMonitorSpec.claudeDefault` (`app-macos/Sources/Ghostty/TerminalPaneSession.swift`, `fromWire`). Claude's agent markers are `"Claude Code"`, `"auto mode on"`, `"/effort"` — a Codex pane renders none of them, so **every Codex worker's pane monitor was pinned to `notDetected`**, while Claude's busy marker `"esc to interrupt"` matched Codex verbatim. Detection that can never succeed alongside a busy signal that always matches is precisely the confidently-wrong state the marker discipline exists to prevent.

## Why a stability pass, given V5 already captured the literals

The pivot spike captured Codex's marker _strings_ under GhosttyKit. It did not measure how long each one stays in the **viewport**, and that is the property a scrape depends on: the app reads `GHOSTTY_POINT_VIEWPORT` (`GhosttyTerminalView.readVisibleContents`), i.e. the visible screen, not scrollback. Under `--no-alt-screen` — chosen deliberately so history accumulates (pivot V2) — anything that is not redrawn at the bottom of the screen eventually scrolls away for good.

That turned out to matter. Two of V5's six literals do not survive contact with the stability question, and a spec transcribed straight from the table would have shipped a monitor that goes `notDetected` a few seconds into the first turn.

## Apparatus, and its one honest weakness

Three live `codex` TUI sessions in a tmux pane (120×30), polled with `tmux capture-pane -p` — no `-S`, so each poll is the visible pane, the same read the app performs. 910 polls total. Scripts and representative captures are in the artifacts directory.

**This is a terminal emulator, not GhosttyKit.** The apparatus is therefore weaker than the Grok capture and than pivot V5, and the literals here are _not_ independently re-derived from it — they are V5's GhosttyKit-captured strings, re-observed for persistence. What this run measures is a scrollback/viewport property (does chrome stay on the visible screen?), which is emulator-independent for two VT-compatible emulators, and which pivot V2 already confirmed exists under GhosttyKit specifically (viewport and full-screen reads diverge under `--no-alt-screen`, byte-identical without it). A GhosttyKit-hosted re-run would tighten the evidence; it would have to invert the observed scroll-out for the conclusion to change.

| run | polls | interval | turn shape                                                           |
| --- | ----- | -------- | -------------------------------------------------------------------- |
| 1   | 130   | 0.5 s    | `seq 1 60 && sleep 20` (one tool call, screenful+ of output)         |
| 2   | 400   | 0.25 s   | four commands incl. `seq 1 200` (heavy output)                       |
| 3   | 380   | 0.25 s   | `sleep 30s` only (almost no output) + a mid-turn keystroke injection |

## Results

Full per-run tables in [`runs-summary.txt`](codex-tui-liveness-marker-stability-artifacts/runs-summary.txt).

| literal                      | run 1 (130)            | run 2 (400)              | run 3 (380)              | verdict                              |
| ---------------------------- | ---------------------- | ------------------------ | ------------------------ | ------------------------------------ |
| `>_ OpenAI Codex`            | 23, last poll 23       | 61, last poll 61         | 379 (no scroll)          | scrolls out — cannot carry detection |
| `/model to change`           | 25, last poll 25       | 61, last poll 61         | 379 (no scroll)          | same                                 |
| `permissions:`               | 0                      | 0                        | 0                        | **never rendered** — dropped         |
| `esc to interrupt`           | 25/25 busy, 0/105 idle | 112/112 busy, 0/288 idle | 134/134 busy, 0/246 idle | busy marker, exact                   |
| `Booting MCP server:`        | 0                      | 5 (polls 5–9)            | 3 (polls 3–5)            | real, ~1 s, easily missed            |
| `›`                          | 130/130                | 400/400                  | 379/380                  | **the durable agent marker**         |
| `■ Conversation interrupted` | 0                      | 0                        | 0                        | no interrupt in these runs           |

### 1. The startup banner is not a liveness marker

`>_ OpenAI Codex (v0.145.0)` and `/model to change` live in a boxed header printed once. Under `--no-alt-screen` that box is ordinary scrollback: the last poll either literal appeared in was 25/130 in run 1 (~12 s) and 61/400 in run 2 (~15 s), and neither came back. Run 3 kept it for the whole session only because a bare `sleep 30s` never produced a screenful of output — which is exactly the case a real worker is not. Compare [`run2-poll001-startup-banner.txt`](codex-tui-liveness-marker-stability-artifacts/run2-poll001-startup-banner.txt) with [`run2-poll200-after-scroll-out.txt`](codex-tui-liveness-marker-stability-artifacts/run2-poll200-after-scroll-out.txt).

They stay in the declared set — they are precise, they cost nothing under OR-semantics, and they are the strongest signal during startup — but something else has to hold afterwards.

### 2. `›` is what holds

The composer is redrawn at the bottom of the viewport on every frame, and each submitted user message stays in the transcript prefixed the same way. `›` (U+203A) was present in 909/910 polls — the single miss is run 3's poll 1, before the TUI had painted. It is present during heavy tool output, while busy, and while parked.

Bare `›` is generic, and Grok's capture warned specifically about a bare prompt glyph. That warning was about **prompt** detection: Grok's history lines also start with `❯`, so a bottom-up scan could mistake history for the live prompt. Codex has the same history shape, and the same bottom-up scan resolves it — `promptLine` returns the _last_ matching line, which is always the live composer, because the only thing rendered below it is the model/cwd status line. For **agent** detection the genericity is benign: a false positive means "a Codex composer is on screen" in a pane Boss spawned Codex into.

### 3. `permissions:` never rendered

The header box carried `model:` and `directory:` rows only, in all three runs and at both the startup and post-resolve renders. V5 lists `permissions:` among the header literals; it is not reproducible here, so declaring it would be a guess. Dropped.

### 4. `esc to interrupt` is an exact busy signal, and does not stick

Present on 271/271 busy polls across the three runs and on 0/639 non-busy polls, and each run's busy region is a **single contiguous span** (`(1,25)`, `(5,116)`, `(3,136)`) — no flicker, so a 2-poll idle debounce is enough. It appears inside the working footer, e.g. `• Working (9s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close`.

`Working (` is a strict subset (it lags the footer by a few polls at turn start) and adds nothing, so it is not declared.

Run 3 injected a message into the composer mid-turn to check the one way this marker could go wrong: the mid-turn queued-message affordance V4 observed (`Messages to be submitted after next tool call (press esc to interrupt and send immediately)`) contains the busy literal, and would pin the pane to `working` forever if it persisted into the transcript. It never appeared in this run — `tmux send-keys … Enter` did not queue the message; Codex 0.145 showed `tab to queue message` in the footer and left the text sitting in the composer (still there at poll 380, see [`run3-poll050-composer-with-text.txt`](codex-tui-liveness-marker-stability-artifacts/run3-poll050-composer-with-text.txt)). That is a keystroke-API difference from V4's `ghostty_surface_text` + Return path and **is not evidence against V4's finding** — it means the persistence question for that affordance is unmeasured. What _is_ measured: with no queued message, `esc to interrupt` vanished the instant the turn ended and stayed absent for 244 further polls.

### 5. `Booting MCP server:` is real but blink-and-miss

Observed in runs 2 and 3 for ~1 s at startup, in a scratch home with no `mcp_servers` in `config.toml` (so it comes from account-side app tools, not Boss config), and missed entirely by run 1's 0.5 s cadence. Declared anyway: it is measured, and a miss is free — `starting` and `busy` both classify as `working` in `PaneMonitorTracker.evaluate`.

### 6. Composer placeholder: a caveat for prompt-submit detection

The empty composer renders a rotating placeholder (`› Improve documentation in @filename` in run 1, `› Run /review on my current changes` in run 2), and a screen scrape cannot distinguish placeholder from typed text. So `PaneMonitorTracker.promptHasInput` reads a _parked_ Codex composer as "has input", and the `promptJustSubmitted` heuristic never fires for Codex. Harmless: that heuristic only pre-arms `turnInFlight`, and Codex's busy marker sets it directly on the very next poll.

## Declared spec

Shipped in `engine/driver/src/codex/pane_monitor.rs`, targeting the **bare interactive TUI** — the one shape `CodexDriver` spawns. There is no `codex exec` pane, so no markers are declared for one.

```rust
PaneMonitorSpec {
    agent_markers: vec![">_ OpenAI Codex".into(), "/model to change".into(), "›".into()],
    busy_markers: vec!["esc to interrupt".into()],
    starting_markers: vec!["Booting MCP server:".into()],
    prompt_prefixes: vec!["›".into()],
    idle_debounce_polls: 2,
}
```

The version is stripped from `">_ OpenAI Codex"` so a CLI bump cannot silently un-detect a pane — the same reasoning behind Grok's `"Grok 4"`.

### Driver comparison

| role     | Claude                                        | Grok (`--no-alt-screen`)                     | Codex (`--no-alt-screen`)                  |
| -------- | --------------------------------------------- | -------------------------------------------- | ------------------------------------------ |
| agent    | `Claude Code`, `auto mode on`, `/effort`      | `Shift+Tab:mode`, `always-approve`, `Grok 4` | `>_ OpenAI Codex`, `/model to change`, `›` |
| busy     | `esc to interrupt`                            | `Esc:cancel`, `[stop]`                       | `esc to interrupt`                         |
| starting | `Accessing workspace:`, `Quick safety check:` | `Starting session`                           | `Booting MCP server:`                      |
| prompt   | `❯`                                           | `│ ❯`                                        | `›`                                        |

Codex and Claude share a busy literal because both CLIs happen to phrase the affordance the same way. That is a coincidence of vocabulary, not shared configuration: each set is declared independently by its own driver, and nothing here loosens Claude's markers to accommodate Codex.

## Not measured

- **Interrupt chrome.** `■ Conversation interrupted` (V5, V3) never rendered in these runs — no Esc was sent. It is not a `PaneMonitorSpec` field, so nothing depends on it here.
- **Persistence of the mid-turn queued-message affordance**, per §4.
- **Post-exit viewport.** Codex's retained scrollback keeps `›` history lines, so agent detection stays true after the process exits — the same behaviour Claude's markers have in a retained viewport, and the pane's liveness comes from the pid, not the scrape.
