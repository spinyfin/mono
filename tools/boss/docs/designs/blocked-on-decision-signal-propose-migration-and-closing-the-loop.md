# Blocked-on-decision: the proposal is the unit of identity, urgency, and answer

- **Date:** 2026-08-20
- **Status:** design (precedes implementation)
- **Provenance:** project_design for "Blocked-on-decision: signal, propose migration, and closing the loop"
- **Related designs:** [worker-proposal-api](worker-proposal-api-replace-fragile-worker-to-engine-seams.md), [attentions](attentions.md), [dispatch-halt-state-vs-attention-items](dispatch-halt-state-vs-attention-items.md), [notification-dedup-scoring](notification-dedup-scoring.md), [attention lifecycle](../attention-lifecycle.md)
- **Worked example / acceptance test:** the `gpt-5.6-terra` merge-conflict worker that hit `change/file-count` at 31 files, emitted a well-formed `[blocked]` marker at 03:40, was unseen until ~19:11, and then refused the operator's bypass instruction four times because `AGENTS.md` forbids every bypass with no carve-out

## TL;DR

A worker blocked on an operator decision is not an attention item that failed to beep. It is a **pending proposal** whose answer is engine-owned state. Authorisation is real only as an engine-delivered, typed response to a specific proposal the worker itself raised, carrying that proposal's identity. Free text in a pane is not authorisation. Workers cannot grant themselves one.

When this is implemented, the incident runs to completion without anyone reading a pane: the worker proposes a typed block, the operator gets an OS notification and an unmissable card, the operator picks "authorise `BYPASS_CHANGE_FILE_COUNT` for this PR", the engine writes that decision onto the proposal and into the work-item description, the worker reads `boss propose --list` and acts, and the PR body names the decision.

## Goals

A worker that is genuinely blocked on an operator decision reaches the operator promptly, gets a typed answer, and acts on it — without the operator reading a pane, and without workers being able to self-authorise exceptions.

That is three legs, kept as separate ordered PRs:

1. **Signal.** Blocked-on-decision is a distinct, high-priority, unmissable operator surface. It is not another row in the existing attention soup.
2. **Propose.** The blocked-on-decision flow migrates onto `boss propose blocked` as the first seam of the existing proposal API, with the `[blocked]` marker kept as a coexistence path, not a flag day.
3. **Close the loop.** The operator's answer is a typed disposition of that proposal. The worker treats that disposition as authoritative for that one decision, and treats every other shape of "the operator said so" as not authorisation.

Also settle the live contradiction the incident exposed: `change/file-count` is `allow_bypass: true` while `AGENTS.md` forbids invoking any bypass. The check is right; the rules need the carve-out this design builds.

## Non-goals

- **Notifying on every attention item.** That is the current problem with more volume. OS notifications in this project are for blocked-on-decision only.
- **A wholesale cutover of every marker seam.** Effort-escalation, deferred-scope, follow-ups, triage, and PR-created keep their existing migration flags and recipes. This project takes blocked as the first candidate and stops there.
- **Deleting the `[blocked]` marker.** It is the bootstrap fallback when `boss propose` is unreachable. It stays indefinitely, as the proposal-API design already required.
- **Folding blocked-on-decision into AttentionGroups (`question` / `followup`).** [attentions.md](attentions.md) explicitly non-goals "Blocking a worker on an answer." Mixing a slot-holding wait into that store would recreate the exact confusion that store was split to avoid.
- **A severity/priority dimension on all `work_attention_items` kinds.** A high-priority tier that other kinds can be promoted into will fill up. Blocked-on-decision is a separate concept, not a badge on the existing enum.
- **Migrating dispatch/execution mechanics off attention items.** [dispatch-halt-state-vs-attention-items.md](dispatch-halt-state-vs-attention-items.md) already owns that. This project does not reopen it.
- **Holding a worker pane forever so a probe can land.** Delivery must survive the worker having exited. Slot reclaim is noted under Not now; v1 does not require the pane to stay up.
- **A mechanical PreToolUse hook that allows a bypass only when a token file exists.** Prompt-plus-`boss propose --list` is the v1 authority check. A hook is stronger and is Not now.
- **Using product-level `boss decision` records (`D<n>`).** Those are standing, product-scoped rulings that outlive any one work item. A blocked answer is scoped to one proposal on one run. Reusing `D<n>` would make a one-shot exception look like a standing licence.
- **Coordinator-private actions as implementation tasks.** Flag flips in the running engine, DB backfills, and anything under Application Support are operator steps in this doc, not rows.

## What exists today (verified against source and `--help`)

### Two attention stores, three surfaces, none of them show `worker_blocked`

The engine has ~49 `work_attention_items.kind` values registered in `ATTENTION_LIFECYCLES` (`tools/boss/engine/core/src/attention_lifecycle.rs`). They split into the four `ClearedBy` shapes in [attention-lifecycle.md](../attention-lifecycle.md). `worker_blocked` and `worker_escalation` are `ClearedBy::ProducerReconciles`: the coordinator's probe is the documented ack, and an automatic clear would un-pause the auto-nudge without an answer.

