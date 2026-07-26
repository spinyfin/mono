# Boss-ism leakage: generic regex check in checkleft, adopted by boss and flunge

- **Status:** design — proposed, not yet implemented.
- **Date:** 2026-07-23.
- **Project:** `proj_18c50fb41e3fa880_195` — "Boss-ism leakage: generic regex check in checkleft, adopted by boss and flunge" (product: Boss, `prod_18a1c4016e0cef98_1`).
- **Author:** design-producing run `exec_18c524840d302620_4`.
- **Source investigations:** operator brief (2026-07-23); `bossism-prompt-analysis.md`, `checkleft-wasm-regex-scope.md` (coordinator scratchpad). Line references below verified against `main@origin` (`3aeba8d7`) at author time.

## TL;DR

Boss-internal references (work-item ids like `T3124`, exec ids, cube workspace names, phrases like "the operator") leak into PR titles, PR bodies, commit messages, and code comments. The automated reviewer catches them **after** the fact and forces a full revision cycle for every one — 36% of all review rows carry an `agent_isms` finding, and a single stray `T1234` costs a whole revision. The fix is deterministic gating at **author time**, on two surfaces that no single mechanism can see at once: the engine's editorial PreToolUse hook owns `gh pr|issue` text, and a new generic regex check in checkleft owns the diff (code comments, committed docs) plus the PR description and commit messages at CI time. Both must draw their pattern set from **one** shared definition so author-time and review-time cannot drift.

The single most valuable, cheapest change is already built and switched off: the `editorial_controls` feature flag. Turning it on (staged: observe/audit first, then enforce) covers the 41% PR-title/body half immediately. The checkleft work covers the 59% code-comment/doc half and is the larger build.

## Goals

- Catch Boss-ism leaks **deterministically at author time**, before they reach a PR artifact, so the reviewer's `agent_isms` dimension stops firing and no revision cycle is spent deleting ids from a body.
- Cover **both** leak surfaces without a gap between them: (a) PR title / body / issue-and-comment bodies created via `gh`; (b) code comments, committed docs, commit messages, and the PR description as seen at CI time.
- Build the file/description check in checkleft's **newer wasm-style form** (a bundled component check), config-driven so one check type can be instantiated per repo with per-instance patterns, severity, and messages.
- Make the check reusable by both **boss (mono)** and **flunge**, driven by CHECKS.yaml config rather than embedded logic.
- Keep the author-time regex and the review-time regex **unified on one definition** (`BOSS_ID_RE`) so the two cannot drift.
- Control false positives with checkleft's **existing host exclusion mechanism**, not a new one.

## Non-goals

- **Not** softening the reviewer rubric or making `agent_isms` non-revision-forcing. The reviewer is correct; this project fixes the upstream so the reviewer stops having anything to find.
- **Not** silent engine-side id stripping from PR bodies — that leaves broken sentences and hides the problem. The mechanism blocks and asks the author to fix, it does not rewrite ids away.
- **Not** an `exclude_files` allowlist for design docs (or anything else) as a way to dodge the check. Exclusions are for genuine false positives, scoped narrowly.
- **Not** a prompt-only change. Widening the worker prompt is included but is inert while `editorial_controls` is off and probabilistic even when on; it is strictly secondary to the deterministic gates.
- **Not** a second, independent regex definition. Everything unifies on `BOSS_ID_RE`.
- **Not** per-commit granularity for commit-message checking in v1 (see Chosen approach → Commit messages). Deferred, not forgotten.
- **Not** the flunge `select_base_local` push-gate question. Whether flunge's pinned `alpha.100` carries that fix is a separate concern; it is called out as an investigation task because it changes the urgency of the flunge pin bump, but fixing it is not this project's deliverable.

## Background — measured reality (2026-07-23)

The full quantification lives in the operator brief; the load-bearing facts:

