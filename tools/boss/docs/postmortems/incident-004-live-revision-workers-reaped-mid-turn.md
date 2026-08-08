# Incident 004 — Engine reaped live revision workers mid-turn, discarding their work

- **Date:** 2026-08-08 (America/Chicago). Focal pair on flunge PR #1342 between ~15:23 and ~15:41; the same finalization path had been active for at least three days prior.
- **Severity:** High — silent work loss on live revision workers, plus false-success board status for a revision that produced nothing. Includes a case where a revision minted to fix live production regressions was reaped before push while the board reported success.
- **Status:** Documented. Guard remediation for the fast path is already in flight as separate work and is not redesigned here. This postmortem is doc-only.
- **Class:** Race between merge-poller PR recheck and a still-working agent: a staged PR URL is treated as "worker done," terminalizing and reaping a live mid-turn execution. Related prior: the 2026-07-14 SHA-delta absorption incident whose protection this fast path bypasses.
- **Related:** [`incident-001-pr-fan-out.md`](incident-001-pr-fan-out.md) (wrong-PR finalization killing live workers); 2026-07-14 SHA-delta baseline absorption (guard comment at `completion/recheck.rs:152-166`).

## 1. Verdict

The engine has a **fast path that treats a staged PR URL as permission to finalize and tear down a still-running worker**. When a revision worker's tool stream stages a PR URL — including from a push, or from a non-push `gh pr` command such as `edit` or even `view` — the next merge-poller recheck immediately calls `finalize_pr_transition(…, "pr_recheck_staged")`, terminalizes the execution, and reaps the pane while the agent is still mid-turn (`activity: 'working'`).

That fast path short-circuits two protections sitting **below** it in the same function: the `worker_owns_turn_loop` gate, and the SHA-delta arm that explicitly refuses to absorb a possibly in-flight push and defers to the worker's own Stop boundary. Those guards were written after an earlier race of the same class; the staged-URL path was added above them without inheriting them.

Whether any given worker survives is a coin flip against the ~60 s full-sweep cadence. Staging just after a sweep often works; staging into a sweep that is about to run does not. The feature is correct when lucky; it is destructive when not.

## 2. Summary

On 2026-08-08, two consecutive revision workers on flunge PR #1342 were reaped by the engine while mid-turn:

| Revision | Execution                  | Driver | Outcome                                                                                                                                                       |
| -------- | -------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R1       | `exec_18c9ee2925032810_47` | codex  | Pushed successfully, then reaped **2.4 s** later mid-prompt. Code reached the PR; post-push steps (including the required findings-status comment) never ran. |
| R2       | `exec_18c9ee9f79ea3c08_4f` | claude | Reaped **before any push**. Entire local fix commit discarded with the released workspace. Board still moved the work item as if review-ready.                |

This is not driver-specific and not product-specific. The two cases used different drivers and different workspaces. Across available engine-trace segments, `pr_recheck_staged` finalizations numbered in the tens to low hundreds per day; **what fraction hit live mid-turn workers is not measurable** with current instrumentation (see §9 and §11).

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

### Wider blast radius (unquantified)

Counts of `pr_recheck_staged` finalizations from the available trace (not a sample of mid-turn reaps — every successful staged-path finalization, including legitimate ones):

| Trace segment                     | `pr_recheck_staged` count |
| --------------------------------- | ------------------------- |
| 2026-08-06→07                     | 159                       |
| 2026-08-07                        | 17                        |
| early 2026-08-08                  | 36                        |
| current segment (incident window) | 28                        |

An unknown fraction of those hit live workers. **Do not treat these counts as an incidence rate of work loss.** They establish only that the finalization path fires often, across products, for days. Prevalence of the destructive subset is currently unknowable (see §9).

## 4. Timeline

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

## 5. Root cause

### 5.1 The mechanism

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

### 5.2 `worker_owns_turn_loop` would not have been enough even if reached

```2113:2115:tools/boss/engine/core/src/completion.rs
pub(crate) fn worker_owns_turn_loop(execution: &crate::work::WorkExecution) -> bool {
    ExecutionStatus::is_live(&execution.status) && execution.kind != ExecutionKind::PrReview
}
```

