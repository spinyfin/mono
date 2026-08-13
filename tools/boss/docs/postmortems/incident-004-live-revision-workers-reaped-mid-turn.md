# Incident 004 — Engine reaped live revision workers mid-turn, discarding their work

- **Date:** 2026-08-08 (UTC anchors throughout; local wall clocks rendered as **EDT** for readability — see timezone note below). Focal pair on flunge PR #1342 between ~20:23 and ~20:41 UTC (~16:23–16:41 EDT). The same finalization path had been firing since at least **2026-07-23**; a short retained-trace floor early in the investigation was an **investigator access gap**, not the defect's age (§3).
- **Severity:** High — silent work loss on live revision workers, plus false-success board status for revisions that produced nothing. Canonical measurement window **2026-07-30 → 2026-08-09** (9 d 8 h, floor set by when the `pr completion: execution terminalized` discriminator exists): **224 mid-turn reaps across 127 distinct PRs and 223 work items**, of which **33 are confirmed total losses (a floor)**, 151 partial losses, and 40 rows the method structurally cannot classify. `pr_recheck_staged` alone is **96.4% mid-turn** (216/224). **Five work items still sit in review status having contributed nothing** (§4). Includes a case where a revision minted to fix live production regressions was reaped before push while the board reported success.
- **Status:** Documented. Mid-turn guard for the staged path landed as [mono#2685](https://github.com/spinyfin/mono/pull/2685) (merged 2026-08-09); it defers only within the 45-minute `staged_pr_mid_turn_defer_secs` horizon (default 2,700 s). **Follow-through on AI-1 is now answered and the answer is negative:** the landed guard covers the staged arm only — the detector branch of the same function and `stop_satisfied_clean` are untouched, and the detector branch reaped a mid-turn worker on **6 of 6** occurrences over the measured window (§11 AI-1). Further action items in §11 remain open. This postmortem is doc-only.

> **Timezone note.** Engine log and trace records carry **absolute UTC timestamps** (`…Z` / RFC 3339) — they are not wall-clock-local and do not move when the host zone changes. Example: R1's terminalization is `2026-08-08T20:28:26.375916Z` regardless of where the laptop sits. **Local zone labels in this document are presentation only**, derived from those UTC anchors. The host zone is **not** a stable property of the system: the operator moves between timezones, and `/etc/localtime` on this machine has pointed at both America/Chicago and America/New_York during this investigation window. An earlier draft of this postmortem flipped every wall clock between CDT and EDT while arguing about which zone was "the machine's real zone"; that was investigator confusion about presentation, not missing absolute time in the logs. **UTC is ground truth.** Wall clocks below are fixed to **EDT (UTC−4)** for one consistent local reading; re-derive from the UTC anchors if a different local zone is preferred.

- **Class:** Race between merge-poller PR recheck and a still-working agent: a staged PR URL is treated as "worker done," terminalizing and reaping a live mid-turn execution. Related prior: the 2026-07-14 SHA-delta absorption incident whose protection this fast path bypasses.
- **Related:** [`incident-001-pr-fan-out.md`](incident-001-pr-fan-out.md) (wrong-PR finalization killing live workers); 2026-07-14 SHA-delta baseline absorption (guard comment at `completion/recheck.rs:144-175`); introducing change [mono#465](https://github.com/spinyfin/mono/pull/465) (2026-05-13).

## 1. Verdict

The engine has a **fast path that treats a staged PR URL as permission to finalize and tear down a still-running worker**. When a revision worker's tool stream stages a PR URL — including from a push, or from a non-push `gh pr` command such as `edit` or even `view` — the next merge-poller recheck immediately calls `finalize_pr_transition(…, "pr_recheck_staged")`, terminalizes the execution, and reaps the pane while the agent is still mid-turn (`activity: 'working'`).

That fast path short-circuits two protections sitting **below** it in the same function: the `worker_owns_turn_loop` gate, and the SHA-delta arm that explicitly refuses to absorb a possibly in-flight push and defers to the worker's own Stop boundary. Those guards were written after an earlier race of the same class; the staged-URL path was added above them without inheriting them.

Whether any given worker survives is a race against the ~60 s full-sweep cadence. Staging just after a sweep can work; staging into a sweep that is about to run does not. The feature is correct when lucky and destructive when not — and measurement over the canonical window shows the unlucky case is not the exception but the rule: **216 of 224** staged-path finalizations reaped a worker that was still `working` (**96.4%**, §3).

The defect is a property of `recheck_for_pr`'s structure, not of the staged code block alone. The same function's _detector_ branch is **6 working / 0 idle** over the measured window, and a third path (`stop_satisfied_clean`) reaped mid-turn twice. **No arm of that function establishes that the worker is between turns before it terminalizes.**

## 2. Summary

On 2026-08-08, two consecutive revision workers on flunge PR #1342 were reaped by the engine while mid-turn:

| Revision | Execution                  | Driver | Outcome                                                                                                                                                                                                                                                                                                                        |
| -------- | -------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| R1       | `exec_18c9ee2925032810_47` | codex  | Pushed successfully, then reaped **2.4 s** later mid-prompt. Code reached [flunge#1342](https://github.com/brianduff/flunge/pull/1342); post-push steps (including the required findings-status comment) never ran.                                                                                                            |
| R2       | `exec_18c9ee9f79ea3c08_4f` | claude | Reaped **before any push**. Local fix never reached the PR; lease released. A `jj` search of the shared flunge store (including `flunge-agent-001`, the workspace that ran R2) found no unrecovered R2 object; only the later recovery commit `c1e2c6d101bd` is present (see §12). Board still moved the item as review-ready. |

Occurrence is not driver-specific — though severity is (see §3) — nor is it product-specific. The two cases used different drivers and different workspaces. Over the canonical nine-day window the 224 mid-turn reaps split by driver **codex 79 · claude 64 · grok 7** (plus 74 rows that predate the `driver` column), tracking each day's traffic mix rather than any driver's behaviour (§3).

The focal pair is a calibration case, not the incident. Over the **canonical measurement window** (2026-07-30 19:09 EDT → 2026-08-09 03:15 EDT, 9 d 8 h — the floor is when the `pr completion: execution terminalized` discriminator exists, not log retention): **224 mid-turn reaps** across **127 distinct PRs and 223 work items**, with 151 partial losses, **33 confirmed total losses (a floor)**, and 40 rows the method structurally cannot classify. `pr_recheck_staged` is **96.4% mid-turn** (216/224). Five work items are still sitting in review status having contributed nothing; they are listed in §4. Counts were reconstructed from both `engine-trace.jsonl` and the independent `/tmp/boss-engine.log` surface and agree. Full measurement, limits, and labelling are in §3.

## 3. Impact

### R1 — partial loss (the originally reported mild case)

R1 completed its code change and push (`cube pr update` at 16:28:23.94 EDT; commit `6d54316a11b3`, "Harden crawler metadata delivery"). The engine staged the PR URL from the worker progress stream at 16:28:23.972, rechecked via the staged path, terminalized with `source: 'pr_recheck_staged'`, `target: 'PendingReview'`, and tore the worker down with `pane_outcome: 'Reaped'` while `activity: 'working'`.

R1 had completed steps 1–4 of an 8-step revision prompt. Steps 5–8 never ran. In particular, step 6 — post a findings-status summary comment on the PR — never executed. GitHub confirmed PR #1342 had three comments, all bots; the findings-status comment was absent.

Code reached the branch. Required post-push deliverables did not.

### R2 — total loss (the severe case)

R2 was minted for six review findings on `6d54316a11b3`, including two `[high]` production regressions introduced by R1 (inline `style` attributes on `#root` and `<body>` in `frontend/index.html` that survive React mount, boxing the SPA into a 48rem column and forcing a dark background past MUI Joy `CssBaseline`).

R2 committed fixes locally (`jj describe` at 16:40:56 EDT) but never pushed. At 16:40:46.6 a `gh pr edit` had staged a PR URL. The merge-poller sweep at 16:41:04.23 reaped the worker at 16:41:04.58 with `activity: 'working'`. The workspace lease for `flunge-agent-001` was released ~0.4 s later. Engine logs for the ensuing review path reported `pr_review noop skip … skip_reason: 'sha_unchanged', trigger: 'revision_push'` — the head had not moved, because nothing was pushed.

The work item was nonetheless advanced as being in review. The board reported success for a revision that produced nothing, while live production regressions the revision existed to fix remained on the branch.

**R2 local-commit recoverability.** Agents on this machine share product jj stores across cubes; recoverability is not "outside available material" — it is searchable with `jj`. A `jj` search of the shared flunge store (including `flunge-agent-001`, the workspace that ran R2) found no unrecovered R2 object; only the later recovery commit `c1e2c6d101bd` ("Fix layout/theme leak…", landed via `reconcile_revision` follow-up at ~17:14 EDT) is present. **R2's unrecovered local object was not found.** That is "searched and absent," not "not looked at." Whether the object was never durable, was GC'd, or lives only under an unguessable change-id remains open; it should not be restated as inaccessible evidence.

### Measured blast radius

Every figure in this section was counted from engine trace, and each is labelled **determined** (directly counted) or **estimated** (inferred, with the inference stated). The per-driver severity rates are the only substantially estimated figures; everything else here is determined.

#### Canonical observation window — determined

**Floor:** 2026-07-30 19:09:48 EDT — the moment the `pr completion: execution terminalized` line was introduced. Before that the discriminating `activity` record does not exist at all, so earlier staged hits cannot be classified as mid-turn vs idle.

**Ceiling:** 2026-08-09 03:15:47 EDT.

**Span:** **9 d 8 h**, containing **557 PR-completion terminalizations**. Counts were reconstructed from both `engine-trace.jsonl` and the independent pretty-format surface at `/tmp/boss-engine.log` and agree on every finalization-source total that both can see.

Engine restarts observed inside the denser late portion of the window (determined): 08-06 13:47:06, 08-07 18:32:08, 08-07 22:29:21, 08-08 16:03:33 EDT (plus the 08-09 02:26 restart that brought up the landed guard).

#### Forensic surfaces and the log-loss mechanism

An early draft of this document claimed "an app update at ~12:38 on 08-06 reset the log root after three failed engine starts." **All three parts of that claim are wrong**, and the coordinator-side check that AI-8 asked for has now been run.

- There were **twelve** failed engine starts, not three, followed by a thirteenth that stuck: **thirteen starts between 2026-08-06 13:36:25 and 13:47:06 EDT — 10 m 41 s**.
- **No updater deleted anything.** No app-update log-root reset occurred.
- The real mechanism is **rotation eviction**. `engine-trace.jsonl` rotates on _every engine start_ as well as at ~100 MB, against a keep count of **5** (`DEFAULT_TRACE_MAX_FILES` / `BOSS_ENGINE_TRACE_MAX_FILES`). Thirteen starts is thirteen rotations, which evicted every trace file older than 13:36 EDT in under eleven minutes. **A keep count of 5 is almost certainly too low** for a stream that also rotates on every process start — a short restart storm can erase days of forensics in minutes (AI-8 residual / AI-11).

Evidence — thirteen consecutive `boss-engine logging initialized` records, each failed start emitting only that line plus `starting boss-engine runtime` (UTC as recorded):

```
2026-08-06T17:36:25.192550Z  INFO boss-engine logging initialized     <- start 1
2026-08-06T17:36:32.327599Z  <- start 2      2026-08-06T17:36:38.363774Z  <- start 3
2026-08-06T17:36:45.488614Z  <- start 4      2026-08-06T17:36:52.706916Z  <- start 5
2026-08-06T17:37:02.073821Z  <- start 6      2026-08-06T17:37:15.286215Z  <- start 7
2026-08-06T17:37:24.829516Z  <- start 8      2026-08-06T17:37:36.407543Z  <- start 9
2026-08-06T17:38:11.546309Z  <- start 10     2026-08-06T17:39:50.526917Z  <- start 11
2026-08-06T17:42:40.717903Z  <- start 12     2026-08-06T17:47:06.477542Z  <- start 13 (stuck)
```

Size-based rotation is separately evidenced: `engine-trace.jsonl.1786127630` is 105,104,607 bytes and its last record is followed **1 ms later** by the next file's first record, mid-stream, with no shutdown in between.

**A second eviction happened during this investigation.** An early agent's earliest JSONL record at 13:42:40 EDT was start #12 above; the engine restart at **2026-08-09 02:26:49 EDT** minted a sixth rotation and evicted it. That is itself a demonstration of the retention defect.

An **unrotated, append-only copy of the same tracing stream** exists outside Application Support and is how the full window was recovered:

```
/tmp/boss-engine.log   2,090,647,418 bytes   (~2.0 GiB at time of check)
first: 2026-07-23T19:38:16.568363Z = 2026-07-23 15:38:16 EDT
last:  2026-08-09T07:17:38.662080Z = 2026-08-09 03:17:38 EDT
```

It is the same stream as `engine-trace.jsonl` in pretty format. Code path: `main.rs` `open_log_file` opens it with `create(true).append(true)` only — **no size rotation, no start rotation, no prune**. It survives every restart, carries every field §3's analysis depends on (`source`, `activity`, `pane_outcome`, `path`), and at check time held **16 d 11 h** of history. **That is a standing disk-exhaustion defect, not a convenience:** while the engine (and Boss) stay up almost continuously, this file is only ever appended. ~2 GiB already; left alone it will grow without bound until the host disk fills (AI-12). Whether the worker sandbox permits reading it is **untested, and explicitly a guess in either direction** — this document does not assert either way (§12).

Boss's forensic surfaces:

| Surface                | Default path                                | Retention (as measured)                                                                                                                       |
| ---------------------- | ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `engine-trace.jsonl*`  | under `~/Library/Application Support/Boss/` | **Rotates on every engine start as well as at ~100 MB, keep 5** (likely too low — AI-11). A restart storm can evict days of trace in minutes. |
| `/tmp/boss-engine.log` | `/tmp`                                      | Same stream, pretty format, **never rotated**, survives restarts. **~2 GiB and unbounded** (AI-12). Worker readability untested.              |
| `engine-audit.log`     | Application Support root                    | Intact back to **2026-05-08**. Coarser provenance events. Rotates in-place past 2 MiB.                                                        |
| `dispatch-events`      | Application Support root                    | Intact back to **2026-05-11**, but the **wrong surface** for this question — see below.                                                       |

**Log-loss verdict.** Real loss occurred on the JSONL surface, **but nothing this incident needed was lost** — it was recoverable from `/tmp/boss-engine.log`. Those are separate judgements and both belong on the record. As standing reliability defects: (1) rotate-on-start + keep-5 destroys forensics precisely when they are most wanted; (2) the text log has the opposite problem and will eventually fill the disk. As a fact about _this_ postmortem, the material was recoverable and the measurement below was rebuilt from it.

**`dispatch-events` could never have answered this question.** It is a dispatch-stage ledger: it has no finalization `source` and no `activity` field, so it cannot distinguish a mid-turn reap from a clean idle finalization at all. Its long retention is not a substitute for trace.

**Also not loss: the zero-record hours.** Eleven hours in the denser late portion contain no records (2026-08-05 07:00–08:00; 2026-08-08 04:00–11:00 and 12:00–15:00 EDT). This is the signature of **machine sleep** — periodic timers stop dead and resume cleanly, with the engine up throughout and no restart or error at either boundary. That is a **strong inference, not proof**; `pmset -g log` would settle it and has not been run. It is recorded here so nobody re-reads those gaps as further evidence of log destruction.

#### Headline — determined

| finalization source            | `working` | `idle` |
| ------------------------------ | --------- | ------ |
| `pr_recheck_staged`            | **216**   | 8      |
| `pr_recheck` (detector branch) | **6**     | 0      |
| `stop_satisfied_clean`         | **2**     | 10     |
| `stop_staged`                  | 0         | 289    |
| `stop_sha_delta`               | 0         | 21     |
| `metadata_only_fix`            | 0         | 3      |
| `stop_driver_fallback`         | 0         | 1      |
| `stop_satisfied_merged`        | 0         | 1      |

**224 mid-turn reaps** over the canonical window, spread across **127 distinct PRs and 223 work items**. `pr_recheck_staged` alone is **96.4% mid-turn** (216/224). Pairing is unambiguous: every counted execution has exactly one registry-clear record, median execution-to-slot-clear offset **10 ms**, max **4.13 s**.

The `activity` discriminator validates cleanly: Stop-boundary paths are **325/327 idle** (not categorically safe — see `stop_satisfied_clean` below); recheck paths are **222/230 working**. Finalizing at the worker's own Stop boundary usually finds an idle worker; finalizing from a poller recheck finds one mid-turn — exactly as the mechanism in §6 predicts.

**Scope finding — the missing guard is not confined to the staged fast path.** The detector branch of the same function is **6 working / 0 idle** over the window — every detector-branch PR-completion terminalization measured reaped a mid-turn worker (earliest 2026-07-31 16:37:57 EDT, `exec_18c7486b45a667c8_18`; includes a chore on [mono#2678](https://github.com/spinyfin/mono/pull/2678), 08-07 22:57 EDT, grok). The defect is a property of `recheck_for_pr`'s structure — no arm of it establishes that the worker is between turns — not of the one code block quoted in §6.1. A guard applied only to the staged arm leaves the detector branch fully exposed. That is now AI-1's answer, and it is negative — see §11.

##### A third reap path: `stop_satisfied_clean`

Two finalizations came through `stop_satisfied_clean`, both terminalizing to `InReview` with a paired `activity="working"` and `pane_outcome=Reaped`:

```
2026-07-30T23:39:22.314Z  execution_id="exec_18c74070ff560000_25a" source="stop_satisfied_clean" target=InReview elapsed_ms=0   -> 2026-07-30 19:39:22 EDT
2026-07-31T17:27:28.469Z  execution_id="exec_18c77b5458749660_397" source="stop_satisfied_clean" target=InReview elapsed_ms=0   -> 2026-07-31 13:27:28 EDT
```

The Stop family is therefore not categorically safe: **325/327 idle** over the window. **The mechanism behind these two is unestablished at N=2** — `elapsed_ms=0` on both is suggestive but two cases support no conclusion. This is recorded as a **flag, not a finding**, and the guard landed in [mono#2685](https://github.com/spinyfin/mono/pull/2685) does not touch this path either (§11 AI-1).

##### Staged hits before the join floor

**243 staged fast-path hits** occurred between **2026-07-23 16:13 EDT** and the 2026-07-30 19:09 EDT join floor — 116 on 07-24 alone — earliest `exec_18c5043b60af3f48_7b`. Their **activity-at-teardown is permanently unknowable**: the discriminator record did not exist yet. These 243 are an **unknown, not a liftable floor** — they must not be added to the 224, and they must not be assumed to share its 96.4% mid-turn rate. The defect's presence on that surface is confirmed (the staged path was firing); only the mid-turn rate is unmeasurable.

#### Severity — determined

**33 confirmed total losses** (head unmoved — a floor), **151 partial losses** (head moved, turn incomplete), **40 unknown** (no successor execution on the same PR, so the method cannot classify). See the AI-10 inventory below for how severity was assigned and why 33 is a floor.

##### What "partial loss" actually means

For the 151 partial losses, the **primary code deliverable usually survived**: a commit had already been pushed to the PR head before the reap. The major loss mode the board cares about most (an unpublished fix) was avoided — often by lucky timing relative to the ~60 s sweep, not by design.

What was still lost on those runs:

| Artifact class                                                                                             | Typical fate on partial-loss reaps                                                                                                                             |
| ---------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pushed commit / head movement                                                                              | **Survived** (by definition of partial)                                                                                                                        |
| Post-push prompt steps (PR title/body update, findings-status comment, final head confirm)                 | **Lost** — never ran after reap                                                                                                                                |
| Mid-prompt unfinished work after the push (extra commits, follow-up fixes in the same turn)                | **Lost** if not yet pushed                                                                                                                                     |
| Worker-proposed followups / attentions / structured proposals the agent would have filed later in the turn | **Unknown, plausibly lost** — nothing in the engine records "what the agent was about to raise"; a premature reap can skip those side effects with no residual |
| Board / review status                                                                                      | Misleads only mildly for pure partials (head did move); still hides missing post-push checklist items                                                          |

So partial loss is **not** "we lost the code." It is "we lost the rest of the turn" — and some of that rest is operationally load-bearing (findings-status comments, description accuracy) while some is invisible (followups never filed). The retained trace is the only record that pairs each reap's finalization source and activity with its execution; the ordinary execution ledger does not retain those fields or a per-prompt completion marker. **AI-10** therefore required a coordinator-generated per-execution inventory before reinstatement review could be completed. **That inventory has since been delivered** — 224 rows covering the full canonical window — and is summarized next.

One thing AI-10 asked for is **not obtainable**, and the action item was written as though it were. AI-10 requested "any prompt steps demonstrably unreached" per row. **No current surface supports that**: the trace has no per-prompt-step marker of any kind. The only available inference is the coarse one — "reaped at `activity: working`, therefore the turn did not complete" — which says nothing about _which_ step was in flight. This is recorded as a limitation in §12 rather than silently dropped.

##### What "total loss" means, and how bad each case is

Total loss means the execution never moved the PR head. That is **not** uniform catastrophe:

| Severity tier                          | Meaning                                                                                                                                                           | Examples                                                                                                                                                                                                           |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **A — Deterministically re-doable**    | Another agent can re-derive the work from the same brief (merge conflicts, pure metadata). Time and slots are burned; the _content_ is recoverable by re-running. | Merge-conflict revisions on [mono#2321](https://github.com/spinyfin/mono/pull/2321), [mono#2651](https://github.com/spinyfin/mono/pull/2651)                                                                       |
| **B — Content lost until re-authored** | Unique review-response work never pushed; a later agent must redo the reasoning. The parent PR may still be open.                                                 | R2 on [flunge#1342](https://github.com/brianduff/flunge/pull/1342) (later recovered by a follow-up execution, not by restoring R2's object); [mono#2681](https://github.com/spinyfin/mono/pull/2681) four findings |
| **C — False success**                  | Board/status says reviewed while nothing landed; can block or skip real recovery. Orthogonal to A/B and present in every total loss here.                         | All total losses; mono#2651 is the clearest loop                                                                                                                                                                   |

Tier A is still a real incident (wasted capacity, false success), but it is **not** the same as permanent loss of unique work. Categorize losses along these dimensions rather than treating every total loss as equally catastrophic.

Hand-verified total losses still outstanding on the board (the §4 remediation list), by PR:

| PR           | URL                                           | Work                                              | Tier             |
| ------------ | --------------------------------------------- | ------------------------------------------------- | ---------------- |
| mono #2321   | https://github.com/spinyfin/mono/pull/2321    | Merge-conflict revision                           | A                |
| mono #2681   | https://github.com/spinyfin/mono/pull/2681    | Review revision, 4 findings                       | B                |
| mono #2651   | https://github.com/spinyfin/mono/pull/2651    | Three consecutive merge-conflict revisions (×3)   | A (+ C loop)     |
| flunge #1342 | https://github.com/brianduff/flunge/pull/1342 | Review revision, 6 findings — R2 calibration case | B (later redone) |

Two methodology notes belong with these counts, because both are traps for anyone re-running the measurement:

- **`revision_push_capture` is not proof of a push.** One execution staged push evidence from its tool stream _and_ was `sha_unchanged`, with the PR head unmoved. The tool-stream heuristic false-positives; **head-SHA movement is the decisive signal.**
- **One engine `sha_unchanged` verdict was itself wrong.** On [flunge#1327](https://github.com/brianduff/flunge/pull/1327) the engine recorded `sha_unchanged` at 14:00:56 EDT, but a commit with committer date 14:00:40 — 16 seconds earlier — is in that PR, which merged at 14:02:25 with no other execution able to have pushed it. The engine's head read was stale. Classified as partial: GitHub overrode the engine.

One row is genuinely **unresolvable**. A revision on [flunge#1296](https://github.com/brianduff/flunge/pull/1296) routed via `pr_review cycle bound reached`, which skips the SHA check, and the branch has since been force-pushed, so the head at reap time is unrecoverable. It could be an additional total loss. It is reported as unresolvable rather than assigned to either bucket.

##### Severity inventory (AI-10) — determined counts, estimated rate

The delivered AI-10 artifact is a **224-row per-execution inventory** with columns `utc_ts, execution_id, source, pr_url, kind, driver, pr_head_before, next_pr_head_before, verdict`. Severity was assigned by this document's own method — did the PR head move between this execution and the next one on the same PR:

| verdict                          | count  |
| -------------------------------- | ------ |
| partial (head moved)             | 151    |
| **TOTAL-LOSS (head unmoved)**    | **33** |
| unknown (no successor execution) | 40     |

**33 is a floor and the 17.9% rate (33/184 resolvable) is estimated, not determined.** The method cannot classify a reaped execution that was the _last_ execution on its PR — that is the entire 40-row unknown bucket — and that bucket **systematically hides total losses**, because "reaped revision with no successor" is precisely the false-success pattern this incident is about. The bias is one-directional and it is confirmed, not theoretical: [mono#2681](https://github.com/spinyfin/mono/pull/2681)'s `exec_18c9edb88bbdcef0_1a` and [mono#2651](https://github.com/spinyfin/mono/pull/2651)'s `exec_18c9f058a8d82088_79` both land in the unknown bucket, yet both are established total losses in §4 via live GitHub reads. The true total-loss count is therefore somewhere in [33, 73] and the rate is understated.

The method **cross-validates on the focal pair**: it independently classifies R1 as partial and R2 as total loss, matching the hand analysis at the top of §3.

By execution kind, the 224: revision_implementation 175 · chore_implementation 30 · task_implementation 13 · project_design 3 · investigation_implementation 3.

##### The retry-loop pathology is far wider than one PR

§4 documents three consecutive reaped revisions on [mono#2651](https://github.com/spinyfin/mono/pull/2651) as the clearest false-success loop. Across the canonical window, **27 PRs took three or more mid-turn reaps**:

| reaps | PRs                                                                                                                    |
| ----- | ---------------------------------------------------------------------------------------------------------------------- |
| 6     | mono#2621                                                                                                              |
| 5     | mono#2587, mono#2622, mono#2652, mono#2661                                                                             |
| 4     | flunge#1183, mono#2607, mono#2618, flunge#1189, flunge#1191, mono#2644, mono#2662, mono#2651, flunge#1306, flunge#1341 |
| 3     | twelve further PRs                                                                                                     |

The mono#2651 loop in §4 is not the pathological outlier it reads as; it is the median-to-severe case of a pattern affecting 27 PRs.

#### Breakdowns — determined counts

All breakdowns below cover the **224 mid-turn reaps** in the canonical window (all sources).

- **By execution kind:** revision_implementation 175 · chore_implementation 30 · task_implementation 13 · project_design 3 · investigation_implementation 3.
- **By driver:** **codex 79 · claude 64 · grok 7**, plus **74 rows that predate the `driver` column entirely** (populated only from 2026-08-02; 07-31 alone contributes 56 nulls). Daily splits track each day's traffic mix — 08-03 and 08-04 are codex-heavy, 08-06 and 08-07 are claude-heavy, and the reap mix follows in both cases. That **strengthens** the conclusion that occurrence is not driver-specific: the same conclusion holds across a period whose traffic mix inverted. Quote the conclusion; do not quote any single-day driver mix as a characterization of the defect.

Severity by driver (severity-resolvable rows only; rates estimated):

| driver | severity-resolvable | total loss | rate |
| ------ | ------------------- | ---------- | ---- |
| claude | 55                  | 4          | 7.3% |
| codex  | 64                  | 18         | 28%  |
| grok   | 3                   | 2          | 67%  |

Ordering: grok worst, codex middle, claude best. Magnitudes are soft — the grok rate in particular sits on N=3 resolvable inside a method already known to under-count total losses (the 40-row unknown bucket). A plausible mechanism, still stated as a **hypothesis and not a finding**: non-claude drivers surface a `gh pr` URL into the tool stream earlier relative to their push, so the fast path catches them further from a completed turn.

##### Control: was Grok driver immaturity a confound?

Grok as a first-class interactive driver was still under active development during and just before the window. Relevant landed work includes [mono#2522](https://github.com/spinyfin/mono/pull/2522) (2026-07-29) — "grok: capture PR URLs and evaluate structured output for the Grok dialect" — which closed the gap where Grok's Bash tool shape never fed the shared `pr_url_capture` path at all.

Controls applied here:

1. **Occurrence is not Grok-specific.** codex 79 / claude 64 / grok 7 tracks traffic share. The same `pr_recheck_staged` short-circuit fires for mature Claude sessions (including focal R2). A Grok-only driver bug cannot explain the bulk of the 224.
2. **Severity concentration on Grok is _not_ controlled away, but it is soft.** 2 of 3 severity-resolvable grok rows are total losses (67%). That is compatible with (a) the shared engine defect plus unlucky pre-push staging timing, (b) residual Grok dialect bugs that stage a URL earlier or fail to push before staging, or (c) both. With N=3 resolvable this postmortem **cannot** separate (a) from (b).
3. **What would control it properly (not done here):** per-execution reconstruction of "first staged URL command" vs "first successful push" latency by driver, restricted to runs after Grok PR-URL capture was known-good, with transcript-backed command timelines.

Until that study exists, treat Grok's total-loss figure as **confounded and imprecise**: useful as a smoke signal, not as a pure measure of the staged-path defect's severity by driver.

#### Rate over time — determined counts, estimated trend

Daily mid-turn share across the canonical window runs **34–69%**, with no trend. Illustrative late-window days (determined counts):

| day                | mid-turn reaps | all PR-completion terminalizations | share |
| ------------------ | -------------- | ---------------------------------- | ----- |
| 08-06 (from 13:42) | 30             | 60                                 | 50%   |
| 08-07              | 23             | 54                                 | 43%   |
| 08-08 (to 17:06)   | 10             | 15                                 | 73%   |

**The rate is flat and load-driven, not increasing.** Raw daily counts fall only because dispatch volume falls. Normalized, the defect fires on roughly half of all PR-completion terminalizations, and on **96.4% of everything reaching the staged fast path** (216/224). Apparent burstiness is a load artifact: overnight gaps have zero spawns, not zero defects — and the zero-record hours are machine sleep, not a lull in the defect. There is **no correlation with engine restarts** — rates are unchanged across them.

The trend characterization ("flat, load-driven") is **estimated**: it rests on daily points normalized against spawn volume across the nine-day window, with no monotonic trend.

#### Early evidence the landed guard works — small sample, stated as such

The engine now running is the fix: `engine_build_sha="818e8bc6cdf67ae6de53c1b24a3f693a94bc6083"`, the merge commit of [mono#2685](https://github.com/spinyfin/mono/pull/2685), started **2026-08-09 02:26:53 EDT**. Since that restart there are **five deferrals and zero mid-turn reaps**. The deferral does what it says and the worker then finalizes itself at its own Stop boundary four seconds later:

```
2026-08-09T06:32:20.764628Z  INFO pr-recheck: staged PR URL present but worker is mid-turn; deferring finalization to the worker's own Stop boundary  execution_id="exec_18ca0ee3ce239f08_136"   -> 02:32:20 EDT
2026-08-09T06:32:24.787817Z  INFO pr completion: execution terminalized  execution_id="exec_18ca0ee3ce239f08_136" source="stop_staged" target=InReview   -> 02:32:24 EDT
```

The same pattern holds for `exec_18ca00b2f1bda3f8_f8` and `exec_18ca0f7812b2fc38_19`. Over that ~49-minute window, **15 of 15 terminalizations went through Stop-boundary paths and 0 through `pr_recheck_staged`**, against a 34–69% daily mid-turn share beforehand.

Two caveats must be read with that, plainly:

- **49 minutes and 15 finalizations is a thin sample.** It is directionally consistent with the guard working; it is not a measurement of the guard working.
- **The anti-hang bound is untested in production.** No deferral has yet aged out against the 2,700 s (`staged_pr_mid_turn_defer_secs`) horizon, so the behaviour when a genuinely-working worker crosses that horizon has never been exercised outside tests. Zero expiries observed is not evidence the expiry is safe — it is evidence it has not happened yet.

## 4. Remediation list — work items that read as reviewed but contributed nothing

This is the actionable output of the measurement. **Five work items currently claim review status while nothing from their execution ever reached the PR head.** Each has exactly one completed execution and no recovery run in flight. Verified against GitHub 2026-08-08 17:14 EDT; PR states refreshed 2026-08-09. They are identified here by PR (with links) and by task description.

Severity tag: **A** = deterministically re-doable (merge conflict / same brief); **B** = unique review work not landed; **C** = false-success status (all five carry C).

1. **Merge-conflict revision on [mono#2321](https://github.com/spinyfin/mono/pull/2321)** (A+C) — pushed nothing. Head was `b1de4521` at reap; the PR head later moved to `0dec5c0b` by a _different_ work item's execution. This item reads as reviewed and contributed nothing. The conflict work itself is re-doable by another agent; the false success is the lasting damage.
2. **Review revision on [mono#2681](https://github.com/spinyfin/mono/pull/2681)** (B+C) — head `8d2300f5`, **unchanged since 08-07 23:55 EDT** at original verification. Four review findings were **never addressed by any other agent**: as of 2026-08-09 the PR still has only that single head commit, and it merged without a findings-fix commit. The only other execution on that item failed. This is not "another agent picked it up later."
3. **Merge-conflict revision on [mono#2651](https://github.com/spinyfin/mono/pull/2651)** (1 of 3) (A+C) — head `3cded858`, **unchanged since 08-06 14:07 EDT** at original verification; PR still `CONFLICTING` as of 2026-08-09.
4. **Merge-conflict revision on [mono#2651](https://github.com/spinyfin/mono/pull/2651)** (2 of 3) (A+C) — same PR, same unmoved head.
5. **Merge-conflict revision on [mono#2651](https://github.com/spinyfin/mono/pull/2651)** (3 of 3) (A+C) — same PR, same unmoved head.

### mono #2651 is the worst case and the clearest demonstration of the failure mode

Three merge-conflict revisions in quick succession, at 16:52, 17:02 and 17:04 EDT on 08-08. Each was reaped the instant it typed a `gh pr` command. Each was recorded as successful. None of them moved the head.

**The defect manufactures a retry loop that consumes a worker every few minutes and can never converge**, because the recorded outcome of every attempt is success. Nothing in the system can distinguish "the conflict was resolved" from "the resolver was killed before it pushed," so the item is re-minted, re-reaped, and re-recorded as done, indefinitely.

### Recovered without intervention — not outstanding

The flunge #1342 review revision (R2 above) is **not** on the list: it recovered on its own, because `reconcile_revision` spawned a follow-up that pushed successfully. It is mentioned only to establish that a recovery path exists and that it did **not** fire for the five items above. (This says nothing about whether R2's own local commit survived — see §12; the item recovered because later work redid it, not because the lost commit was retrieved.)

**Framing correction: R2 was itself a `reconcile_revision` creation.** Describing R2 as "the original" and the later execution as "the recovery" is slightly off. The dispatch-decision log shows R2's own execution was minted by `reconcile_revision`, and the two later executions were further re-creations _on the same task id_. What happened is not "an attempt, then a recovery" but "a task that `reconcile_revision` re-fired three times until one of them survived long enough to push."

**Why the recovery path fired for one item and not the other five was recorded here as an open question** — and a significant one, because if `reconcile_revision` were reliable, the false-success class would be self-healing. The dispatch-decision log now supplies a **structural difference** between the two cases, though not yet a confirmed mechanism.

flunge#1342's R2 task was re-created twice more **on the same work item**:

```
2026-08-08T20:31:35.942024Z  dispatch_decision: reconcile_revision created a revision_implementation execution (no live or reconcilable execution existed for this task)
    work_item_id=task_18c9ee9f79dff720_4e  execution_id=exec_18c9ee9f79ea3c08_4f   <- R2 itself
2026-08-08T20:58:15.117785Z  ... same work_item_id ...  execution_id=exec_18c9f013d033d518_6d   <- re-creation 1
2026-08-08T20:58:22.440447Z  ... same work_item_id ...  execution_id=exec_18c9f01584aebd40_6f   <- re-creation 2 (this one pushed)
    -> 16:31:35 / 16:58:15 / 16:58:22 EDT
```

mono#2651's three merge-conflict revisions were each a **different, once-only** work item, one execution apiece — so there was never a second attempt on any one of them to reconcile:

```
2026-08-08T20:51:00.993594Z  work_item_id=task_18c9efaebc62d530_62  execution_id=exec_18c9efaebc687698_63
2026-08-08T20:52:28.110629Z  work_item_id=task_18c9efc304f5a548_67  execution_id=exec_18c9efc304fcb1f8_68
2026-08-08T21:03:10.811210Z  work_item_id=task_18c9f058a8cbbca8_78  execution_id=exec_18c9f058a8d82088_79
    -> 16:51:00 / 16:52:28 / 17:03:10 EDT
```

A distinct halting path also exists and is not rare: `reconcile_revision: spawning conflict/CI attempt retired; settled revision to in_review (halting re-dispatch loop) … attempt_status=failed`, **62 occurrences** in the window. Re-dispatch is bounded, and the bound settles the item to `in_review` — which is the same false-success terminal state this incident is about.

**Candidate mechanism — a lead, not a finding.** `reconcile_revision` re-fires only when "no live or reconcilable execution existed for this task." A mid-turn reap terminalizes the execution as _successfully completed_ toward `PendingReview`/`InReview` — which plausibly reads as **reconcilable**, so no recovery execution is minted. On that reading, the defect suppresses its own recovery path by construction, and flunge#1342 recovered only because something else re-fired the same task. **This is explicitly not established.** Confirming it requires reading the `reconcile_revision` predicate in engine source, which was **not** done. Do not cite it as the explanation until that read happens.

## 5. Timeline

### 5.1 Introduction of the bug (causal pre-history)

The defect did not begin on 2026-08-08. It was introduced and then repeatedly layered under without the mid-turn invariant.

| When (UTC / local as noted) | Change                                                                                                                      | What it did relative to this incident                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **2026-05-13**              | [mono#465](https://github.com/spinyfin/mono/pull/465) — _feat(engine): primary-path PR URL capture from worker hook stream_ | **Introducing change.** Workers were stranding because cold-path PR detection (jj + `gh api commits/{sha}/pulls`) kept regressing. The task implemented capture of PR URLs from Bash `PostToolUse` tool output into an in-memory `StagedPrUrlCache`, and taught **both** `on_stop_inner` **and** `recheck_for_pr` to finalize immediately when a URL is staged. The PR body claimed _“the transition still fires on the Stop hook… nothing transitions until Stop”_ while simultaneously shipping a merge-poller test that _requires_ `recheck_for_pr` to finalize from the staged cache without Stop. That contradiction is the root design defect. Breadth (`gh pr create` / `view` / `edit`) was intentional: those commands print the URL on stdout. |
| **2026-05-15**              | [mono#594](https://github.com/spinyfin/mono/pull/594)                                                                       | Tightened capture so _arbitrary_ Bash stdout with a PR URL would not stage; gated on `is_gh_pr_command` for `create\|view\|list\|edit`. Fixed wrong-PR binding; **kept the broad command set** that arms staging from read/metadata commands.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **2026-05-29**              | [mono#959](https://github.com/spinyfin/mono/pull/959)                                                                       | SHA-delta gate for revision stranding — a real contribution check, but on arms **below** the staged short-circuit.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| **2026-07-13**              | [mono#1443](https://github.com/spinyfin/mono/pull/1443)                                                                     | Stopped revisions jumping to in_review at dispatch (`stop_seen`); again does not protect the staged primary path.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| **2026-07-14**              | [mono#1977](https://github.com/spinyfin/mono/pull/1977) + SHA-delta absorption comment                                      | Explicit “do not absorb a possibly in-flight push” lesson written into the SHA-delta arm — the invariant this incident re-violates via the short-circuit above it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| **2026-07-29**              | [mono#2522](https://github.com/spinyfin/mono/pull/2522)                                                                     | Grok gains working PR-URL capture feed — Grok sessions can now arm the same staged path.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **2026-07-23 15:38 EDT**    | Earliest record on the unrotated `/tmp/boss-engine.log` surface                                                             | The real forensic floor (`2026-07-23T19:38:16.568363Z`). The staged fast path is already firing by **16:13 EDT the same day** (243 hits before 07-30). An earlier short JSONL floor (~08-06) was a rotation-eviction artifact, not the defect's age (§3).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| **2026-07-30 19:09 EDT**    | `pr completion: execution terminalized` log line introduced                                                                 | **Join floor for activity-paired analysis.** Before this line exists, a finalization cannot be paired to an `activity` value at all, so the 243 earlier staged hits are permanently unclassifiable (§3). Everything measured as a "mid-turn reap" in this document is bounded below by this date, not by log retention.                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| **2026-08-09**              | [mono#2685](https://github.com/spinyfin/mono/pull/2685)                                                                     | Guard: defer staged-PR-URL recheck finalization while the worker is mid-turn, but only within `staged_pr_mid_turn_defer_secs` (default 45 minutes / 2,700 s). **Remediation for the fast path** (landed after the focal incident).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |

### 5.2 Focal pair — 2026-08-08

Anchors are from engine trace and runtime state. **UTC is ground truth** on every log line; wall clocks below are rendered as **EDT** for a fixed local reading (see the timezone note at the head of this document).

| Time         | Event                                                                                                                                       |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------- |
| 16:13:08     | `48a34a246ae6` committed — original implementation on [flunge#1342](https://github.com/brianduff/flunge/pull/1342)                          |
| 16:20:56     | First automated review mints revision R1                                                                                                    |
| 16:23:07     | R1 starts — `exec_18c9ee2925032810_47`, driver codex, workspace `flunge-agent-011`                                                          |
| 16:25:42     | R1 commit authored                                                                                                                          |
| 16:27:17     | Prior merge-poller sweep (gives R1 **~1.33 s** of runway after staging)                                                                     |
| 16:28:10     | `6d54316a11b3` committed — "Harden crawler metadata delivery"                                                                               |
| 16:28:23.94  | `cube pr update` tool output returns (the push)                                                                                             |
| 16:28:23.972 | `pr_url_capture`: staged PR URL from worker progress stream; `revision_push_capture` staged push evidence                                   |
| 16:28:25.297 | pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector                                                 |
| 16:28:26.376 | PR completion: execution terminalized; teardown in flight — `source: 'pr_recheck_staged'`, `target: 'PendingReview'` (`…T20:28:26.375916Z`) |
| 16:28:26.413 | live-state registry: slot entry cleared — `activity: 'working'`; driver workspace teardown entered (`reason_detail: 'pr_recheck_staged'`)   |
| 16:28:27.784 | worker teardown complete — `path: 'pr_recheck_staged'`, `pane_outcome: 'Reaped'`                                                            |
| 16:28:29     | Automated review of `6d54316a11b3` begins                                                                                                   |
| 16:31:34     | Review completes — 6 findings                                                                                                               |
| 16:31:35     | Revision R2 minted — **by `reconcile_revision`** (`…T20:31:35.942024Z`; see §4)                                                             |
| 16:31:49     | R2 starts — `exec_18c9ee9f79ea3c08_4f`, driver claude, workspace `flunge-agent-001`                                                         |
| 16:40:46.6   | `gh pr edit` output stages the PR URL                                                                                                       |
| 16:40:56     | `jj describe` — work committed locally (never pushed)                                                                                       |
| 16:41:04.23  | Merge-poller sweep                                                                                                                          |
| 16:41:04.58  | Reaped — `activity: 'working'`; never pushed                                                                                                |
| 16:41:05     | Workspace lease released; engine logs `pr_review noop skip … skip_reason: 'sha_unchanged', trigger: 'revision_push'`                        |
| 16:58:15     | `reconcile_revision` re-creates the same work item (`exec_18c9f013d033d518_6d`)                                                             |
| 16:58:22     | `reconcile_revision` re-creates it again (`exec_18c9f01584aebd40_6f`) — this execution pushes the fix                                       |

### A/B contrast the same afternoon (why it looked fine)

| Case                                                            | Staging relative to sweep                                 | Post-push budget used                                | Finalization path   | Activity at teardown |
| --------------------------------------------------------------- | --------------------------------------------------------- | ---------------------------------------------------- | ------------------- | -------------------- |
| [mono#2683](https://github.com/spinyfin/mono/pull/2683)         | staged 16:15:22.88, **0.46 s after** sweep at 16:15:22.42 | findings comment at 16:16:15 — **65.6 s** after push | `stop_staged`       | `idle`               |
| [mono#2682](https://github.com/spinyfin/mono/pull/2682)         | succeeded around 16:20:15                                 | (full window)                                        | correct path        | (completed)          |
| [flunge#1342](https://github.com/brianduff/flunge/pull/1342) R1 | staged into a window with **~1.33 s** to next recheck     | reaped **2.4 s** after push                          | `pr_recheck_staged` | `working`            |
| [flunge#1342](https://github.com/brianduff/flunge/pull/1342) R2 | staged off `gh pr edit` ~17 s before sweep                | reaped before push                                   | `pr_recheck_staged` | `working`            |

The successful #2683 case consumed 65.6 s of a ~66 s window. It was not comfortably inside budget; it barely made it. Survival is phase-dependent on the poller, not a property of the worker doing the right thing.

## 6. Root cause

### 6.0 Causal chain (how this became production behavior)

A postmortem root cause that stops at “the short-circuit is wrong” is incomplete. The chain that produced the short-circuit:

1. **A real product defect created a task.** Cold-path PR binding (workspace `jj log` → `gh api commits/{sha}/pulls` → head match) kept regressing; workers stranded in `waiting_for_input` with unbound PRs. That operational pain justified primary-path capture work ([mono#465](https://github.com/spinyfin/mono/pull/465)).
2. **The feature was implemented as capture _plus_ poller finalization.** Staging from the hook stream is the right recovery idea. Teaching `recheck_for_pr` to call `finalize_pr_transition` as soon as a URL is staged is the defective coupling: capture became permission to kill the worker.
3. **The implementation was deficient relative to its own narrative.** The PR text insisted transitions wait for Stop; the code and tests wired the merge poller to finalize without Stop. Review and tests covered “staged URL binds and detector is skipped,” not “worker still mid-turn must not be reaped.”
4. **No subsequent layer fixed the short-circuit.** [#594](https://github.com/spinyfin/mono/pull/594) narrowed _what_ may stage; [#959](https://github.com/spinyfin/mono/pull/959)/[#1443](https://github.com/spinyfin/mono/pull/1443)/[#1977](https://github.com/spinyfin/mono/pull/1977) added contribution and stop-boundary protections **below** the staged return. The mid-turn invariant never re-applied to the primary path until [mono#2685](https://github.com/spinyfin/mono/pull/2685).

### 6.1 The mechanism — what “staged” means (not git staging)

Verified against this checkout.

The line references and snippets are against base commit `f58448d7`, which predates the [mono#2685](https://github.com/spinyfin/mono/pull/2685) guard; they show the defect as it existed during the incident, not current main.

**“Staged” here is not `git add` / `jj` staging.** It is an in-memory engine cache (`StagedPrUrlCache`: `execution_id → pr_url`). Population path:

1. The worker runs a shell tool (`Bash` / `run_terminal_command`) whose command matches `is_gh_pr_command_str` — today: `cube pr create|update|ensure`, or `gh pr create|view|list|edit`.
2. The tool’s stdout/stderr (driver-normalized) contains a canonical `https://github.com/<owner>/<repo>/pull/<N>` URL.
3. On the engine’s `PostToolUse` hook path, `pr_url_capture` extracts that URL and **records it against the live execution** (“stages” it). First write wins so a later `gh pr view` of another PR cannot clobber the worker’s own create URL.
4. Separately, some commands also contribute _push evidence_ (`revision_push_capture`) when the tool stream looks like a push — that heuristic is **not** authoritative (see §3).

**What triggers the reap (core confusion resolved):** not “a commit was pushed.” The merge poller runs ~every 60 s, calls `recheck_for_pr`, sees a staged URL for a still-live execution, and **immediately** finalizes + tears down. R2 never pushed; the trigger was **`gh pr edit` printing the PR URL**, which armed the cache ~17 s before the next sweep. R1 _had_ pushed via `cube pr update`, but the reap still came from the same staged-URL fast path, 2.4 s after that tool returned — not from a Stop event.

In `recheck_for_pr` (`tools/boss/engine/core/src/completion/recheck.rs`), when a PR URL has already been staged from the worker's tool-output stream, the function **immediately** finalizes:

```55:72:tools/boss/engine/core/src/completion/recheck.rs
        if let Some(staged_url) = self
            .verified_staged_pr_url(execution_id, &execution, "pr-recheck")
            .await
        {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
            return self
                .finalize_pr_transition(
                    execution_id,
                    staged_url,
                    WorkerPrCompletionTarget::InReview,
                    "pr_recheck_staged",
                )
                .await;
        }
```

That return short-circuits everything below it, including:

1. **`worker_owns_turn_loop` gate** (`recheck.rs:81-88`) — intended to keep the poller's candidate path off executions that should not be finalized by recheck:

```81:88:tools/boss/engine/core/src/completion/recheck.rs
        if !super::worker_owns_turn_loop(&execution) {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                kind = %execution.kind,
                "pr-recheck: skipping fallback — execution does not own a live worker turn loop",
            );
            return StopOutcome::RunningNoStagedPr;
        }
```

2. **SHA-delta arm protection** (`recheck.rs:144-175`) — written after a 2026-07-14 incident, with an explicit comment that a poller sweep must **not** absorb a possibly in-flight push and must defer to the worker's own Stop boundary:

```144:175:tools/boss/engine/core/src/completion/recheck.rs
                // Head moved but revision_stop_contributed_head doesn't
                // match (or was never set). This could be a genuine
                // parent-worker push — OR the revision's own worker is
                // still actively running and simply hasn't reached its own
                // Stop boundary yet: ...
                // Do NOT mutate `pr_head_before` here —
                // 2026-07-14 incident (exec_18c2124d2f06d768_106d):
                // a poller sweep raced a live worker's in-flight push,
                // absorbed the just-pushed head as the new baseline here,
                // ...
                // Leave the baseline untouched and defer to the worker's
                // own next Stop; ...
                tracing::debug!(
                    ...
                    "pr-recheck: revision Contributed unattributed — deferring to the worker's \
                     own Stop boundary rather than absorbing a possibly-in-flight push as baseline",
                );
                return StopOutcome::AwaitingInput;
```

The hazard the SHA-delta comment describes — poller races a live worker mid-session — is exactly the hazard the staged-URL fast path walks into by finalizing and tearing down.

### 6.2 `worker_owns_turn_loop` would not have been enough even if reached

```2113:2115:tools/boss/engine/core/src/completion.rs
pub(crate) fn worker_owns_turn_loop(execution: &crate::work::WorkExecution) -> bool {
    ExecutionStatus::is_live(&execution.status) && execution.kind != ExecutionKind::PrReview
}
```

It admits any live non-reviewer execution. It has **no notion of mid-turn vs idle**. A revision that is actively tooling (`activity: 'working'`) still "owns" the turn loop. Reaching this gate would not have stopped R1 or R2; the staged fast path simply never reaches it.

(Note on the recheck gate's polarity: when `worker_owns_turn_loop` is **false**, recheck skips. Live revision workers return **true**, so they would proceed into later arms. The gate excludes non-live and reviewer executions; it does not protect mid-turn producers.)

### 6.3 Staging is armed far more broadly than "the worker pushed"

```304:324:tools/boss/engine/core/src/pr_url_capture.rs
pub fn is_gh_pr_command_str(command: &str) -> bool {
    if command.contains("cube pr create") || command.contains("cube pr update") || command.contains("cube pr ensure") {
        return true;
    }
    let command = peel_shell_c_payload(command).unwrap_or(command);
    matches!(
        classify(command),
        Some(inv)
            if inv.noun == GhNoun::Pr
                && matches!(inv.subcommand.as_str(), "create" | "view" | "list" | "edit")
    )
}
```

Accepted commands:

- Write / push wrappers: `cube pr create|update|ensure`, `gh pr create`
- Metadata / read: `gh pr edit`, `gh pr view`, `gh pr list`

R2 was reaped off a **`gh pr edit`**. A read-only **`gh pr view`** would arm the same path. Staging therefore fires well before "the worker has delivered its push and finished its prompt."

##### Why the command set is this broad (and why that is terrible)

This is not an accident of one careless regex. The introducing PR ([mono#465](https://github.com/spinyfin/mono/pull/465)) **explicitly** listed `gh pr create` / `view` / `edit` as the stdout sources that print a PR URL, and designed capture around that set so the engine would stop depending on jj/GitHub reconstruction. [mono#594](https://github.com/spinyfin/mono/pull/594) then _kept_ `view|list|edit` when it added `is_gh_pr_command`, because the immediate incident it fixed was _wrong_ URLs staged from non-`gh pr` shell output (chore descriptions, `bossctl` dumps) — the fix was “only deliberate `gh pr …` commands,” not “only commands that mean the worker is done.”

**Transcript access for the introducing agent:** Boss's read-only worker `boss` CLI (`boss task`, `boss pr`, `boss cost`, `boss context`, …) has **no verb that loads a historical execution transcript** by exec id or PR. Transcript files live under coordinator paths (`~/.claude/projects/…`, Grok session dirs) described in `tools/boss/docs/forensic-surfaces.md`, which workers must not open under Application Support, and which are not exposed via `boss engine …` here. **The introducing agent’s private transcript cannot therefore be quoted.** The public PR body and commit message for #465 _are_ available and are sufficient to explain the breadth choice above. If the coordinator needs agent-turn-level detail beyond that, the forensic-surfaces / transcript-CLI gap must be addressed separately.

Coupling “any URL-printing `gh pr` command” to “finalize and reap” should not have survived review. Capture breadth optimized for binding recovery is the wrong breadth for a path that kills the worker.

### 6.4 Finalization tears down the live worker

`finalize_pr_transition` (`completion/pr_transition.rs`) records completion, may map `InReview` → `PendingReview` when a reviewer is enqueued (`pr_transition.rs:226-230`), logs `pr completion: execution terminalized; teardown in flight` (`:255-260`), then calls `finish_worker_teardown` (`:279-286`), which releases the pane (`pane_outcome` includes `Reaped`) and frees the cube lease (`teardown.rs:86-156`).

The live-state registry clears the slot and logs `activity` at clear time (`live_worker_state.rs:654-662`) — the incidental signal that the worker was still `working` rather than idle.

### 6.5 Race against the merge-poller sweep

- **Configured full-sweep interval:** 60 seconds — `tools/boss/engine/core/src/app/server.rs:1059` (`Duration::from_secs(60)` passed into `spawn_merge_poller`).
- **Recheck invocation on each sweep:** `merge_poller/sweep.rs:417-420` iterates `pending_pr_recheck` and calls `sweep_pending_pr` (`:858-863`), which delegates to `recheck_for_pr`.
- **Observed wall-clock gaps** between sweeps in the incident material: roughly **66–91 seconds** (interval plus sweep work). The successful A/B case used ~65.6 s of runway.

**Line-reference correction:** the investigation brief cited `merge_poller/sweep.rs:418-419` and `:858-915` as the cadence site. Those lines are the **pending-PR recheck invocation path**, not the interval constant. The configured cadence lives at `app/server.rs:1059` (and is assumed throughout merge-poller comments as "today's 60s sweep"). The 66–91 s figure is observed wall-clock from trace, not a constant in source.

## 7. Contributing factors

### 7.1 Largest factor — no explicit, trustworthy “worker is done” signal

The engine still **infers** completion from proxies (staged PR URL, Stop hooks, SHA deltas, prompt checklists). None of those is a worker-authored, schema-checked declaration that “this execution’s outcome is X.” Until that exists, every clever finalization path will keep rediscovering the same class of bug: mistaking an intermediate tool event for terminal success.

Proxies that failed here:

| Proxy                   | Why it is not “done”                                                        |
| ----------------------- | --------------------------------------------------------------------------- |
| Staged PR URL           | Armed by `gh pr view` / `edit` mid-investigation                            |
| `cube pr update` / push | Leaves post-push steps and mid-turn followups unfinished                    |
| Stop hook               | Correct boundary when it fires; the staged recheck path did not wait for it |
| Board `PendingReview`   | Written by the bad finalization itself                                      |

Boss has repeatedly filed and partially shipped work in this neighborhood (e.g. historical “detect worker task completion and move to In-Review,” metadata-only finalize attempts, `NO_CHANGES_NEEDED` / nothing-to-do finalization, mid-turn deferral in [mono#2685](https://github.com/spinyfin/mono/pull/2685)). **The durable product gap remains:** an explicit execution outcome (delivered change / metadata-only change / nothing-to-do / blocked) that the engine requires before terminalizing. Until that lands, danger and complexity continue on every path that guesses done-ness from side effects. See AI-3 and AI-6.

### 7.2 Post-push deliverables live only in the prompt

Revision steps after the push are prompt text, not engine actions (`runner/prompt.rs`):

- Step 4 — push via `cube pr update` (`:1653-1655`)
- Step 5 — update PR title/description (`:1657-1671`)
- Step 6 — findings-status PR comment (`:1673-1721`)
- Steps 7–8 — confirm head / print URL (`:1723-1728`)

Anything the prompt asks for **after** the push is exposed to this race by construction. When the engine treats staging (or push) as "done," steps 5–8 are structurally orphaned. Whether post-push deliverables should remain prompt-only is an open product/engine design question (action item below).

### 7.3 False-success status without an associated _change_

R2 terminalized toward review despite producing no push. The engine separately observed `sha_unchanged` when deciding whether to enqueue a reviewer (`pr_transition.rs:122-129` logs `pr_review noop skip` with `skip_reason`; noop classification includes `"sha_unchanged"` in `finalize_passes.rs` around the noop gate). The status transition and the SHA check are not coupled as a success criterion: **terminalization to PendingReview / InReview does not require that this execution produced an associated change.**

“Associated change” is intentionally broader than “a new commit”:

| Legitimate revision outcome                                 | Evidence the engine should require                                                                  |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| Code / conflict resolution landed                           | Head SHA movement attributable to this execution                                                    |
| Metadata-only (PR title/body/labels only)                   | Explicit metadata-only path + observed PR metadata mutation                                         |
| Nothing to do (finding already fixed, false positive, etc.) | Explicit worker outcome (e.g. `NO_CHANGES_NEEDED` / structured skip) — **not** silence after a reap |
| Blocked                                                     | Explicit `[blocked]` (or equivalent) with reason                                                    |

Requiring “a commit” alone is too narrow (metadata-only revisions are real). Requiring **some explicit outcome** is not. R2 had neither head movement nor metadata-only proof nor an explicit no-op — yet the board showed success. That is the major product defect.

### 7.4 Protection existed and was bypassed by layering

The SHA-delta guard at `recheck.rs:144-175` encodes the exact lesson of 2026-07-14. The staged-URL fast path was added **above** it as a primary path that returns before the guard runs. Whatever review process added the fast path did not re-apply the prior incident's invariant ("do not finalize/absorb while a live worker may still be mid-session") to the new branch.

This is a **layering / short-circuit** failure, not a missing idea: the idea was already written a few dozen lines down. The short-circuit was present from the introducing change (§5.1 / §6.0), not only from a later regression.

### 7.5 Detection is incidental and not aggregated

The only field that distinguishes a mid-turn reap from a legitimate idle finalization is `activity` on the `live-state registry: slot entry cleared` log line (`live_worker_state.rs:654-662`). That field is not elevated to a metric, alert, or attention item. Therefore:

- Operators cannot see a dashboard of mid-turn reaps.
- Nothing in the running system partitions `pr_recheck_staged` finalizations into "safe idle" vs "destructive working." The partition in §3 exists only because the raw trace was pulled offline and each finalization was hand-paired to its registry-clear record.
- **Prevalence was measurable, but only by bespoke offline reconstruction.** It was not visible to anyone operating the system, which is why a **96.4%** staged-path mid-turn rate (216/224) ran for the full measured window without raising anything.

Severity was worse. The engine records a head SHA _before_ an execution runs but never _after_ teardown, so "did this execution actually push?" had to be reconstructed from the _next_ execution's `pr_head_before` snapshot or a live GitHub read. That is why one row is permanently unresolvable and why one engine `sha_unchanged` verdict turned out to be a stale-read false negative (both in §3). See AI-5.

### 7.6 Source-reference verification notes

| Brief citation                                         | Verified in this checkout | Notes                                                                                                                                |
| ------------------------------------------------------ | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `recheck.rs:55-73` staged fast path                    | **Yes** (`:55-72`)        | Matches.                                                                                                                             |
| `recheck.rs:81-89` turn-loop gate                      | **Yes** (`:81-88`)        | Matches.                                                                                                                             |
| `recheck.rs:144-176` SHA-delta protection              | **Yes** (`:144-175`)      | Comment text is multi-line; substance matches the brief's paraphrase.                                                                |
| `completion.rs:2113-2115` `worker_owns_turn_loop`      | **Yes**                   | Predicate is live status ∧ not `PrReview` only.                                                                                      |
| `pr_url_capture.rs:304-324` staging predicate          | **Yes**                   | Includes `view` / `list` / `edit`.                                                                                                   |
| `prompt.rs:1673-1721` / push at `:1653-1655`           | **Yes**                   | Step 6 findings comment after step 4 push.                                                                                           |
| `merge_poller/sweep.rs:418-419`, `:858-915` as cadence | **Partial**               | Those lines are recheck invocation, not the 60 s interval. Cadence: `app/server.rs:1059`. Observed 66–91 s is wall-clock from trace. |

## 8. What went well

- **Forensic reconstruction of the focal pair is tight.** Sub-second timestamps on staging, recheck, terminalization, activity-at-clear, and teardown path form a complete causal chain for R1 and R2 without needing to re-open live logs in this run.
- **A clean A/B from the same afternoon exists.** mono #2683 / #2682 vs flunge #1342 shows the same feature succeeding and failing as a pure phase race, which makes the root cause teachable rather than speculative.
- **The prior incident left a correct written invariant** in the SHA-delta arm. The right idea was already in the file; the gap is that a newer path does not share it.
- **Activity-at-clear logging already records the distinguishing signal.** Instrumentation for detection is partially present; it is not aggregated or alerted.
- **Driver diversity in the failure pair** (codex then claude) rules out a single-driver misconfiguration as the explanation — since confirmed at scale by the occurrence split across all three drivers (§3).
- **The blast radius turned out to be measurable after all**, from `activity`-at-clear paired against finalization source. The discriminator validates well against control groups — Stop paths are **325/327 idle** over the canonical window — so the 216/224 staged mid-turn figure rests on a signal that demonstrably separates the two families rather than on inference. (The two Stop-family exceptions are themselves a finding, not noise in the discriminator: see `stop_satisfied_clean` in §3.)
- **The measurement cross-checked on two independent log surfaces.** Reconstructions from `engine-trace.jsonl` and `/tmp/boss-engine.log` agree on every finalization-source total both can see, which is what makes the 224-row inventory trustworthy.

## 9. What went wrong

### 9.1 Mechanism (runtime)

- A fast path finalizes and reaps live workers without inheriting protections a few lines below it — and the same missing check is absent from the detector branch of the same function (6 working / 0 idle) and from `stop_satisfied_clean` (2 working / 10 idle), so the defect is structural to finalization, not local to one code block.
- Staging treats `gh pr view|list|edit` like a push, so metadata and read commands can arm teardown.
- Post-push prompt steps are structurally races, not guaranteed deliverables.
- Board status can report "in review" for a revision that never produced an associated change — false success while production regressions remain. **Five work items are in exactly that state right now** (§4).
- The race is not an edge case. **224 mid-turn reaps across 127 PRs** over the canonical nine-day window, with the staged path **96.4% mid-turn** (216/224). The rate is flat across the whole window and across every engine restart in it.
- The only mid-turn reap signal is a log field that is not metricked, so nothing raised an alarm while that failure rate ran for at least the nine days that are measurable — and the path was already firing a week before the discriminating record existed at all. Sizing it required an offline trace reconstruction, twice.
- **The defect plausibly suppresses its own recovery path.** `reconcile_revision` re-fires only when no reconcilable execution exists; a mid-turn reap records success. That is a lead rather than a confirmed mechanism (§4), but the retry-loop pathology it would explain is real and affects **27 PRs**, not one.
- No head SHA is recorded after teardown, so "did this execution push?" is not answerable from the engine's own records — one severity determination is permanently unresolvable as a result.
- A successful worker on the same afternoon used 65.6 s of ~66 s of runway — the system was already operating at the edge of its budget when "working."
- There is still no explicit worker-done / outcome signal, so every finalization path keeps inventing proxies.

### 9.2 How the change was introduced (process / meta)

Verified from the public PR body and commit message of the introducing change ([mono#465](https://github.com/spinyfin/mono/pull/465)); the authoring agent’s private transcript is **not** readable from this worker (see §6.3).

- **Problem was real and urgent** (stranded workers, fragile reconstruction) — good reason to ship capture.
- **Scope mixed two concerns** without separating their safety properties: (1) _observe_ PR URLs from the tool stream, (2) _finalize and reap_ when observed. (1) is recovery; (2) is lifecycle. Shipping them as one primary path made capture lethal.
- **Self-contradictory acceptance criteria:** prose promised Stop-only transitions; tests required poller finalization from the staged cache. Review that checked the tests without checking the safety claim would pass both.
- **Tests encoded the bug as the happy path** (`recheck_for_pr_uses_staged_pr_url_and_skips_detector`) without a mid-turn / `activity: working` negative case.
- **Breadth optimized for URL recovery** (`view`/`edit` included because they print URLs) with no second gate before teardown.
- **Later incidents patched sibling arms** (SHA-delta, `stop_seen`, branch verification) without re-opening the short-circuit above them — classic layering failure, and a review process that treats “new primary path” as local rather than as a re-validation of prior invariants.

That process story is as important as the mechanism: the same defect class will recur on the next “make completion more reliable” feature until explicit outcomes and mid-turn guards are mandatory on every finalize path.

## 10. Detection and response

### Detection (as of the incident)

| Signal                                                                | Present?  | Actionable?                                                            |
| --------------------------------------------------------------------- | --------- | ---------------------------------------------------------------------- |
| `source: 'pr_recheck_staged'` on terminalization                      | Yes (log) | Counts finalizations, not mid-turn harm                                |
| `activity: 'working'` on slot clear                                   | Yes (log) | Distinguishes mid-turn reaps; **not aggregated**                       |
| `pane_outcome: 'Reaped'` with working activity                        | Yes (log) | Same as above                                                          |
| Metric / attention for mid-turn `pr_recheck_staged`                   | **No**    | Prevalence invisible in-system; only recoverable offline               |
| Operator-facing banner when a revision finalizes with `sha_unchanged` | **No**    | R2 looked successful on the board                                      |
| Head SHA recorded _after_ teardown                                    | **No**    | "Did this execution push?" is not answerable from engine records alone |

Discovery of this incident was from a **symptom** (missing findings-status comment on R1), not from an engine-raised attention item. R2's total loss was found by forensic follow-up on the same PR, not by the board. Nothing in this table fired for any of the other **223** mid-turn reaps in the canonical window — every one of those counts came from pulling raw trace off-box, not from any operator-facing surface.

### Response

This document is the response artifact for the investigation. Staged-path mid-turn deferral **landed** as [mono#2685](https://github.com/spinyfin/mono/pull/2685). Two follow-ups have since been answered: the log forensic check (AI-8) is **done** and refuted its own premise, and detector-branch coverage (AI-1) was **checked and found absent**. Remaining action items — extending the guard to the detector branch and `stop_satisfied_clean`, the staging predicate, explicit outcomes, metrics, `pr_head_after`, trace keep-count / rotation policy, and bounding `/tmp/boss-engine.log` — are listed in §11 and are deliberately not redesigned here.

## 11. Action items

Owners are **surfaces** (files / subsystems), not people. **AI-N** means action item _N_ in this section and is used for every cross-reference. None of these are implemented by this document. AI-1's staged-path guard **landed** as [mono#2685](https://github.com/spinyfin/mono/pull/2685) but covers only one of the three known reap paths; AI-8 and AI-10 are **✅ done** and their status blocks below record what they found. Everything else is open work.

These are engineering fixes to the defect. They are **not** a substitute for the recovery work in §4: the five work items listed there are already broken and will not be repaired by any code change here.

### Immediate — completion / recheck

1. **Guard `recheck_for_pr` — the whole function, not just the staged arm — so no path through it can finalize a still-working live worker.** Surface: `tools/boss/engine/core/src/completion/recheck.rs` staged arm (`:55-72`) **and its detector branch**, coordinated with `finalize_pr_transition` / teardown in `completion/pr_transition.rs` and `completion/teardown.rs`. Inherit the 2026-07-14 invariant already written at `recheck.rs:144-175` (defer to the worker's own Stop boundary while mid-session). The measurement in §3 found mid-turn reaps through the detector branch as well (`pr_recheck`, 6 working / 0 idle), so **a fix scoped to the staged block alone would leave a live path open.**

   **Status: PARTIALLY LANDED — detector-branch coverage checked, and it is ABSENT.** Staged-path mid-turn deferral **landed** as [mono#2685](https://github.com/spinyfin/mono/pull/2685) (merged 2026-08-09; task tracking “Staged-PR-URL recheck fast path reaps live workers mid-turn”). The running engine is that fix — `engine_build_sha="818e8bc6cdf67ae6de53c1b24a3f693a94bc6083"`, the merge commit, started 2026-08-09 02:26:53 EDT.

   Reading the merged diff, `should_defer_staged_pr_recheck()` is called at **exactly one site**: inside the `verified_staged_pr_url(…)` block at `recheck.rs:56`. Therefore:

   - **No change to the detector branch of `recheck_for_pr`.** That branch is **6 working / 0 idle** over the canonical window — every detector-branch PR-completion terminalization measured reaped a mid-turn worker, earliest 2026-07-31 16:37:57 EDT (`exec_18c7486b45a667c8_18`). It remains fully exposed.
   - **No change to `stop_satisfied_clean`**, the third reap path (2 working / 10 idle, §3). It was not known when [mono#2685](https://github.com/spinyfin/mono/pull/2685) was written and is not covered by it.
   - **The anti-hang bound is retained**, as intended: `staged_pr_mid_turn_defer_secs`, default `DEFAULT_BUILD_WAIT_HORIZON_SECS` = 2,700 s, measured from `StagedPrUrlEntry::staged_at`.

   **Remaining work on this item:** extend the guard to the detector branch and to `stop_satisfied_clean`, and assert in regression tests that `activity: working` cannot terminalize on **any** of the three paths before the horizon. Early production evidence that the staged-arm guard itself works is in §3 — five deferrals, zero mid-turn reaps, 15/15 Stop-boundary finalizations in ~49 minutes — but that sample is thin, and **no deferral has yet aged out against the 2,700 s horizon, so the anti-hang bound is untested in production.**

### Near-term — staging predicate

2. **Narrow what arms PR-URL staging for finalization.** Surface: `tools/boss/engine/core/src/pr_url_capture.rs` (`is_gh_pr_command_str`, `:304-324`). At minimum, separate "URL observed for binding" from "permission to finalize and reap." Read-only / metadata commands (`gh pr view`, `list`, `edit`) must not be sufficient to trigger `pr_recheck_staged` teardown of a live execution.

### Near-term — false-success status / explicit outcomes

3. **Do not terminalize a revision implementation to PendingReview / InReview without an explicit associated _change_ or an explicit nothing-to-do outcome.** Surface: `completion/pr_transition.rs` (`finalize_pr_transition`, reviewer enqueue / noop skip around `:122-230`) and the SHA-delta / contribution gates in `completion/recheck.rs`.

   - **Change** may be: head SHA movement attributable to this execution, **or** an explicit metadata-only path with observed PR metadata mutation (title/body/labels — revisions that only edit the PR description are legitimate).
   - **Nothing to do** is also legitimate (finding already fixed, false positive, conflict already resolved elsewhere) — but it must be an **explicit worker outcome** (structured skip / `NO_CHANGES_NEEDED` class), not silence after a mid-turn reap.
   - The `sha_unchanged` observation already exists on the review-skip path (`finalize_passes.rs` noop gate); couple an equivalent check to status success so a reaped no-push, no-metadata, no-explicit-noop revision cannot read as review-ready.

### Near-term — detection / metrics

4. **Emit a first-class metric (and preferably an attention item) when a `pr_recheck_staged` finalization clears a slot with `activity != idle` (or equivalent mid-turn signal).** Surfaces: `live_worker_state.rs` (`release_slot`, `:654-662`), completion teardown path, merge-poller / completion metrics registry. Goal: make mid-turn reap **countable and alertable in-system**. The §3 measurement shows this is achievable from data the engine already emits — but only offline, by hand, and only back as far as log retention. Prevalence should not require a forensic exercise to see.

5. **Record a `pr_head_after` on the teardown record, read at the moment the fast path decides to terminalize.** Surfaces: `completion/pr_transition.rs` (`finalize_pr_transition`) and `completion/teardown.rs`. The engine records a head SHA before an execution and never after it, so every severity determination in §3 had to be reconstructed from the _next_ execution's `pr_head_before` snapshot or a live GitHub read — which is why one row (flunge #1296) is permanently unresolvable and why one `sha_unchanged` verdict (flunge #1327) was a stale-read false negative. A `pr_head_after` field makes "did this execution move the head?" a single query, and it incidentally hands the fast path the very signal it needs to decide correctly (cf. AI-3).

### Structural — prompt vs engine ownership of post-push work

6. **Decide whether post-push deliverables (PR description update, findings-status comment) remain prompt-only or become engine-owned / pre-finalize gates.** Surface: `tools/boss/engine/core/src/runner/prompt.rs` revision steps (`:1653-1728`) and the completion Stop / recheck contract. If they stay in the prompt, the engine must not finalize until Stop (or an explicit worker "done" signal). If they move into the engine, the race class shrinks by construction.

### Structural — turn-loop predicate

7. **If recheck continues to gate on worker liveness shape, extend the predicate beyond `is_live && kind != PrReview`.** Surface: `completion.rs` `worker_owns_turn_loop` (`:2113-2115`) and call sites in `recheck.rs` / `stop.rs`. Mid-turn vs idle must be visible to any path that can reap. Note: this is **not** a substitute for AI-1 if the staged path still short-circuits the gate.

### Forensic — log retention / pre-window truth

8. **✅ Coordinator-side: prove whether pre-2026-08-06 `engine-trace` (and useful audit events) still exist on disk.**

   **Status: ✅ DONE, and the original hypothesis was wrong in every particular.** The check found (§3): the earlier material _did_ exist, on an unrotated `/tmp/boss-engine.log`; the "app update reset the log root" story is false — no updater deleted anything; there were **twelve** failed engine starts, not three; and the actual mechanism is **rotation eviction**, because `engine-trace.jsonl` rotates on _every engine start_ as well as at ~100 MB against a keep count of 5. The §3 measurement was re-run across the recovered surface over the full join-floor window.

   **The residual reliability defect this item anticipated is real and is now confirmed, with a different cause than assumed.** Rotate-on-every-start means a restart storm destroys forensics precisely when they are most wanted: thirteen starts in 10 m 41 s evicted everything older than 13:36 EDT, and a later single restart evicted an earlier investigator's retained window. Residual engineering work is split into AI-11 (trace keep count / start-rotation policy) and AI-12 (bound the unbounded text log). **Nothing this incident needed was lost** — but that was luck, not design.

### Forensic — introducing-agent transcript (optional)

9. **If public PR text for [mono#465](https://github.com/spinyfin/mono/pull/465) is insufficient for process review, coordinator retrieves the authoring execution transcript** (Claude/Grok session JSONL via `work_runs.transcript_path` or equivalent). This worker has no read-transcript CLI. Not required to understand the mechanism; useful only for deeper process archaeology.

### Forensic — partial-loss reinstatement inventory

10. **✅ Coordinator-side: publish a per-execution inventory for the partial losses before reinstatement review.** Source it from the retained trace join that pairs finalization source with the `activity: working` slot-clear record, then record the PR link, execution id, timestamp, driver, and severity verdict.

    **Status: ✅ DELIVERED — with one sub-request that turned out to be underivable.** The inventory is **224 rows** covering the canonical window, with columns `utc_ts, execution_id, source, pr_url, kind, driver, pr_head_before, next_pr_head_before, verdict`. Severity by this document's own method: 151 partial · **33 total loss** · 40 unknown. See §3 for the full breakdown, the systematic under-count in the unknown bucket, and the 27 PRs that took three or more mid-turn reaps.

    **Underivable sub-request:** this item asked for "any prompt steps demonstrably unreached" per row. **No current surface can supply that** — the trace carries no per-prompt-step marker, so the only available inference is the coarse "reaped at `activity: working`, therefore the turn did not complete." The item was written as though per-step attribution were obtainable; it is not. Making it obtainable would require the engine to record prompt-step progress, which is new instrumentation, not a query. Recorded in §12.

    Reinstatement review of the partial-loss PRs for missing post-push deliverables is still outstanding; the inventory unblocks it.

### Forensic — trace keep count / start-rotation policy

11. **Raise the `engine-trace.jsonl` keep count and stop engine starts from consuming rotation slots.** Surface: `tools/boss/engine/core/src/trace_rotation.rs` (`DEFAULT_TRACE_MAX_FILES = 5`, `rotate_on_startup`). A keep count of **5** is almost certainly too low for a stream that also rotates on every process start: thirteen starts in 10 m 41 s during this incident evicted everything older than those eleven minutes, and a later single restart evicted an investigator's retained window. Change the policy so size-based rotation (or an intentional retention window) governs how much history is kept, and so a restart storm cannot erase forensics on its own. Optionally retain completion-critical records on a longer-lived surface. Related residual from AI-8.

### Forensic — bound `/tmp/boss-engine.log`

12. **Stop `/tmp/boss-engine.log` from growing without bound.** Surface: `tools/boss/engine/core/src/main.rs` (`DEFAULT_LOG_PATH`, `open_log_file` — currently `create(true).append(true)` only; no size cap, no rotation, no prune). At investigation time the file was **~2.0 GiB** and held 16 d 11 h of the pretty-format stream. The engine and Boss stay up almost continuously, so nothing external is truncating it either. **This will eventually fill the host disk.** Options (pick one and implement): size- or time-based rotation with a keep count; truncate-on-start for the text layer (JSONL remains the durable forensic surface); or write the text layer under the Application Support tree with the same rotation policy as the audit log. Decide explicitly whether the pretty text log is a deliberate durable forensic surface or a debug convenience that must not outgrow the disk — today it is the former by accident and the latter by intent. Related residual from AI-8; distinct from AI-11 (JSONL keep-too-low vs text unbounded growth are opposite failure modes).

## 12. Incomplete evidence (stated plainly)

### Resolved since first publication

- **~~Pre-window log availability is unproven either way.~~ ✅ RESOLVED — it was an investigator access gap, not data loss.** Of the two failure modes this document originally listed, **mode 2 is correct**: an unrotated `/tmp/boss-engine.log` held the same stream back to 2026-07-23, and the measurement was re-run across it (§3). The original "app update wiped the log root" story is refuted in every particular. **Real log loss does occur** on the JSONL surface — `engine-trace.jsonl` rotates on every engine start against keep 5, so a restart storm evicts days of trace in minutes, and it did so twice during this investigation — **but nothing this incident needed was lost.** Residual defects carried as AI-11 (keep count / start-rotation) and AI-12 (unbounded text log).
- **~~Partial-loss reinstatement inventory is unenumerated.~~ ✅ RESOLVED — delivered at 224 rows** (AI-10, §3). Per-PR reinstatement review is still outstanding, but it is no longer blocked on data.
- **~~Detector-branch coverage of the landed guard is unconfirmed.~~ ✅ RESOLVED — coverage is ABSENT** (AI-1, §11). `should_defer_staged_pr_recheck()` has exactly one call site, in the staged arm. The detector branch (6 working / 0 idle) and `stop_satisfied_clean` (2 working / 10 idle) are both still exposed.
- **~~Do engine timestamps lack absolute timezones?~~ ✅ RESOLVED — they do not.** Log and trace records are stamped in absolute UTC (`…Z`). Local zone labels in this document are presentation only; the host zone moves with the operator and is not a stable system property (timezone note at head of document).

### Still incomplete

- **R2 local commit recoverability:** A `jj` search of the shared flunge store (including `flunge-agent-001`, the workspace that ran R2) found no unrecovered R2 object; only the later recovery commit `c1e2c6d101bd` is present. Do not assert “outside available material.” Still do not assert durable recovery of R2’s original object — only that search returned absence, while the work item later recovered via redo (§4). **Unchanged and still open.**
- **Activity-at-teardown for the 243 pre-2026-07-30 staged hits is permanently unknowable.** The `pr completion: execution terminalized` record that carries the discriminator did not exist before 2026-07-30 19:09 EDT. No future analysis can classify those 243 hits, and they must not be assumed to share the measured 96.4% mid-turn rate. This is a hard limit, not a pending task.
- **Severity of the 40 no-successor rows in the AI-10 inventory is unresolved**, and that bucket **systematically hides total losses** (§3). Resolving it requires live GitHub head reads per PR. AI-5 (`pr_head_after` on the teardown record) fixes this class structurally for future incidents but cannot retroactively fill these rows.
- **One severity row is unresolvable.** A revision on [flunge#1296](https://github.com/brianduff/flunge/pull/1296) routed via `pr_review cycle bound reached`, skipping the SHA check, and the branch has since been force-pushed. The head at reap time is unrecoverable, so it cannot be assigned to total or partial loss. It may be an additional total loss.
- **The mechanism behind the two `stop_satisfied_clean` mid-turn reaps is unestablished.** N=2, both with `elapsed_ms=0` and both terminalizing to `InReview` with `activity="working"`. Recorded in §3 as a **flag, not a finding** — do not reason from it beyond "the Stop family is not categorically safe."
- **Whether the 2,700 s anti-hang horizon ever finalizes a genuinely-working worker is untested in production.** Zero expiries have been observed since the guard landed. Zero expiries is not evidence of safety; it is evidence the case has not arisen yet.
- **Whether workers may read `/tmp/boss-engine.log` is untested.** The Application Support prohibition does not reach it, but no worker has attempted the read and no policy statement covers it. This document deliberately asserts nothing in either direction.
- **Whether the eleven zero-record hours are machine sleep is a strong inference, not proof.** The signature fits (periodic timers stop dead and resume cleanly, engine up throughout, no restart at either boundary). `pmset -g log` would settle it and has not been run.
- **Per-driver severity rates are estimated, not determined.** Canonical window: claude 4/55 (7.3%), codex 18/64 (28%), grok 2/3 (67%) — all sit on top of the under-counting unknown bucket (§3). Grok’s concentration remains **confounded** by in-development driver support during the period. The proposed timing mechanism (non-claude drivers surfacing a `gh pr` URL earlier relative to their push) remains a **hypothesis**.
- **Per-prompt-step attribution is not derivable from any current surface.** AI-10 asked for "prompt steps demonstrably unreached" per row; the trace has no per-step marker, so the only inference available is the coarse "reaped while `working`, therefore incomplete." Obtaining it requires new instrumentation, not a better query.
- **Why `reconcile_revision` recovered [flunge#1342](https://github.com/brianduff/flunge/pull/1342) and not the five items in §4:** a **structural difference is now visible** — flunge#1342's task was re-created twice on the same work item, while mono#2651's attempts were three distinct once-only work items (§4). A **candidate mechanism** is recorded there (a mid-turn reap terminalizes toward `PendingReview`/`InReview`, which may read as "reconcilable," suppressing recovery). **It is a lead, not a finding:** confirming it requires reading the `reconcile_revision` predicate in engine source, which has not been done. This remains the largest open product question.
- **Introducing-agent private transcript:** not readable from this worker (§6.3). Public PR #465 text used instead.

## 13. Lessons

1. **A guard below a short-circuit is not a guard.** New primary paths must re-apply prior incident invariants, or they reintroduce the same hazard with a different name.
2. **"URL staged" is not "worker finished."** Staging is a binding hint; finalization and reap require a turn boundary or an explicit done/outcome signal.
3. **Read/edit/view must not arm teardown.** Capture breadth optimized for recovery of missed PR opens is the wrong breadth for a path that kills the worker — and that breadth was intentional in the introducing PR, which is how it survived review.
4. **Prompt steps after the push are optional under a race.** If the engine can finalize on push-related signals, post-push prompt work is best-effort only.
5. **False success is worse than visible failure** when the board hides total work loss and live production regressions. Worse still, it is self-perpetuating: [mono#2651](https://github.com/spinyfin/mono/pull/2651) (§4) shows a false-success loop re-minting the same revision every few minutes, because each failure is recorded as a win — and it is not one PR's misfortune but a pattern across **27 PRs** that took three or more mid-turn reaps. Require an associated _change_ or explicit nothing-to-do — not silence.
6. **If the only distinguishing field is a log attribute, prevalence is invisible until it is a metric.** The data was there the whole time — 224 mid-turn reaps sitting in trace across nine days — and it took two hand reconstructions to see any of it. "Not aggregated" and "not happening" are indistinguishable from the operator's chair.
7. **Record state after the action, not only before it.** Without a post-teardown head SHA, the engine cannot answer its own most important question — did this execution deliver anything? — which left one row permanently unresolvable and one verdict wrong.
8. **A phase race measured is rarely a coin flip.** This one was assumed roughly 50/50 from two cases and measured at **96.4%** mid-turn on the staged path (216/224). Estimate the rate before deciding a race is tolerable — and measure against the full join-floor window, not whatever log rotation left on disk that day.
9. **Not every total loss is catastrophic.** Deterministically re-doable work (merge conflicts) wastes capacity; unique unrecovered work loses content; false success corrupts control flow. Categorize along those axes.
10. **Worker “can’t see logs” is not the same as “logs are gone.”** This document originally reported an app update wiping the log root after three failed starts. The coordinator check found no update, twelve failed starts, and an intact unrotated copy of the whole stream in `/tmp` — the material was there the entire time. The caveat was correctly labelled an analysis gap rather than a finding, and that labelling is the only reason the wrong story never hardened into a fix for a problem that did not exist. **Label inferences as inferences; the discipline pays off exactly when the inference is wrong.**
11. **Rotate-on-start destroys evidence when evidence matters most.** Trace rotation triggered by engine start, against a keep count of 5, means a restart storm — the very event you most want to investigate — evicts its own precursors (AI-11). It happened twice inside this investigation. Retention policy must be indexed to the incident, not to process lifecycle.
12. **An unbounded append-only log will eventually fill the disk.** `/tmp/boss-engine.log` is the same stream in pretty format with no rotation at all; it was already ~2 GiB and growing because the engine almost never stops (AI-12). Opposite failure mode from keep-too-low, same root cause: no deliberate retention policy for that surface.
13. **UTC is ground truth; local zone labels are presentation.** Engine timestamps already carry absolute UTC (`…Z`). Host zone moves with the operator and must not be treated as a stable system property. Derive any local wall clock from the UTC anchor; do not re-stamp history when `/etc/localtime` changes.
14. **PR prose that contradicts its own tests is a review smell.** #465 said Stop-only; its recheck test required poller finalization. Trust the code path the tests lock in.

## 14. Follow-up code changes

This document is **doc-only**. Recommended work is listed in §11 (engineering fixes) and §4 (recovery of the five falsely-reviewed work items) and should be filed as separate chores/tasks against the engine completion, PR-URL capture, merge-poller, prompt-runner, and logging surfaces. The staged-path mid-turn guard (item 1) landed as [mono#2685](https://github.com/spinyfin/mono/pull/2685); do not redesign it here. Detector-branch coverage has now been verified and is **absent**, and a third exposed path (`stop_satisfied_clean`) has since been identified — extending the guard to both, raising the trace keep count / stopping start-rotation (AI-11), and bounding `/tmp/boss-engine.log` (AI-12) are the highest-value remaining items and should be filed separately.