- The preventive layer **already exists and is off.** `editorial_controls` has `default_enabled: false` (`tools/boss/engine/feature-flags/src/lib.rs:96-105`, verified) and is absent from the runtime `feature-flags.toml`. It gates three surfaces at once: the `[editorial-rules]` worker-prompt block (`runner.rs:1518-1525`), the PreToolUse `gh pr|issue` interceptor (`tools/boss/engine/core/src/editorial_hook.rs`), and the editorial evaluator (`tools/boss/engine/editorial/src/lib.rs`). Because it is off, **the worker prompt currently says nothing about Boss-isms** and the reviewer catches ~100% of leaks because nothing upstream runs.
- The reviewer's authority: `tools/boss/engine/pr-review/src/render.rs:637-701`, dimension `agent_isms`, with an escalation clause (`:694-701`) that forces a revision for **any** confirmed finding regardless of severity. That escalation is the volume driver.
- The regex already exists but runs too late: `BOSS_ID_RE = \b[TP]\d+\b` at `tools/boss/engine/core/src/boss_construct_scan.rs:24`, wired at review time only (`runner.rs:809-814`, `:1196-1198`).
- **Leak shapes:** 55% bare `T<n>`/`P<n>`, 15% "the operator"/actor, 15% historical narration, 12% brief/chore/effort-level. **Surface split: 41% PR title/body, 59% code comments and committed docs.**
- **checkleft already carries PR description and commit messages into checks.** The `change-set` record has `commit-description` and `pr-description` (`tools/checkleft/wit/check.wit:100-101`, verified), populated by `attach_description_context` (`main.rs:1593-1620`). Commit messages arrive as the **whole pushed range joined into one string** via `vcs.commit_descriptions_since(base_sha)`. The data is present; the blocker is **scheduling** — `Runner::schedule_runs` iterates changed files only (`runner.rs:1058`), so a check with no matching changed file is never scheduled.
- The wasm mechanism is mature: six bundled wasm checks ship today, and the add-a-check recipe is written down at `tools/checkleft/src/external/bundled.rs:16-33` (verified). `baked_in_block_patterns()` in editorial (`lib.rs:149`, verified) holds phrase patterns today but **not** `[TP]\d+` or "the operator".

## Alternatives considered

### Alternative 1 — Reviewer-only (status quo), or tuning the reviewer

Do nothing new; keep relying on the `agent_isms` reviewer dimension, optionally down-grading its escalation so a stray id no longer forces a full revision.

**Rejected.** This is the FORBIDDEN band-aid: it fixes the symptom (revision cost) by blinding the correct detector, and it still catches every leak _after_ the artifact is public. The operator direction is explicit: prefer deterministic gating at author time over catching downstream. Tuning the reviewer accepts that leaks reach PRs and just makes them cheaper to tolerate.

### Alternative 2 — Engine-side silent stripping of ids from PR bodies

Have the editorial hook rewrite `gh pr create` bodies, deleting any matched id before the call reaches GitHub.

**Rejected.** Deleting `T1234` from "This implements T1234's API" yields "This implements 's API" — a broken sentence, and it hides that the author leaked at all. It also cannot touch code comments or committed docs (59% of leaks), which the hook never sees. The chosen design uses **Block** (deny + ask the author to fix), never a rewrite, for boss-isms.

### Alternative 3 — A new built-in checkleft check (embedded in the binary)

Add the regex scan as a built-in like `typo.rs` / `workflow_run_patterns.rs`, embedded in the checkleft binary.

**Rejected.** The operator plan specifies the newer wasm-style form, and it is the right call: built-ins are compiled into the binary and are not config-instantiable per repo the way wasm checks are, so boss and flunge could not each carry their own pattern set and exclusions cleanly. The wasm path is production-mature and gives per-instance configuration for free.

### Alternative 4 — One mechanism for everything (hook-only, or checkleft-only)

Pick a single surface owner. Hook-only cannot see code comments or committed docs. checkleft-only cannot see PR title/body at the `gh pr create` moment (only later at CI, and never the issue/comment bodies the hook intercepts).

**Rejected.** Neither surface sees everything. The design keeps both and defines a clean division of labour (below) so a leak cannot fall between them, unified on one regex so they cannot drift.

## Chosen approach

Two deterministic gates, one shared pattern definition, staged rollout.

### Division of labour (design decision #2)

