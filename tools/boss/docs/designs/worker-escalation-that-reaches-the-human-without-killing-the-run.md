# Worker escalation that reaches the human without killing the run

- **Date:** 2026-09-15
- **Provenance:** execution `exec_18d5a5d4368daee8_2c` (project_design), project "Worker escalation that reaches the human without killing the run"
- **Related designs:** [worker-proposal-api](worker-proposal-api-replace-fragile-worker-to-engine-seams.md) (the `boss propose` family this verb joins), [attentions](attentions.md), [dispatch-halt-state-vs-attention-items](dispatch-halt-state-vs-attention-items.md), [unify-blocking-signal-remediation](unify-blocking-signal-remediation.md), [work-kanban](work-kanban.md), [automated-reviewer-pass-on-every-agent-authored-pr](automated-reviewer-pass-on-every-agent-authored-pr.md) (review pool), [comment-triggered-document-revisions](comment-triggered-document-revisions.md) (answer agent, the reduced-prompt precedent)
- **Related operational docs:** [worker-liveness-contract](../worker-liveness-contract.md), [attention-lifecycle](../attention-lifecycle.md), [post-crash-recovery](../post-crash-recovery.md)

**TL;DR:** Add `boss propose escalate`, a gated proposal kind whose durable effect is a first-class `worker_escalations` row, never an attention item. The escalating worker stays alive in its pane, holding its slot and lease, in `waiting_human`. A reduced-prompt supervisor agent on the review pool adjudicates first and may only _approve_ or _refer to the human_; every failure of the supervisor routes to the human, never to a decision. The human sees the request as distinct chrome on the kanban card, decides in a sheet, and approval is injected into the live pane as a prompt while denial atomically marks the work item `blocked` with the explanation and reaps the worker. The contested property is stated up front: **the pending state is a typed row that the kanban reads directly, not an attention, and the worker's slot stays occupied for as long as the human takes.**

## Goals

- A worker that hits a gate it has no authority to cross can ask for a decision **and keep running**: its pane, slot, cube lease, and in-context understanding of the work survive until a decision arrives.
- The request reaches the operator on the surface he already uses, the kanban board, as a visible, non-interrupting card state with a one-gesture path to a decision.
- A supervisory agent with a reduced prompt adjudicates first, approving obviously reasonable requests itself so the human only sees the ones that need him.
- Approval reaches the live worker immediately as a prompt; the worker resumes in place. Denial reaps the worker and marks the work item `blocked` with the explanation attached, so the reason and a pointer to the work product survive the run.
- The pending state is durable and explicit, written in the same transaction as any transition that publishes an event the admission guards react to, so the orphan sweep, the husk reconciler, and the redispatch guards can never mistake a waiting worker for a dead or parked one.
- The gate keeps firing. This design gives the human a way to decide; it does not soften anything.

## Non-goals

- **Softening any gate.** No exclusions, allowlists, or threshold changes to checkleft or `cube pr create`. The incident's `max_files=30` gate fires exactly as before.
- **Fixing the `finalize_declared_blocked` write-ordering race.** It is being handled separately. This design only guarantees the escalate path does not acquire the same shape (see "Atomicity").
- **Replacing `boss propose blocked` or `boss propose done` as verbs.** Both stay. The worker prompt's _guidance_ about them changes (in scope); their wire contract does not.
- **Granting the worker new mechanical capabilities on approval** (for example a one-shot gate bypass token). v1 approval is instructional: the human's words reach the worker as a prompt. Whether a follow-on capability grant is worth building is listed under deferred work and asked in the questions manifest.
- **Timeouts that decide.** Nothing in this design auto-approves or auto-denies. The one bounded wait, the supervisor horizon, falls through to the human, which is a routing choice, not a decision.
- **A general worker-to-human chat channel.** One escalation carries one decision request. Iterative back-and-forth is the coordinator's probe, unchanged.
- **App-side polling.** The app stays a thin client of engine pushes, as it is today for every kanban state.

## The problem, precisely

A worker blocked by a hard gate today has these moves, and each one loses:

