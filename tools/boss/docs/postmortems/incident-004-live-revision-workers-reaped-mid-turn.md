# Incident 004 — Engine reaped live revision workers mid-turn, discarding their work

- **Date:** 2026-08-08 (America/Chicago). Focal pair on flunge PR #1342 between ~15:23 and ~15:41; the same finalization path had been active for at least two days prior (a retention-bounded floor — see §3).
- **Severity:** High — silent work loss on live revision workers, plus false-success board status for revisions that produced nothing. Measured over the retained trace window: **63 mid-turn reaps out of 64 staged-path finalizations (98.4%)** — 6 total losses, 57 partial losses, and **5 work items still sitting in review status having contributed nothing** (§4). Includes a case where a revision minted to fix live production regressions was reaped before push while the board reported success.
- **Status:** Documented. Guard remediation for the fast path is already in flight as separate work and is not redesigned here. This postmortem is doc-only.
- **Class:** Race between merge-poller PR recheck and a still-working agent: a staged PR URL is treated as "worker done," terminalizing and reaping a live mid-turn execution. Related prior: the 2026-07-14 SHA-delta absorption incident whose protection this fast path bypasses.
- **Related:** [`incident-001-pr-fan-out.md`](incident-001-pr-fan-out.md) (wrong-PR finalization killing live workers); 2026-07-14 SHA-delta baseline absorption (guard comment at `completion/recheck.rs:152-166`).

## 1. Verdict

The engine has a **fast path that treats a staged PR URL as permission to finalize and tear down a still-running worker**. When a revision worker's tool stream stages a PR URL — including from a push, or from a non-push `gh pr` command such as `edit` or even `view` — the next merge-poller recheck immediately calls `finalize_pr_transition(…, "pr_recheck_staged")`, terminalizes the execution, and reaps the pane while the agent is still mid-turn (`activity: 'working'`).

That fast path short-circuits two protections sitting **below** it in the same function: the `worker_owns_turn_loop` gate, and the SHA-delta arm that explicitly refuses to absorb a possibly in-flight push and defers to the worker's own Stop boundary. Those guards were written after an earlier race of the same class; the staged-URL path was added above them without inheriting them.

Whether any given worker survives is a race against the ~60 s full-sweep cadence. Staging just after a sweep can work; staging into a sweep that is about to run does not. The feature is correct when lucky and destructive when not — and measurement of the retained trace window shows the unlucky case is not the exception but the rule: **63 of 64** staged-path finalizations reaped a worker that was still `working` (§3).

The defect is a property of `recheck_for_pr`'s structure, not of the staged code block alone. A further occurrence finalized through the same function's _detector_ branch and was also reaped mid-turn. **No arm of that function establishes that the worker is between turns before it terminalizes.**

## 2. Summary

On 2026-08-08, two consecutive revision workers on flunge PR #1342 were reaped by the engine while mid-turn:

| Revision | Execution                  | Driver | Outcome                                                                                                                                                                                           |
| -------- | -------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R1       | `exec_18c9ee2925032810_47` | codex  | Pushed successfully, then reaped **2.4 s** later mid-prompt. Code reached the PR; post-push steps (including the required findings-status comment) never ran.                                     |
| R2       | `exec_18c9ee9f79ea3c08_4f` | claude | Reaped **before any push**. The local fix commit never reached the PR and the workspace lease was released (recoverability unknown; see §12). Board still moved the work item as if review-ready. |

Occurrence is not driver-specific — though severity is (see §3) — nor is it product-specific. The two cases used different drivers and different workspaces, and the measured window confirms both: occurrences split mono 31 / flunge 32, and by driver claude 56 / grok 4 / codex 3, which tracks the traffic split.

The focal pair is a calibration case, not the incident. Across the 2 d 3 h 24 m of retained engine trace, **64 finalizations took the staged fast path and 63 of them reaped a worker that was still mid-turn (98.4%)** — 6 total losses and 57 partial losses. Five work items are still sitting in review status having contributed nothing; they are listed in §4. The full measurement, its limits, and its labelling are in §3.