| Surface                                      | Owner                                           | When                                     | Covers                                                                                             |
| -------------------------------------------- | ----------------------------------------------- | ---------------------------------------- | -------------------------------------------------------------------------------------------------- |
| PR title, PR body, `gh issue`/comment bodies | Editorial PreToolUse hook (`editorial_hook.rs`) | At `gh pr\|issue create` — before GitHub | The 41% PR-title/body half, plus issue/comment text the diff never contains                        |
| Code comments, committed docs (diff)         | checkleft regex check (file-scoped)             | CI                                       | Bulk of the 59% code-comment/doc half                                                              |
| PR description + commit messages             | checkleft regex check (changeset-scoped)        | CI                                       | Belt-and-suspenders on the PR body (hook is first line); commit-message bodies the hook never sees |

The PR description is deliberately covered by **both** the hook (earliest, at author time) and checkleft (CI backstop, e.g. if the body was edited on GitHub after creation). That overlap is defense-in-depth, not drift — because both consult the same pattern set.

### One shared pattern definition (design decision, anti-drift)

The FORBIDDEN list prohibits a second independent regex. The engine already has `BOSS_ID_RE` (`boss_construct_scan.rs:24`) used at review time. The design **hoists `BOSS_ID_RE` (and the boss-construct phrase set) into a shared lower crate** consumed by both the review-time scan and the author-time editorial evaluator, so those two engine surfaces are provably identical.

checkleft is a different story: its patterns are **config** (CHECKS.yaml strings), consumed in separate repos (mono builds checkleft from source; flunge pins a release), across the wasm boundary. True code-sharing with the engine's Rust constant is not possible. The design therefore treats the canonical pattern **strings** as the single source and mitigates checkleft duplication with (a) a single canonical CHECKS fragment reused by boss and flunge, and (b) a checkleft self-test asserting the fragment's `[TP]\d+` pattern is byte-identical to the engine's `BOSS_ID_RE` source string. This is a weaker guarantee than the in-engine hoist and is flagged as an open question (below).

### The generic regex check (checkleft, wasm)

A new bundled component check, e.g. `text/forbidden-pattern`, authored per `bundled.rs:16-33`: an rlib crate under `tools/checkleft/checks/text/forbidden-pattern/` with a `#[check]` fn, added to the `preinstalled-bundle`'s single `export_checks!` and to `BUNDLED_CHECK_DEFS`. It is **generic** — its config is a list of named patterns, each with a per-instance message and severity; it is not Boss-specific. Config shape (illustrative):

```yaml
- id: no-boss-isms
  check: text/forbidden-pattern
  scope: [files, changeset] # files = diff; changeset = pr-description + commit-descriptions
  patterns:
    - name: boss-work-item-id
      regex: '\b[TP]\d+\b'
      message: "Internal work-item id — remove before publishing."
    - name: operator-actor
      regex: '(?i)\bthe operator\b'
      message: "Internal actor reference — rephrase."
  policy: { severity: error }
  exclude:
    - "**/testdata/**"
```

File reads use plain `std::fs` against the WASI preopen sandbox (the established pattern). False-positive control is the **host exclusion mechanism** (`config.rs:458-465`, `:505-511`) — global `exclude:` accumulating down the CHECKS tree plus per-check `exclude:`, applied via `ExclusionMatcher::filter_changeset` before lowering to the guest. No new FP mechanism is introduced.

### Non-file (changeset) scheduling

To validate the PR description and commit messages, a check must be schedulable **without** a changed-file trigger. The design adds a `scope: changeset` (equivalently `always_run`) config concept and a corresponding branch in `Runner::schedule_runs` (`runner.rs:1049-1176`) that schedules such checks once per run regardless of the changed-file set. Descriptions are already copied into each scheduled run (`runner.rs:1138-1141`), so the guest can read `commit-description`/`pr-description` once scheduled.