Separately, [attentions.md](attentions.md) shipped **AttentionGroups** (`question`, `followup`) — agent-authored, actionable, grouped. That is a different table. The macOS Notifications toolbar badge is `openAttentionGroupCount` over those groups, not over `work_attention_items`.

What the app actually renders from `work_attention_items`:

| Kind / family                             | App surface                                                                                                                                      |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `deferred_scope`                          | Kanban-card badge + popup (Accept / Create task)                                                                                                 |
| `external_tracker_*`                      | Product-level banners                                                                                                                            |
| `question` / `followup` (AttentionGroups) | Notifications toolbar window                                                                                                                     |
| Review-lane entry (not an attention kind) | OS notification via `ReviewNotificationCenter`, **only when the app is backgrounded**                                                            |
| `dispatch_failed_*` columns               | Card "Failed to start" banner (not an attention item)                                                                                            |
| `worker_blocked`, `worker_escalation`     | **No Swift references.** `attentionItemCreated` is received and immediately filtered to `deferred_scope` in `ChatViewModel+DeferredScope.swift`. |

[dispatch-halt-state-vs-attention-items.md](dispatch-halt-state-vs-attention-items.md) already recorded the sibling finding: _"worker_blocked and worker_escalation attention items are unreachable from every read surface"_ and classified both kinds as correctly being attention items. The prescribed fix was "make the existing attention item reachable", not a representation change. That fix did not land. The incident is that gap in production.

OS notifications exist for one event only: a task entering Review. There is no notification path for a worker that has stopped and is waiting.

### Volume, and why a count from this workspace is not available

This design is required to measure volume by kind over a representative window rather than assume which kinds dominate. Cube workers cannot query the coordinator DB (`~/Library/Application Support/Boss/` is off-limits; `bossctl` is coordinator-only). `boss attention list` lists AttentionGroups, not `work_attention_items`. There is no worker-accessible histogram.

That is a finding, not a skip. The structural evidence is stronger than a ranking would have been for _this_ signal:

- `worker_blocked` is not on any app surface, so it cannot have been "lost in the volume of a list it does not appear on."
- The Notifications window's volume is questions and follow-ups, a different store.
- The kinds that _would_ drown a shared high-priority tier, if we built one, are the dispatch/execution mechanics already classified as Bucket A in the dispatch-halt design (`churn_guard_parked`, `dispatch_stage_stalled`, `pane_death_*`, trunk-queue infra, spawn/driver lifecycle). Those are over-raised relative to "a human must answer a question now": they describe engine state the board should show as mechanics, and a later successful run often supersedes them. `ci_remediation_exhausted` is the brief's own example of an informational stop: the system has given up and nothing is waiting on a person.
- `worker_blocked` is the opposite: a live worker is holding a slot, burning nothing, waiting on a specific person to answer a specific question. Latency is the whole cost.

If an operator later dumps production counts, they should change the de-noise list, not the concept. The concept does not depend on `worker_blocked` being rare; it depends on it being a different kind of wait.

### `boss propose` — implemented, flag-gated, unused on this path

Verified from `boss propose --help` and the source, not from an empty grep.

**CLI (shipped).** `boss propose blocked --reason <REASON>` exists. So do `effort-escalation`, `followup-task`, `deferred-scope`, `attention`, `automation-outcome`, `pr-created`, and `boss propose --list`. Submission is synchronous. `--help` text still describes blocked as "Recorded as `proposed`; once the apply pipeline lands this files a worker-signal attention and pauses the auto-nudge loop."

**Ledger (shipped).** `worker_proposals` holds `id`, `execution_id`, `work_item_id`, `kind`, `payload_json`, `idempotency_key`, `state` (`proposed` / `applied` / `rejected` / `superseded` / `expired`), `decided_by` (`policy` / `coordinator` / `human`), `decision_reason`, `applied_ref`, timestamps. `boss propose --list` returns the worker's own work item's rows across executions. There is no answer payload and no Answer RPC.

**Apply (shipped).** `apply_policy(Blocked) = AutoApply`. `apply_blocked` writes a `worker_blocked` attention item with the reason and a "acking the worker (e.g. `bossctl probe`) resolves it" footer. That commit stamps `state = applied`, `decided_by = policy`. The proposal is then "done" as far as the ledger is concerned — before any human has answered.

**Expiry (shipped, and wrong for this loop).** `proposal_expiry_sweep` expires undecided `blocked` and `effort_escalation` rows when the owning execution terminals, because the v1 proposal-API design treated them as in-flight-only (nudge-pause on the live run). A blocked decision that must survive the worker exiting cannot expire with the run. `followup_task` already outlives its execution; blocked must too.

**Flags (all default off).**