## 3. Impact

### R1 — partial loss (the originally reported mild case)

R1 completed its code change and push (`cube pr update` at 15:28:23.94; commit `6d54316a11b3`, "Harden crawler metadata delivery"). The engine staged the PR URL from the worker progress stream at 15:28:23.972, rechecked via the staged path, terminalized with `source: 'pr_recheck_staged'`, `target: 'PendingReview'`, and tore the worker down with `pane_outcome: 'Reaped'` while `activity: 'working'`.

R1 had completed steps 1–4 of an 8-step revision prompt. Steps 5–8 never ran. In particular, step 6 — post a findings-status summary comment on the PR — never executed. GitHub confirmed PR #1342 had three comments, all bots; the findings-status comment was absent.

Code reached the branch. Required post-push deliverables did not.

### R2 — total loss (the severe case)

R2 was minted for six review findings on `6d54316a11b3`, including two `[high]` production regressions introduced by R1 (inline `style` attributes on `#root` and `<body>` in `frontend/index.html` that survive React mount, boxing the SPA into a 48rem column and forcing a dark background past MUI Joy `CssBaseline`).

R2 committed fixes locally (`jj describe` at 15:40:56) but never pushed. At 15:40:46.6 a `gh pr edit` had staged a PR URL. The merge-poller sweep at 15:41:04.23 reaped the worker at 15:41:04.58 with `activity: 'working'`. The workspace lease for `flunge-agent-001` was released ~0.4 s later. Engine logs for the ensuing review path reported `pr_review noop skip … skip_reason: 'sha_unchanged', trigger: 'revision_push'` — the head had not moved, because nothing was pushed.

The work item was nonetheless advanced as being in review. The board reported success for a revision that produced nothing, while live production regressions the revision existed to fix remained on the branch.

**Whether R2's local commit remains recoverable from the released workspace is not known.** That evidence is outside the material available for this postmortem.

### Measured blast radius

Every figure in this section was counted from engine trace, and each is labelled **determined** (directly counted) or **estimated** (inferred, with the inference stated). The per-driver severity rates are the only substantially estimated figures; everything else here is determined.

#### Observation window — determined

Earliest retained trace record 2026-08-06 12:42:40 CDT, latest 2026-08-08 16:06:12 CDT: a span of **2 d 3 h 24 m** over **92,124 records** — the full stream, no truncation. The retention boundary was verified two ways; queries before 08-06 return zero records.

**The window is a retention boundary, not a defect boundary.** An app update at ~12:38 CDT on 08-06 reset the log root after three failed engine starts. The defect may predate the window entirely, and the first occurrence lands **23 minutes into the earliest retained segment** — the path was already firing on day one of what we can see. **All counts below are a floor, not a total.**

Engine restarts inside the window (determined): 08-06 12:47:06, 08-07 17:32:08, 08-07 21:29:21, 08-08 15:03:33 CDT.

#### Headline — determined

**64** finalizations went through the staged fast path. Of those, **63 were mid-turn reaps** (`activity: 'working'` on the paired live-state registry clear) and **1** was clean: **98.4%**.

Pairing is unambiguous. Every execution had exactly one registry-clear record, median offset **7 ms**, max **1.62 s** — there is no ambiguous attribution anywhere in the set.

The `activity` discriminator validates cleanly against control groups:

| finalization source            | `working` | `idle` |
| ------------------------------ | --------- | ------ |
| `stop_staged`                  | 0         | 61     |
| `stop_sha_delta`               | 0         | 3      |
| `pr_recheck_staged`            | **63**    | 1      |
| `pr_recheck` (detector branch) | **1**     | 0      |

Stop-boundary paths are **64/64 idle**. Recheck paths are **64/65 working**. The signal separates the two families exactly as the mechanism in §6 predicts: finalizing at the worker's own Stop boundary finds an idle worker; finalizing from a poller recheck finds one mid-turn.