| Move                                  | What actually happens                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | Where it loses                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `boss propose blocked --reason …`     | Auto-applies in the submit transaction: files a `worker_blocked` attention item (`work/proposal_apply.rs:578`) and pauses the auto-nudge loop on the run's next Stop (`completion/nudge.rs:67` via `unresolved_worker_signal_reason`, `completion/worker_signals.rs:782`). The run keeps living.                                                                                                                                                                                                                                                                                                                   | The signal terminates in a `work_attention_items` row that no read surface reaches: the app renders those rows only in the product sidebar sync banner, never on a card (confirmed: zero hits for `worker_blocked` in `app-macos/Sources`). The dispatch-halt design already records this as "unreachable from every read surface" (`dispatch-halt-state-vs-attention-items.md:48`). The only ack gesture is a coordinator probe (`app/probes.rs:1007`), so a worker that waits is waiting on the coordinator noticing, which is the same failure the marker incident of 2026-07-02 documents in `worker_escalation.rs:1-9`. |
| `boss propose done --outcome blocked` | `finalize_declared_blocked` (`completion/run_done_declaration.rs:309`) calls `finalize_idle_park` (`completion/nudge.rs:675`): status `abandoned`, pane reaped, lease released, `autostart` cleared, then files `run_done_declared_blocked`. Since mono#2963 and mono#2965 the park is honoured on every automatic mint path (`work/dispatch_admission.rs`, `deliberate_parked`), the workspace is preserved with `preferred_workspace_id`, and an operator `bossctl work start` resumes a replacement worker in that workspace with a `BLOCKED WORKSPACE RECOVERY` brief (`runner/prompt/workspace_recovery.rs`). | The run is still over: the worker's in-context understanding is gone and the replacement re-orients from a brief. The human-facing half is still an attention row (`run_done_declared_blocked`), and the un-park gesture is an explicit `bossctl work start` or kanban drag-to-Doing that nothing on the board prompts, so the card sits in Doing looking healthy until someone happens to look. The park attention is written after the terminal event, which is the ordering race the parent description names and which is being fixed separately.                                                                        |
| Stop without declaring                | The run-done backstop holds, asks once, then trips the nudge breaker, which also runs `finalize_idle_park` (`completion/nudge.rs:562`, `:675`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | Same terminal outcome as the row above, slower, and with a stale comment at `completion/nudge.rs:561` claiming the execution "stays in `waiting_human`" when it does not.                                                                                                                                                                                                                                                                                                                                                                                                                                                    |

Three facts follow. First, the live-run half already exists: `propose blocked` does not terminate anything, and the nudge suppression it relies on is exactly the shield an escalating worker needs. Second, the terminate-and-resume half now also exists: mono#2963 and mono#2965 made a blocked declaration a durable park that automatic dispatch honours and that resumes in the preserved workspace, so the incident's "replacement redoes 55 files" consequence is already addressed for the declared-blocked path. Third, every path's _human-facing_ half still ends in an attention row, and the operator has stated that an attention firing is operationally equivalent to nothing having happened. The `attentions.md` non-goal "the task does not halt waiting for a human" (`attentions.md:31`, reconfirmed at `:273`) was a decision about **agent-authored questions**, made so that ten mid-task questions would not stall ten workers. It was never a decision about hard gates. No recorded reason exists for "a gated worker should end itself"; the prompt text that instructs `propose blocked` then `propose done --outcome blocked` (`runner/prompt.rs:1206`) is a convenience pairing that turned into a defect surface. That absence is a finding, not a lost rationale.

## Alternatives considered

### A. Reuse the attention-group model with a per-card badge (the deferred-scope shape)

Model an escalation as an attention group with a new `escalation` member kind, reuse `AttentionGroupCard`'s answer UI, and add a per-card badge the way `DeferredScopeCardBadge` does for deferred scope.

This is the closest existing precedent and it is not a strawman: deferred scope already puts an attention-derived badge on a Review-lane card with an accept gesture (`DeferredScopeAttentions.swift:53`). The reasons it does not transfer:

- **The load-bearing property is different.** A deferred-scope attention describes something that happened; nothing waits on it. An escalation gates a live worker holding a slot, and dispatch admission must read it. The dispatch-halt design's rule (`dispatch-halt-state-vs-attention-items.md:11-13`) is that state the board and the dispatcher must read gets a typed field, and it explicitly rejected rendering attention items on cards because "it would have made the wrong representation load-bearing rather than fixing it" (`:4`). Deferred scope passes that test (if every deferred-scope row vanished the board would still show what is going on); an escalation fails it.
- **Attention lifecycles auto-clear on positive evidence** (`attention-lifecycle.md:12-31`). `WorkResumed` clearing on a later run start is precisely wrong for an escalation that must outlive the worker's death (see "The escalation outlives its execution").
- **The operator's stated fact.** The Notifications window and the bell badge are the attention surface, and he does not read them. A badge that opens the Notifications window still terminates there.

The reusable parts are reused anyway: the sheet borrows `AttentionMemberRow`'s prompt controls, and the engine push pattern is the same as `AttentionGroupUpdated`.

### B. Route to the coordinator session as the supervisor

Deliver the escalation to the long-lived coordinator (Picard) by probe and let it approve or forward.

The coordinator is the precedent for "an agent adjudicates worker signals": its probe _is_ the ack gesture for `worker_blocked` today (`attention_lifecycle.rs:265`). Rejected as the first hop for three checkable reasons: the coordinator may be absent, restarting, or mid-handoff, so the escalation's latency is unbounded and unobservable; its context is large and its judgment on a narrow yes/no is less consistent than a fresh agent with a fixed rubric; and it has no path to the kanban, so "forward to the human" would recreate the unreachable-attention path. The operator also specified a reduced-prompt agent. The coordinator keeps its probe for everything else.

### C. Terminate-and-resume: make the park durable and resumable instead of keeping the worker alive

Fix `finalize_declared_blocked`'s ordering, register its attention kind, and resume a parked run in its original workspace (`cube workspace lease --prefer`) when the human answers.

This is not hypothetical: it is what the repository does today for a declared-blocked run after mono#2963 (deliberate park honoured on every automatic mint path via `work_item_is_deliberately_parked`) and mono#2965 (`preferred_workspace_id` / `allow_dirty` / `prefer_is_soft` on the replacement execution, verified by `blocked_workspace_predecessor`, and the `BlockedInPlace` recovery brief). Rejected as the primary path for two checkable reasons: it discards the worker's in-context state and re-pays spawn plus re-orientation, which the recovery brief itself acknowledges by telling the replacement to re-read and re-validate everything it inherited; and its un-park gesture is an explicit `bossctl work start` or a kanban drag-to-Doing, neither of which is preceded by anything on the board saying a decision is wanted, so it does not answer the "reaches the human" half at all. Requirement 1 is also explicit: the worker stays alive. The machinery is composed in, not discarded: it is exactly what this design uses when an escalating worker dies while pending (see "The escalation outlives its execution").

### D. Human-only adjudication, no supervisor

Simpler, one fewer agent kind. Rejected because the operator required the supervisor, and because the supervisor is what keeps the human's queue short. A flag (`escalation_supervisor`) turns the supervisor off and routes straight to the human, so the simpler shape is available operationally without being the design.

## Chosen approach

### Overview

```
worker ──boss propose escalate──▶ engine: worker_escalations row (pending_supervisor), in the submit tx
   │                                   │
   │ ends its turn; pane idle          ├─▶ supervisor execution on the review pool (EscalationReview)
   │ status: waiting_human             │       verdict: approve ─────────────┐
   │ nudge/backstop suppressed         │       verdict: refer / fail / none ─┼─▶ pending_human
   │                                   │                                     │
   │                                   ├─▶ kanban card: "Needs decision" chrome + sheet
   │                                   │       human: approve (+message) ────┤
   │                                   │       human: deny (+reason) ────────┼─▶ denied: tasks.status=blocked,
   │                                   │                                     │   execution abandoned, reap
   ◀── prompt injected into the pane ──┘ approved: inject_pane_text_verified ┘
```

### Relationship to `propose blocked` and `propose done --outcome blocked`

`escalate` **subsumes** `propose blocked` for the case it was being used for, and **composes with** `done`:

- `propose blocked` keeps its wire contract (auto-applied attention plus nudge pause, ack by coordinator probe). Its role narrows to "tell the coordinator I need help I cannot phrase as a decision", and the worker prompt stops recommending it for gates. It is also the shape the `[blocked]` bootstrap marker maps to, which is why it cannot be removed.
- `propose done --outcome blocked` remains the terminal statement for a worker that has genuinely decided to stop. The prompt sentence pairing it with `propose blocked` (`runner/prompt.rs:1206`) is replaced: a worker whose work product exists and who is stopped only by a decision it lacks authority to make must call `escalate`, and must not call `done` while an escalation is open (the engine refuses that call with a typed `escalation_pending` error, see "Worker-side contract").
- After denial the engine terminalizes the run itself; the worker never gets to declare.

### CLI surface

```
boss propose escalate --summary "<one line: what decision is needed>" \
    --proposed-action "<one line: what I will do if approved>" \
    --context-file ctx.md          # what was tried, the exact gate/output, what is at stake
boss propose escalate --withdraw --escalation esc_…   # I found a way; cancel my own pending request

boss propose escalation-verdict --escalation esc_… --verdict approve --reason-file r.md [--message-file m.md]
boss propose escalation-verdict --escalation esc_… --verdict refer   --reason-file r.md
   # supervisor-tier only; never `deny`

bossctl escalations list [--state pending_human|…] [--product …]
bossctl escalations show esc_…
bossctl escalations decide esc_… --approve [--message-file m.md] | --deny --reason-file r.md
```

Submission follows the family's conventions (`worker-proposal-api…md` §"CLI surface"): synchronous validation with field-level typed errors, idempotency key derived from execution id plus kind plus payload hash, durable persistence before the command exits. `--summary` and `--proposed-action` are single-line and bounded; `--context-file` is markdown and bounded (32 KiB, the same class of cap as `--body-file` elsewhere). Validation rejects an empty `--proposed-action`: an escalation without a concrete proposed action is a `propose blocked`, and the error says so.

On success the CLI prints the escalation id, its state, and one instruction:

```
esc_… pending_supervisor
Stop now and end your turn. Do not declare done. You will receive the decision as a prompt in this pane.
```

### Proposal kinds and payloads

Two new `ProposalKind` variants (`protocol/src/types/proposal.rs`):

| Kind                 | Payload                                                                | Apply policy                                                                                                                                                                                                                                                                                                                                                                                     |
| -------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `escalate`           | `{summary, proposed_action, context_markdown}` or `{withdraw: esc_id}` | **Gated, with a synchronous durable effect.** Unlike `followup_task`, whose only in-transaction effect is an attention-group member, this kind writes the `worker_escalations` row in the submit transaction. The proposal row's own state stays `proposed` until the escalation reaches a terminal state, then mirrors it (`applied` on approve, `rejected` on deny, `superseded` on withdraw). |
| `escalation_verdict` | `{escalation_id, verdict: approve \| refer, reason, message?}`         | **Auto-apply with verification:** the submitting execution must be the `EscalationReview` execution bound to that escalation, and the escalation must be `pending_supervisor`; otherwise typed `escalation_not_pending` / `not_bound_to_escalation`.                                                                                                                                             |

Rate caps: one open escalation per execution at a time (a second `escalate` while one is pending returns the open row with `already_submitted: true`, not a new escalation), and the family's per-kind cap of 8 per run bounds a worker that escalates, withdraws, and escalates again.

`escalate` is added to the in-flight-only set for `proposal_expiry_sweep` _only for the proposal row_: an escalation row is never expired by that sweep (see "The escalation outlives its execution").

### Data model

```sql
CREATE TABLE IF NOT EXISTS worker_escalations (
  id                   TEXT PRIMARY KEY,            -- esc_<hexnanos>_<hexcounter> via next_id("esc")
  proposal_id          TEXT NOT NULL REFERENCES worker_proposals(id),
  execution_id         TEXT NOT NULL REFERENCES work_executions(id),
  work_item_id         TEXT NOT NULL,
  product_id           TEXT NOT NULL,
  summary              TEXT NOT NULL,
  proposed_action      TEXT NOT NULL,
  context_markdown     TEXT NOT NULL,
  state                TEXT NOT NULL,
      -- pending_supervisor | pending_human | approved | denied | withdrawn
  supervisor_execution_id TEXT,                     -- the EscalationReview execution, once dispatched
  supervisor_outcome   TEXT,
      -- approved | referred | unavailable | failed | timed_out | disabled   (why the human is seeing it)
  supervisor_reason    TEXT,                        -- the supervisor's own words, shown in the sheet
  decided_by           TEXT,                        -- supervisor | human | worker (withdraw)
  decision_message     TEXT,                        -- approval message delivered to the worker, or denial reason
  delivery_state       TEXT,                        -- NULL | delivered | failed | resumed_by_redispatch
  delivery_detail      TEXT,
  head_change_id       TEXT,                        -- jj change id of the worker's @ at submission
  head_commit_id       TEXT,
  workspace_id         TEXT,                        -- cube workspace id at submission
  created_at           TEXT NOT NULL,
  supervisor_dispatched_at TEXT,
  human_visible_at     TEXT,                        -- when state became pending_human
  decided_at           TEXT,
  UNIQUE (execution_id) WHERE state IN ('pending_supervisor','pending_human')   -- one open per execution
);
CREATE INDEX IF NOT EXISTS idx_worker_escalations_open_work_item
  ON worker_escalations(work_item_id) WHERE state IN ('pending_supervisor','pending_human');
```

Protocol type `WorkerEscalation` uses `#[derive(bon::Builder)]` with `#[builder(on(String, into))]` per the repo convention (well over five fields). The DB mapper stays a struct literal.

The `head_change_id` / `head_commit_id` / `workspace_id` triple is captured **by the engine at submission** by running `jj log -r @ --no-graph -T 'change_id ++ " " ++ commit_id'` in the execution's recorded `workspace_path` (a local subprocess, no network, in keeping with the termination-path rule in `run_done_declaration.rs`'s module doc). It is what makes the work product addressable after a denial: cube workspaces share one object store, so the commit is reachable by id even after the workspace is recycled, and the blocked explanation carries it.