| Flag                            | Default | What it actually gates                                                                                                                                                                            |
| ------------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `worker_proposals`              | off     | Master kill switch. Every per-seam flag is read as `worker_proposals && <seam>`.                                                                                                                  |
| `worker_signal_proposals_seam`  | off     | Proposals-first read in `detect_and_file_worker_signals`, **and** the worker prompt teaching `boss propose blocked` / `effort-escalation` instead of the markers.                                 |
| `deferred_scope_proposals_seam` | off     | Same recipe for `[deferred-scope]`. Out of scope here.                                                                                                                                            |
| `followup_proposals_seam`       | off     | Same recipe for follow-ups. Out of scope here.                                                                                                                                                    |
| `worker_rpc_tier`               | off     | Worker-classified connections denied mutating taxonomy verbs. Until this is on, `boss task update` from a worker shell is `RpcTier::User`. Authority in this design must not depend on that flag. |
| `heuristic_blocker_detection`   | off     | Phrase-allowlist net under a missing marker. Unrelated.                                                                                                                                           |

There is no `automation_outcome_proposals_seam` or `pr_created_proposals_seam` flag in the registry yet. Those seams are not this project.

**Prompt (flag-gated).** With the seam off (production today), workers are taught `[blocked] reason="…"`. With the seam on, they are taught `boss propose blocked --reason "…"` and the `[blocked]` line as bootstrap fallback only. The bar already in that directive — reason must name an external fact; duration, context usage, and "a step was not completed" are not blockers — is the prior work this design builds on. There is no repo match for the phrase "raise the bar for workers asking the operator questions"; the load-bearing text is `worker_escalation_protocol_directive` in `tools/boss/engine/core/src/runner/prompt.rs`.

**Ack today.** `WorkDb::resolve_worker_signal_attentions_for_execution` marks every open `worker_blocked` / `worker_escalation` item for that execution `resolved` when **any** probe is delivered. The operator's answer is the probe's free-text body. That is how the incident's operator "answered."

### Probe delivery is best-effort, and Grok rejects mid-turn

`AgentDriver::mid_turn_pane_input()` defaults to `Rejects`. Claude and Codex override to `Buffers`. Grok is undeclared, so `Rejects` (`tools/boss/engine/driver/src/grok.rs`). A parked worker (Idle after Stop) can take a probe; a working Grok worker cannot. A probe targeting a run that then terminals is `Abandoned`. The work item's description is interpolated into the successor execution prompt under `details:` (`runner/work_item.rs`); a live worker does not re-read the description mid-session.

The incident worker had already Stopped, so a probe _did_ land. Leg 3 failed on authority, not delivery. Delivery still has to be designed for the worker that has exited, because occupying a slot for fifteen hours is the other half of the incident.

## Alternatives considered

### A. New `work_attention_items` kind, or a severity column on the existing kinds

Add `worker_blocked_urgent`, or `priority = high` on `worker_blocked`, and OS-notify everything at that priority.

**Rejected.** `worker_blocked` already exists and is correctly classified (dispatch-halt design). A new kind is the smallest diff that does not change the concept. A severity dimension is worse: every producer will want "high", and the new tier becomes the old soup one level up. The load-bearing property is not "this attention is louder"; it is "a specific proposal is waiting on a specific person." That property lives on the proposal row, which already has identity, payload, and (once this lands) an answer. Attention items stay a coexistence shim so marker-only workers still file something the new surface can read.

### B. Route blocked into AttentionGroups (`question`) and answer them like design-doc questions

Reuse the Notifications window, question types, and group-action → revision pipeline.

**Rejected.** [attentions.md](attentions.md) non-goals synchronous "agent waits for human": a design question does not halt the worker, and actioning a group produces a later revision, not a resume of _this_ run. Blocked-on-decision is the opposite wait. Putting it in that window also puts it in the volume the operator already trains themselves to skim. Precedent that looks similar (typed question, typed answer) fails on the dimension that matters here: whether the worker is holding a slot pending the answer.

### C. "Operator instruction overrides prohibitions" in `AGENTS.md`, with no typed channel

Teach workers that a probe, a pane message, or a description sentence from the operator is enough to bypass.

**Rejected.** That is the fail-open the anti-bypass rules exist to prevent. Workers will talk themselves into believing they received authorisation they did not — from their own reasoning, from a quoted instruction, from a plausible-sounding coordinator sentence. The incident is bad; a worker that bypasses `file-count` because it _imagined_ approval is worse. Free text has no identity, no `decided_by`, and no way to tell a genuine operator answer from a sentence that looks like one.

### D. Engine applies the exception itself (worker never writes the bypass)

On "authorise `BYPASS_CHANGE_FILE_COUNT`", the engine edits the commit message or PR body.

**Rejected for v1.** The durable checkleft surface is the commit description (see `tools/checkleft/userdoc/docs/bypass.md`); the PR body is best-effort. The worker already owns the commit and the PR. Engine-side GitHub writes for a bypass would be a second, less-reviewed path to the same bytes, and would not generalise to the other blocked shapes (scope cut, "stop and leave it", missing credential). The worker writes the exception; the engine writes the _authorisation record_ the worker is required to have read.