**Scope finding — the missing guard is not confined to the staged fast path.** One further occurrence finalized through the _detector_ branch of the same function and was also reaped mid-turn: a chore implementation on mono #2678, 08-07 21:57 CDT, grok driver. The defect is therefore a property of `recheck_for_pr`'s structure — no arm of it establishes that the worker is between turns — not of the one code block quoted in §6.1. A guard applied only to the staged arm would leave the detector branch exposed.

#### Severity — determined

**6 total loss** (never pushed) and **57 partial loss** (pushed, then killed before finishing the prompt).

The six total losses, by PR:

| PR           | Work                                                          |
| ------------ | ------------------------------------------------------------- |
| mono #2321   | Merge-conflict revision                                       |
| mono #2681   | Review revision, 4 findings                                   |
| mono #2651   | Three consecutive merge-conflict revisions (×3, three losses) |
| flunge #1342 | Review revision, 6 findings — R2 above, the calibration case  |

Two methodology notes belong with these counts, because both are traps for anyone re-running the measurement:

- **`revision_push_capture` is not proof of a push.** One execution staged push evidence from its tool stream _and_ was `sha_unchanged`, with the PR head unmoved. The tool-stream heuristic false-positives; **head-SHA movement is the decisive signal.**
- **One engine `sha_unchanged` verdict was itself wrong.** On flunge #1327 the engine recorded `sha_unchanged` at 13:00:56 CDT, but a commit with committer date 13:00:40 — 16 seconds earlier — is in that PR, which merged at 13:02:25 with no other execution able to have pushed it. The engine's head read was stale. Classified as partial: GitHub overrode the engine.

One row is genuinely **unresolvable**. A revision on flunge #1296 routed via `pr_review cycle bound reached`, which skips the SHA check, and the branch has since been force-pushed, so the head at reap time is unrecoverable. It could be a seventh total loss. It is reported as unresolvable rather than assigned to either bucket.

#### Breakdowns — determined counts

All breakdowns below cover the **63 staged-path mid-turn reaps**; the detector-branch occurrence is counted separately.

- **By repo:** mono 31 · flunge 32. Essentially even — this is not product-specific.
- **By execution kind:** revision_implementation 50 · chore_implementation 10 · investigation_implementation 1 · project_design 1 · task_implementation 1.
- **By driver:** claude 56 · grok 4 · codex 3. This tracks the traffic split, so **occurrence is not driver-specific.**

Severity by driver is a different story, and must be read per-driver rather than pooled:

| driver | occurrences | total loss | rate     |
| ------ | ----------- | ---------- | -------- |
| claude | 56          | 1          | 1.8%     |
| grok   | 4           | 4          | **100%** |
| codex  | 3           | 1          | 33%      |

Every grok run that reached this path lost everything. The occurrence and total-loss counts are **determined**; the **rates are estimated** — N is 4 for grok and 3 for codex, so those percentages carry no useful precision and must not be quoted without their denominators. A plausible mechanism, stated as a **hypothesis and not a finding**: non-claude drivers surface a `gh pr` URL into the tool stream earlier relative to their push, so the fast path catches them further from a completed turn.

#### Rate over time — determined counts, estimated trend

| day                | mid-turn reaps | all PR-completion terminalizations | share |
| ------------------ | -------------- | ---------------------------------- | ----- |
| 08-06 (from 12:42) | 30             | 60                                 | 50%   |
| 08-07              | 23             | 54                                 | 43%   |
| 08-08 (to 16:06)   | 10             | 15                                 | 73%   |

**The rate is flat and load-driven, not increasing.** Raw daily counts fall only because dispatch volume falls (213 → 164 → 48 spawns). Normalized, the defect fires on roughly half of all PR-completion terminalizations, and on essentially **100% of everything reaching the staged fast path** (63/64). Apparent burstiness is a load artifact: the 08-08 overnight gap has zero spawns, not zero defects. There is **no correlation with any of the four engine restarts** — rates are unchanged across all of them.

