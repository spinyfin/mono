# Grok TUI liveness markers under GhosttyKit

- **Date:** 2026-07-27
- **Kind:** empirical GhosttyKit-hosted observation — no Boss production Swift/Rust
- **Pins:**
  - `grok 0.2.112 (9bbd559437aa) [stable]`
  - GhosttyKit prebuilt `ghosttykit-5659cef` (sha256 `82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1`, same as `MODULE.bazel` `@ghostty_kit`)
- **Apparatus:** throwaway AppKit host linking the pinned GhosttyKit (`ghostty_surface_new` / `ghostty_surface_read_text` / SendToPane inject). **Not** standalone Ghostty.app.
- **Isolation:** `GROK_HOME=/tmp/grok-liveness-spike/home` (auth + trusted_folders only). Never the operator's real `~/.grok` as runtime home.
- **Related:** [ghostty-grok-pane-viability.md](./ghostty-grok-pane-viability.md); design T-03 / `PaneMonitorSpec` in [grok-as-a-first-class-interactive-agent-driver.md](../designs/grok-as-a-first-class-interactive-agent-driver.md); Claude scrape literals in `tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift` and `TerminalPaneSession.swift`.

## Why this investigation exists

The Grok driver design deliberately refuses to invent pane-monitor marker strings. A wrong busy marker produces a monitor that is confidently incorrect — worse than today's honest `notDetected`. This doc records the surface substrings that stably indicate, for a Grok TUI in a GhosttyKit pane:

| Field (design `PaneMonitorSpec`) | Meaning                                     |
| -------------------------------- | ------------------------------------------- |
| `agent_markers`                  | Agent is present in the pane                |
| `busy_markers`                   | A turn is in flight                         |
| `starting_markers`               | Session is still starting                   |
| `prompt_prefixes`                | Input prompt line prefix                    |
| `idle_debounce_polls`            | Stable-prompt polls before idle (Claude: 2) |

Markers must be measured for **stability across polls**, not one-off presence. Capture was repeated under each candidate pane mode so the mode recommendation and the marker set are settled together.

## Verdict (read this first)

### Recommended pane mode

**`--no-alt-screen`.**

| Mode                     | Flag              | Busy marker `Esc:cancel`              | Composer `│ ❯`             | Post-`/quit` chrome                                              | Notes                                                                             |
| ------------------------ | ----------------- | ------------------------------------- | -------------------------- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| **no_alt (recommended)** | `--no-alt-screen` | **stable** (59/59 busy polls; 0 idle) | **stable**                 | Retained in viewport (~2.9 KiB residual)                         | Same live chrome as default; better scrollback / post-exit scrape                 |
| default / fullscreen     | _(neither flag)_  | **stable** (same as no_alt)           | **stable**                 | **Torn down** (viewport collapses to ~663 B shell + resume hint) | Alt-screen teardown matches the viability spike warning                           |
| `--minimal`              | `--minimal`       | **absent** (0 hits across entire run) | **absent** (bare `❯` only) | History remains, but busy chrome is weak                         | Experimental scrollback-native UI; **not** suitable for Claude-shaped busy scrape |

`--minimal` is **not** the better pane mode for Boss monitoring. It never rendered `Esc:cancel`, `[stop]`, `Shift+Tab:mode`, or the boxed composer during a multi-poll tool turn under GhosttyKit. Phase classification fell through to "idle" for most of the sleep tool because the busy chrome the other modes expose simply is not there. Prefer `--no-alt-screen` in the spawn command (settles design OQ-5 / T-03 together).

### Recommended `PaneMonitorSpec` field values (for mode `--no-alt-screen`)

Concrete values for the future Grok `AgentDriver::pane_monitor_spec()` (Rust-side only in a later PR — this investigation does not land production code):

```rust
PaneMonitorSpec {
    // OR-semantics, same as Claude's claudeVisible checks.
    // "always-approve" assumes Boss spawn keeps --always-approve (design does).
    // "Shift+Tab:mode" is permission-mode-independent footer chrome.
    // "Grok 4" matches the observed footer "Grok 4.5 (high) · …" without pinning the patch model id.
    agent_markers: vec![
        "Shift+Tab:mode".into(),
        "always-approve".into(),
        "Grok 4".into(),
    ],
    // Primary busy signal: footer affordance present on every busy poll, absent on every idle poll.
    // Secondary: "[stop]" had the same 59/59 busy / 0 idle profile under no_alt.
    busy_markers: vec![
        "Esc:cancel".into(),
        "[stop]".into(),
    ],
    // Prefix matches both "Starting session…" (unicode ellipsis) and any ASCII variant.
    starting_markers: vec![
        "Starting session".into(),
    ],
    // Boxed composer only — NOT bare "❯".
    // Bare U+276F also prefixes historical user-message lines (`     ❯ Use the shell…`),
    // so Claude's "❯"-only prompt finder would treat history as the live prompt after turn 1.
    // The idle composer line trims to a prefix of "│ ❯"; history lines do not.
    prompt_prefixes: vec![
        "│ ❯".into(),
    ],
    // Same as Claude. Esc:cancel drops cleanly on idle; two 0.5 s polls are enough.
    idle_debounce_polls: 2,
}
```