## Chosen approach

**Contested property, named:** authorisation is only real as an engine-stamped disposition of a `worker_proposals` row the worker itself submitted (or that the engine minted from a `[blocked]` marker), with `decided_by` in `{human, coordinator}` and the proposal's id in hand. `decided_by = policy` is "we heard you and filed the signal", never "you may bypass." Pane text, the worker's own reasoning, and a description sentence that merely _looks_ like approval are not authorisation.

That property is checkable: `boss propose --list --kind blocked --json` returns engine-owned rows. Workers cannot set `decided_by` on submit. The residual risk is a worker that does not consult the list and hallucinates approval — prompt-enforced, and said plainly in Risks.

### Leg 1 — blocked-on-decision is its own operator surface

The destination concept is **a pending blocked proposal** (and, during coexistence, an open `worker_blocked` attention item that has not yet been dual-written to a proposal). It is not a new attention kind, not a severity bit, and not an AttentionGroup.

**Engine.** A dedicated list RPC and live event, modelled on `list_deferred_scope_attentions` / `deferredScopeAttentionsList`, returning currently-waiting blocked items for the selected product. The engine filters; the app does not grow an attention-kind switchboard. The row the UI binds to carries: work item id and name, execution id, proposal id when one exists, reason, age (`created_at` / `last_raised_at`), and whether the worker is still live. Filing `worker_blocked` (marker path or propose path) publishes the event. Resolving it publishes the update that takes the banner down.

**App (thin client).** Two surfaces, both driven off that list:

1. **OS notification** on raise, identifier `boss.blocked:<proposal-or-attention-id>`. Unlike `ReviewNotificationCenter`, this notification **presents even when Boss is frontmost** (banner + sound). The incident operator was in the app and looking at the wrong pane for fifteen hours. Suppressing the banner because the app is active is how that failure repeats. Tapping it focuses the work item.
2. **Unmissable kanban treatment on the Doing card:** a dedicated blocked-waiting banner (not a tiny badge in the strip next to deferred-scope). It shows the reason, the age, and that a decision is waiting. Age is visible. Age does not page again.

No second notification on a timer. A block unanswered for an hour is worse than one unanswered for a minute — that is why age is on the banner — but staleness must not manufacture a second class of urgency. One raise, one OS notification, the banner stays until the operator answers or dismisses.

**What this deliberately does not do.** It does not add `worker_blocked` to the Notifications toolbar. It does not OS-notify `ci_remediation_exhausted`, trunk-queue kinds, pane-death, or questions. Those stay where they are. De-noise here is _exclusion from the new surface_, not a cleanup of the old one. Cleaning Bucket A kinds off `work_attention_items` remains the dispatch-halt project's job.

**Coordinator.** The coordinator prompt currently documents `bossctl probe` as the way to talk to a worker. Leg 1 does not change that yet; the new surface is for the operator. Teaching the coordinator not to treat a probe as the blocked _answer_ is Leg 3.

### Leg 2 — migrate this flow to `boss propose`, feature-by-feature

Blocked is the first seam. The machinery (CLI, ledger, apply, flags, proposals-first Stop path, marker fallback counters) already exists. What this project adds is a payload that an operator can _answer without guessing_, and a coexistence story that includes the answer path.

**Payload.** Replace today's `{ reason: String }` with a typed decision request. `reason` stays as the one-line summary (and as the marker-coexistence key). New required fields:

| Field           | What it is                                               | Incident example                                                                                                                                                                                                                                  |
| --------------- | -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `reason`        | One-line summary, same contract as today (external fact) | `checkleft change/file-count at 31 files, max_files=30; repo policy forbids self-bypass`                                                                                                                                                          |
| `blocked_on`    | The concrete constraint                                  | `change/file-count` (`max_files=30`, observed 31)                                                                                                                                                                                                 |
| `already_tried` | Non-empty list of legitimate options already exhausted   | Reconciled the stacked conflict; cannot split the 31st file without leaving the tree unbuildable; `AGENTS.md` forbids setting the bypass                                                                                                          |
| `decision`      | The question, as a question                              | Authorise `BYPASS_CHANGE_FILE_COUNT` for this PR, cut scope, or stop?                                                                                                                                                                             |
| `options`       | Two or more `{ id, label, if_chosen }`                   | `authorise_bypass` → write `BYPASS_CHANGE_FILE_COUNT=<reason citing prp_…>` in the commit description and proceed; `cut_scope` → revert files (not viable for this conflict resolution; worker must say so); `stop` → leave the PR, do not bypass |

`if_chosen` is load-bearing. An operator answering must not have to guess what their answer will cause.