The counts in the table are determined. The trend characterization ("flat, load-driven") is **estimated**: it rests on three daily points, two of which are partial days, normalized against spawn volume.

## 4. Remediation list — work items that read as reviewed but contributed nothing

This is the actionable output of the measurement. **Five work items currently claim review status while nothing from their execution ever reached the PR head.** Each has exactly one completed execution and no recovery run in flight. Verified against GitHub 2026-08-08 16:14 CDT (determined). They are identified here by PR and by task description.

1. **Merge-conflict revision on mono #2321** — pushed nothing. Head was `b1de4521` at reap; the PR head is now `0dec5c0b`, moved by a _different_ work item's execution. This item reads as reviewed and contributed nothing.
2. **Review revision on mono #2681** ("4 finding(s)") — head `8d2300f5`, **unchanged since 08-07 22:55 CDT**. Four review findings were never addressed. The only other execution on that item failed.
3. **Merge-conflict revision on mono #2651** (1 of 3) — head `3cded858`, **unchanged since 08-06 13:07 CDT**; the PR is still `CONFLICTING`.
4. **Merge-conflict revision on mono #2651** (2 of 3) — same PR, same unmoved head.
5. **Merge-conflict revision on mono #2651** (3 of 3) — same PR, same unmoved head.

### mono #2651 is the worst case and the clearest demonstration of the failure mode

Three merge-conflict revisions in quick succession, at 15:52, 16:02 and 16:04 CDT on 08-08. Each was reaped the instant it typed a `gh pr` command. Each was recorded as successful. None of them moved the head.

**The defect manufactures a retry loop that consumes a worker every few minutes and can never converge**, because the recorded outcome of every attempt is success. Nothing in the system can distinguish "the conflict was resolved" from "the resolver was killed before it pushed," so the item is re-minted, re-reaped, and re-recorded as done, indefinitely.

### Recovered without intervention — not outstanding

The flunge #1342 review revision (R2 above) is **not** on the list: it recovered on its own, because `reconcile_revision` spawned a follow-up that pushed successfully. It is mentioned only to establish that a recovery path exists and that it did **not** fire for the five items above. (This says nothing about whether R2's own local commit survived — see §12; the item recovered because later work redid it, not because the lost commit was retrieved.)

**Why the recovery path fired for one item and not the other five is an open question**, and a significant one: if `reconcile_revision` were reliable, the false-success class would be self-healing. It is not, and nothing in the measured data explains the difference.

## 5. Timeline

All times America/Chicago, 2026-08-08. Anchors are from engine trace and runtime state reproduced for this writeup (not re-derived from live logs in this checkout).

| Time         | Event                                                                                                                                     |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| 15:13:08     | `48a34a246ae6` committed — original implementation                                                                                        |
| 15:20:56     | First automated review mints revision R1                                                                                                  |
| 15:23:07     | R1 starts — `exec_18c9ee2925032810_47`, driver codex, workspace `flunge-agent-011`                                                        |
| 15:25:42     | R1 commit authored                                                                                                                        |
| 15:27:17     | Prior merge-poller sweep (gives R1 **~1.33 s** of runway after staging)                                                                   |
| 15:28:10     | `6d54316a11b3` committed — "Harden crawler metadata delivery"                                                                             |
| 15:28:23.94  | `cube pr update` tool output returns (the push)                                                                                           |
| 15:28:23.972 | `pr_url_capture`: staged PR URL from worker progress stream; `revision_push_capture` staged push evidence                                 |
| 15:28:25.297 | pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector                                               |
| 15:28:26.376 | PR completion: execution terminalized; teardown in flight — `source: 'pr_recheck_staged'`, `target: 'PendingReview'`                      |
| 15:28:26.413 | live-state registry: slot entry cleared — `activity: 'working'`; driver workspace teardown entered (`reason_detail: 'pr_recheck_staged'`) |
| 15:28:27.784 | worker teardown complete — `path: 'pr_recheck_staged'`, `pane_outcome: 'Reaped'`                                                          |
| 15:28:29     | Automated review of `6d54316a11b3` begins                                                                                                 |
| 15:31:34     | Review completes — 6 findings                                                                                                             |
| 15:31:35     | Revision R2 minted                                                                                                                        |
| 15:31:49     | R2 starts — `exec_18c9ee9f79ea3c08_4f`, driver claude, workspace `flunge-agent-001`                                                       |
| 15:40:46.6   | `gh pr edit` output stages the PR URL                                                                                                     |
| 15:40:56     | `jj describe` — work committed locally (never pushed)                                                                                     |
| 15:41:04.23  | Merge-poller sweep                                                                                                                        |
| 15:41:04.58  | Reaped — `activity: 'working'`; never pushed                                                                                              |
| 15:41:05     | Workspace lease released; engine logs `pr_review noop skip … skip_reason: 'sha_unchanged', trigger: 'revision_push'`                      |

