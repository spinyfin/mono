# Postmortem: how the Codex driver diverged from the Claude pattern unvalidated

- **Date:** 2026-07-29
- **Kind:** process postmortem — doc-only. No code, design-doc, or work-item changes.
- **Subject:** the "Codex as a first-class agent driver" project — its design doc ([PR #2285](https://github.com/spinyfin/mono/pull/2285)), its spike and investigation writeups, and its 57 work-item rows.
- **Questions asked:** how the divergence from the Claude execution pattern happened, and what mechanism would have caught it sooner.
- **Tree read at:** `main` @ `332cf85c` (2026-07-29). All timestamps below are **America/Chicago** (CDT, UTC−5), converted from GitHub's UTC `mergedAt`.
- **Comparison arms:** the "Grok as a first-class interactive agent driver" project; the "Agent-driver abstraction: decouple Boss from Claude Code" project.
- **Not in scope:** whether to move Codex to the TUI. That question is open and is not relitigated here.

## Verdict

**No one decided to build Codex differently from Claude.** The one-shot, non-interactive execution shape was inherited through three documents, each of which had a locally good reason to carry it, and it was never framed as a choice that needed making. The record contains no rejection of the interactive TUI written before the shape was built — the first written rejection appears on 2026-07-29, three days after the driver shipped, in [PR #2533](https://github.com/spinyfin/mono/pull/2533), and a revision row is already open to correct it.

**The validation that existed was real, careful, and pointed the wrong way.** A genuine empirical spike ran on 2026-07-26 and _did_ exercise the interactive TUI in a real Ghostty pane. It confirmed every property a Claude-shaped Codex driver would have needed. Those results were recorded as characterisation and never routed into a shape decision, because the spike's charter was to settle a dispute _about_ `codex exec`, not to choose between execution modes.

This is not a story of negligence. Every artifact in the chain is unusually rigorous — the design doc re-ran its whole spike across two CLI versions, the pane spike disclosed its own apparatus weaknesses and re-tested on an honest one, and the driver's source comments are candid about what is unvalidated. The failure is structural: **nothing in the process required the execution shape to be an explicit decision, so no amount of care at any single step would have surfaced it.**

## Method

Every claim below is cited to a file at `main` @ `332cf85c`, to a merged PR, or to the project's own work-item record via `boss context`. Where I infer rather than observe, the sentence says so. The work-item record is quoted by row _title_ only; internal ids are deliberately omitted.

I verified each "established fact" supplied in this investigation's brief rather than assuming it. Two came back different from the brief's characterisation, and both are noted inline: the interactive TUI is **not** listed in the design doc's Non-goals, and the Phase-1 acceptance sweep **did** gate the two rows that named it.

---

## 1. Timeline

### Inheritance (before the project existed)

| When             | What                                                                                                                                                                                     | Evidence                                                                                                                                                               |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| earlier          | Claude driver spawns an **interactive** REPL in a pane: `claude --model … "$(cat .claude/initial-prompt.txt)"`, no `--print`. Subsequent turns are typed into the pane.                  | `tools/boss/engine/core/src/transient_recovery.rs:6-8`; `tools/boss/engine/driver/src/claude.rs:486-536`; `tools/boss/protocol/src/wire.rs:1827` (`SendInputToWorker`) |
| 2026-06-01 15:06 | Claude folder-trust pre-seeding lands ([PR #1180](https://github.com/spinyfin/mono/pull/1180)) — later relevant as precedent that first-run trust dialogs are a solved class of problem. | PR #1180                                                                                                                                                               |
| earlier          | The Copilot CLI backend design describes `copilot -p "$(cat …)"` as the "**Equivalent of `claude "$(cat …)"`**".                                                                         | `tools/boss/docs/designs/copilot-cli-as-alternative-worker-backend.md:42,264`                                                                                          |
| earlier          | The agent-driver abstraction design fixes the execution model as a Non-goal: "The execution model stays: **embed the agent CLI in a ghostty terminal pane**."                            | `tools/boss/docs/designs/agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md:19`                                            |

### The project

| When (America/Chicago)   | Event                                                                                                                                                                                                                                                                                                                                                                                                                           | Evidence                                                                                 |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| 2026-07-24               | Design brief written. It asks the author to establish "whether a **headless/non-interactive mode exists** and what it guarantees." It does not ask which of Codex's modes matches Boss's topology.                                                                                                                                                                                                                              | design row description, `boss context`                                                   |
| 2026-07-24 14:56         | Design doc opened as PR #2285.                                                                                                                                                                                                                                                                                                                                                                                                  | PR #2285                                                                                 |
| 2026-07-24 23:14         | "Decide Codex progress channel (stdout JSONL over hooks)" merged ([PR #2343](https://github.com/spinyfin/mono/pull/2343)). Its method is explicitly "driven through `codex exec --json` (the non-interactive mode a driver would actually invoke)".                                                                                                                                                                             | `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md:6`         |
| 2026-07-26 03:03         | Engine-side stdout-JSONL progress reader merged ([PR #2363](https://github.com/spinyfin/mono/pull/2363)).                                                                                                                                                                                                                                                                                                                       | PR #2363                                                                                 |
| 2026-07-26 04:07         | Turn boundary routed through a driver trait method ([PR #2361](https://github.com/spinyfin/mono/pull/2361)).                                                                                                                                                                                                                                                                                                                    | PR #2361                                                                                 |
| 2026-07-26 05:56 → 06:29 | **Pane-viability spike** opened and merged ([PR #2392](https://github.com/spinyfin/mono/pull/2392)). Q3/Q4/Q5 run the **bare interactive TUI** in a real Ghostty window and under a pty master.                                                                                                                                                                                                                                 | `tools/boss/docs/investigations/ghostty-codex-pane-viability.md:396-509`                 |
| 2026-07-26 06:58         | `CodexDriver` skeleton merged ([PR #2404](https://github.com/spinyfin/mono/pull/2404)) — **20 minutes before the design doc it implements**.                                                                                                                                                                                                                                                                                    | PR #2404                                                                                 |
| 2026-07-26 07:18         | **Design doc merged** (PR #2285) as a single 1048-line file, spike amendment already folded in.                                                                                                                                                                                                                                                                                                                                 | `jj diff -r 35bdd6be --stat`                                                             |
| 2026-07-26 07:25 → 09:04 | Auth isolation ([#2405](https://github.com/spinyfin/mono/pull/2405)), hook-trust gate ([#2408](https://github.com/spinyfin/mono/pull/2408)), spawn + owned `CODEX_HOME` ([#2414](https://github.com/spinyfin/mono/pull/2414)). `--json` enters the spawn line here.                                                                                                                                                             | PRs #2405/#2408/#2414                                                                    |
| 2026-07-26 19:19 → 22:10 | Home retention ([#2422](https://github.com/spinyfin/mono/pull/2422)), JSONL normalisation ([#2424](https://github.com/spinyfin/mono/pull/2424)), rollout tail hardening ([#2441](https://github.com/spinyfin/mono/pull/2441)).                                                                                                                                                                                                  | PRs #2422/#2424/#2441                                                                    |
| **2026-07-27 05:21**     | **"Fix three blockers that made every Codex dispatch fail"** ([PR #2447](https://github.com/spinyfin/mono/pull/2447)) — bad config key, missing `--skip-git-repo-check`, `AGENTS.md` written where Codex never reads it. _Every_ dispatch had been dying before the worker's first turn.                                                                                                                                        | PR #2447                                                                                 |
| 2026-07-27 18:12 → 21:17 | Repairs that only a live worker can surface: bazel sandbox permissions ([#2453](https://github.com/spinyfin/mono/pull/2453)), jj store write access ([#2464](https://github.com/spinyfin/mono/pull/2464)), one-shot clean exit misread as pane death ([#2474](https://github.com/spinyfin/mono/pull/2474)).                                                                                                                     | PRs #2453/#2464/#2474                                                                    |
| 2026-07-27               | **Grok design doc written** — same author-facing template, opposite outcome. It names the topology in its title and records an explicit rejection of "drive Grok headless … matching the Codex shape".                                                                                                                                                                                                                          | `tools/boss/docs/designs/grok-as-a-first-class-interactive-agent-driver.md:6,14,557-561` |
| 2026-07-29 02:31         | Pre-trust and config-dir gitignore made driver-supplied ([PR #2498](https://github.com/spinyfin/mono/pull/2498)) — folder trust becomes a generic driver capability.                                                                                                                                                                                                                                                            | PR #2498                                                                                 |
| 2026-07-29 12:32 → 18:49 | The long tail: rollout `exit_code` ([#2509](https://github.com/spinyfin/mono/pull/2509)), sandbox denials as a failure signal ([#2514](https://github.com/spinyfin/mono/pull/2514)), `ProgressFidelity::Rich` split ([#2512](https://github.com/spinyfin/mono/pull/2512)), fatal driver errors ([#2521](https://github.com/spinyfin/mono/pull/2521)), abandoned commands ([#2519](https://github.com/spinyfin/mono/pull/2519)). | those PRs                                                                                |
| 2026-07-29 15:39         | **"Run Codex workers at Claude parity: sandbox behind a flag, dangerous by default"** ([PR #2506](https://github.com/spinyfin/mono/pull/2506)). Codex had been running at "a strictly worse posture than the Claude driver has ever run at".                                                                                                                                                                                    | `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md:682`                     |
| 2026-07-29 18:48         | **`--json` removed from the pane spawn line** ([PR #2532](https://github.com/spinyfin/mono/pull/2532)), ~3 days and ~20 merged PRs after its premise was superseded.                                                                                                                                                                                                                                                            | PR #2532                                                                                 |
| 2026-07-29 18:11         | First written rejection of the interactive TUI ([PR #2533](https://github.com/spinyfin/mono/pull/2533)), still open. A revision row — "Rewrite TUI rejection on complexity grounds, not false impossibility" — is already `active`.                                                                                                                                                                                             | PR #2533; `boss context`                                                                 |

### Where the record is silent

**There is no artifact anywhere in this chain that records a decision to use `codex exec` rather than the interactive TUI.** Not in the design brief, not in the design doc, not in the progress-channel decision, not in the spike. The design doc has a full `## Alternatives considered` section with three entries — hook callbacks, post-hoc guardrails, and `codex app-server` (`codex-as-a-first-class-agent-driver.md:629-651`) — and **the TUI is not one of them.** All three alternatives are about _how to observe or guard `codex exec`_, not about whether `exec` is the right shape.

Correcting the brief's premise: the TUI is also **not** in the doc's Non-goals (`:34-41`). Those name Codex Cloud, `app-server`, `mcp-server` and `remote-control`, and say "v1 drives `codex exec` only" — an assertion of the outcome, with no supporting argument.

An undocumented decision is a finding in its own right. Here the stronger reading is available: the absence is not a lost rationale, it is the absence of a decision.

---

## 2. How we got here — inheritance, not decision

### 2.1 Three inheritance steps, each locally reasonable

**Step 1 — the Copilot design drew a false equivalence, and it was true enough at the time.** `copilot-cli-as-alternative-worker-backend.md:42` puts `-p / --prompt "<text>"` in a table cell whose right-hand column reads "Equivalent of `claude "$(cat …)"`". That equivalence holds for _how the prompt gets in_ and fails for _what the process does afterwards_: `claude "$(cat …)"` seeds turn one of a long-lived REPL; `copilot -p` runs once and exits. Nothing downstream needed the distinction yet, so nothing surfaced it.

**Step 2 — the abstraction froze the container, not the session.** The agent-driver abstraction's Non-goal (`:19`) is "embed the agent CLI in a ghostty terminal pane". A one-shot `codex exec` in a pane satisfies that sentence completely. The properties Boss actually depends on — a session that survives its turn, reads stdin for the whole run, and accepts typed input — are nowhere named as the invariant, even though the engine's transient-recovery reconciler, `SendInputToWorker`, probe/nudge, and Esc-interrupt all rest on them. **The invariant was stated at the level of the container when the load-bearing property was the session lifecycle.**

**Step 3 — the design brief asked a yes/no question about headlessness.** The brief instructs: establish "whether a **headless/non-interactive mode exists** and what it guarantees" (design row description). That is a question with one obvious answer and no comparison in it. The design doc answers it faithfully, in a section titled "### Invocation and headless mode" (`:153`).

### 2.2 The one place a rationale appears, and why it reads as one

`codex-as-a-first-class-agent-driver.md:155` opens the invocation section:

> `codex exec` is a real, first-class non-interactive mode — **not a scraped TUI.**

That clause does the work of an argument without being one. It invokes "scraped TUI" as self-evidently inferior — but **Boss's reference driver is exactly an embedded TUI whose pane the app reads.** The sentence is true about Codex and silently disqualifying about Boss's own production topology. It is the closest thing in the record to a justification for the shape, and it argues against the pattern the project was supposed to match.

Alternative 3's rejection compounds it (`:651`): `app-server` is rejected partly because "it is a fundamentally different execution model from Boss's 'agent CLI in a ghostty pane' (which the … design explicitly holds fixed as a non-goal)" — the abstraction design. The abstraction's container-level invariant is here used to _defend_ `exec` — the one shape that satisfies the letter of the invariant while breaking its substance.

_(Inference, labelled as such: I read the "not a scraped TUI" phrasing as inherited framing rather than a considered position, because no other sentence in 1127 lines defends the shape and because the same author-facing template three days later produced an explicit TUI-vs-headless comparison for Grok.)_

### 2.3 The `--json` thread: a requirement that outlived its premise inside a single commit

The brief asked whether this was drift between revisions. It was not.

`jj diff -r 35bdd6be --stat` shows the design doc landed as **one commit adding 1048 lines**. That single commit contains both:

- `:147` — "`--json` events go to stdout … The JSONL stream is uncontaminated, so the reader needs no filtering." This premise requires an **engine-side stdout reader**.
- `:662` — under pane hosting the engine "**cannot** attach to that stdout", so the selected transport is "the engine-side, run-correlated rollout-file tail (`ProgressIngress::AgentJsonlFile`)".

The pane-viability spike merged at 06:29 and the design doc at 07:18 the same morning; the amendment was **folded into the doc before merge** rather than appended as a revision. That is good doc hygiene and it is exactly what hid the contradiction: **no diff was ever produced that showed "transport changed" next to "spawn line unchanged", so no reviewer and no automated check ever saw the two facts adjacent.**

The consequence shipped and persisted. `CodexDriver::progress_observation_wiring` (`tools/boss/engine/driver/src/codex.rs:1350-1358`) unconditionally returns `AgentJsonlFile`; the `StdoutJsonl` path is reachable but never constructed. `--json` therefore bought nothing from #2414 (2026-07-26 09:04) until #2532 (2026-07-29 18:48) — and cost something visible, because it replaced Codex's human-readable pane transcript with raw JSONL. That is what finally triggered its removal: not a design review, but **a human looking at an ugly pane**.

The design doc named the risk class and missed this instance of it. `:49`: "**The brief's ground-truth section has already drifted** … Treat the line numbers in _this_ doc the same way." Drift was anticipated for _citations_ and not for _requirements_.

The same pattern shows up in the work-item record. The row **"Codex: attach Ghostty pane stdout to engine progress ingress"** is marked `done` — its title states a premise the project had already abandoned in favour of the rollout tail. A row name, like a flag, outlived the thing that motivated it.

---

## 3. Why validation did not catch it

### 3.1 The spike ran the TUI, confirmed it worked, and did not treat that as a finding

This is the most important item in the postmortem, and it is not a story about a spike that was too shallow. It is the opposite.

The spike's charter (`ghostty-codex-pane-viability.md:15-20`) is stated plainly:

> Two careful positions currently contradict each other on an empirical question: **Design claim:** PR #2363 landed an engine-side stdout JSONL reader … **Review claim:** the app owns the pty and the engine only ever receives `shell_pid` … This spike settles that (Q1) and six neighboring execution-shape questions by observation.

**Both sides of the dispute already presupposed `codex exec`.** The spike inherited its scope from a disagreement that lived entirely downstream of the shape choice. It was chartered to settle _how to observe the chosen shape_, and it did that superbly.

But three of its seven questions ran the bare interactive TUI, and every one came back positive:

| Q   | What was run                                                                        | Result                                                                                                                                                                               | Citation   |
| --- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------- |
| Q3  | `codex --no-alt-screen … "<prompt>"` — **interactive TUI, not `exec`**, no keypress | Positional prompt **auto-submits**, "matching the `claude "$(cat prompt.txt)"` shape". Session stays alive after the turn. Rollout `session_meta` shows `"originator": "codex-tui"`. | `:396-433` |
| Q4  | Same, inside a **real Ghostty window**                                              | No alt-screen CSI, scrollback preserved, `TERM=xterm-ghostty`, turn succeeds. Verdict: "**Not a blocker for a TUI-in-pane design (if one were chosen).**"                            | `:437-461` |
| Q5  | TUI with a long turn; Esc at t≈7s, follow-up at t≈12s                               | `turn_aborted` (`reason: "interrupted"`) in the rollout, **process survives**, **session accepts another turn**.                                                                     | `:465-509` |

Those three results are, together, the complete feasibility case for a Claude-shaped Codex driver: seedable by positional prompt, renders correctly in a Ghostty pane, survives its turn, interruptible by Esc, resumable — and **machine-readable**, since the TUI writes the same `rollout-*.jsonl` records the shipped `CodexRolloutProgressSession` already parses. The spike proved this on 2026-07-26, the same morning the design merged.

Three structural features of the writeup kept those results from becoming a decision:

1. **The results are filed as claim-verification, not as options.** The "Design-claims matrix" (`:640-658`) has a `Supports / Refutes` column. Q3/Q4/Q5 each land in it as "**Supports**" — supporting a claim, not favouring a path. The matrix has no column for "and therefore which shape?"
2. **The shape section only asks whether the chosen shape is adequate.** "Immediate decisions this unblocks" §1 is titled "**Codex v1 shape:**" and reads (`:662`): "Non-interactive `codex exec --json` is empirically fine for observation **when** the observer owns stdout, tails the rollout, **or is the GhosttyKit embedder**". It evaluates whether `exec` clears the bar. It never asks whether the TUI clears it _better_. **This is confirmatory validation: the test could return "the chosen path works" or "the chosen path is broken", but never "a different path is better."**
3. **The shape question is not in "Residual unknowns."** That section lists seven honest gaps (`:688-697`) — app→engine IPC, `exec` + Esc, `--ephemeral`, Linux/Windows, rollout rotation, deep scrollback, cost. "Whether the TUI is the better worker shape" is not among them, because it was never open.

Q3's own interpretation shows the circularity most sharply (`:433`): "Drivers that need 'process ends when turn ends' want `codex exec`, not bare `codex`." That requirement is not a Boss requirement — Boss's Claude and Grok drivers explicitly do _not_ have it, and #2474 later had to teach the engine to stop treating a clean one-shot exit as a pane death. **A requirement manufactured by the chosen option was used to reject the alternative.**

### 3.2 The conformance harness pinned the shape's artifacts, not the properties that mattered

PR #2447's own writeup states this better than I could:

> This is the check that would have caught bug 1 — **every existing test in this module pinned the `--json` stream contract, not the config schema.**

The harness was doing real work; it was pointed at the incidental surface of the chosen shape. `--json` was in `CODEX_EXEC_REQUIRED_FLAGS`, so the test suite was actively _enforcing_ the vestigial flag for three days. When #2532 removed it, "two spawn-contract assertions in `pane_spawn.rs` initially failed on the hardcoded `--json` string". **A conformance pin on a superseded requirement converts drift into an enforced invariant.**

### 3.3 The acceptance sweep — it gated exactly what named it, which was two rows

Correcting the brief's framing, because the record is more specific and more interesting than "a gate that did not gate anything."

From `boss context`, the row "Phase-1 acceptance sweep: 10 Codex chores to green PRs" is `blocked`, with:

- **prerequisites (5):** reference-driver conformance harness (`done`), SendToPane guard (`done`), stdout-JSONL + turn boundaries (`done`), attach Ghostty pane stdout to progress ingress (`done`), and **"Codex: exec resume probe and pane-lifecycle semantics spike" (`todo`)** — the one that keeps it blocked.
- **dependents (1):** "Codex eligibility for design, investigation and postmortem kinds" (`blocked`), which in turn blocks "Codex eligibility for review and conflict-resolution kinds" (`blocked`).

So the gate **held**. Phases 2 and 3 never started. The design's stated intent (`:1049-1055`, "a sweep, not an implementation — listed separately and after the work it validates") was honoured exactly.

The problem is where it sits. **A gate placed at the end of Phase 1 can only hold back Phase 2.** It has no authority over Phase 1 itself, and Phase 1 is where all seventeen `done` implementation and repair rows live — including every post-hoc discovery in §1's long tail. "Phase 1 is not yet accepted" was never a state that stopped work; it was a row that had not run. Work kept flowing into Phase 1 for three days after the phase's own acceptance criterion became unreachable, and nothing in the system treats "the acceptance row for the phase you are currently building in is blocked" as a signal at all.

That is the precise finding: **the acceptance criterion was correctly specified, correctly wired, and positioned where it could not do the job people assumed it was doing.**

### 3.4 What the first genuine end-to-end exercise actually was

Eight PRs touching the Codex driver path merged from the skeleton (2026-07-26 06:58) up to #2447 (2026-07-27 05:21): #2404, #2405, #2408, #2414, #2422, #2424, #2432, #2441 — skeleton, auth isolation, hook trust, spawn/provisioning, home retention, progress normalisation, agents-list parity, rollout hardening. Across that entire window, **every `--driver codex` dispatch died before the worker's first turn** (PR #2447: "Every `--driver codex` dispatch died before the worker's turn ever started").

The first exercise that would have caught it was not a Boss dispatch. PR #2447's validation section describes reproducing "the exact production provisioning shape **by hand**" — a workspace with no `.git`, a hand-written `config.toml`, `AGENTS.md` planted at `$CODEX_HOME`. That is a faithful and careful reproduction, and it is still a reproduction. Its three bugs are all _integration_ bugs — jj workspaces have no `.git`; Codex ignores `.codex/AGENTS.md`; a config key was nested wrong — and integration bugs are exactly the class that a hand-built fixture is structurally unable to find, because the fixture is built from the same beliefs that produced the code.

**Eight merged PRs of build-out preceded any attempt to run the thing end to end.** The repairs that followed through 2026-07-29 (bazel sandbox, jj store access, clean-exit-as-death, exit codes, denials, fatal errors, abandoned commands, Claude sandbox parity) are the ordinary consequence of that ordering, not a series of independent oversights.

### 3.5 What kept drifting in the design doc, and why

Five revision rows exist purely to fight staleness: "Amend design for … spike findings", "Remove landed rows and correct stale claims in Codex design doc", "Correct remaining stale G-4 gap-table verdict", "Constrain Codex design freshness pass", "Finish bounded Codex design freshness pass".

Characterising what drifted: the doc is a **gap analysis whose gaps were being closed while it was the reference document**. Its per-capability table, its task breakdown, and its open-questions list are all statements about the _present_ state of the abstraction, and each merged PR falsified a few of them. The doc had no separation between durable content (what Codex is; why each choice was made) and perishable content (which gaps remain open; which rows are landed). Every freshness pass was therefore unbounded by construction — hence two consecutive rows named "**Constrain** Codex design freshness pass" and "**Finish bounded** Codex design freshness pass".

_(Inference: I read the repeated freshness passes as a symptom of mixed durability, not of author inattention — the same doc re-ran its entire empirical basis across two CLI versions rather than bumping a version string, which is the opposite of inattention.)_

---

## 4. What would have caught it

Five mechanisms, ordered by ratio of what-it-would-have-caught to what-it-would-have-cost. Each is traceable to a specific failure above.

### R1 — A rejected alternative must be tested against the existing drivers before rejection

**Rule:** when a design rejects an execution shape, transport, or control mechanism that an existing Boss driver already uses in production, the rejection must name that driver and say why the reasoning does not apply to it.

**What it would have caught:** all three grounds in PR #2533 fail this test immediately. "A persistent multi-turn session with no batch exit" describes the Claude driver (`transient_recovery.rs:6-8`). "Its abort path is a live Esc keystroke, not a signal Boss can send programmatically" describes `InterruptDelivery::PaneEsc`, whose own definition at `driver/src/lib.rs:757-759` reads "**Claude's interactive-TUI path today**" and which Grok ships at `grok.rs:629-631`. "No `--json`/machine-readable mode at all" is refuted by the project's own spike — the TUI's rollout at `ghostty-codex-pane-viability.md:418-427,484-488` carries the records `CodexRolloutProgressSession` parses. The two fallback arguments fail the same way: directory trust is a generic driver-supplied capability (#2498, #1180, Grok's `GROK_FOLDER_TRUST`), and "live-session lifecycle is hard" is not credible when two drivers do it.

**Cost:** a paragraph per rejection, at design-review time. **This is the single highest-value item here** — it is the only one that would have forced the shape question to be asked at all.

### R2 — A new driver must justify each divergence from the reference driver, per capability

**Rule:** a driver's capability declaration carries, for every capability where it differs from `ClaudeDriver`, one sentence naming the divergence and its reason. Reviewable as a table; mechanically checkable as _presence_ (see §5a).

**What it would have caught:** the shape divergence would have appeared as divergences in `mid_turn_pane_input` (`Rejects` vs Claude's `Buffers`), in progress ingress (`AgentJsonlFile` vs `HookCallback`), and in process lifetime — three rows in a table, in one place, in Phase 0. Today those facts are correct but scattered across `codex.rs:1350`, `:1557`, and the design doc.

It would also have surfaced a live inconsistency the current process tolerates: `CodexDriver` declares `ProbeDelivery::PaneText` and `InterruptDelivery::PaneEsc` (`codex.rs:1529-1537`) — **identical to Claude's** — while its own comments say "Esc semantics on non-interactive `codex exec` are unvalidated" and the design doc says Esc is "TUI-only … there is no Esc surface on the exec worker shape this design drives" (`:541`). The comments are honest; the declaration reads as parity. `InterruptDelivery::Unsupported` exists (`lib.rs:762`) and was available. The generalisable gap is that **a capability declaration cannot currently distinguish "I do this" from "I have verified this works"**, so an unvalidated inheritance and a proven implementation are indistinguishable to any reader or check.

**Cost:** one table per driver design, plus a `#[must_use]`-style justification field on divergent declarations.

### R3 — The first end-to-end dispatch is a Phase-0 gate, not a Phase-1 exit criterion

**Rule:** a new driver's first gate is one real Boss-dispatched work item reaching a PR. It sits **before** the second implementation row, not after the last one. A hand-built CLI reproduction does not satisfy it.

**What it would have caught:** all three of #2447's blockers, on 2026-07-26 instead of 2026-07-27, before eight PRs of build-out. Probably also #2453 (bazel), #2464 (jj store), and #2474 (clean exit read as pane death) — every one of which is an integration fact about cube workspaces or the engine's reaper that no fixture reproduces.

**Cost:** genuinely front-loaded. It requires spawn, provisioning, auth, and progress ingress to work before anything else is polished, which is a less comfortable build order. That cost is the point — it is what converts integration risk from a Phase-1 tail into a Phase-0 gate.

### R4 — Phase gates must gate the phase they belong to

**Rule:** a phase's acceptance row blocks _entry to the next phase_ **and** flags the current phase when it becomes unreachable. If the acceptance row for the phase you are working in is `blocked`, new implementation rows in that phase need an explicit override.

**What it would have caught:** not the shape choice — this one is honest about its limits. It would have caught the _accretion_: seventeen `done` rows kept landing into a phase whose acceptance criterion was unreachable because a prerequisite spike ("Codex: exec resume probe and pane-lifecycle semantics spike") was still `todo`. Under this rule, that `todo` becomes visible as a stop signal on day one instead of an inert row.

**Cost:** low — the dependency edges already exist and `boss context` already returns them. This is a reconciler over data Boss has (§5a).

### R5 — Design docs separate durable content from perishable content

**Rule:** a design doc's gap tables, task breakdowns, and open-question lists live in a clearly marked perishable section. Freshness passes are scoped to that section only.

**What it would have caught:** not the shape choice. It would have bounded the five freshness-pass revisions, including the two whose titles are about constraining the pass itself. Filed here because it is cheap and directly traceable, not because it is important.

**Cost:** a section boundary and a convention.

### What I am not recommending

I am not recommending that any of this be resolved by moving Codex to the TUI. That question is genuinely open, the correct answer depends on tradeoffs this postmortem did not evaluate, and the in-flight revision "Rewrite TUI rejection on complexity grounds, not false impossibility" is the right venue for it. **The process finding stands independently of how that question is answered:** even if `codex exec` turns out to be the right shape, it was never chosen, and that is what needs fixing.

---

## 5. Where each recommendation belongs

### (a) Enforceable in code or CI

- **R4** — a reconciler over existing dependency edges: for each `active`/`doing` row, if its phase's acceptance row is `blocked` by an unmet prerequisite, raise an attention item. All inputs are already in the work graph and already surfaced by `boss context`.
- **R2, partially** — a driver-conformance test asserting that every capability where a driver's declaration differs from `ClaudeDriver`'s carries a non-empty justification string. This checks _presence_, not quality; quality is (b). The existing reference-driver conformance harness is the natural home.
- **R2, the validated-vs-declared gap** — a code change (out of scope here): let a control-verb declaration record whether it is verified for this driver's shape or inherited pending validation, so `PaneEsc`-with-an-unvalidated-comment cannot read as parity.
- **Not enforceable:** R1 and R3. R1 requires judging whether an argument applies to another driver. R3 is a sequencing rule about what work is _allowed to start_, which is a coordinator decision, not a CI check.

### (b) Rules for how design docs and projects are structured

- **R1** — add a required subsection to the design-doc convention: _"Divergences from the reference driver, and why the reference driver's approach does not apply."_ Every rejected alternative that an existing driver uses must be named there.
- **R2** — the per-capability divergence table becomes a standard section of any driver design doc.
- **R5** — the durable/perishable split becomes a design-doc convention.
- **A brief-writing rule, from §2.1 step 3.** The design brief asked "whether a headless/non-interactive mode exists". A brief for an integration project should ask **"which of the target's execution modes matches the topology Boss already runs, and what is lost by choosing another?"** A yes/no question about one mode cannot produce a comparison. This is the cheapest single change in this document and it is upstream of everything else.

### (c) Rules that must bind future coordinator sessions

Durable coordinator rules live in the checked-in coordinator prompt — a Swift string literal, `grep bossSystemPrompt`, currently `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`. **This PR does not edit it.** A separate chore is needed to amend it with:

1. **R3 sequencing** — when planning a new driver or backend integration project, the first gate after the skeleton is one real dispatched work item reaching a PR. Do not file a second implementation row until it passes.
2. **R1 at brief-writing time** — when a project integrates a tool that an existing driver already integrates differently, the brief must ask for the comparison explicitly, and must not ask a yes/no question about a single mode.
3. **Spike chartering** — a spike that inherits its questions from a dispute inherits that dispute's premises. When chartering an "execution-shape" spike, state explicitly whether it is _choosing between_ shapes or _validating_ one, and if the latter, name what is being assumed. Had the pane-viability spike carried that sentence, its own Q3/Q4/Q5 results would have been unignorable.
4. **R4 monitoring** — treat "the acceptance row for the active phase is blocked" as a condition to raise with the operator, not as ordinary backlog state.

---

## 6. What generalises beyond Codex

### Grok avoided this, and the reason is mechanical enough to reuse

The Grok project is the control group, and it is a clean one: same author-facing template, same abstraction, three days later, opposite outcome.

**What Grok did differently, in order of how transferable it is:**

1. **The project title names the topology.** "Grok as a first-class **interactive** agent driver" versus "Codex as a first-class agent driver". Naming the topology in the title makes the shape a first-class, contested property of the project rather than an implementation detail — something a reviewer can disagree with before any code exists. _This is the highest-leverage difference and it costs one word._
2. **The rejected shape is an explicit alternative.** `grok-…​.md:557-561`, "**Alternative 1: drive Grok headless (`-p` / `grok agent`), matching the Codex shape**", rejected with a full argument: no Esc surface, no live session, "probe becomes resume-as-new-process with all the pane-lifecycle complexity the Codex project is still carrying", no `AwaitingInputSignal` attachment point. Note the form — the rejection is argued **against a named existing driver's experience**, which is R1 executed by hand.
3. **The doc names the cost of the Codex divergence in its own verdict.** `:14`: "Everything the Codex project had to invent — a transport for pane-hosted progress, a resume-as-new-process probe model, a way to reason about a worker that exits between turns — Grok simply does not need."
4. **It used the Codex doc as a structural template and deliberately did not smooth over disagreement.** `:6`: "This doc mirrors its section list deliberately; where the two reach different conclusions, the difference is called out rather than smoothed over."
5. **It refused to declare an unvalidated capability.** `:486`: `mid_turn_pane_input` "should structurally be `Buffers`… **But the spike's probes were post-turn and post-Esc, not mid-turn.** The default is `Rejects` for exactly this reason … the driver must not declare `Buffers` on a structural argument alone." This is precisely the discipline the Codex `PaneEsc` declaration lacks, applied by the author's judgement rather than by any rule.

**The honest caveat:** Grok's shape was settled by scope before the doc was written — `:561` says "it is a **settled scope decision**, so the rejection is recorded rather than argued", and then argues it anyway. So Grok did not _discover_ the right shape through a better process; it was _given_ the right shape and then documented the reasoning well. That is still the reusable lesson, and it sharpens it: **the difference that mattered was upstream, in how the project was framed and named, not in how well the design was executed.** Both design docs are excellent. Only one was asked the right question.

### The abstraction project has the same latent gap

The agent-driver abstraction is not at fault for the Codex outcome, but its Non-goal (`:19`) is the load-bearing sentence: "The execution model stays: embed the agent CLI in a ghostty terminal pane." **That constraint is satisfiable by a shape that breaks every engine mechanism built on top of it.** The abstraction's own consumers — transient recovery, `SendInputToWorker`, probe/nudge, Esc interrupt, pane-death detection — all assume a session that outlives its turn, and none of them says so.

**Correction (read at `main@19473a983517ba87fb638bbdffbff966e16807fa`):** the claim in this paragraph that nothing in the trait requires a driver to declare its process lifetime, and that Codex declares it only "in prose comments," is false on `main` and recommendation 3 below is already satisfied. `WorkerProcessLifetime` is a real trait method (`tools/boss/engine/driver/src/lib.rs:2004`), with variants `Persistent` (default, `:861`) and `OneTurnPerProcess` (`:874`) declared at `tools/boss/engine/driver/src/lib.rs:851`; the default is asserted by a test at `tools/boss/engine/driver/src/lib.rs:2328`. The Codex driver declares `OneTurnPerProcess` through this trait method, not merely in a comment, at `tools/boss/engine/driver/src/codex.rs:1728-1729`.

The surviving half of the gap is untouched by this correction: nothing rejects a driver whose _declared_ lifetime contradicts the mechanisms the engine actually applies to it (transient recovery, `SendInputToWorker`, probe/nudge, Esc interrupt, pane-death detection all still assume a session that outlives its turn, and the trait declaration alone does not make any of them check it). **A third driver could still declare a lifetime and have the engine ignore the mismatch.** That enforcement gap is the durable fix that belongs to the abstraction project, not to any driver.

---

## 7. Conclusions

1. **There was no decision.** The one-shot shape was inherited across three documents — a Copilot-era equivalence that was true about prompt delivery and false about process lifetime, an abstraction invariant stated at the container level, and a design brief that asked whether a headless mode existed. The design doc's `Alternatives considered` section never contained the TUI. The record is silent because nothing was decided, not because a rationale was lost.
2. **The care was real; the aim was inherited.** The spike, the two-version re-run, the apparatus-honesty revision, and the driver's own source comments are all high-quality work. None of them could surface the problem, because every one of them was chartered downstream of the unmade choice.
3. **Validation confirmed rather than compared.** The spike ran the interactive TUI three times, got three positive results, recorded them as claim-support, and wrote "Not a blocker for a TUI-in-pane design (**if one were chosen**)". That parenthetical is the whole finding.
4. **The acceptance gate worked and was in the wrong place.** It blocked Phases 2 and 3 exactly as designed. It could not touch Phase 1, which is where everything went wrong.
5. **The cheapest fixes are upstream of design.** Name the topology in the project title; ask the brief for a comparison rather than an existence check; require a rejected alternative that an existing driver already uses to be argued against that driver. Grok got all three, by framing rather than by process, and paid none of the cost.

## Follow-up work for the operator to file

Filed as proposals via this run's followups manifest; listed here for the reader.

1. **Amend the coordinator prompt** with the four rules in §5c. Requires editing `bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`; deliberately not done in this PR.
2. **Add the divergence-justification section** to the design-doc convention (§5b), and the per-capability divergence table to the driver-design template.
3. **Already satisfied** — the trait declaration for process lifetime relative to a turn exists (`WorkerProcessLifetime` at `tools/boss/engine/driver/src/lib.rs:851`/`:2004`, declared by Codex at `tools/boss/engine/driver/src/codex.rs:1728-1729`; see the correction in §6). What remains: restate the abstraction's execution-model invariant in terms of session lifecycle rather than pane containment, and make the abstraction _reject_ a driver whose declared lifetime contradicts the mechanisms applied to it.
4. **Add the phase-gate reconciler** (R4) over the dependency edges Boss already stores.
5. **Let capability declarations distinguish verified from inherited-pending-validation**, so `CodexDriver`'s `PaneEsc`/`PaneText` cannot read as parity with Claude while its own comments call the semantics unvalidated.