**What the pending state is not.** No attention item is filed for a pending escalation. The kanban reads the row through a typed field (below), and dispatch admission reads the row directly. If every attention item in the system vanished, the board would still show the escalation, which is the test `dispatch-halt-state-vs-attention-items.md:13` asks of a load-bearing state.

### Worker-side contract and the waiting posture

After a successful `escalate`, the prompt tells the worker to end its turn. The engine does not depend on the worker obeying:

- **Nudge and backstop suppression.** `unresolved_worker_signal_reason` (`completion/worker_signals.rs:782`) gains a second predicate: an open `worker_escalations` row for the execution. Because `nudge_or_park` checks this before the breaker (`completion/nudge.rs:67`), and the run-done backstop's `Ask` routes through `nudge_or_park` (`completion/metadata_gate.rs:606`), both the "produce a PR" loop and the "are you done?" ask are held for the life of the escalation. The new `StopOutcome::EscalationPending` variant publishes live-state reason `worker_escalation_pending` so the card's live-status row can say "Waiting for a decision".
- **Death sweeps.** An idle worker is outside `stale_worker_sweep` by construction (`activity != Working` ⇒ `not_working_skipped`, `stale_worker_sweep.rs:836`). `dead_pid_sweep`, `dead_pane_sweep`, and `lost_workspace_sweep` need positive death evidence and cannot fire on a live pid in an existing workspace. `husk_pane_sweep` never retires a pane whose spawn token resolves to a `work_runs` row. Nothing here needs a change; the design records the dependency so a future change to `stale_worker_sweep`'s exemption is reviewed against it (a compile-time or unit assertion in the same PR pins "a `waiting_human` execution with an open escalation is never a stale-sweep candidate").
- **Durable status.** When the worker ends its turn, the driver's awaiting-input signal mirrors `waiting_human` onto the row (`awaiting_input_status.rs`), which is `is_live()`. This design writes **no** new execution status: `waiting_human` already means "blocked on a person" and has exactly one writer, and the liveness contract forbids a second one (`worker-liveness-contract.md:39-41`). The escalation row, not the status, is the durable fact.
- **Worker keeps working?** A worker that ignores the instruction and keeps taking turns is not reaped and not nudged; it is simply a worker with an open escalation. If it later calls `done`, the engine refuses with `escalation_pending` and the CLI tells it to `--withdraw` first or wait. If it calls `escalate` again, it gets the open row back.
- **`hold_registry`** is not used. It is in-memory and cleared on restart; the escalation row is the shield and is re-read on every check.