### A/B contrast the same afternoon (why it looked fine)

| Case            | Staging relative to sweep                                 | Post-push budget used                                | Finalization path   | Activity at teardown |
| --------------- | --------------------------------------------------------- | ---------------------------------------------------- | ------------------- | -------------------- |
| mono PR #2683   | staged 15:15:22.88, **0.46 s after** sweep at 15:15:22.42 | findings comment at 15:16:15 — **65.6 s** after push | `stop_staged`       | `idle`               |
| mono PR #2682   | succeeded around 15:20:15                                 | (full window)                                        | correct path        | (completed)          |
| flunge #1342 R1 | staged into a window with **~1.33 s** to next recheck     | reaped **2.4 s** after push                          | `pr_recheck_staged` | `working`            |
| flunge #1342 R2 | staged off `gh pr edit` ~17 s before sweep                | reaped before push                                   | `pr_recheck_staged` | `working`            |

The successful #2683 case consumed 65.6 s of a ~66 s window. It was not comfortably inside budget; it barely made it. Survival is phase-dependent on the poller, not a property of the worker doing the right thing.

## 6. Root cause

### 6.1 The mechanism

Verified against this checkout.

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

### 6.4 Finalization tears down the live worker

`finalize_pr_transition` (`completion/pr_transition.rs`) records completion, may map `InReview` → `PendingReview` when a reviewer is enqueued (`pr_transition.rs:226-230`), logs `pr completion: execution terminalized; teardown in flight` (`:255-260`), then calls `finish_worker_teardown` (`:279-286`), which releases the pane (`pane_outcome` includes `Reaped`) and frees the cube lease (`teardown.rs:86-156`).

The live-state registry clears the slot and logs `activity` at clear time (`live_worker_state.rs:654-662`) — the incidental signal that the worker was still `working` rather than idle.

### 6.5 Race against the merge-poller sweep

- **Configured full-sweep interval:** 60 seconds — `tools/boss/engine/core/src/app/server.rs:1059` (`Duration::from_secs(60)` passed into `spawn_merge_poller`).
- **Recheck invocation on each sweep:** `merge_poller/sweep.rs:417-420` iterates `pending_pr_recheck` and calls `sweep_pending_pr` (`:858-863`), which delegates to `recheck_for_pr`.
- **Observed wall-clock gaps** between sweeps in the incident material: roughly **66–91 seconds** (interval plus sweep work). The successful A/B case used ~65.6 s of runway.

**Line-reference correction:** the investigation brief cited `merge_poller/sweep.rs:418-419` and `:858-915` as the cadence site. Those lines are the **pending-PR recheck invocation path**, not the interval constant. The configured cadence lives at `app/server.rs:1059` (and is assumed throughout merge-poller comments as "today's 60s sweep"). The 66–91 s figure is observed wall-clock from trace, not a constant in source.

## 7. Contributing factors

### 7.1 Post-push deliverables live only in the prompt

Revision steps after the push are prompt text, not engine actions (`runner/prompt.rs`):

- Step 4 — push via `cube pr update` (`:1653-1655`)
- Step 5 — update PR title/description (`:1657-1671`)
- Step 6 — findings-status PR comment (`:1673-1721`)
- Steps 7–8 — confirm head / print URL (`:1723-1728`)