### Claude comparison (reference)

| Role          | Claude (production)                                | Grok (`--no-alt-screen`, this capture)               |
| ------------- | -------------------------------------------------- | ---------------------------------------------------- |
| agent present | `"Claude Code"` / `"auto mode on"` / `"/effort"`   | `"Shift+Tab:mode"` / `"always-approve"` / `"Grok 4"` |
| busy          | `"esc to interrupt"` (case-insensitive)            | `"Esc:cancel"` (exact; also `"[stop]"`)              |
| starting      | `"Accessing workspace:"` / `"Quick safety check:"` | `"Starting session"`                                 |
| prompt prefix | `"❯"` (U+276F)                                     | `"│ ❯"` (boxed; bare `❯` is ambiguous with history)  |
| idle debounce | `2`                                                | `2`                                                  |

## Method

Throwaway host: [`grok-tui-liveness-markers-artifacts/ghosttykit_host/`](./grok-tui-liveness-markers-artifacts/ghosttykit_host/).

Per mode (`SPIKE_PANE_MODE=no_alt|minimal|default`):

1. GhosttyKit surface with `initial_input` launching a shell script.
2. Script runs isolated `grok <mode-flag> --always-approve --trust --session-id <uuid> --cwd …` with a positional prompt that uses the shell tool (`sleep 14`) then replies with a canary.
3. Host polls `ghostty_surface_read_text(VIEWPORT|SCREEN)` every ~0.35 s, writes every changed viewport to `snaps/snap_NNN_tT_phase.txt`, and tallies candidate substrings into start / busy / idle buckets using chrome-based phase classification (not canary tokens alone — those also appear inside the still-running user prompt).
4. After true idle (`Worked for` + no `Esc:cancel`), injects a short follow-up turn, then `/quit`.