### The supervisor

A new execution kind `EscalationReview` and a new `WorkerKind::EscalationSupervisor`, modeled on the answer agent (`answer_agent.rs`, `worker_setup.rs:100-135`):

- **Enforcement:** `--permission-mode dontAsk` with an allowlist of read-only tools plus exactly one mutation, `boss propose escalation-verdict`, and the same deny belt as the answer agent. It cannot edit files, push, open PRs, touch cube, or call any other proposal verb.
- **Inputs, assembled by the engine into the brief** (a `compose_escalation_supervisor_prompt` alongside `compose_answer_agent_prompt`, `runner/prompt.rs:1583`): the escalation payload verbatim; the work item's name, description, kind, and effort; the project's name and description; the escalating execution's kind; the worker-tier `boss context` bundle for that work item; a bounded tail of the escalating run's transcript (the last 200 lines, sanitized the way the engine already sanitizes transcript excerpts); and the `jj diff --stat` of the worker's `@` against `main@origin`, computed by the engine in the worker's workspace at dispatch time and pasted into the brief as text. The supervisor does **not** lease a repository workspace and does not read the worker's workspace itself. Its cwd is a per-run scratch directory holding the brief. Rationale: it is judging the _request_, not re-doing the review; leasing a checkout would cost a cube lease and give it a place to write.
- **Prompt (the rubric, in full):**
  > You are adjudicating one escalation from a Boss worker that is blocked by a gate it cannot cross on its own authority. Your only job is to decide whether the worker's proposed action is reasonable enough to approve without a human, or must be referred to the human. You may not deny. Approve only if all of these hold: the request is specific and bounded to the work item; the proposed action is reversible or lands through the normal PR review path; it does not ask to bypass, disable, or weaken a safety, secrecy, cost, destructive-action, or repository-integrity control (checkleft gates, hooks, protected branches, credentials, deletion); it does not expand scope beyond the work item; and the context shows the worker actually tried the obvious alternatives. If any of these is unclear, refer. State your reason in two to five sentences a human can act on. Submit exactly one verdict with `boss propose escalation-verdict`, then stop.