Anything the prompt asks for **after** the push is exposed to this race by construction. When the engine treats staging (or push) as "done," steps 5–8 are structurally orphaned. Whether post-push deliverables should remain prompt-only is an open product/engine design question (action item below).

### 7.2 False-success status without head movement

R2 terminalized toward review despite producing no push. The engine separately observed `sha_unchanged` when deciding whether to enqueue a reviewer (`pr_transition.rs:122-129` logs `pr_review noop skip` with `skip_reason`; noop classification includes `"sha_unchanged"` in `finalize_passes.rs` around the noop gate). The status transition and the SHA check are not coupled as a success criterion: **terminalization to PendingReview / InReview does not require that this execution moved the PR head.**

### 7.3 Protection existed and was bypassed by layering

The SHA-delta guard at `recheck.rs:144-175` encodes the exact lesson of 2026-07-14. The staged-URL fast path was added **above** it as a primary path that returns before the guard runs. Whatever review process added the fast path did not re-apply the prior incident's invariant ("do not finalize/absorb while a live worker may still be mid-session") to the new branch.

This is a **layering / short-circuit** failure, not a missing idea: the idea was already written a few dozen lines down.

### 7.4 Detection is incidental and not aggregated

The only field that distinguishes a mid-turn reap from a legitimate idle finalization is `activity` on the `live-state registry: slot entry cleared` log line (`live_worker_state.rs:654-662`). That field is not elevated to a metric, alert, or attention item. Therefore:

- Operators cannot see a dashboard of mid-turn reaps.
- Nothing in the running system partitions `pr_recheck_staged` finalizations into "safe idle" vs "destructive working." The partition in §3 exists only because the raw trace was pulled offline and each finalization was hand-paired to its registry-clear record.
- **Prevalence was measurable, but only by bespoke offline reconstruction** — and only within the retention window. It was not visible to anyone operating the system, which is why a 98.4% failure rate ran for the full retained window without raising anything.

Severity was worse. The engine records a head SHA _before_ an execution runs but never _after_ teardown, so "did this execution actually push?" had to be reconstructed from the _next_ execution's `pr_head_before` snapshot or a live GitHub read. That is why one row is permanently unresolvable and why one engine `sha_unchanged` verdict turned out to be a stale-read false negative (both in §3). See action item 5.

### 7.5 Source-reference verification notes

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
- **The blast radius turned out to be measurable after all**, from `activity`-at-clear paired against finalization source. The discriminator validates cleanly against control groups (Stop paths 64/64 idle), so the 63/64 figure rests on a signal that demonstrably separates the two families rather than on inference.

## 9. What went wrong

- A fast path finalizes and reaps live workers without inheriting protections a few lines below it — and the same missing check is absent from the detector branch of the same function, so the defect is structural to `recheck_for_pr`.
- Staging treats `gh pr view|list|edit` like a push, so metadata and read commands can arm teardown.
- Post-push prompt steps are structurally races, not guaranteed deliverables.
- Board status can report "in review" for a revision that never moved head — false success while production regressions remain. **Five work items are in exactly that state right now** (§4).
- The race is not an edge case. **63 of 64** staged-path finalizations in the retained window reaped a mid-turn worker, and the rate is flat across the whole window and across four engine restarts.
- The only mid-turn reap signal is a log field that is not metricked, so nothing raised an alarm while a 98.4% failure rate ran for days; sizing it required an offline trace reconstruction.
- No head SHA is recorded after teardown, so "did this execution push?" is not answerable from the engine's own records — one severity determination is permanently unresolvable as a result.
- A successful worker on the same afternoon used 65.6 s of ~66 s of runway — the system was already operating at the edge of its budget when "working."

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

Discovery of this incident was from a **symptom** (missing findings-status comment on R1), not from an engine-raised attention item. R2's total loss was found by forensic follow-up on the same PR, not by the board. Nothing in this table fired for any of the other 61 staged-path mid-turn reaps or the separately counted detector-branch reap — the measurement in §3 came from pulling raw trace off-box, not from any operator-facing surface.