**Locationless findings (design decision #4).** A changeset-scoped finding has no natural `file:line`. `Finding.location` is `option<location>` so it is legal, but SARIF and GitHub check-run rendering of a locationless finding is **unverified today** and must be settled before this ships. The design proposes: prefer a **synthetic location** (a virtual path such as `<pr-description>` / `<commit-messages>`) so every finding renders as a normal annotation, falling back to bare locationless only if verification shows GitHub renders it acceptably. This is scoped as an explicit investigation task ahead of the scheduling implementation.

### Commit messages: joined-range string is sufficient for v1 (design decision #3)

Commit messages arrive as **one joined string** for the whole pushed range. Per-commit granularity would require a WIT change and pushes the work to `large`. The design decides the joined string **is sufficient for v1**: the check only needs to detect that a boss-ism is present anywhere in the pushed commit messages and tell the author to fix it; a finding attributed to `<commit-messages>` is actionable. Per-commit line-precise attribution is gilding and is explicitly deferred (`future / not a v1 blocker`).

### Editorial layer: enable, extend, unify, staged rollout (design decision #1)

The editorial machinery is written and disabled — the cheapest change with the largest immediate effect. But "flip the flag" is not the whole of it, and flipping enforcement on a live fleet has real blast radius (an over-matching pattern could **deny** legitimate PR creation). The design stages it:

1. **Extend + unify** `baked_in_block_patterns()` (`editorial/src/lib.rs:149`) to add `\b[TP]\d+\b` (via the hoisted `BOSS_ID_RE`), "the operator"/actor references, "the brief", and chore/revision vocabulary, with disposition **Block** (not Rewrite). Unify on the shared `BOSS_ID_RE` so review-time and author-time cannot drift.
2. **Enable in observe/audit mode first.** Turn on `editorial_controls` and widen the worker-prompt text (the scope line at `runner.rs:1863-1866`, prohibitions after `:1885` in `render_editorial_rules_block`, one just-in-time line in `pr_terminal_directive` at `runner.rs:1953`). In this stage the prompt block is active and the hook **audits** (emits `editorial_actions` events) so the false-positive rate of the new patterns is measured on the live fleet with **zero blocking risk**.
3. **Enforce.** Once audit data shows the patterns do not over-match, confirm the hook's Block path actually denies at the live PreToolUse registration and close any gap. (Note: `editorial_hook.rs` already implements a full `permissionDecision: deny` path with a loop guard at `:82-135`, so the prior editorial design doc's "observe-only" caveat may be stale — this must be **verified**, not assumed, before flipping enforcement.)

The audit trail from stage 2 is the false-positive canary that makes flipping enforcement on a live fleet safe. Blast radius of an over-match is bounded to "a worker's `gh pr create` is denied and it must rephrase"; the loop guard (third deny flips to allow + emits an attention) prevents a worker from being stuck.

### False-positive policy (design decision #5)

- `\b[TP]\d+\b` will match legitimate `T1000`, a bare Rust generic is excluded by the `\d+` requirement, and matrix labels / real ticket ids remain a risk. Mitigation is the **host exclusion mechanism**, scoped narrowly (e.g. test fixtures, files that legitimately cite T-numbers), plus per-instance allowlisting of specific ids in config. Not a new mechanism, not a blanket design-doc exclusion.
- "the operator" has innocent senses (a mathematical or language operator). The design's default is to ship it as an actor-reference pattern but let the human decide **block vs. warn** initially (open question) given the higher innocent-sense rate; severity is per-instance in config, so this is a config choice, not a code change.

### Coordinator id injection (design decision #6)

Ids reach the worker via the coordinator-authored `description` (`runner.rs:3751`, injected at `:1505-1510`). Blocking at the exit while injecting at the entrance is fighting the symptom. **The design's position:** the brief legitimately needs ids — a worker must know which work item it is executing — so removing them entirely would break the worker's ability to reference its own task. The right complementary move is to **mark** injected ids as not-for-repetition (a sentinel or an explicit prompt line that these ids are internal and must never appear in PR/commit/code text), so the prompt and the deterministic gates reinforce each other. Whether to change the coordinator's injection at all is a human decision (open question); it is not a v1 blocker for the gates, which catch leaks regardless of how the id arrived.

## Risks / open questions