- **Output contract:** one `escalation_verdict` proposal. `approve` may carry a `message` that is delivered to the worker together with the approval. `refer` records `supervisor_outcome = referred` and its reason, which the human sees in the sheet.
- **Pool and slots:** the **review pool** (`review-N`, global slot ids 25 to 40, `coordinator.rs:198-217`), dispatched with `pool_dispatch_policy_for_worker_id`'s pinned driver at `PoolModelTier::Strong`. Reasons, each checkable against existing practice: the interactive pool is the wrong place because the escalating worker is itself holding an interactive slot, so a full interactive pool would deadlock the supervisor behind the very workers waiting on it; the review pool is already the pool for read-only judgment agents on a strong model, is exempt from an operator dispatch pause (`drain_ready_queue` holds only `paused && !is_review`), and has per-pool exhaustion visibility. `kind_always_dispatches_on_pool_driver` and `migrate_backfill_pool_driver_decisions` both gain the new kind (the function doc names the SQL as a second site to update). One escalation review consumes one unit; the pre-merge batch reservation arithmetic (`work/review_batches.rs:134-206`) is not changed, and the review's admission uses the same `can_admit` shape so a full pool defers rather than fails.
- **When the supervisor is unavailable or fails, the human gets the escalation.** Every case routes to `pending_human` with `supervisor_outcome` recording why, and none of them decides anything:
  - `disabled`: flag `escalation_supervisor` off.
  - `unavailable`: no review slot admitted within `ESCALATION_SUPERVISOR_ADMISSION_HORIZON_SECS` (10 minutes, retried on each pool release like `sweep_deferred_review_admission`).
  - `failed`: the supervisor execution ends without a verdict (driver error, dead pane, a `done` declaration without a verdict), detected by subscribing to its `ExecutionTerminal` and re-checked by the escalation sweep.
  - `timed_out`: dispatched but no verdict within `ESCALATION_SUPERVISOR_VERDICT_HORIZON_SECS` (15 minutes); the supervisor execution is cancelled.
  - Malformed verdicts never reach the row: they are refused at submit with a typed error the supervisor can fix in-run, like every other proposal.

### The human surface

**Card chrome (the operator's suggestion, adopted).** `Task` in the `WorkTree` reply gains an optional typed field `escalation: Option<TaskEscalationSummary>` (`{id, state, summary, supervisor_outcome, worker_alive, pending_since}`), joined from the open `worker_escalations` row by `get_work_tree`. The macOS mirror `WorkTask` and `WorkCardSnapshot` gain the same field (three places, as the app's own convention requires, plus the `WorkBoardCardBadgeStripSlice` so `.equatable()` redraws). The card renders:

- a raised-hand icon (`hand.raised.fill`) next to the title with the summary as tooltip, and a "Needs decision" caption where the "Blocked by …" caption sits for dependency blocks (`WorkBoardCardTitleRow.swift:75-91`);
- a distinct tint branch in `cardBackground` / `borderColor` (`WorkBoardCard.swift:493`, `:526`) that sits **above** `showsBlockedChrome` in precedence and uses a colour the board does not already use for blocked (orange) or frontier (green); purple is the proposal, chosen in implementation against the actual palette;
- top-sort within Doing, the same treatment `work-kanban.md:81-87` gives `blocked`, and a "Needs decision" filter next to "Blocked only";
- the live-status row reads "Waiting for a decision" while `worker_alive`, and "Worker ended while waiting" otherwise.

Only `pending_human` escalations get the chrome. A `pending_supervisor` escalation shows the ordinary waiting indicator with the live-status text "Escalation under review"; the operator asked not to be interrupted, and a request the supervisor will approve in two minutes should not flash on the board.

**No interruption.** No sheet auto-presents, no macOS notification, no bell badge change. The one board-level affordance is a count chip in the board toolbar ("2 need decisions") that applies the filter.

**The decision sheet.** Clicking the icon or the caption opens `EscalationDecisionSheet`, bound to a `@Published var pendingEscalationDecision: WorkerEscalation?` on `ChatViewModel` in the same way the existing `pendingWorkCreateRequest` sheets are (`ContentView.swift:402-488`). It shows: the task name and a link to its detail; the summary and proposed action; the worker's context markdown, rendered; the supervisor's outcome and reason (or "sent straight to you: <reason>"); the worker's head change id and workspace id; the age; whether the worker is still alive. Two buttons:

- **Approve** with an optional message field. The message is the text the worker receives in addition to "Approved: <proposed_action>".
- **Deny** with a required reason field. The button is disabled until the reason is non-empty, and the sheet states what denial does: the worker is stopped and the task is marked blocked with this reason.

Both call `FrontendRequest::DecideEscalation { escalation_id, decision, message }` and receive `FrontendEvent::EscalationDecided` or a typed error (`escalation_not_pending` when a sibling client or the supervisor decided first; the sheet then refreshes and closes). `bossctl escalations decide` is the same RPC from the terminal, so the whole path is testable without the app.