It admits any live non-reviewer execution. It has **no notion of mid-turn vs idle**. A revision that is actively tooling (`activity: 'working'`) still "owns" the turn loop. Reaching this gate would not have stopped R1 or R2; the staged fast path simply never reaches it.

(Note on the recheck gate's polarity: when `worker_owns_turn_loop` is **false**, recheck skips. Live revision workers return **true**, so they would proceed into later arms. The gate excludes non-live and reviewer executions; it does not protect mid-turn producers.)

### 5.3 Staging is armed far more broadly than "the worker pushed"

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

### 5.4 Finalization tears down the live worker

`finalize_pr_transition` (`completion/pr_transition.rs`) records completion, may map `InReview` → `PendingReview` when a reviewer is enqueued (`pr_transition.rs:226-230`), logs `pr completion: execution terminalized; teardown in flight` (`:255-260`), then calls `finish_worker_teardown` (`:279-286`), which releases the pane (`pane_outcome` includes `Reaped`) and frees the cube lease (`teardown.rs:86-156`).

The live-state registry clears the slot and logs `activity` at clear time (`live_worker_state.rs:654-662`) — the incidental signal that the worker was still `working` rather than idle.

### 5.5 Race against the merge-poller sweep

- **Configured full-sweep interval:** 60 seconds — `tools/boss/engine/core/src/app/server.rs:1059` (`Duration::from_secs(60)` passed into `spawn_merge_poller`).
- **Recheck invocation on each sweep:** `merge_poller/sweep.rs:417-420` iterates `pending_pr_recheck` and calls `sweep_pending_pr` (`:858-863`), which delegates to `recheck_for_pr`.
- **Observed wall-clock gaps** between sweeps in the incident material: roughly **66–91 seconds** (interval plus sweep work). The successful A/B case used ~65.6 s of runway.

**Line-reference correction:** the investigation brief cited `merge_poller/sweep.rs:418-419` and `:858-915` as the cadence site. Those lines are the **pending-PR recheck invocation path**, not the interval constant. The configured cadence lives at `app/server.rs:1059` (and is assumed throughout merge-poller comments as "today's 60s sweep"). The 66–91 s figure is observed wall-clock from trace, not a constant in source.

## 6. Contributing factors

### 6.1 Post-push deliverables live only in the prompt

Revision steps after the push are prompt text, not engine actions (`runner/prompt.rs`):

- Step 4 — push via `cube pr update` (`:1653-1655`)
- Step 5 — update PR title/description (`:1657-1671`)
- Step 6 — findings-status PR comment (`:1673-1721`)
- Steps 7–8 — confirm head / print URL (`:1723-1728`)

Anything the prompt asks for **after** the push is exposed to this race by construction. When the engine treats staging (or push) as "done," steps 5–8 are structurally orphaned. Whether post-push deliverables should remain prompt-only is an open product/engine design question (action item below).

### 6.2 False-success status without head movement

R2 terminalized toward review despite producing no push. The engine separately observed `sha_unchanged` when deciding whether to enqueue a reviewer (`pr_transition.rs:122-129` logs `pr_review noop skip` with `skip_reason`; noop classification includes `"sha_unchanged"` in `finalize_passes.rs` around the noop gate). The status transition and the SHA check are not coupled as a success criterion: **terminalization to PendingReview / InReview does not require that this execution moved the PR head.**

### 6.3 Protection existed and was bypassed by layering

The SHA-delta guard at `recheck.rs:144-175` encodes the exact lesson of 2026-07-14. The staged-URL fast path was added **above** it as a primary path that returns before the guard runs. Whatever review process added the fast path did not re-apply the prior incident's invariant ("do not finalize/absorb while a live worker may still be mid-session") to the new branch.

This is a **layering / short-circuit** failure, not a missing idea: the idea was already written a few dozen lines down.

### 6.4 Detection is incidental and not aggregated

The only field that distinguishes a mid-turn reap from a legitimate idle finalization, in the material available, is `activity` on the `live-state registry: slot entry cleared` log line (`live_worker_state.rs:654-662`). That field is not elevated to a metric, alert, or attention item. Therefore:

- Operators cannot see a dashboard of mid-turn reaps.
- Trace counts of `pr_recheck_staged` cannot be partitioned into "safe idle" vs "destructive working."
- Prevalence is **unknown and unknowable from current data.**

### 6.5 Source-reference verification notes

| Brief citation                                         | Verified in this checkout | Notes                                                                                                                                |
| ------------------------------------------------------ | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `recheck.rs:55-73` staged fast path                    | **Yes** (`:55-72`)        | Matches.                                                                                                                             |
| `recheck.rs:81-89` turn-loop gate                      | **Yes** (`:81-88`)        | Matches.                                                                                                                             |
| `recheck.rs:144-176` SHA-delta protection              | **Yes** (`:144-175`)      | Comment text is multi-line; substance matches the brief's paraphrase.                                                                |
| `completion.rs:2113-2115` `worker_owns_turn_loop`      | **Yes**                   | Predicate is live status ∧ not `PrReview` only.                                                                                      |
| `pr_url_capture.rs:304-324` staging predicate          | **Yes**                   | Includes `view` / `list` / `edit`.                                                                                                   |
| `prompt.rs:1673-1721` / push at `:1653-1655`           | **Yes**                   | Step 6 findings comment after step 4 push.                                                                                           |
| `merge_poller/sweep.rs:418-419`, `:858-915` as cadence | **Partial**               | Those lines are recheck invocation, not the 60 s interval. Cadence: `app/server.rs:1059`. Observed 66–91 s is wall-clock from trace. |

## 7. What went well

- **Forensic reconstruction of the focal pair is tight.** Sub-second timestamps on staging, recheck, terminalization, activity-at-clear, and teardown path form a complete causal chain for R1 and R2 without needing to re-open live logs in this run.
- **A clean A/B from the same afternoon exists.** mono #2683 / #2682 vs flunge #1342 shows the same feature succeeding and failing as a pure phase race, which makes the root cause teachable rather than speculative.
- **The prior incident left a correct written invariant** in the SHA-delta arm. The right idea was already in the file; the gap is that a newer path does not share it.
- **Activity-at-clear logging already records the distinguishing signal.** Instrumentation for detection is partially present; it is not aggregated or alerted.
- **Driver diversity in the failure pair** (codex then claude) rules out a single-driver misconfiguration as the explanation.

## 8. What went wrong

- A fast path finalizes and reaps live workers without inheriting protections a few lines below it.
- Staging treats `gh pr view|list|edit` like a push, so metadata and read commands can arm teardown.
- Post-push prompt steps are structurally races, not guaranteed deliverables.
- Board status can report "in review" for a revision that never moved head — false success while production regressions remain.
- The only mid-turn reap signal is a log field that is not metricked, so multi-day blast radius cannot be sized.
- A successful worker on the same afternoon used 65.6 s of ~66 s of runway — the system was already operating at the edge of its budget when "working."

## 9. Detection and response

### Detection (as of the incident)

| Signal                                                                | Present?  | Actionable?                                      |
| --------------------------------------------------------------------- | --------- | ------------------------------------------------ |
| `source: 'pr_recheck_staged'` on terminalization                      | Yes (log) | Counts finalizations, not mid-turn harm          |
| `activity: 'working'` on slot clear                                   | Yes (log) | Distinguishes mid-turn reaps; **not aggregated** |
| `pane_outcome: 'Reaped'` with working activity                        | Yes (log) | Same as above                                    |
| Metric / attention for mid-turn `pr_recheck_staged`                   | **No**    | Prevalence unknown                               |
| Operator-facing banner when a revision finalizes with `sha_unchanged` | **No**    | R2 looked successful on the board                |

Discovery of this incident was from a **symptom** (missing findings-status comment on R1), not from an engine-raised attention item. R2's total loss was found by forensic follow-up on the same PR, not by the board.

### Response

This document is the response artifact for the investigation. Code remediation for the guard is **in flight as separate work** and is deliberately not redesigned or re-specified here (see action item 1).

## 10. Action items

Owners are **surfaces** (files / subsystems), not people. None of these are implemented by this document. Item 1's design is already in flight elsewhere — reference only.

### Immediate — completion / recheck (in flight)

1. **Guard the staged-URL fast path so it cannot finalize a still-working live worker.** Surface: `tools/boss/engine/core/src/completion/recheck.rs` staged arm (`:55-72`), coordinated with `finalize_pr_transition` / teardown in `completion/pr_transition.rs` and `completion/teardown.rs`. Inherit the 2026-07-14 invariant already written at `recheck.rs:144-175` (defer to the worker's own Stop boundary while mid-session). **Remediation design is in flight as separate work — do not duplicate that design here; land and verify it there.**

### Near-term — staging predicate

2. **Narrow what arms PR-URL staging for finalization.** Surface: `tools/boss/engine/core/src/pr_url_capture.rs` (`is_gh_pr_command_str`, `:304-324`). At minimum, separate "URL observed for binding" from "permission to finalize and reap." Read-only / metadata commands (`gh pr view`, `list`, `edit`) must not be sufficient to trigger `pr_recheck_staged` teardown of a live execution.

### Near-term — false-success status

3. **Do not terminalize a revision implementation to PendingReview / InReview without evidence this execution contributed a head movement (or an explicit metadata-only path).** Surface: `completion/pr_transition.rs` (`finalize_pr_transition`, reviewer enqueue / noop skip around `:122-230`) and the SHA-delta / contribution gates in `completion/recheck.rs`. The `sha_unchanged` observation already exists on the review-skip path (`finalize_passes.rs` noop gate); couple an equivalent check to status success so a reaped no-push revision cannot read as review-ready.

### Near-term — detection / metrics

4. **Emit a first-class metric (and preferably an attention item) when a `pr_recheck_staged` finalization clears a slot with `activity != idle` (or equivalent mid-turn signal).** Surfaces: `live_worker_state.rs` (`release_slot`, `:654-662`), completion teardown path, merge-poller / completion metrics registry. Goal: make mid-turn reap **countable and alertable** so prevalence is measurable. Until this lands, do not claim a numeric incidence rate.

### Structural — prompt vs engine ownership of post-push work

5. **Decide whether post-push deliverables (PR description update, findings-status comment) remain prompt-only or become engine-owned / pre-finalize gates.** Surface: `tools/boss/engine/core/src/runner/prompt.rs` revision steps (`:1653-1728`) and the completion Stop / recheck contract. If they stay in the prompt, the engine must not finalize until Stop (or an explicit worker "done" signal). If they move into the engine, the race class shrinks by construction.

### Structural — turn-loop predicate

6. **If recheck continues to gate on worker liveness shape, extend the predicate beyond `is_live && kind != PrReview`.** Surface: `completion.rs` `worker_owns_turn_loop` (`:2113-2115`) and call sites in `recheck.rs` / `stop.rs`. Mid-turn vs idle must be visible to any path that can reap. Note: this is **not** a substitute for action item 1 if the staged path still short-circuits the gate.

## 11. Incomplete evidence (stated plainly)

- **R2 local commit recoverability:** unknown. The workspace lease for `flunge-agent-001` was released; whether the commit object remains in a shared jj store or any backup is outside the evidence packaged for this postmortem. Do not assert recovered or lost beyond "not pushed; lease released."
- **True incidence of mid-turn reaps across the multi-day window:** **not measurable** with current instrumentation. Counts of `pr_recheck_staged` finalizations are not a substitute. An unknown fraction hit live workers. No estimate is offered.
- **Engine-side design of the in-flight guard fix:** deliberately not re-derived here; tracked as separate work.
- **Full product-by-product blast list:** not available; only that the path is not flunge-specific (codex + claude, different workspaces; staged finalizations across the engine-wide trace).

## 12. Lessons

1. **A guard below a short-circuit is not a guard.** New primary paths must re-apply prior incident invariants, or they reintroduce the same hazard with a different name.
2. **"URL staged" is not "worker finished."** Staging is a binding hint; finalization and reap require a turn boundary or stronger done signal.
3. **Read/edit/view must not arm teardown.** Capture breadth optimized for recovery of missed PR opens is the wrong breadth for a path that kills the worker.
4. **Prompt steps after the push are optional under a race.** If the engine can finalize on push-related signals, post-push prompt work is best-effort only.
5. **False success is worse than visible failure** when the board hides total work loss and live production regressions.
6. **If the only distinguishing field is a log attribute, prevalence is fiction until it is a metric.**

## 13. Follow-up code changes

This document is **doc-only**. Recommended work is listed in §10 and should be filed as separate chores/tasks against the engine completion, PR-URL capture, merge-poller, and prompt-runner surfaces. The staged-path guard fix (item 1) is already in flight and must not be redesigned or re-implemented from this PR.