1. **Enforcement blast radius.** Flipping `editorial_controls` to enforce on a live fleet could deny legitimate PRs if a pattern over-matches. Mitigated by the observe/audit stage and the loop guard, but the human should land on whether the staged plan is acceptable before enforcement.
2. **Hook enforcement may already be live, or may be observe-only.** `editorial_hook.rs` implements a full deny path, but the prior editorial design doc described observe-only. This must be **verified** before stage 3, not assumed either way.
3. **checkleft ↔ engine pattern duplication.** The in-engine hoist is drift-proof; the checkleft config duplication is not. Is a canonical CHECKS fragment + a byte-identity self-test sufficient, or should we invest in a shared source of truth across the wasm/config boundary?
4. **Locationless finding rendering** in SARIF + GitHub check-runs is unverified. Must be settled (synthetic virtual path vs. bare locationless) before changeset scheduling ships.
5. **"the operator" — block or warn** initially, given innocent senses?
6. **Coordinator id policy** — keep injecting ids (marked not-for-repetition), or stop injecting them into the description at all?
7. **flunge push-gate urgency.** Whether flunge's pinned `alpha.100` carries the `select_base_local` fix (`base.rs:298-317` at mono HEAD) is unverified. If it does not, flunge's push gate is no-opping and the pin bump is more urgent than it looks.

## Proposed implementation task breakdown

Dependency-ordered, PR-sized, single-subsystem. Parallelism is noted per depth. Two independent tracks — **checkleft** (mono, then a release) and **editorial** (mono engine) — can proceed at the same time; adoption tasks join them at the end.

### Depth 0 — may all run in parallel (no dependencies)

**Task A — Generic `text/forbidden-pattern` wasm check (file-scoped).**
Scope: author the generic regex check as a bundled component per `bundled.rs:16-33` — rlib crate under `tools/checkleft/checks/text/forbidden-pattern/`, wired into `preinstalled-bundle`'s `export_checks!` and `BUNDLED_CHECK_DEFS`, plus the manifest. Config = list of named patterns with per-instance message and severity; file-scoped scanning of the diff only; reuse the host exclusion mechanism for FP control. No Boss-specific logic — the check is generic and config-driven.
Effort: `small`. Dependencies: none.

**Task B — Investigation: locationless / changeset-scoped finding rendering.**
Scope: determine how a `Finding` with no `location` (and with a synthetic virtual-path location such as `<pr-description>`) renders in checkleft's SARIF output and GitHub check-run annotations. Produce a concrete recommendation (synthetic path vs. bare locationless) that Task C consumes. No code change beyond a throwaway probe.
Effort: `small`. Dependencies: none. Runs parallel to A, E, J.

**Task E — Unify and extend the editorial pattern set (engine).**
Scope: hoist `BOSS_ID_RE` and the boss-construct phrase set out of `boss_construct_scan.rs` into a shared lower crate consumed by both the review-time scan and the editorial evaluator; extend `baked_in_block_patterns()` (`editorial/src/lib.rs:149`) with `\b[TP]\d+\b` (via the hoisted regex), "the operator"/actor references, "the brief", and chore/revision vocabulary, all disposition **Block**. No behaviour change while the flag is off; this is purely the pattern/plumbing unification.
Effort: `small`–`medium`. Dependencies: none. Runs parallel to A, B, J.

**Task J — Investigation: flunge push-gate + PR-number path on pinned `alpha.100`.**
Scope: verify whether flunge's pinned checkleft `alpha.100` carries the `select_base_local` origin-preferring fix (`base.rs:298-317` at mono HEAD) and whether the `CHECKS_PR_NUMBER` PR-description path works there. Output is a written finding that informs the urgency/ordering of Task K; no code change.
Effort: `small`. Dependencies: none. Runs parallel to A, B, E.

### Depth 1

**Task C — Non-file (changeset) scheduling in checkleft.**
Scope: add the `scope: changeset` / `always_run` config concept, add the branch in `Runner::schedule_runs` (`runner.rs:1049-1176`) that schedules such checks once per run regardless of the changed-file set, and implement finding rendering per Task B's recommendation (synthetic location or verified locationless). Descriptions are already copied into scheduled runs, so no WIT change is needed.
Effort: `medium`. Dependencies: **Task B**. (Independent of A on the host side, but A provides the eventual consumer.)

**Task G — Enable editorial in observe/audit mode + widen worker prompt (engine).**
Scope: turn on `editorial_controls` in the runtime `feature-flags.toml`; widen the worker-prompt editorial text (scope line at `runner.rs:1863-1866`, prohibitions after `:1885` in `render_editorial_rules_block`, one line in `pr_terminal_directive` at `runner.rs:1953`); ensure the hook runs in audit/observe posture so `editorial_actions` events measure the new patterns' FP rate with no blocking. Add the coordinator-side "these ids are internal, do not repeat" marker to injected descriptions **only if** the human chooses to keep injecting ids (open question 6) — otherwise omit.
Effort: `small`. Dependencies: **Task E**. Co-edits `runner.rs`/editorial with E and F — sequence E → G → F; G and F must forward-port E's changes preservingly (integrate, never delete).