**CLI.** `boss propose blocked` gains `--blocked-on`, `--already-tried` (repeatable), `--decision`, and `--option id=…;label=…;if_chosen=…` (repeatable), with `--*-file` variants for long text. `--reason` stays. Validation rejects: empty `already_tried`, fewer than two options, options missing `if_chosen`, `reason` / `decision` that fail the existing length/quote rules. That is how the over-asking bar is _enforced_ rather than only stated: a worker that has not tried anything, or that is asking "which of two fine approaches", cannot submit a well-formed blocked proposal. Quality of the `already_tried` sentences remains prompt-level; presence is schema-level.

**Apply-policy change, in this leg's payload PR or the next, but designed here:** `blocked` flips from `AutoApply` (stamp `applied` / `decided_by=policy` when the attention is filed) to the **followup_task pattern**: stage the visibility effect at submit (file `worker_blocked`, pause nudge), leave `state = proposed`, `decided_by` unset. "We heard you" and "the operator decided" stop being the same event. `effort_escalation` stays AutoApply — it is not this project's wait.

**Expiry.** Remove `blocked` from the in-flight-only expiry set. A pending blocked proposal outlives its execution, like `followup_task`. Otherwise an operator who answers after the worker has gone idle has nothing to answer.

**Coexistence, no flag day.**

- Marker parsing in `detect_and_file_worker_signals` stays, flag on or off, forever for `[blocked]`.
- When the seam flags are on and a matching proposal already exists, the marker is skipped (already shipped).
- When the seam flags are on and the marker is the only signal (bootstrap, old prompt, uncontrolled driver), the legacy filer still runs **and** upserts a `worker_proposals` row with `reason` filled and the rich fields empty / a single synthesised `options` of `{ id: "free_text", label: "operator free-text answer", if_chosen: "worker follows the description block" }`. The answer path then always has a proposal id.
- When the seam flags are off, today's marker-only path remains byte-for-byte. The new list RPC still surfaces those `worker_blocked` attention items, so Leg 1 works before any flag flip.
- Workers on old prompts keep emitting `[blocked]`. Drivers we do not control keep emitting `[blocked]`. Neither needs a coordinated cutover.

**Flags, defaults, flip criterion.**