Evidence: [`grok-tui-liveness-markers-artifacts/evidence/{no_alt,minimal,default}/`](./grok-tui-liveness-markers-artifacts/evidence/). In-repo `snaps/` holds four representative viewports per mode (starting / busy / idle / post_exit); the full high-frequency series is regenerated locally (see [Appendix: regenerating full snap series](#appendix-regenerating-full-snap-series)). Stability counts in this doc come from the full poll stream (`marker_stability.tsv`, `phases.tsv`, `SUMMARY.txt`), not from the curated snaps alone.

## Per-mode results

### `--no-alt-screen` (recommended)

| Metric                    | Value                                  |
| ------------------------- | -------------------------------------- |
| session                   | `11eb0dd3-9ca9-45b1-8de7-5d7d086bcfb5` |
| polls / snaps             | 84 / 69                                |
| start / busy / idle polls | 11 / 59 / 9                            |
| seed + follow-up canaries | both observed                          |
| `grok_exit`               | 0                                      |

**Stability (hits in bucket / bucket polls):**

| Candidate                                | start | busy      | idle  | Role                                     |
| ---------------------------------------- | ----- | --------- | ----- | ---------------------------------------- |
| `Starting session` / `Starting session…` | 4/11  | 0         | 0     | **starting**                             |
| `Esc:cancel`                             | 0     | **59/59** | **0** | **busy (primary)**                       |
| `[stop]`                                 | 0     | **59/59** | **0** | busy (secondary)                         |
| `Waiting for response`                   | 0     | 14/59     | 0     | early-busy only                          |
| `Ctrl+b:send to bg`                      | 0     | 42/59     | 0     | tool-running busy                        |
| `Shift+Tab:mode`                         | 4+    | 59/59     | 9/9   | **presence**                             |
| `always-approve`                         | 4+    | 59/59     | 9/9   | **presence**                             |
| `Grok 4` / `Grok 4.5`                    | 4+    | 59/59     | 9/9   | **presence**                             |
| `│ ❯`                                    | 4+    | 59/59     | 9/9   | **composer (prompt)**                    |
| `❯` alone                                | 4+    | 59/59     | 9/9   | also on history lines — do not use alone |
| `Worked for`                             | 0     | 0         | 9/9   | idle aftermath (not a busy marker)       |
| Claude's `esc to interrupt`              | 0     | 0         | 0     | **never rendered**                       |

**Representative chrome:**

Starting (snap ~3 s):

```text
    ⠙ Starting session… 0.1s
  ╭─────────────────────────────────────────────╮
  │ ❯                                           │
  ╰──────────────────── Grok 4.5 (high) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+;:queue  │  Ctrl+.:shortcuts
```

Busy, mid tool (snap ~17 s into sleep):

```text
  ┃  ◆ Run Sleep for 14 seconds as requested
    ⠹ Sleep for 14 seconds as requested… 14s    … [↓][stop]
  ╭─────────────────────────────────────────────╮
  │ ❯                                           │
  ╰──────────────────── Grok 4.5 (high) · always-approve ─╯
  Shift+Tab:mode  │  Esc:cancel  │  Ctrl+b:send to bg  │  Ctrl+.:shortcuts
```

Idle after seed:

```text
     LIVE_SEED_DONE
     Worked for 19s
  ╭─────────────────────────────────────────────╮
  │ ❯                                           │
  ╰──────────────────── Grok 4.5 (high) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+.:shortcuts
```

Note: the empty composer box (`│ ❯`) stays on screen during busy turns. **Busy must be decided from `Esc:cancel` / `[stop]`, not from prompt absence.**

### Default (fullscreen / alt-screen)

Live-session markers match `--no-alt-screen` within noise (`Esc:cancel` 59/59 busy, 0 idle; same presence/prompt chrome).

**Difference that matters for Boss:** on `/quit`, alt-screen teardown drops TUI chrome from the viewport (`viewport_final_bytes: 663` vs `2941` under no_alt). The viability spike already warned about this. Prefer inline for scrape continuity.

### `--minimal`

| Observation                                   | Detail                                                                                                             |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `Esc:cancel`                                  | **0 hits** across 80 polls                                                                                         |
| `[stop]`                                      | **0 hits**                                                                                                         |
| `Shift+Tab:mode` / `Ctrl+.:shortcuts` / `│ ❯` | **0 hits**                                                                                                         |
| Presence                                      | `Grok Build  v0.2.112` splash + footer `Grok 4.5 (high) · always-approve`                                          |
| Busy chrome                                   | Only short-lived `Waiting for response…` (11 busy polls); long tool run was mis-bucketed idle by chrome classifier |
| Prompt                                        | Bare `❯` on its own line under the splash, not boxed                                                               |
| Docs                                          | Flag is experimental; finalized blocks print into native scrollback                                                |

**Conclusion:** richer scrollback does **not** compensate for the missing interrupt/busy footer. A monitor wired with `Esc:cancel` would report perpetual idle under `--minimal`. Reject for v1 Boss pane mode.

## Implementation notes for T-15 / the Swift rename

1. **Match exact case for `Esc:cancel`.** Claude's busy check is `localizedCaseInsensitiveContains("esc to interrupt")`. Grok's string is `Esc:cancel` — case-insensitive contains still works if the needle is `esc:cancel`, but do not invent `esc to interrupt` for Grok; it never appears.
2. **Prompt prefix is not Claude-identical.** Keep Claude on `"❯"`; Grok on `"│ ❯"`. Sharing a single `"❯"` prefix would false-positive on Grok history user lines.
3. **`always-approve` in `agent_markers` depends on spawn flags.** The design's execution shape always passes `--always-approve`. If a future spawn drops it, rely on `Shift+Tab:mode` / `Grok 4` instead — both were co-stable here.
4. **Idle debounce 2 is enough** under 0.5 s polling (Claude's cadence). `Esc:cancel` was present on every busy poll and absent on every idle poll; no multi-poll flicker observed.
5. **Surface scrape remains a fallback.** Hooks / `LiveWorkerState` stay authoritative for progress. This marker set only needs to stop lying during the pre-hook window.

## What was deliberately not claimed

- Marker strings under models other than the observed `Grok 4.5` footer (use `"Grok 4"` as a soft prefix, not a hard model pin).
- Busy chrome under fullscreen **vim** mode (docs: Esc does not cancel there; Boss must not enable vim mode).
- Stability across hour-long turns that scroll the tool line out of the viewport — `Esc:cancel` lives in the footer and stayed put for a 14 s tool; long-scroll risk is lower than mid-viewport canaries but unproven for multi-minute tools.
- That `--minimal` can never grow an `Esc:cancel` footer in a future Grok release — re-test if adopting it later.

## Artifacts

| Path                                                                                             | Contents                                                                                                                 |
| ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------ |
| [`grok-tui-liveness-markers-artifacts/PINS.txt`](./grok-tui-liveness-markers-artifacts/PINS.txt) | GhosttyKit + grok pins                                                                                                   |
| [`…/ghosttykit_host/`](./grok-tui-liveness-markers-artifacts/ghosttykit_host/)                   | Throwaway AppKit host source + `run.sh`                                                                                  |
| [`…/evidence/no_alt/`](./grok-tui-liveness-markers-artifacts/evidence/no_alt/)                   | SUMMARY, PINS, phases.tsv, marker_stability.tsv, host.log, screen_final/viewport_final, timeline, 4 representative snaps |
| [`…/evidence/minimal/`](./grok-tui-liveness-markers-artifacts/evidence/minimal/)                 | same structure                                                                                                           |
| [`…/evidence/default/`](./grok-tui-liveness-markers-artifacts/evidence/default/)                 | same structure                                                                                                           |

### Per-mode evidence index (committed)

Each of `evidence/{no_alt,minimal,default}/` keeps:

| File / path            | Role                                                                                          |
| ---------------------- | --------------------------------------------------------------------------------------------- |
| `SUMMARY.txt`          | poll/snap counts, canaries, exit code, pins                                                   |
| `PINS.txt`             | mode-local pin echo                                                                           |
| `phases.tsv`           | phase transitions with wall-clock + viewport size                                             |
| `marker_stability.tsv` | candidate × start/busy/idle hit rates (full poll stream)                                      |
| `host.log`             | host inject / poll log                                                                        |
| `screen_final.txt`     | full screen scrape at end of run                                                              |
| `viewport_final.txt`   | viewport scrape at end of run (post-`/quit` chrome retention difference)                      |
| `timeline.txt`         | wall-clock event timeline                                                                     |
| `pane_script.sh`       | script the surface shell ran                                                                  |
| `snaps/`               | **four** representative viewports only (starting, busy/mid-tool, idle, post_exit) — see below |

**Representative snaps retained (not the full high-frequency series):**

| Mode    | starting                       | busy / mid-tool                                       | idle                              | post_exit                   |
| ------- | ------------------------------ | ----------------------------------------------------- | --------------------------------- | --------------------------- |
| no_alt  | `snap_004_t003.2_starting_tui` | `snap_044_t017.2_busy_seed`                           | `snap_064_t024.2_idle_after_seed` | `snap_069_t028.4_post_exit` |
| default | `snap_004_t001.8_starting_tui` | `snap_036_t013.0_busy_seed`                           | `snap_064_t022.8_idle_after_seed` | `snap_069_t027.0_post_exit` |
| minimal | `snap_003_t002.6_starting_tui` | `snap_036_t014.1_idle_or_early` (mid-tool mis-bucket) | `snap_061_t022.9_idle_after_seed` | `snap_066_t026.7_post_exit` |

For minimal, the mid-tool row is labeled `idle_or_early` because chrome-based classification fell through without `Esc:cancel` — that mis-bucket is itself evidence for rejecting `--minimal`.

Reproduce (also regenerates the full snap series under each mode's `snaps/`):

```sh
# link ghosttykit-5659cef → ghosttykit_host/.local-GhosttyKit.xcframework
export GROK_HOME=/tmp/grok-liveness-spike/home   # isolated home with auth + trust
export SPIKE_CWD=/tmp/grok-liveness-spike/cwd
export SPIKE_PANE_MODE=no_alt                    # or minimal | default
./ghosttykit_host/run.sh
```

## Appendix: regenerating full snap series

The committed tree intentionally omits the ~60–70 high-frequency `snaps/snap_*.txt` dumps per mode (~9k LOC of near-duplicate viewports). Stability arithmetic in the verdict tables was computed over the full poll stream and is preserved in `marker_stability.tsv`, `phases.tsv`, and `SUMMARY.txt`.

To re-materialize the full series for a mode (overwrites that mode's `snaps/` and companion logs under `evidence/<mode>/`):

```sh
cd tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts
# Ensure GhosttyKit pin matches PINS.txt / MODULE.bazel @ghostty_kit
#   ln -sfn /path/to/ghosttykit-5659cef.xcframework ghosttykit_host/.local-GhosttyKit.xcframework
export GROK_HOME=/tmp/grok-liveness-spike/home   # auth + trusted_folders only; never real ~/.grok
export SPIKE_CWD=/tmp/grok-liveness-spike/cwd
for mode in no_alt minimal default; do
  export SPIKE_PANE_MODE=$mode
  ./ghosttykit_host/run.sh
done
```

`run.sh` writes under `evidence/$SPIKE_PANE_MODE/` (including every changed viewport as `snaps/snap_NNN_tT_phase.txt`). After regenerating, re-curate the four representatives if desired, or keep the full series only locally — do not re-commit bulk snaps without a reviewability plan.

Host details: [`ghosttykit_host/README.md`](./grok-tui-liveness-markers-artifacts/ghosttykit_host/README.md).