### Depth 2

**Task D — Extend `text/forbidden-pattern` to scan PR description + commit messages.**
Scope: extend the guest check from Task A so that when its config `scope` includes `changeset`, it scans `pr-description` and the joined `commit-description` string in addition to (or instead of) the diff, emitting findings with the location convention chosen in B/C. Joined-range commit string only — no per-commit granularity.
Effort: `small`. Dependencies: **Task A, Task C**. (Co-edits the check crate from A — D forward-ports A; sequential by dependency anyway.)

**Task F — Verify and enable editorial hook enforcement (engine).**
Scope: verify whether `editorial_hook.rs`'s `permissionDecision: deny` path (`:82-135`) actually reaches the live PreToolUse registration or is still observe-only as the prior design doc claimed; close any gap so Block findings deny the `gh pr|issue` call at author time, with the existing loop guard intact. Flip from audit posture (Task G) to enforcement only after Task G's audit data shows the patterns do not over-match.
Effort: `medium`. Dependencies: **Task G** (needs audit data + prompt/flag in place). Co-edits editorial with E/G — forward-port preservingly.

### Depth 3

**Task H — Adopt the checks in boss (mono) CHECKS.yaml.**
Scope: instantiate `text/forbidden-pattern` in mono's CHECKS tree with the boss-ism patterns (`[TP]\d+`, "the operator", "the brief", chore/revision vocab), `scope: [files, changeset]`, per-instance severities, and narrow exclusions (test fixtures, files that legitimately cite T-numbers) using the host exclusion mechanism. Include the byte-identity self-test asserting the config `[TP]\d+` matches the engine's `BOSS_ID_RE` source. Config-only; mono builds checkleft from source, so no release is required for mono adoption.
Effort: `small`. Dependencies: **Task A, Task D**. Runs parallel to Task I. Edits a different file (mono CHECKS.yaml) than the flunge adoption (Task L), so no overlap.

**Task I — Cut a checkleft release containing the new checks.**
Scope: publish a checkleft release (from mono) that includes Tasks A, C, and D, so flunge can pin it. Release operation rather than a source PR — represented here as the gating milestone the flunge pin bump waits on.
Effort: `trivial` (release-op). Dependencies: **Task A, Task C, Task D**. Runs parallel to Task H.

### Depth 4

**Task K — Bump flunge's checkleft pin.**
Scope: move `REPOBIN.toml` + `REPOBIN.lock` + `bin/checkleft.lock` together to the release from Task I (recipe at `bin/checkleft.lock:10-24`). All three files must move as one; `.buildkite/scripts/run_checks.sh` prefers repobin and falls back to `bin/checkleft`. Fold in any urgency signal from Task J's push-gate finding.
Effort: `small`. Dependencies: **Task I** (informed by Task J).

### Depth 5

**Task L — Adopt the checks in flunge CHECKS.yaml.**
Scope: instantiate `text/forbidden-pattern` in flunge's CHECKS tree with the same canonical boss-ism pattern fragment and appropriate flunge-local exclusions; confirm the `CHECKS_PR_NUMBER` PR-description path works in flunge CI. Config-only; edits flunge CHECKS.yaml, a different file than Task H, so the two adoption tasks do not overlap.
Effort: `small`. Dependencies: **Task K**.

### Deferred / not a v1 blocker

- **Per-commit granularity for commit-message findings.** Requires a WIT change to carry commits individually; pushes the scheduling work to `large`. `future / not a v1 blocker` — the joined-range string is sufficient for v1.
- **Shared source of truth across the wasm/config boundary** (beyond the canonical fragment + self-test). Only pursue if the self-test proves insufficient in practice. `future / not a v1 blocker`.
- **Coordinator stops injecting ids entirely** (as opposed to marking them not-for-repetition). Depends on open question 6; the gates work regardless. `future / not a v1 blocker`.