### Response

This document is the response artifact for the investigation. Code remediation for the guard is **in flight as separate work** and is deliberately not redesigned or re-specified here (see action item 1).

## 11. Action items

Owners are **surfaces** (files / subsystems), not people. None of these are implemented by this document. Item 1's design is already in flight elsewhere — reference only.

These are engineering fixes to the defect. They are **not** a substitute for the recovery work in §4: the five work items listed there are already broken and will not be repaired by any code change here.

### Immediate — completion / recheck (in flight)

1. **Guard `recheck_for_pr` — the whole function, not just the staged arm — so no path through it can finalize a still-working live worker.** Surface: `tools/boss/engine/core/src/completion/recheck.rs` staged arm (`:55-72`) **and its detector branch**, coordinated with `finalize_pr_transition` / teardown in `completion/pr_transition.rs` and `completion/teardown.rs`. Inherit the 2026-07-14 invariant already written at `recheck.rs:144-175` (defer to the worker's own Stop boundary while mid-session). The measurement in §3 found a mid-turn reap through the detector branch as well (`pr_recheck`, 1 working / 0 idle), so **a fix scoped to the staged block alone would leave a live path open.** **Remediation design is in flight as separate work — do not duplicate that design here; land and verify it there, but confirm its scope covers both branches.**

### Near-term — staging predicate

2. **Narrow what arms PR-URL staging for finalization.** Surface: `tools/boss/engine/core/src/pr_url_capture.rs` (`is_gh_pr_command_str`, `:304-324`). At minimum, separate "URL observed for binding" from "permission to finalize and reap." Read-only / metadata commands (`gh pr view`, `list`, `edit`) must not be sufficient to trigger `pr_recheck_staged` teardown of a live execution.

### Near-term — false-success status

3. **Do not terminalize a revision implementation to PendingReview / InReview without evidence this execution contributed a head movement (or an explicit metadata-only path).** Surface: `completion/pr_transition.rs` (`finalize_pr_transition`, reviewer enqueue / noop skip around `:122-230`) and the SHA-delta / contribution gates in `completion/recheck.rs`. The `sha_unchanged` observation already exists on the review-skip path (`finalize_passes.rs` noop gate); couple an equivalent check to status success so a reaped no-push revision cannot read as review-ready.

### Near-term — detection / metrics

4. **Emit a first-class metric (and preferably an attention item) when a `pr_recheck_staged` finalization clears a slot with `activity != idle` (or equivalent mid-turn signal).** Surfaces: `live_worker_state.rs` (`release_slot`, `:654-662`), completion teardown path, merge-poller / completion metrics registry. Goal: make mid-turn reap **countable and alertable in-system**. The §3 measurement shows this is achievable from data the engine already emits — but only offline, by hand, and only back as far as log retention. Prevalence should not require a forensic exercise to see.

5. **Record a `pr_head_after` on the teardown record, read at the moment the fast path decides to terminalize.** Surfaces: `completion/pr_transition.rs` (`finalize_pr_transition`) and `completion/teardown.rs`. The engine records a head SHA before an execution and never after it, so every severity determination in §3 had to be reconstructed from the _next_ execution's `pr_head_before` snapshot or a live GitHub read — which is why one row (flunge #1296) is permanently unresolvable and why one `sha_unchanged` verdict (flunge #1327) was a stale-read false negative. A `pr_head_after` field makes "did this execution move the head?" a single query, and it incidentally hands the fast path the very signal it needs to decide correctly (cf. action item 3).

### Structural — prompt vs engine ownership of post-push work

6. **Decide whether post-push deliverables (PR description update, findings-status comment) remain prompt-only or become engine-owned / pre-finalize gates.** Surface: `tools/boss/engine/core/src/runner/prompt.rs` revision steps (`:1653-1728`) and the completion Stop / recheck contract. If they stay in the prompt, the engine must not finalize until Stop (or an explicit worker "done" signal). If they move into the engine, the race class shrinks by construction.

### Structural — turn-loop predicate

7. **If recheck continues to gate on worker liveness shape, extend the predicate beyond `is_live && kind != PrReview`.** Surface: `completion.rs` `worker_owns_turn_loop` (`:2113-2115`) and call sites in `recheck.rs` / `stop.rs`. Mid-turn vs idle must be visible to any path that can reap. Note: this is **not** a substitute for action item 1 if the staged path still short-circuits the gate.

## 12. Incomplete evidence (stated plainly)

- **R2 local commit recoverability:** unknown. The workspace lease for `flunge-agent-001` was released; whether the commit object remains in a shared jj store or any backup is outside the evidence packaged for this postmortem. Do not assert recovered or lost beyond "not pushed; lease released." Note that the flunge #1342 _work item_ recovered (§4) because `reconcile_revision` spawned a follow-up that redid and pushed the work — that says nothing about the fate of R2's own commit object.
- **Counts are a floor, not a total.** The observation window is bounded by log retention, not by the defect's lifetime: an app update reset the log root at ~12:38 CDT on 08-06, and the first occurrence is 23 minutes into the earliest retained record. How long the path had been firing before that is unknown.
- **One severity row is unresolvable.** A revision on flunge #1296 routed via `pr_review cycle bound reached`, skipping the SHA check, and the branch has since been force-pushed. The head at reap time is unrecoverable, so it cannot be assigned to total or partial loss. It may be a seventh total loss.
- **Per-driver severity rates are estimated, not determined.** grok 4/4 and codex 1/3 are exact counts over tiny denominators; the resulting 100% and 33% carry no useful precision. The proposed mechanism (non-claude drivers surfacing a `gh pr` URL earlier relative to their push) is a **hypothesis**, untested against the trace.
- **Why `reconcile_revision` recovered flunge #1342 and not the five items in §4:** unexplained. Nothing in the measured data distinguishes them. This is the largest open question left by the measurement.
- **Engine-side design of the in-flight guard fix:** deliberately not re-derived here; tracked as separate work.

## 13. Lessons

1. **A guard below a short-circuit is not a guard.** New primary paths must re-apply prior incident invariants, or they reintroduce the same hazard with a different name.
2. **"URL staged" is not "worker finished."** Staging is a binding hint; finalization and reap require a turn boundary or stronger done signal.
3. **Read/edit/view must not arm teardown.** Capture breadth optimized for recovery of missed PR opens is the wrong breadth for a path that kills the worker.
4. **Prompt steps after the push are optional under a race.** If the engine can finalize on push-related signals, post-push prompt work is best-effort only.
5. **False success is worse than visible failure** when the board hides total work loss and live production regressions. Worse still, it is self-perpetuating: mono #2651 (§4) shows a false-success loop re-minting the same revision every few minutes, forever, because each failure is recorded as a win.
6. **If the only distinguishing field is a log attribute, prevalence is invisible until it is a metric.** The data was there the whole time — 63 staged-path mid-turn reaps sitting in trace, plus the separately counted detector-branch reap — and it took a hand reconstruction to see any of it. "Not aggregated" and "not happening" are indistinguishable from the operator's chair.
7. **Record state after the action, not only before it.** Without a post-teardown head SHA, the engine cannot answer its own most important question — did this execution deliver anything? — which left one row permanently unresolvable and one verdict wrong.
8. **A phase race measured is rarely a coin flip.** This one was assumed roughly 50/50 from two cases and turned out to be 98.4%. Estimate the rate before deciding a race is tolerable.

## 14. Follow-up code changes

This document is **doc-only**. Recommended work is listed in §11 (engineering fixes) and §4 (recovery of the five falsely-reviewed work items) and should be filed as separate chores/tasks against the engine completion, PR-URL capture, merge-poller, and prompt-runner surfaces. The staged-path guard fix (item 1) is already in flight and must not be redesigned or re-implemented from this PR.