**Refresh.** The engine publishes the existing `WorkInvalidated` on every escalation state change (the card's field rides the debounced `get_work_tree` refetch, `ChatViewModel+EventHandling.swift:740`) and a new `EscalationUpdated { escalation }` push so an open sheet refreshes in place. No app-side polling.

### Approval: delivery to the live worker

Approval by either party runs one function, `deliver_escalation_approval`:

1. Write `state = approved`, `decided_by`, `decision_message`, `decided_at` in one transaction. From this moment the nudge suppression no longer holds, which is what we want: the worker is about to be prompted.
2. Compose the prompt: "Escalation esc\_… APPROVED by <supervisor|human>: <proposed_action>. <message>. Continue from where you stopped; do not restart the work. Your workspace and commit are unchanged." Delivery uses `inject_pane_text_verified` (`app/pane_delivery.rs:789`) with the interrupting probe posture, the same primitive `bossctl probe` uses (`ProbeRun { urgent: true, interrupt: true }`), so the text lands now even if the worker is mid-turn on a driver that accepts mid-turn input.
3. Record the settled outcome on the row: `delivery_state = delivered` on `Confirmed` or `PaneEcho`; `failed` with `delivery_detail` on `Unconfirmed` or `NotAcceptingInput`. A failed delivery is loud: the card keeps its chrome with the caption "Approved, but the worker did not receive it", the sheet offers "Retry delivery", and an engine audit line is written. It is never silently retried in a loop; the escalation sweep retries once per pass with a bounded count (3), after which the row stays `failed` for the human.

Approval does not change the task's status. The worker was `active` throughout.

### Denial: atomic blocked transition, then reap

Denial is human-only. Its durable effects are written in **one SQLite transaction**, in this order, with the terminal event staged for post-commit flush via `stage_execution_terminal` (`work/exec_status_helpers.rs:102-118`), never published inline:

1. `worker_escalations`: `state = denied`, `decided_by = human`, `decision_message = <reason>`, `decided_at`.
2. `task_blocked_signals` (the side table from `unify-blocking-signal-remediation.md:39`): a row with reason `escalation_denied` and a body containing the human's reason, the escalation summary, the proposed action, and the recovery pointer (`workspace_id`, `head_change_id`, `head_commit_id`).
3. `tasks.status = blocked` (or the chore equivalent), `last_status_actor = human`, `autostart` cleared.
4. `work_executions`: `abandoned`, with `record_worker_idle_abandonment`'s detail set to the denial reason.
5. Stage `ExecutionTerminal`.

After commit, the teardown sequence runs exactly as `finalize_idle_park` runs it today: forget in-memory trackers, `finish_worker_teardown` (pane reap, cube lease release, slot release), then `publish_work_item_changed` so the card moves and re-renders as blocked with the reason in its blocked chip.

Why this cannot acquire the race shape the parent description names: the orphan sweep considers only `active` rows (`orphan_sweep.rs` candidates), and by the time any subscriber can observe `ExecutionTerminal` the row is durably `blocked`. The `deliberate_parked` admission fact (`work/dispatch_admission.rs`), which keys on `run_done_outcome` plus the park attention kinds, is not relied on at all; no attention item participates, and no `run_done_outcome` is stamped, because the worker never declared anything. `worker_readoption`'s `Reap{TerminalByDecision}` classification for an `abandoned` execution with a live pane is the correct outcome here, because the pane is about to be reaped by the same code path.

### The escalation outlives its execution

A pending escalation must survive the worker dying (driver crash, engine host reboot, an operator `bossctl agents reap`). Otherwise the death sweeps would orphan the execution and the orphan sweep would mint a replacement that repeats the incident.

- **Admission gate.** `DispatchAdmissionFacts` (`work/dispatch_admission.rs`) gains an `escalation_pending: bool` fact, read from the open `worker_escalations` row, and every automatic mint path that mono#2963 already routes through `deliberate_parked` (`orphan_sweep`, `rescan_active_dispatch`, `reconcile_active_dispatch`, `reconcile_work_item_execution`, `reconcile_revision_execution`) treats it as blocking, emitting `skipped_reason: escalation_pending`. Unlike `deliberate_parked`, an explicit operator start does **not** clear it: the decision belongs to the escalation, and `bossctl work start` on a work item with an open escalation is refused with a message pointing at `bossctl escalations decide`. Same fail-loud-and-hold posture as the existing facts: an unreadable escalation state is not a licence to redispatch.
- **Status while dead.** The card's `worker_alive` flips to false (derived from the execution's `is_live()` at query time, never stored), the caption changes, and the escalation stays exactly where it was in the state machine. The supervisor, if pending, still runs: its inputs were snapshotted at dispatch.
- **Approve when dead** materializes as a resumption, not an in-place prompt: `deliver_escalation_approval` detects the non-live execution and instead mints a replacement execution with the same shape mono#2965 gives a blocked-run replacement (`preferred_workspace_id = workspace_id`, `allow_dirty`, `prefer_is_soft`), so `blocked_workspace_predecessor`'s verification and the `BlockedInPlace` / `BlockedFresh` recovery brief apply unchanged, with one addition: an "APPROVED ESCALATION" block ahead of the recovery block carrying the summary, the proposed action, and the decider's message. `delivery_state = resumed_by_redispatch`. This is alternative C, composed in for the one case where it is the only option.
- **Deny when dead** performs steps 1 to 3 of the denial transaction and skips the execution write and teardown.

### Feature flags and rollout

- `worker_escalation` (master, default off): the `escalate` kind is accepted, the row is written, the card and sheet work. Off means submit returns a typed `kind_disabled` error whose message tells the worker to use `propose blocked` and stop, which is today's behaviour, and the prompt directive is not emitted.
- `escalation_supervisor` (default on when the master is on): off routes every escalation to `pending_human` with `supervisor_outcome = disabled`.
- Rollback is a flag flip; no data is destroyed. Pending rows on a flipped-off engine remain visible to `bossctl escalations list` and still gate dispatch, so turning the flag off never silently frees a gated work item.

### Prompt updates

`runner/prompt.rs`: the "If you are blocked" section (`:1125-1160`) gains `escalate` as the first verb, with the gate example from the incident as its worked example, and the sentence that pairs `propose blocked` with `done --outcome blocked` (`:1206`) is replaced with: "`blocked` means you have genuinely decided to stop. If your work exists and only a decision you lack authority for is stopping you, call `escalate` instead and wait; do not declare done." The `run_done_directive` states that `done` is refused while an escalation is open. The conflict-resolution stop-condition text at `:857` and the two `:1337`/`:1390` sites keep `propose blocked` for the wedged-build case (there is no proposed action to approve).

### Observability

