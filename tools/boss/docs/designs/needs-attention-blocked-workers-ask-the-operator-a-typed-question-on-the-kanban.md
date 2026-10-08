# Needs Attention: blocked workers ask the operator a typed question on the kanban

- **Date:** 2026-10-05
- **Provenance:** execution `exec_18dbad1b68396fc8_1b` (project_design), project "Needs Attention: blocked workers ask the operator a typed question on the kanban"
- **Status:** design, awaiting review. Supersedes [worker-escalation-that-reaches-the-human-without-killing-the-run](worker-escalation-that-reaches-the-human-without-killing-the-run.md) (mono#2970), which proposed a keep-alive `boss propose escalate` and was never implemented.
- **Related designs:** [worker-proposal-api](worker-proposal-api-replace-fragile-worker-to-engine-seams.md) (the `boss propose` family), [dispatch-halt-state-vs-attention-items](dispatch-halt-state-vs-attention-items.md) (typed state the board reads is a field, not an attention), [attentions](attentions.md), [work-kanban](work-kanban.md)
- **Related operational docs:** [attention-lifecycle](../attention-lifecycle.md), [worker-liveness-contract](../worker-liveness-contract.md), [post-crash-recovery](../post-crash-recovery.md)
- **Baseline:** origin `main` at `cc72dac8`. Open PR mono#3061 (`boss propose wait`) touches the same engine files; expect rebase conflicts, no semantic overlap.

**TL;DR:** A worker that cannot finish without a human decision still ends its run with `boss propose done --outcome blocked`, but may attach one typed question (`--question`, `--answer-type yes-no`, `--explanation`). The engine stores that question in a dedicated `operator_questions` table, terminalizes the run exactly as a blocked declaration does today, and parks the task as `blocked` with the new reason `awaiting_operator_answer`, which the app routes into a collapsible "Needs Attention" section inside Doing. The card shows the question inline with a detail disclosure and Yes/No buttons. Yes appends a dated operator authorization to the task description and restarts the task in the same workspace through the existing explicit-dispatch path. No converts the park into the existing `worker_failed` shape, which already means "Backlog, blocked, with explanation". The contested property, stated up front: **the question is durable engine state on the work item that the kanban reads directly, the worker that asked it is gone by the time it is answered, and Yes is a restart with the authorization written into the brief, not a resume.**

## Goals

- A worker blocked on a decision only a human can make (relax a repository check, exceed a limit, proceed without a credential) ends with a _specific, answerable_ question rather than a prose summary the operator has to decode.
- The question lives **on the kanban card** in a new "Needs Attention" section inside Doing, modelled on the Done column's collapsible "Merging" section, visible only while it has cards.
- The operator answers inline on the card. Yes records an explicit, dated authorization in the task brief and restarts the task immediately. No parks the task in Backlog, blocked, with the question and explanation preserved as the reason.
- A task that is waiting for an answer is never rescheduled with an agent by any automatic path.
- Only Yes/No is required now, but the question model is a tagged enum so more answer types can be added without changing the storage shape, the wire shape, or the card's dispatch logic.
- A blocked run that attaches no question behaves exactly as today (`worker_failed`, Backlog).

## Non-goals

- **Keeping the asking worker alive.** The operator chose the simpler shape: the worker ends, Yes restarts. The keep-alive `waiting_human` state and the supervisor adjudication from the superseded escalation design are not built, and this design does not reserve hooks for them beyond the question row itself. The operator may revisit keep-alive later; nothing here forecloses it, because a future keep-alive path would create the same `operator_questions` row from a different proposal kind.
- **Routing the question through attentions.** No attention group or item is created for a question. The `attentions` table's `yes_no | multiple_choice | prompt` vocabulary is reused only as the `kind` tags of the new enums, so the two surfaces agree on names.
- **Implementing answer types beyond Yes/No.** `multiple_choice` and `prompt` are reserved tag values and rejected at validation until a follow-up implements them.
- **More than one question per run.** One run ends with at most one question. A worker that needs several decisions asks the most blocking one; the engine rejects a `run_done` that carries more than one.
- **Softening any gate.** Nothing in this design bypasses checkleft, `cube pr create` limits, or any CI check. Yes gives the next worker _written authorization_ to request a relaxation through the normal human-approved route; the restarted worker still cannot flip a check on its own.
- **Timeouts.** No automatic Yes or No. An unanswered question waits until the operator answers it or restarts, edits, or deletes the task through another path.
- **A general worker-to-operator chat.** One question, one answer, one restart.
- **Chores.** v1 covers tasks (`tasks` rows). A chore execution that attaches a question is accepted by validation but finalized through the plain `worker_failed` path with the question text folded into `blocked_detail`, so nothing is lost and nothing new is required of the chore surfaces. Extending the section to chores is a visible gap listed under risks, not a deferred task, because the operator spec is scoped to tasks.

## The problem, precisely

Today a worker that hits a human-only decision has exactly one sanctioned move: `boss propose done --outcome blocked --summary "..."`. The engine's path is `handle_submit_proposal` (engine/core/src/app/proposals.rs) → `apply_run_done` (work/proposal_apply.rs, stamps `work_executions.run_done_outcome`) → `finalize_declared_run_done` → `finalize_declared_blocked` (completion/run_done_declaration.rs) → `finalize_worker_failure` (completion/nudge.rs) → `record_worker_failure` (work/pr_flow.rs), which sets `tasks.status = 'blocked'`, `blocked_reason = 'worker_failed'`, `blocked_detail = <summaries>`, `autostart = 0`, and preserves the workspace preference on the execution.

Three facts shape the design:

1. **The park already works.** `task_accepts_execution` (work/exec*status_helpers.rs) refuses `worker_failed` rows, so automatic dispatch never restarts them. An explicit `RequestExecution` (`request_execution_in_tx_with_live_check`, work/dispatch_helpers.rs) resets `worker_failed` to `todo` and clears the blocker. "No → Backlog with explanation" and "Yes → restart" are therefore both existing engine moves. What is missing is the \_question*, the _routing_ of the card, and the _authorization_ text.
2. **The human-facing half is prose in a tooltip.** `blocked_detail` holds the worker's summary concatenated with any `propose blocked` reason, rendered by `WorkBlockedBadge.badgeTooltip` on a Backlog card. The operator has to read a paragraph, infer that a decision is wanted, work out what the decision is, edit the brief by hand, and press Start. Nothing in that chain says "the worker asked you something".
3. **Backlog is the wrong column.** `WorkTask.boardColumn` (app-macos/Sources/Models.swift) sends every `blocked` task to Backlog unless `isReviewPhaseBlocked`. A task waiting on the operator is work in progress that is stuck on the operator, not work that has been shelved. The operator does not scan Backlog for stuck work and, per the project description, does not read the Notifications window either.

No recorded reason exists for "a worker's request for a human decision is prose in a blocked tooltip". The superseded escalation design established that the attention-first routing was a decision about _agent-authored questions during a design pass_, never about hard gates, and that the `blocked` summary shape was a convenience pairing. That absence of a decision is the finding this design acts on.

## Alternatives considered

### A. An attention group with a `yes_no` member, surfaced as a card badge

Reuse `attention_items`' question model (`question_type`, `prompt_text`, `answer`) and `answer_attention` (work/attentions.rs), and add a badge on the card that opens the attention in `AttentionsView`.

Rejected on two checkable grounds. First, the operator has stated as a fact that attentions are not read; a badge that terminates in the Notifications window is the same unreachable path with an extra click. Second, [dispatch-halt-state-vs-attention-items](dispatch-halt-state-vs-attention-items.md) already settled the rule: state that the board and the dispatcher must both read is a typed field, and rendering attention rows on cards was rejected there because it "would have made the wrong representation load-bearing". A waiting-for-answer task is exactly that case: `task_accepts_execution` has to refuse it and the board has to route it. Deferred-scope badges are the precedent that _does_ use an attention on a card, and the reasoning does not disqualify them: a deferred-scope row describes something that happened and gates nothing. The reusable part is reused anyway: the `yes_no | multiple_choice | prompt` names become the enum tags.

### B. Keep the worker alive and inject the answer into its pane (the superseded escalation design)

`boss propose escalate`, a `waiting_human` execution state, a supervisor agent, and prompt injection on approval.

Not rejected on merit. The operator chose the terminate-and-restart shape because it is a fraction of the surface area (no new execution state, no liveness-contract changes, no supervisor pool, no prompt injection) and because mono#2963 and mono#2965 already made a blocked declaration resume in the preserved workspace, which removes the "replacement redoes 55 files" cost that motivated keep-alive. The cost of this choice is real and named: the restarted worker re-reads the brief and re-derives context instead of resuming mid-thought. The authorization text in the brief is how that cost is bounded. Keep-alive stays a possible later addition.

### C. Store the question as columns on `tasks`

Add `operator_question_json`, `operator_question_asked_at`, and `operator_question_execution_id` to `tasks`, cleared when answered.

Rejected because clearing the columns on answer erases the record of what was asked and answered, which is the one thing a later reader (the next worker, a reviewer of the PR, the operator a week later) needs. Keeping the answered question on the task row instead would mean the row carries one question's history forever, and a second question overwrites the first. The task already has more than five fields and a builder; adding three nullable columns whose meaning depends on each other is the shape the dedicated table avoids.

### D. Store the question on `work_executions`

Add the same columns to the execution that asked. The execution is the natural owner of "what this run ended with", and `run_done_outcome` already lives there.

Rejected as the _primary_ store because the answer and the authorization belong to the work item, not the run: the row that asked is `failed` and terminal by the time Yes is pressed, the row that benefits is the next execution, and the board renders the task. Reading "the open question" would mean joining the task to its latest failed execution and trusting that no later execution was minted in between, which is precisely the race the answer RPC has to be robust to. The execution keeps a pointer (the proposal id already links them through `worker_proposals.execution_id`), the table owns the state.

### E. Carry the question on `boss propose blocked` instead of `boss propose done`

Rejected because `finalize_declared_run_done` branches on the `run_done` payload, and the question must be in the payload the finalize reads so that "blocked with a question" and "blocked without one" are decided in one place from one row. The two-verb pairing (`propose blocked` then `propose done --outcome blocked`) is the convenience that turned into a defect surface in the escalation analysis; adding a third place to look would widen it. `propose blocked` keeps its meaning (a live run noting a blocker) unchanged.

### F. Answer from the app with `UpdateWorkItem` (patch the description, clear the block, press Start)

Rejected because it is three round trips with no atomicity: a crash or a second click between them leaves a task whose brief says "authorized" but which is still parked, or which was dispatched twice. The answer has to be one engine transaction with a single-use state transition.

## Chosen approach

### 1. Question model (protocol)

New types in `boss-protocol`, next to `RunDoneProposalPayload`:

```rust
/// What a blocked worker is asking the operator to decide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorQuestion {
    /// Specific, answerable, with the numbers that matter.
    /// e.g. "Approve bypass of the 30 max file limit (48 needed)?"
    pub text: String,
    pub answer_type: OperatorAnswerType,
    /// Why the worker is asking and what Yes buys. Rendered behind the
    /// card's detail disclosure, never inline.
    pub explanation: String,
}

/// Tagged so new answer types add a variant, not a column.
/// Tag values match `attentions.question_type` so the two surfaces
/// share a vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperatorAnswerType {
    YesNo,
    // Reserved, rejected by validation until implemented:
    // MultipleChoice { choices: Vec<String> },
    // Prompt,
}

/// The operator's answer, shaped by the question's `answer_type`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperatorAnswer {
    YesNo { value: bool },
}
```

`RunDoneProposalPayload` gains one optional field: `question: Option<OperatorQuestion>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Existing payloads deserialize unchanged.

**Validation** (`boss-engine-proposal-validation`, the `ProposalKind::RunDone` arm of `validate_payload`): `question` is permitted only when `outcome = blocked` (otherwise a field error on `question`: "a question is only meaningful on a blocked declaration"); `text` is required, non-empty, at most 500 characters, single line; `explanation` is required, non-empty, at most `MAX_LONG_FIELD_CHARS`; `answer_type.kind` must be `yes_no` (any other tag is a field error naming the unsupported kind). The invariant stated at the level that is load-bearing: **a stored `operator_questions` row always has an answer type the card knows how to render**, because the card's Yes/No buttons are the only answer UI in v1 and a stored question the card cannot answer would be a permanent park.

### 2. Storage: a dedicated `operator_questions` table (one migration)

```sql
CREATE TABLE operator_questions (
  id            TEXT PRIMARY KEY,            -- 'oq_' prefix
  work_item_id  TEXT NOT NULL,
  execution_id  TEXT NOT NULL,               -- the run that asked
  proposal_id   TEXT NOT NULL,               -- the run_done row; payload_json is the text's source of truth
  question_json TEXT NOT NULL,               -- OperatorQuestion, canonical JSON from validation
  status        TEXT NOT NULL CHECK (status IN ('open', 'answered', 'withdrawn')),
  answer_json   TEXT,                        -- OperatorAnswer; NULL unless answered
  answered_by   TEXT,                        -- 'human' (app or CLI); NULL unless answered
  answered_at   TEXT,
  withdrawn_reason TEXT,                     -- e.g. 'restarted_without_answer', 'task_deleted'
  created_at    TEXT NOT NULL
);
CREATE UNIQUE INDEX operator_questions_one_open_per_item
  ON operator_questions(work_item_id) WHERE status = 'open';
CREATE INDEX operator_questions_by_item ON operator_questions(work_item_id, created_at);
```

The partial unique index is the race guard: there is at most one open question per work item, enforced by the database rather than by the ordering of application code. The row is inserted inside `apply_run_done`'s transaction, from the validated payload, so the question exists if and only if the declaration applied. `question_json` duplicates the proposal payload deliberately: the proposal ledger stays the audit source of truth (the project description's rule), the table row is what every reader joins on, and the duplication is a copy of immutable validated JSON, so the two cannot drift.

**How it survives restart.** The row is engine state, not memory. Engine restart changes nothing. Task restart through Yes marks the row `answered` and leaves it in place; the next execution is a new row with its own id, and the authorization it reads is in the task description, not in the question row. Task restart through any _other_ explicit path (`bossctl work start`, drag to Doing, `boss task move`) marks the open row `withdrawn` with reason `restarted_without_answer` inside the dispatch transaction (see §5). The history of questions on a task is the ordered set of rows, which `boss task show` lists.

### 3. Finalize: the question variant of a blocked declaration

`finalize_declared_blocked` in `completion/run_done_declaration.rs` reads the applied `run_done` proposal's `question`. Without one, the path is byte-for-byte today's. With one, it calls a sibling of `record_worker_failure`, `record_worker_awaiting_operator_answer`, which does everything `record_worker_failure` does (execution `status = 'failed'`, lease and workspace released, workspace preference preserved, diagnostic folded into run history) except the task write, which becomes:

```sql
UPDATE tasks
   SET status = 'blocked',
       blocked_reason = 'awaiting_operator_answer',
       blocked_detail = <question.text>,
       autostart = 0,
       last_status_actor = 'engine',
       updated_at = ?now
 WHERE id = ?1 AND deleted_at IS NULL AND status IN ('active', 'todo');
```

`blocked_detail` is set to the question text alone so that any surface that only knows `blocked_detail` (the tooltip, `boss task show`, the coordinator's probe) still shows the question verbatim rather than the concatenated summaries; the explanation and summary are reachable through the question row and the proposal ledger. The task is left in `blocked`, not `active`, so every existing guard that treats `blocked` as non-dispatchable keeps working. The same `in_review` / domain-reason protection `record_worker_failure` applies (a helper execution must not stomp a parent that is in review) applies here unchanged: if the task is not `active`/`todo`, the question is still stored, but the task row is left alone and the question is immediately `withdrawn` with reason `task_not_parkable`, because a question nobody can see on a card is a silent park.

**Why the execution is `failed`.** The execution status vocabulary is untouched: a question-ending run is terminal, its slot and lease are released, and every sweep (`orphan_sweep`, the husk reconciler, `work_item_is_deliberately_parked`) already treats `failed` with `run_done_outcome = 'blocked'` correctly. "This run ended by asking" is expressed by the `operator_questions` row and the task's `blocked_reason`, which is the dimension the equivalence with `worker_failed` holds on: **resource lifecycle and sweep behaviour are identical; dispatch admission and board routing are not.** Adding an execution status would touch the `CHECK` clause, `is_terminal`, the app's runtime decoder, and every status match, for no reader that needs it.

**Never auto-rescheduled.** `task_accepts_execution` (work/exec_status_helpers.rs) gains `awaiting_operator_answer` alongside `worker_failed`. That one line is the admission guard; `work_item_is_deliberately_parked` is unchanged because it already returns `false` for a `failed` latest execution, which is the compatibility shape the comment there describes. No attention item is filed for the park (today's `worker_failed` path files none for a declared block either, so nothing is removed).

### 4. The answer RPC

```rust
FrontendRequest::AnswerOperatorQuestion {
    /// `oq_…`, or a task id / friendly short id, resolved to that task's open question.
    id: String,
    answer: OperatorAnswer,
}
```

Reply: `FrontendEvent::WorkItemUpdated` carrying the task as it stands after the answer (the same event `UpdateWorkItem` replies with), plus a `WorkItemUpdated` push on the product topic so every connected app converges. Errors are typed: `NotFound` (no such question, or the task has none), `Conflict { state: answered | withdrawn, answer: Option<OperatorAnswer> }` (the question is no longer open), and `ValidationFailed` (answer kind does not match the question's `answer_type`).

**Idempotency and races.** One transaction:

1. `UPDATE operator_questions SET status = 'answered', answer_json = ?, answered_by = 'human', answered_at = ?now WHERE id = ?1 AND status = 'open'`. If zero rows were affected, re-read the row: if it is `answered` with an equal `answer_json`, return success with the current task (a double click or a retried CLI call is a no-op, and no second dispatch happens because the row transition is the only thing that triggers one); if it is `answered` with a different answer or `withdrawn`, return `Conflict`. Equality is on the canonical JSON, so "Yes then Yes" is idempotent and "Yes then No" is refused rather than silently flipping a task that is already being redispatched.
2. Re-read the task inside the same transaction and require `status = 'blocked' AND blocked_reason = 'awaiting_operator_answer'`. If the task moved under the operator (edited to another status, deleted, restarted through another path that somehow did not withdraw the row), roll back and return `Conflict` with the task's current state. This is belt-and-braces over §5's withdrawal; the unique index guarantees there is only ever one row to check.
3. Branch on the answer.

**Yes.** In the same transaction: append the authorization block (§6) to `tasks.description` through the same write boundary `UpdateWorkItem` uses, so `validate_description_update` runs (the append only grows the text, so it always passes); then `UPDATE tasks SET status = 'todo', blocked_reason = NULL, blocked_detail = NULL, autostart = 1, last_status_actor = 'human', updated_at = ?now`. `autostart = 1` is what makes the card route to Doing the instant the event lands (`boardColumn`'s `todo where autostart` arm) rather than flickering through Backlog until the worker starts; it is cleared on start exactly as for any other autostarted task. Then, still in the transaction, call `request_execution_in_tx_with_live_check` with `preferred_workspace_id` = the asking execution's preserved workspace preference and `entry_point = OperatorAnswer`, so the restart lands in the workspace the branch state is already in. After commit, the ordinary coordinator dispatch picks up the `ready` execution; the answer RPC does not wait for a worker to start.

**No.** In the same transaction: `UPDATE tasks SET blocked_reason = 'worker_failed', blocked_detail = ?detail, last_status_actor = 'human', updated_at = ?now WHERE id = ?1 AND blocked_reason = 'awaiting_operator_answer'`, where `detail` is:

```
Operator declined on 2026-10-05: "Approve bypass of the 30 max file limit (48 needed)?"

Worker's explanation: this PR sweeps residual non-tmux references and adds an architecture guard test, touching every Boss package (48 files).

Run summary: <the run_done summary>
```

`status` stays `blocked`, `autostart` stays `0`, so the card routes to Backlog through `boardColumn`'s default arm with the standard blocked badge and this text as its tooltip. This is the existing `worker_failed` representation, so `task_accepts_execution` keeps refusing it and an explicit Start later clears it exactly as it clears any worker failure. Nothing new is needed on the No path beyond the detail text.

**CLI.** `boss task answer <task-id|oq-id> (--yes | --no)` calls the same RPC. It exists so the operator can answer from a terminal and so the engine tests can exercise the whole path without the app. `boss task show` prints the open question (text, answer type, asked-at, execution) and the answered history.

### 5. Withdrawal: when something else moves the task

`request_execution_in_tx_with_live_check` already resets `worker_failed` rows on explicit dispatch. It gains, in the same statement group: `UPDATE operator_questions SET status = 'withdrawn', withdrawn_reason = 'restarted_without_answer' WHERE work_item_id = ?1 AND status = 'open'`, and the task reset's `blocked_reason = 'worker_failed'` predicate becomes `blocked_reason IN ('worker_failed', 'awaiting_operator_answer')`. `DeleteWorkItem` withdraws with reason `task_deleted`. `UpdateWorkItem` patches that change `status` away from `blocked` withdraw with reason `status_edited`. A `blocked_reason` change through any path other than the answer RPC is treated as an operator override and withdraws with reason `reason_edited`. Each of these is one statement next to the existing write, and each leaves the row as a record of a question that was asked and never answered, which `boss task show` lists so nobody wonders whether the answer was lost. An answer click that lands after any of them returns `Conflict` and the app drops the card from Needs Attention on the next `WorkItemUpdated`.

### 6. The exact text appended on Yes

Appended to `tasks.description`, separated from the existing brief by a blank line and a rule:

```

---

## Operator authorization (2026-10-05 14:32 UTC)

- **Question the previous worker asked:** Approve bypass of the 30 max file limit (48 needed)?
- **Answer:** Yes
- **The worker's explanation:** this PR sweeps residual non-tmux references and adds an architecture guard test, touching every Boss package (48 files).
- **Asked by run:** `exec_18dbad1b68396fc8_1b`

The operator recorded this answer on the kanban. It is explicit, human-granted approval for exactly what the question asks and nothing broader. Treat it as the authorization the worker rules require before relaxing a check or exceeding a limit for this task; state in the PR body that the operator authorized it on the date above. It does not authorize bypassing any other check, and it does not change what the repository's checks enforce.
```

The timestamp is the engine's `now_string()` rendered in UTC with minutes. The heading is a fixed string so the worker prompt can refer to it by name and a reviewer can grep for it. A second Yes on a later question appends a second block; blocks are never edited or removed by the engine. The "Asked by run" line is the only identifier in the block and uses the `exec_` form, never a friendly short id, so a worker quoting it cannot trip the text-leakage checks.

### 7. Worker prompt

The `blocked` bullet in `runner/prompt.rs` (the `## Declaring your run finished` section) gains the question flags, and the earlier "STOP and surface it for operator approval" sentence in the checks section points at them. The guidance, in substance:

- When the only thing between you and delivery is a decision a human must make (relax a check, exceed a limit, a missing authorization, two instructions that conflict), end with `propose done --outcome blocked` **and attach the question**: `--question "<one specific question>" --answer-type yes-no --explanation "<why, and what Yes buys>"`.
- Phrase it so Yes or No is a complete answer: name the check or limit, the number you need against the number allowed, and what you will do on Yes. "Approve bypass of the 30 max file limit (48 needed)?" is the model; "Can I proceed?" is not.
- Put the justification in `--explanation`, not in the question. The operator sees the question on the card and opens the explanation only if they need it.
- One question per run. Ask the one that blocks you; if Yes would only reveal a second decision, say so in the explanation.
- Before asking, check the brief for a `## Operator authorization` section. If the authorization you need is already there, you are not blocked; proceed and cite it in the PR body.
- Do not ask for things you are allowed to decide, and do not ask in order to hand back work that is merely hard. The question path is for authorization, not effort.

The `compose_prompt_tests` snapshots change in the same PR, and a new snapshot pins the bullet. The test that pins today's blocked bullet is updated in the same diff that changes the premise, not left to defend the old text.

### 8. App: the Needs Attention section and the inline question

**Wire.** `Task` gains `operator_question: Option<OperatorQuestionView>` (id, `text`, `answer_type`, `explanation`, `asked_at`, `execution_id`), populated by the task mapper from the single open `operator_questions` row for the task (the partial unique index makes that a single-row lookup). `None` when there is no open question. The field is a projection, like `blocked_attempt_id`; the mapper sets it explicitly from a named column, per the builder-pattern rule for DB mappers.

**Routing.** `WorkTask.boardColumn` adds `case "blocked" where isAwaitingOperatorAnswer: return .doing`, where `isAwaitingOperatorAnswer` is `status == "blocked" && blockedReason == "awaiting_operator_answer" && operatorQuestion != nil`. The membership predicate gates on `status`, for the same stale-scalar reason `WorkBlockedBadge` gates on `status`: a `blocked_reason` or question that outlives the status must not route a card. `WorkBlockedBadge.label(forReason:)` learns the new reason ("Needs Answer") and adds it to `knownReasons`.

**Section.** `computeWorkSections(in: .doing)` splits items by `isAwaitingOperatorAnswer`, the same way `.done` splits by `isInMergingSection`, and prepends `needsAttentionSection(items:)`, a sibling of `mergingSection` returning `nil` when empty: `id: "doing-needs-attention"`, title "Needs Attention", `isCollapsible: true`, `defaultExpanded: true`, a new `WorkBoardGroupKey.needsAttention`. Ordering is `asked_at` ascending (oldest question first), ties on task id. Under project grouping the section still sits above the project groups: a stuck task is stuck regardless of which project it belongs to, and the project groups are built from the remaining items. The optimistic-kanban group resolver (`ChatViewModel+OptimisticKanban.swift`) maps the new key the same way it maps `.merging`.

**Card.** A card in the section renders, below the title, in order: the question text; a "Why?" disclosure; a row of two buttons, "Yes" and "No". The `WorkCardSnapshot` carries the question fields so the card stays a pure render of its snapshot. Pressing a button calls `sendAnswerOperatorQuestion(id:answer:)` on the engine client, disables both buttons until the reply arrives, and shows a transient error on `Conflict`/`NotFound` while leaving the card in place until the next `WorkItemUpdated` moves it. Keyboard: the buttons are ordinary focusable controls; no default button, because a stray Return must not authorize a bypass.

**When the text is too long for the card.** The question is rendered with a three-line limit and tail truncation; the full text is in the card's tooltip and in the detail disclosure. The "Why?" disclosure is a popover anchored to the card containing the full question, the explanation, the run summary, and the asked-at time, scrollable and never truncated. The blocked badge's tooltip shows `blocked_detail`, which is the question text, so hovering the badge also reveals it. A question is capped at 500 characters by validation precisely so the three-line inline rendering is the common case and the popover is the exception.

### 9. What a blocked run without a question does

Nothing changes. `finalize_declared_blocked` without a `question` runs today's `record_worker_failure`, the task is `worker_failed` in Backlog, the tooltip holds the summaries, and an explicit Start restarts it. A run that files `propose blocked` and then `propose done --outcome blocked` without a question is likewise unchanged. The question is strictly additive on the declaration.

### 10. Sequence

```
worker                 engine                                   app
  |  propose done --outcome blocked --question ... |
  |------------------------------------->|
  |        validate_payload (question allowed only on blocked)
  |        apply_run_done: stamp execution, INSERT operator_questions(open)
  |        finalize_declared_blocked -> record_worker_awaiting_operator_answer
  |          execution failed, lease released, workspace preference kept
  |          tasks: blocked / awaiting_operator_answer / detail = question
  |        publish WorkItemUpdated ------------------------------------->|
  |                                                  card routes to Doing > Needs Attention
  |                                                  operator presses Yes
  |                                   <-- AnswerOperatorQuestion {id, yes_no:true}
  |        UPDATE operator_questions ... WHERE status='open'  (1 row or idempotent/conflict)
  |        append authorization to description
  |        tasks: todo / autostart=1 / reason cleared
  |        request_execution (preferred workspace = the asking run's)
  |        commit; reply + publish WorkItemUpdated ----------------------->|
  |        coordinator dispatches the ready execution        card shows in Doing, worker starts
```

### 11. Second question type: create a prerequisite task

`OperatorAnswerType` gains `CreatePrerequisiteTask { name, brief }` for the case where the task cannot proceed until some other, unrelated piece of work lands first (the motivating example: a CI-fix revision finds a pre-existing test fixture that races retention cleanup; that needs its own fix on main and is out of scope for the revision). It is separate from `boss propose followup-task`, which stays the non-blocking "nice to have later" path.

**Worker side.** `boss propose done --outcome blocked --answer-type create-prerequisite-task --prerequisite-name <name> --prerequisite-brief <brief> --explanation <one line: why this task cannot unblock without it>`. `--question` is not accepted for this type: validation writes the wording ("Create prerequisite task '<name>'? This task will wait for it.") so every card reads the same, and replaces whatever `text` the payload carried. The name is single-line and at most 200 characters; the brief is bounded like any long field. The worker prompt teaches when to use this type and that the brief must stand alone, because a different worker picks the prerequisite up cold.

**Kanban.** Identical to the Yes/No question: same Needs Attention section, same inline card, same Yes/No buttons, and the same `OperatorAnswer::YesNo` on the wire. The "Why?" popover additionally shows the proposed task's name and brief. An older app build that does not know the kind falls back to the plain blocked card, exactly as for any undecodable answer type.

**Yes**, in the one answer transaction (the transaction is now `Immediate`, so racing answers serialize on the write lock and the loser is an idempotent repeat):

1. Look for an equivalent open task: same product, same name once trimmed and case-folded, not `done`/`archived`/deleted, oldest first, skipping any candidate that already depends on the blocked task (it would form a cycle). If one exists, reuse it; otherwise create the proposed task as a chore in the same product with `autostart` on and the brief as its description. The recent-name guard `check_recent_duplicate` is bypassed (`force_duplicate`) in the create arm, as batch-accept does, because the open-task lookup above is the duplicate check here and the 60-second guard would only reject re-proposing a task that just finished.
2. Add a `blocks` edge, blocked task depends on the prerequisite, through `add_dependency_edge_in_tx`. The blocked task is first put in `todo` with `autostart` on so the edge's ordinary auto-block parks it as `blocked` / `dependency`; it leaves Needs Attention because the question is `answered` and the block reason is no longer `awaiting_operator_answer`.
3. Append a dated "Operator-approved prerequisite" note to the brief, saying whether the prerequisite was created or an existing task was linked.
4. Leave a `waiting_dependency` execution on the blocked task (non-revision kinds), carrying the asking run's workspace preference. The dependency cascade only promotes an execution that already exists, and the ordinary reconcile deliberately never mints a replacement after a terminal run, so without this row the task would unblock to `todo` and stay there. When the prerequisite reaches its satisfied state the existing cascade moves the task to `todo` and promotes that execution to `ready`.

Any failure (a cycle, a description guard) rolls the whole answer back and the question stays open. A repeated Yes finds the `answered` row first and returns the current task without touching anything; a No after a Yes is a `Conflict`.

**No** is the Yes/No decline unchanged: `worker_failed` in Backlog with the decline detail, which quotes the prerequisite question.

**Revision dependents.** The dependency layer treats a prerequisite in `in_review` as satisfied for a `revision` dependent so a revision can stack on its own chain's open PR. `gating_prereqs_for` limits that relaxation to prerequisites in the revision's own chain: an unrelated prerequisite linked by this question (new or deduplicated) gates a revision until it is `done`, and completion redispatches it.

**Dedup.** The equivalence lookup skips tasks a human has blocked, and a matching `todo` task with autostart off has autostart turned on (and the note says so), so the linked prerequisite is always one that will be dispatched.

## Risks / open questions

- **The restarted worker may not honour the authorization.** It is text in the brief, and the worker rules still say "never relax a repository check without approval". The prompt change (§7) tells the worker that the `## Operator authorization` section _is_ that approval, but a check relaxation still has to be made as a reviewable change in the PR; the authorization does not make `checkleft` or `cube pr create` behave differently. A reviewer should confirm this is the intended strength: the operator is authorizing the worker to _propose_ the relaxation with human backing, not granting a mechanical bypass.
- **Execution status stays `failed`.** Any dashboard or cost report that counts `failed` executions will count question-ending runs as failures. The `operator_questions` row and `run_done_outcome = 'blocked'` disambiguate for anyone who joins, but a reader who only looks at status will over-count. If that matters, a later change can add a `run_done_question_id` projection to execution listings without a status change.
- **Chores are out of v1** (see Non-goals). A chore worker that attaches a question gets the plain `worker_failed` fold. Worth confirming that chores do not hit operator-approval gates often enough to need the section now.
- **`autostart = 1` on Yes.** It is set so the card moves to Doing immediately. If dispatch is paused engine-wide, the task will sit in Doing as `todo` with the autostart marker until dispatch resumes, which is the same behaviour as any autostarted task today, but an operator who just pressed Yes may expect a worker within seconds. The answer reply could carry the dispatch admission outcome so the app can show "queued: dispatch paused"; this design leaves the existing autostart presentation in place and flags it.
- **Worker over-asking.** The question path is cheaper for a worker than finishing. The prompt text guards against it and one question per run bounds it, but the real check is operator experience; if Needs Attention fills with effort-shaped questions, the prompt guidance needs tightening, not the model.
- **Rebase against mono#3061.** `boss propose wait` edits the same CLI, validation, apply, finalize, and prompt files. The implementer of the backend row should rebase onto it once it merges rather than racing it.

## Proposed implementation task breakdown

Breakdown size: 2 entries (2 in-scope, 0 deferred) — the change has exactly two review seams: the engine/protocol/CLI path that makes a question durable, parks the task, and answers it (usable end-to-end from the CLI alone), and the app path that renders the section and calls the answer RPC; the operator spec fixes this at two rows and nothing in the design adds a third seam.

### Backend, CLI, and worker prompt: typed question on a blocked declaration, durable park, answer RPC

**Scope.** Everything the engine needs for a blocked declaration to carry a question and for an operator to answer it from the CLI: the `OperatorQuestion` / `OperatorAnswerType` / `OperatorAnswer` protocol types and the optional `question` on `RunDoneProposalPayload`; validation (question only on `blocked`, `yes_no` only, length caps); the `operator_questions` table and migration with the one-open-per-item partial unique index; the insert in `apply_run_done`; `record_worker_awaiting_operator_answer` and the `finalize_declared_blocked` branch; the `task_accepts_execution` guard; the `AnswerOperatorQuestion` RPC with Yes (append authorization, `todo` + `autostart = 1`, request execution in the preserved workspace) and No (convert to `worker_failed` with the declined-question detail) in one transaction, idempotent on repeated equal answers and `Conflict` otherwise; withdrawal on explicit dispatch, delete, and status/reason edits; the `operator_question` projection on the `Task` wire type and its mapper; `boss propose done --question/--answer-type/--explanation` and `boss task answer`; the worker-prompt change and its snapshot tests. Chores take the fold-into-`worker_failed` path described in Non-goals.

**Deliverables.**

- Protocol types and the `question` field, with serde round-trip tests including the reserved-tag rejection.
- Migration creating `operator_questions` and both indexes; schema test proving the partial unique index refuses a second open row.
- Validation tests: question on `delivered` rejected; non-`yes_no` rejected; over-long text rejected; valid question canonicalises.
- Finalize tests: blocked with question → task `blocked`/`awaiting_operator_answer`, detail is the question text, execution `failed`, lease released, workspace preference preserved, no attention item; blocked without question → unchanged `worker_failed` path (existing tests still pass unmodified).
- Admission test: `awaiting_operator_answer` is never auto-dispatched.
- Answer RPC tests: Yes appends the exact authorization block and mints a `ready` execution with the preserved workspace preference; No produces the `worker_failed` detail; double Yes is a no-op with one execution minted; Yes-then-No is `Conflict`; answer after explicit restart is `Conflict` and the row is `withdrawn`; answer after delete is `NotFound`.
- CLI: `boss propose done` flags with clap `requires` wiring; `boss task answer --yes|--no`; `boss task show` prints open and historical questions.
- Prompt: updated blocked bullet and checks-section pointer; `compose_prompt_tests` snapshots updated in the same diff.

**Effort:** large.

**Dependencies:** none.

Scope: in-scope

### App: Needs Attention section with the inline question and Yes/No

**Scope.** The macOS app half: parse `operator_question` on `WorkTask`; `isAwaitingOperatorAnswer` and the `boardColumn` arm that routes it to Doing; `needsAttentionSection(items:)` modelled on `mergingSection`, inserted by `computeWorkSections(in: .doing)` above project groups, with the new `WorkBoardGroupKey` case and its optimistic-kanban mapping; the card rendering (three-line question, "Why?" popover with the full question, explanation, summary and asked-at, Yes/No buttons with in-flight disabling and conflict handling); `WorkBlockedBadge` label and `knownReasons` for the new reason; `sendAnswerOperatorQuestion` on the engine client and the `WorkItemUpdated` reply handling. Runs against the RPC and wire fields the first entry lands, so it must follow it.

**Deliverables.**

- Parser test: `operator_question` present/absent decodes; a `blocked` task with the reason but no question does not route to Doing.
- `boardColumn` and section tests: routing, ordering by `asked_at`, section omitted when empty, section present under both flat and project grouping.
- Card snapshot or view tests for the inline question, the truncation at three lines, and the disabled state while an answer is in flight.
- Engine-client request encoding test for `answer_operator_question`.
- A capture of the section with one card (attached via `boss attach`, described in the PR body), stating what was verified.

**Effort:** medium.

**Dependencies:** Backend, CLI, and worker prompt: typed question on a blocked declaration, durable park, answer RPC.

Scope: in-scope

The two entries are strictly sequential (the app reads wire fields and calls an RPC that only exist after the first lands); there is no parallelism in this breakdown and the two touch disjoint files.