| Stage                          | `worker_proposals`        | `worker_signal_proposals_seam` | Prompt teaches                                           | Evidence to leave this stage                                                                                                                                                                                                                                  |
| ------------------------------ | ------------------------- | ------------------------------ | -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Today                          | default off               | default off                    | `[blocked]` marker                                       | —                                                                                                                                                                                                                                                             |
| After this project's PRs merge | still default off         | still default off              | still markers                                            | Code is in; behaviour unchanged until an operator flips                                                                                                                                                                                                       |
| Operator soak                  | enabled in the debug pane | enabled (requires master)      | `boss propose blocked` + rich flags; marker is bootstrap | `worker_proposals.fallback_hit.blocked` is quiet over a soak window (existing counter, already the seam's documented exit criterion) **and** at least one real blocked proposal has been submitted, answered, and acted on (the worked example, or a sibling) |
| Default-on                     | default true              | default true                   | propose-first                                            | Same counters, plus no incident of a blocked worker that only emitted a marker _and_ was ignored because the prompt no longer taught it. `[blocked]` remains in the prompt as bootstrap.                                                                      |

Flipping the running engine's flags is an **operator step**, not a task. Changing the code default is a later one-line PR that this design does not file; do it when the soak evidence exists, not in the same change that lands the payload.

Do not enable the prompt teaching (`worker_signal_proposals_seam`) until the rich fields exist. Teaching `--reason` only would ship a typed identity with nothing for the operator to pick.

### Leg 3 — closing the loop, and the authority problem

**Mechanical half.** New engine RPC: answer a blocked proposal. The operator (app) or coordinator (`bossctl`) sends `{ proposal_id, option_id }` or, for synthesised marker rows, `{ proposal_id, free_text }`. The engine, in one transaction:

1. Verifies the row is `kind = blocked`, `state = proposed`, and the option id is in the payload (or free-text is allowed because the payload was marker-synthesised).
2. Sets `state = applied`, `decided_by = human` (app) or `coordinator` (CLI), `decision_reason` = the option label / free text, `decided_at` now, and stores the chosen `option_id` + `if_chosen` in an `answer_json` column (or in `decision_reason` structured enough to round-trip — prefer a real column; `decision_reason` is currently a prose string).
3. Appends an **AUTHORIZATION** block to the work item's description. This is the durable channel. Shape:

```
<!-- boss-authorization prp_<id> -->
Operator decision on blocked proposal `prp_<id>` (`decided_by=human`):
chose `authorise_bypass`.
Worker: write `BYPASS_CHANGE_FILE_COUNT=…` citing this proposal id, then proceed.
This block is not authorisation by itself. Confirm with `boss propose --list --kind blocked`.
<!-- /boss-authorization -->
```

4. Resolves the open `worker_blocked` attention for that execution. **Stop resolving `worker_blocked` on an arbitrary probe.** Probe-as-ack remains correct for `worker_escalation`; it is the incident's failure mode for blocked. Split the resolver.
5. Resume:
   - If a live parked worker exists for that execution: best-effort probe whose body is the AUTHORIZATION block plus "call `boss propose --list --kind blocked` and act." Grok parked after Stop can take this. Grok mid-turn cannot; the description is waiting for the next spawn.
   - If no live worker (exited, pane released, probe would `Abandon`): `request_execution` for a successor run of the same work item. The successor prompt interpolates the description, so the block is in `details:` at spawn. `boss propose --list` still shows the disposition across executions (already shipped).

Do not start a second worker while the first is still in the pane. Do not rely on the first pane remaining up. The successor path is the one that survives the incident's fifteen-hour idle.

**Hard half — how a worker distinguishes a genuine decision from text that looks like one.**

The verifiable artefact is the proposal row. Procedure, taught in the engine worker prompt _and_ in `AGENTS.md`:

1. You may exercise a prohibited action only for a check / rule the _options you submitted_ named.
2. Immediately before doing it, run `boss propose --list --kind blocked --json`.
3. Authorisation exists iff a row has `id` equal to the `prp_…` you received at submit (or the id in the AUTHORIZATION block), `state = applied`, and `decided_by` of `human` or `coordinator`. `policy` does not count. A matching `reason` without that `decided_by` does not count.
4. Then do exactly what `if_chosen` said. Not a broader class of bypasses. Not a second check.
5. Record it in the PR body (and, for checkleft, in the commit description, which is the durable bypass surface): proposal id, `decided_by`, chosen option. Silent exceptions are not allowed.

What is **not** authorisation, named so a worker can fail closed:

- Free text in the pane, including a probe that says "set the bypass."
- The AUTHORIZATION HTML comment in the description, _by itself_. It is the resume brief, and it tells the worker to confirm via `--list`. A worker that acts on the comment without the list call is doing it wrong; the comment says so.
- The worker's own earlier sentence that it "got approval."
- A coordinator or operator sentence of uncertain provenance.

`decided_by` is engine-stamped at the answer RPC. `SubmitProposal` cannot set it. That is true even while `worker_rpc_tier` is off: workers can `boss task update` a description (so they _could_ forge a comment block), but they cannot forge the list row. That is why the list call is the check, and why the description block is explicitly not sufficient. If that still sounds like "trust the worker to call `--list`", it is, and Risks says so.

**The contradiction.** `change/file-count` stays `allow_bypass: true`. File-count is a scope heuristic; a merge-conflict resolution that legitimately touches 31 files has no "root cause" to fix, which is why the check advertises a bypass. `AGENTS.md` gains a carve-out in the same section that currently forbids every bypass:

> You may invoke a check bypass (for example `BYPASS_CHANGE_FILE_COUNT=…` in the commit description) **only** when `boss propose --list --kind blocked` shows `state=applied` and `decided_by` of `human` or `coordinator` for a proposal **you** submitted whose chosen option names that bypass. Cite the proposal id in the PR body and in the bypass reason. Every other path — operator prose, your own reasoning, a description sentence, a probe — is still forbidden.

Disabling `allow_bypass` on the check would make the incident unsolvable even _with_ operator approval. The affordance is correct; the missing consumer is the carve-out.

**Landing sites for the contract** (all real files, all in-repo):

- Worker rules: root `AGENTS.md` (the file the incident worker cited).
- Engine-injected worker prompt: `tools/boss/engine/core/src/runner/prompt.rs` (`worker_escalation_protocol_directive` and the blocked-answer procedure).
- Coordinator prompt: `bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift` — edit that source, not the runtime `CLAUDE.md` the app rewrites. Teach: do not probe "set the bypass"; answer the proposal. Probe remains for questions, nudges, and effort-escalation ack.

**Over-asking.** Qualifies as blocked when all three hold: the worker exhausted the legitimate options, the remaining action is reserved to a human by repo rules or an unresolvable external fact, and it cannot proceed either way. The incident qualifies. "Which of two acceptable refactors" does not; the existing "Avoid asking the human for permission during this pass" line plus the external-fact rule already say so.

Enforcement that is not merely prose:

- Schema: non-empty `already_tried`, ≥2 options with `if_chosen`. Submitting "I am stuck" without options fails at the CLI.
- Rate caps already on `SubmitProposal` (8 per kind per execution) bound loops.
- Measurable later (operator step, not a task): count of blocked proposals the operator rejects as "not actually blocked", and blocked-submitted / runs-completed. If the reject rate is high, tighten the schema or the prompt; do not add a new attention kind.

Prompt guidance remains necessary for the quality of `already_tried`. Schema cannot tell a real exhausted attempt from a padded sentence. Say that: presence is enforced; honesty is not.

### Worked example, after this lands

1. Conflict-resolution worker hits `change/file-count` at 31 / 30. It cannot split the diff. `AGENTS.md` tells it to stop and surface. It runs `boss propose blocked` with `blocked_on=change/file-count`, `already_tried` naming the reconciliation and the rule, and three options including `authorise_bypass` whose `if_chosen` is "write `BYPASS_CHANGE_FILE_COUNT=…` citing this `prp_…` in the commit description and proceed."
2. Engine stages a `worker_blocked` attention, leaves the proposal `proposed`, pauses auto-nudge, publishes the blocked-waiting event.
3. Operator gets an OS notification (app focused or not) and a Doing-card banner. They do not open the pane. They pick `authorise_bypass`.
4. Engine stamps `decided_by=human`, appends the AUTHORIZATION block, resolves the attention, probes if the worker is parked, otherwise starts a successor.
5. Worker (same session or successor) calls `boss propose --list --kind blocked`, sees `applied` + `human` + `authorise_bypass`, writes the bypass citing `prp_…`, records the same in the PR body, pushes. Checkleft accepts the directive. The loop completed without a pane read, and without the worker having been able to mint that `decided_by`.

If instead the operator is offline for hours, the banner's age is the only extra urgency. The successor path still works when they answer. The worker is not required to occupy a slot the whole time; if the pane is gone, step 4 takes the successor branch.

## Risks / open questions

- **The `--list` check is prompt-enforced.** A worker that skips it and treats the description comment, or a probe, as approval will still bypass. v1 has no PreToolUse gate keyed on a proposal id. That is a real hole, named so it is not mistaken for a solved property. A later hook is Not now.
- **`worker_rpc_tier` is still default off.** Workers can `boss task update` the description and forge an AUTHORIZATION comment. They cannot forge `decided_by` on the proposal row. The design relies on that split. Enabling the tier is pre-existing work, not this project.
- **Gated `blocked` vs today's AutoApply tests.** Several tests assert that submit stamps `applied` and files attention in one commit. The flip to stage-and-stay-`proposed` is a behaviour change and must update those tests in the same PR as the flip, or the tests will pin the old "policy applied = decided" confusion.
- **Probe-as-ack split.** Coordinators currently probe to un-pause a blocked worker. After the split, a probe without an answer RPC leaves the proposal `proposed` and the nudge paused. That is correct and will feel like a regression until the coordinator prompt (Leg 3) ships. Land the prompt change in the same train as the resolver split, not weeks later.
- **OS notification while frontmost.** Review notifications suppress in-foreground on purpose. Blocked does not. Operators who already find Review banners noisy may object. The incident is the argument: the operator _was_ in the app.
- **Production volume by kind is not in this doc.** An operator with `bossctl` can dump `work_attention_items` grouped by `kind` over a week and, if the ranking surprises, revisit which kinds are excluded from the new surface. The concept (proposal, not severity) should not change.

Reviewer forks that would actually change the design: (1) insist on a mechanical hook in v1, (2) insist blocked stay AutoApply and grow a parallel answer table, (3) put blocked in the Notifications window after all. Those are not left as hedges; the chosen approach above is the recommendation.

## Not now

- Enabling `worker_proposals` / `worker_signal_proposals_seam` in the running engine (operator debug-pane step; soak criterion above).
- Changing those flags' code defaults to on.
- Migrating effort-escalation, deferred-scope, follow-ups, triage, or PR-created (existing flags and recipes).
- Deleting the `[blocked]` marker parser.
- OS-notifying `worker_escalation`, `ci_remediation_exhausted`, or any other kind.
- Putting `worker_blocked` into the Notifications toolbar.
- Reclaiming the worker slot on block (park and release). v1 may leave the pane up; it must not _require_ it.
- PreToolUse / checkleft enforcement that a `BYPASS_*` reason cite a live `prp_…`.
- Product-level `boss decision` (`D<n>`) as the store.
- Dispatch-halt Bucket A migrations.

## Proposed implementation task breakdown

Breakdown size: 6 entries (6 in-scope, 0 deferred) — three ordered legs over engine, app, CLI, and two prompt files; each leg is one reviewable engine-or-payload change plus its thin client or rules landing site, which is the actual seam count, not a band target.

Parallelism: entries 1 and 3 may start together (no file overlap). Entry 2 depends on 1. Entry 4 depends on 3. Entry 5 depends on 2 and 4 and co-edits the Swift files entry 2 will have touched — sequence it after 2 and forward-port preservingly. Entry 6 depends on 4 and co-edits `runner/prompt.rs` with entry 3 — sequence it after 3 and forward-port preservingly.

### Engine blocked-waiting list RPC and live event

**Scope:** Add an engine-owned query and live event, modelled on `list_deferred_scope_attentions` / `deferredScopeAttentionsList`, that returns currently-waiting blocked-on-decision items for a product: open `worker_blocked` attention items, joined to a `worker_proposals` row of `kind = blocked` when one exists. Include work item id/name, execution id, proposal id, reason, timestamps, and whether the worker is still live. Publish the event when `worker_blocked` is filed or resolved. Add a coordinator CLI list verb (`bossctl work blocked list` or equivalent) so this PR has an exercised caller without the app. The app must not filter attention kinds itself. No answer RPC, no payload change, no flag flip.

**Effort:** medium

**Dependencies:** none

Scope: in-scope

### App OS notification and kanban banner for blocked-waiting

**Scope:** Thin client over the list RPC/event from the previous entry. OS notification on raise with identifier `boss.blocked:<id>`, presenting banner+sound even when Boss is frontmost (deliberately unlike `ReviewNotificationCenter`). Tap focuses the work item. Doing-card banner showing reason and age, distinct from the deferred-scope badge strip. Banner down on the resolved event. No answer controls in this PR. No Notifications-toolbar membership. No notifications for other attention kinds.

**Effort:** medium

**Dependencies:** Engine blocked-waiting list RPC and live event

Scope: in-scope

### Typed blocked proposal payload, CLI, validation, and prompt teaching

**Scope:** Expand `BlockedProposalPayload` with `blocked_on`, `already_tried`, `decision`, and `options[{id,label,if_chosen}]`; keep `reason`. Reject empty `already_tried` or fewer than two complete options at validation. Extend `boss propose blocked` with the matching flags and `--*-file` variants. Update `apply_blocked`'s attention body to render the new fields (still a visibility write). When `worker_signal_proposals_seam` is on, `worker_escalation_protocol_directive` teaches the rich `boss propose blocked` invocation; the `[blocked]` marker remains documented as bootstrap fallback. Marker parsing behaviour when the flags are off stays unchanged. Do not flip flag defaults. Do not add the answer RPC here.

**Effort:** medium

**Dependencies:** none

Scope: in-scope

### Engine answer, durable description, resume, and probe-as-ack split

**Scope:** Flip `blocked` from AutoApply to staged-visibility / stay-`proposed` (followup_task pattern): submit files `worker_blocked` and pauses nudge but does not stamp `decided_by = policy` as the decision. Remove `blocked` from in-flight expiry. When the legacy `[blocked]` marker files an attention and no matching proposal exists, upsert a `worker_proposals` row (reason filled; synthesised free-text option) so every waiting block has a proposal id. Add the answer RPC + coordinator CLI: option id, or free text on synthesised rows; stamp `state = applied`, `decided_by = human|coordinator`, persist the chosen option; append the AUTHORIZATION description block; resolve `worker_blocked` for that execution only. Stop `resolve_worker_signal_attentions_for_execution` from clearing `worker_blocked` on an arbitrary probe; leave `worker_escalation` on the probe-ack path. Resume: probe a live parked worker with the AUTHORIZATION block; if none, dispatch a successor execution of the same work item. Sweep tests that pinned AutoApply-as-decided in the same change. Engine-owned throughout; no app branching to compensate.

**Effort:** large

**Dependencies:** Typed blocked proposal payload, CLI, validation, and prompt teaching

Scope: in-scope

### App answer controls on the blocked-waiting banner

**Scope:** Thin client over the answer RPC. The Doing-card banner renders the proposal's `options` as choices (and a free-text field only when the payload is marker-synthesised). Choosing one calls the engine, then relies on the existing resolved event to take the banner and OS notification down. No engine-side resume logic in the app. Co-edits the banner/notification Swift files from "App OS notification and kanban banner for blocked-waiting": integrate that PR's work, never delete it.

**Effort:** medium

**Dependencies:** App OS notification and kanban banner for blocked-waiting; Engine answer, durable description, resume, and probe-as-ack split

Scope: in-scope

### Authority contract: AGENTS.md carve-out, worker prompt, coordinator prompt

**Scope:** One reviewable contract change across the three in-repo landing sites. Root `AGENTS.md`: keep the hard prohibition on self-bypass; add the carve-out that a check bypass may be invoked only when `boss propose --list --kind blocked` shows `state=applied` and `decided_by` of `human` or `coordinator` for a proposal this worker submitted whose chosen option names that bypass; require citing the proposal id in the PR body and in the bypass reason; name pane text, probes, and description sentences as insufficient. Do not set `allow_bypass: false` on `change/file-count` in `CHECKS.yaml`. Engine worker prompt (`tools/boss/engine/core/src/runner/prompt.rs`): teach the `--list` check, the `if_chosen` discipline, and the PR-body audit line; keep the existing external-fact bar and extend it with the schema fields. Coordinator prompt (`bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`, not the runtime `CLAUDE.md`): answer blocked proposals via the answer verb; do not treat `bossctl probe "set the bypass"` as authorisation; probe-as-ack remains for effort-escalation only. Co-edits `runner/prompt.rs` with the payload/teaching PR: forward-port preservingly.

**Effort:** medium

**Dependencies:** Engine answer, durable description, resume, and probe-as-ack split

Scope: in-scope