Counters registered through the metrics framework: `escalation.submitted`, `.withdrawn`, `.supervisor_approved`, `.supervisor_referred`, `.supervisor_unavailable`, `.supervisor_failed`, `.supervisor_timed_out`, `.human_approved`, `.human_denied`, `.delivery_failed`, `.resumed_by_redispatch`, and a gauge `escalation.pending_human`. Each state transition writes an `engine-audit.log` line keyed by escalation id and execution id. `bossctl escalations list` is the operator's ledger, and a pending-for-longer-than-24h escalation is reported there and on the card's age caption, not by an attention.

### Atomicity, stated as the invariant

Every write that an admission guard or a card reads is committed in the same transaction as the state transition it describes, and every event that a synchronous or asynchronous subscriber reacts to is staged into `PendingEvents` and flushed only after that commit. Concretely: the escalation row is written in the `SubmitProposal` transaction; the blocked-status, blocked-signal, and execution-abandoned writes of a denial are one transaction with `ExecutionTerminal` staged; and no path in this design writes an attention item as the durable representation of anything.

## Risks / open questions

- **Slot occupancy while waiting.** An escalating worker holds an interactive slot and a cube lease for as long as the human takes. With `MAX_CONCURRENT_INTERACTIVE_WORKERS = 8`, several long-pending escalations could stall the fleet. v1 accepts this deliberately: the alternative is a timeout, and a timeout that frees the slot must reap the worker, which is the terminate-and-resume path with its costs. The card shows age, `bossctl escalations list` shows the set, and the human can deny. A loud, non-deciding park after a long horizon (worker reaped, escalation stays `pending_human`, approval resumes in the same workspace) is listed as deferred work.
- **What approval actually authorizes.** Approval is instructional. For the incident, "approve: split into two PRs" is directly actionable by the worker; "approve: bypass the gate" is not, because the worker tier cannot invoke a bypass and this design must not add one. The sheet's message field lets the human say which. A capability grant scoped to an escalation id is real follow-on work; the questions manifest asks whether to plan it.
- **Supervisor judgment drift.** A reduced prompt approving things it should refer is the main way this design could do harm. Mitigations: the supervisor may never deny; every supervisor approval is recorded with its reason and counted; the flag routes everything to the human if the rate looks wrong. The rubric's "never weaken a control" line is the load-bearing sentence and should be tested with golden briefs in the conformance suite.
- **Worker does not stop.** A worker that keeps taking turns after escalating wastes tokens but breaks nothing. If this proves common, the CLI's post-submit instruction is the lever, not a reap.
- **Two deciders racing** (supervisor verdict and human decision in the same second) resolve by the row's state check inside each transaction; the loser gets `escalation_not_pending`. The human's sheet handles it by refreshing.
- **Finding surfaced while writing this, out of scope here:** the doc comment at `completion/nudge.rs:561` says a breaker park leaves the execution in `waiting_human` when `finalize_idle_park` writes `abandoned`. It belongs with the separately handled race fix.

## Proposed implementation task breakdown

Breakdown size: 13 entries (11 in-scope, 2 deferred) — the change adds one proposal-kind pair and one table (protocol, schema), threads the row through three engine seams that are separately reviewable (submit-time gating and sweep suppression; the human decision RPC with its approve and deny paths; the supervisor execution kind on the review pool), rewrites the worker prompt, adds an operator CLI, splits the app into the card surface and the decision sheet because they depend on different engine PRs, and carries the dead-worker resumption as its own unit; the two deferred entries record decisions the design made explicitly.

Parallelism notes name file overlap, not just functional independence. `completion/nudge.rs`, `completion/worker_signals.rs`, `orphan_sweep.rs` and `runner/prompt.rs` are the contended engine files; `ChatViewModel*.swift` is the contended app file.

### Protocol types for escalations

Scope: add `ProposalKind::Escalate` and `ProposalKind::EscalationVerdict` with their payload structs and validation in `boss_engine_proposal_validation`; add `WorkerEscalation` (bon builder), `EscalationState`, `SupervisorOutcome`, `TaskEscalationSummary`; add the `escalation` optional field to `Task` with `#[builder(default)]`; add `FrontendRequest::{ListEscalations, GetEscalation, DecideEscalation}` and `FrontendEvent::{EscalationUpdated, EscalationDecided}` plus the typed error codes (`escalation_pending`, `escalation_not_pending`, `not_bound_to_escalation`, `kind_disabled`); wire tests. No engine behaviour.
Effort: medium.
Dependencies: none.
Scope: in-scope

### Schema migration and WorkDb access for worker_escalations

Scope: `migrate_worker_escalations` appended to the migration chain (template and chain-equivalence test updated); `WorkDb` CRUD (`create_escalation` in-transaction, `get`, `list_open_for_work_item`, `has_open_escalation`, state transitions with state-check guards, `set_delivery_state`); the `get_work_tree` join that populates `Task.escalation`; the struct-literal mapper. Unit tests for the one-open-per-execution constraint and the join.
Effort: medium.
Dependencies: Protocol types for escalations.
Scope: in-scope

### Submit-time gating, head snapshot, and sweep suppression

Scope: the `escalate` applier in `work/proposal_apply.rs` writing the row in the submit transaction (gated policy with a durable effect), the `jj` head snapshot of the worker's workspace, `--withdraw`, the `done`-refused-while-pending check in `app/proposals.rs`, the `escalation_pending` predicate added to `unresolved_worker_signal_reason` with a new `StopOutcome::EscalationPending` and live-state reason, the `escalation_pending` fact on `DispatchAdmissionFacts` treated as blocking by every automatic mint path and refusing explicit start (`skipped_reason: escalation_pending`), the `proposal_expiry_sweep` exclusion, the `worker_escalation` flag, counters, and the assertion that a `waiting_human` execution with an open escalation is never a stale-sweep candidate. Behind the flag this PR is inert.
Effort: large.
Dependencies: Schema migration and WorkDb access for worker_escalations.
Scope: in-scope

### boss propose escalate and escalation-verdict CLI verbs

Scope: `propose.rs` subcommands `escalate` (with `--withdraw`) and `escalation-verdict`, `--*-file` variants, typed-error rendering, and the post-submit instruction text. Pure CLI and client crate. Runs in parallel with the two engine entries above (no file overlap).
Effort: small.
Dependencies: Protocol types for escalations.
Scope: in-scope

### Human decision RPC: approve delivery and atomic denial

Scope: `handle_decide_escalation` with the approve path (`deliver_escalation_approval` over `inject_pane_text_verified`, settled delivery recording, bounded retry in the escalation sweep, loud failure state) and the deny path (the single transaction: escalation denied, `task_blocked_signals` row with the recovery pointer, task `blocked`, execution `abandoned`, `ExecutionTerminal` staged; then `finish_worker_teardown`); `ListEscalations` / `GetEscalation`; `EscalationUpdated` / `EscalationDecided` and `WorkInvalidated` publishes; audit lines and counters. Integration test against an isolated engine proving the orphan sweep does not redispatch after a denial.
Effort: large.
Dependencies: Submit-time gating, head snapshot, and sweep suppression.
Scope: in-scope

### Supervisor execution kind on the review pool

Scope: `ExecutionKind::EscalationReview`, `WorkerKind::EscalationSupervisor` with `dontAsk` allowlist and deny belt, `compose_escalation_supervisor_prompt` (rubric, inputs, transcript tail, diff stat), review-pool dispatch including `kind_always_dispatches_on_pool_driver` and the migration backfill list, the `escalation_verdict` applier (calls the shared approve delivery on `approve`, sets `pending_human` on `refer`), the admission and verdict horizons, failure detection via the supervisor's `ExecutionTerminal`, the `escalation_supervisor` flag, and conformance golden briefs for the rubric. Sequenced after the decision RPC because approval delivery is one shared function.
Effort: large.
Dependencies: Human decision RPC: approve delivery and atomic denial.
Scope: in-scope

### Worker prompt rewrite for escalation

Scope: the escalation directive in `runner/prompt.rs` (`escalate` first, incident-derived example, "do not declare done while pending"), removal of the `propose blocked` then `done --outcome blocked` pairing, the `run_done_directive` note about refusal, and the worker preamble's one-paragraph summary; golden prompt tests updated. Last `runner/prompt.rs` pass in this project; forward-port anything the engine entries touched there.
Effort: small.
Dependencies: boss propose escalate and escalation-verdict CLI verbs; Human decision RPC: approve delivery and atomic denial.
Scope: in-scope

### bossctl escalations operator CLI

Scope: `bossctl escalations list|show|decide` over `ListEscalations` / `GetEscalation` / `DecideEscalation`, including the age column and `supervisor_outcome`. Runs in parallel with the supervisor entry and the prompt rewrite (separate crate, no overlap).
Effort: small.
Dependencies: Human decision RPC: approve delivery and atomic denial.
Scope: in-scope

### App: escalation card chrome, filter, and count chip

Scope: `WorkTask` and `WorkCardSnapshot` decoding of `Task.escalation` (plus the badge-strip slice for `.equatable()`), the raised-hand icon and "Needs decision" caption, the tint branch above `showsBlockedChrome`, top-sort within Doing, the "Needs decision" filter, the toolbar count chip, and the live-status texts. Read-only against the work tree; needs no decision RPC.
Effort: medium.
Dependencies: Schema migration and WorkDb access for worker_escalations.
Scope: in-scope

### App: escalation decision sheet

Scope: `EscalationDecisionSheet` bound to `pendingEscalationDecision` on `ChatViewModel`, context rendering, Approve with message, Deny with required reason, in-flight guard set, `EscalationUpdated` / `EscalationDecided` handling in `handle(_:)`, and the `escalation_not_pending` refresh-and-close path. Co-edits `ChatViewModel*.swift` with the card entry; sequenced after it and forward-ports its changes preservingly.
Effort: medium.
Dependencies: App: escalation card chrome, filter, and count chip; Human decision RPC: approve delivery and atomic denial.
Scope: in-scope

### Escalation outliving its execution: dead-worker resumption

Scope: `worker_alive` derivation in the summary, the approve-when-dead branch of `deliver_escalation_approval` that mints a replacement execution with the mono#2965 shape (`preferred_workspace_id`, `allow_dirty`, `prefer_is_soft`) and adds the "APPROVED ESCALATION" block ahead of the existing recovery brief in `runner/prompt/workspace_recovery.rs`, `delivery_state = resumed_by_redispatch`, the deny-when-dead branch that skips the execution write, and an integration test that reaps the escalating worker mid-pending and proves no replacement is minted until the human decides. Co-edits the decision RPC module and `work/dispatch_admission.rs`; sequenced after the supervisor entry.
Effort: medium.
Dependencies: Supervisor execution kind on the review pool.
Scope: in-scope

### Loud slot-release park for long-pending escalations

Scope: an operator-configurable horizon after which a `pending_human` escalation's worker is reaped and its slot and lease released while the escalation stays pending and visibly aged on the card, with approval then taking the dead-worker resumption path. Not a decision, so it does not violate the no-timeout rule, but it converts the live-worker case into the resumption case and the design chose not to do that in v1.
Effort: medium.
Dependencies: Escalation outliving its execution: dead-worker resumption.
Scope: deferred (future / not a v1 blocker) — v1 keeps the worker alive for as long as the human takes; only build this if slot occupancy from pending escalations is observed to stall the fleet.

### Approval-scoped capability grants

Scope: a one-shot, escalation-id-bound grant (for example a bypass token a gate can verify) so that a human approval of "push despite the gate" is mechanically actionable by the worker tier without softening the gate for anyone else. Needs its own design against checkleft and cube.
Effort: large.
Dependencies: Human decision RPC: approve delivery and atomic denial.
Scope: deferred (future / not a v1 blocker) — v1 approval is instructional by design; whether to plan this is a question in the manifest.
