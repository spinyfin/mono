# Boss: Chore Lifecycle When a PR Is Closed Without Merge

## Problem

The chore lifecycle today assumes a PR ends one of two ways: merged, in which case the merge poller (PR #237) flips `status` from `in_review` to `done`; or still open, in which case the chore stays `in_review` until the next sweep. There is no third path.

Concretely, on 2026-05-07 PR #252 was *closed without merge* because it conflated two layers and the user/coordinator decided to scrap it. Chore `task_18ad6b85b6fdd1e0_f` then sat indefinitely at `status='in_review', pr_url=<#252>` even after GitHub reported `state=CLOSED, merged=false`. The merge poller (`tools/boss/engine/src/merge_poller.rs:108-114`) only flips the chore when `state=MERGED` *or* `mergedAt` is set; close-unmerged matches neither and is silently ignored. The coordinator had to manually reset the chore back to `active`.

The valid chore statuses today (`tools/boss/cli/src/main.rs:569`) are `[todo, active, blocked, in_review, done]`. Two states are missing:

- "the work was attempted, the PR was rejected, the chore is back on the table" — the *retry* state.
- "the work is abandoned and not coming back" — the *give-up* state.

The user's framing in the chore notes — *"it's unclear if 'Done' is the right state for such things"* — is right. `done` means the work landed; a closed-unmerged PR did not land. Marking these `done` corrupts history and breaks any consumer that reads `done` as "shipped" (the activity feed, the kanban Done column, and any future dependency edge that gates on `done`). Leaving them `in_review` strands them. Auto-resetting to `active` loses the signal that this chore had a prior failed attempt — and dispatch will happily re-spawn a worker on the same description that just produced a rejected PR, looping the engine indefinitely on bad scope.

This doc proposes two new chore statuses, an extension to the merge poller's existing `gh pr view` probe, a `prior_pr_urls` history column, explicit CLI verbs (`boss chore reopen` / `boss chore abandon`), a heuristic re-dispatch policy, a new `FrontendEvent::ChorePrClosedUnmerged` push, and a one-shot startup sweep that reconciles chores whose PRs were closed-unmerged while the engine was offline. Same product, same merge-poller cadence, no new background loop.

## Goals

- Detect "the chore's PR was closed without merge" automatically, on the same poll cadence as merge detection.
- Add a non-terminal `needs_attention` status that distinguishes "work was attempted, needs human disposition" from `active` (untouched) and `in_review` (PR pending).
- Add a terminal `abandoned` status, explicit and symmetric with `done`, so consumers can read "this work didn't land" without falling back to `done`.
- Preserve the closed PR URL as history so the kanban / activity feed can surface "tried once before" and the worker (on re-attempt) can read what didn't work.
- Auto-detect → `needs_attention` is the default; promotion back to `active` (re-dispatchable) requires an explicit verb. The dispatcher does not auto-pick `needs_attention` chores.
- Add CLI / bossctl verbs symmetric with PR #240's `bossctl work cancel` so a coordinator can dispose of these without hand-editing rows.
- Reconcile on engine startup so chores whose PRs were closed while the engine was offline don't silently stay in `in_review` after restart.
- Migration is additive: existing rows keep their current statuses; a one-shot backfill probes any `in_review`-with-`pr_url` chore whose PR is now closed-unmerged and moves them to `needs_attention`.

## Non-Goals

- **Merge-conflict handling on the dependent PR.** That's `auto-rebase-stacked-prs.md`'s territory.
- **Force-pushed PRs that look closed-and-reopened.** A force-push doesn't change `state`; the existing `pr_url` and PR record persist. No special handling.
- **PRs against a non-`main` base.** Chore lifecycle treats the base branch as opaque; the merge / close signals from `gh pr view` work the same way regardless of base.
- **Auto-fix of the chore description.** When a PR is rejected because the chore as written can't ship, the user (or coordinator) edits the description by hand. No engine-side description rewrite.
- **Bringing back project_tasks** — same enum, same lifecycle apply, but project_tasks have additional ordinal interactions that are out of scope for v1; the rules below extend cleanly to them in a follow-up.
- **Cross-product / cross-repo PRs.** Same restriction as PR #237; the merge poller already operates per-product.
- **Detecting "PR merged with conflicts" or other GitHub anomaly states.** Only `state=CLOSED, merged=false` is in scope; `state=OPEN` (with or without conflicts) leaves the chore in `in_review`.

## Naming

- The new non-terminal status is **`needs_attention`**. It reads as a state (not a verb) and matches the kanban / coordinator use ("chores needing my attention"). I deliberately avoid `rejected` (overloaded with PR review-rejection, which is different), `pending_review_decision` (too long), and `attention` (too vague).
- The new terminal status is **`abandoned`**. Symmetric with `done`: both are terminal, both are written once and not auto-cleared. `abandoned` is the explicit "this is not coming back" state; `done` stays meaning "shipped."
- The history column is **`prior_pr_urls`** (JSON array of strings). New rows store all closed-unmerged PR URLs in chronological order; the live `pr_url` column continues to point at the *current* PR (or NULL when the chore is between attempts).
- The CLI verbs are **`boss chore reopen`** (move `needs_attention | abandoned → active`) and **`boss chore abandon`** (move anything → `abandoned`). Symmetric with `bossctl work cancel` (PR #240): both are dispositive coordinator verbs.
- The bossctl coordinator surface verb is **`bossctl work needs-attention`**, mirroring the existing `bossctl work start` / `bossctl work cancel` shape.

---

## Design Question 1 — State Model

### Options

- **(a) Reuse `active`.** On PR close-unmerged, revert to `active`. No schema change.
- **(b) Add `needs_attention`** (non-terminal). Forces explicit human disposition before the dispatcher picks the chore up again.
- **(c) Add `abandoned`** (terminal). Explicit "not reattempting." Pairs with reopen verbs.
- **(d) Add both `needs_attention` and `abandoned`.** Default-on-close-unmerged is `needs_attention`; the user picks `reopen` (→ `active`) or `abandon` (→ `abandoned`).

### Discussion

**(a) loses the failure signal.** A re-dispatch on the same description that just produced a rejected PR is a tight feedback loop with the wrong fix — the engine has no way to tell "this is a retry, the description needs editing first" from "this is a fresh attempt." The auto-dispatcher's notion of "ready" reduces to `status='active'` and an unleased workspace; there is no idempotency on the chore content. So (a) silently re-spawns workers on chores that humans already decided shouldn't ship as written. This is the worst residual outcome.

**(b) alone — `needs_attention` only — works for the close-unmerged path** (the chore lands somewhere out of `in_review` and out of the dispatcher's pickup pool), but leaves the user with no way to express "this is dead, stop showing it to me." The chore stays in `needs_attention` forever until manually moved to `done` (which is wrong — see Problem) or stays parked.

**(c) alone — `abandoned` only — handles the give-up case** but skips the disposition step. Default-on-close to `abandoned` is too aggressive (the user may want to retry); default-on-close to `active` (i.e. (a)) loops; default to `done` corrupts history. Without an intermediate state the engine has no good default.

**(d) — both — gives the engine an unambiguous default and the user an explicit choice.** Auto-transition is `in_review → needs_attention`; the user (or coordinator) picks the next move with `reopen` or `abandon`. Two new statuses sounds like a lot, but each carries a distinct meaning, and neither overlaps with an existing column. The kanban gets one new lane (`Needs Attention`) which is naturally placed between `Doing` and `Review` (or as a sub-lane of `Blocked` — see Q5).

### Recommendation

**Pick (d).** Add both `needs_attention` (non-terminal) and `abandoned` (terminal). On PR close-unmerged the engine writes `needs_attention`; explicit verbs flip to `active` (reopen) or `abandoned` (abandon). The set of valid statuses becomes:

```text
todo, active, blocked, in_review, needs_attention, done, abandoned
```

Two are terminal (`done`, `abandoned`); five are non-terminal. The dispatcher only picks up `active` (and `todo` via the auto-advance path in `work.rs:650`); `needs_attention` is explicitly *not* re-dispatchable until promoted.

`MoveTarget` (`tools/boss/cli/src/main.rs:578`) gains `NeedsAttention` and `Abandoned` so the existing `boss chore move` verb can write the new states without going through the dedicated reopen/abandon verbs (which are still the canonical surface — see Q6).

### Schema

`status` is a `TEXT` column; widening the enum is a CLI / engine validation change, not a schema change. The migration consists of:

- Validation: `WorkDb` accepts the two new strings on read/write paths. The `validate_status` helper in `work.rs` (the inline match in `update_task` is the current home) gets two new arms.
- Engine literals: anywhere the engine reads `status` with a hard-coded enum (e.g. `list_chores_pending_merge_check`'s `WHERE status = 'in_review'`), audit and adjust. The list of pending-merge candidates is unchanged — `needs_attention` is *not* pending merge — but the kanban grouping and reconcile paths get the new statuses.

The protocol `Task` already serialises `status: String` (`tools/boss/protocol/src/types.rs`), so the Swift side picks up the new strings via JSON without a wire-shape change. A Swift `TaskStatus` enum extension is the only client-side code change.

---

## Design Question 2 — Detection

### What `gh pr view` already returns

The merge poller's probe (`merge_poller.rs:66-115`) already extracts `state` and `mergedAt`:

```rust
"--json", "state,mergedAt",
"--jq",   r#"[(.state // ""), (.mergedAt // "")] | @tsv"#,
```

GitHub's `state` is one of `OPEN | CLOSED | MERGED`. The current code only flags `merged=true` when `state=MERGED` *or* `mergedAt` is non-null. The close-unmerged case is `state=CLOSED, mergedAt=null`. That is already in the response — the code just throws it away.

### Options

- **(i) New polling loop.** A second background task that lists open PRs and joins against closed-unmerged. Independent of `merge_poller`.
- **(ii) Hook into `merge_poller`.** Same `gh pr view` call, additional branch in the response handler. The probe returns `Open | Merged | ClosedUnmerged`, and `sweep_one` dispatches on the variant.
- **(iii) Subscribe to GitHub webhooks.** Push-based, near-zero latency. But requires a publicly reachable webhook endpoint, which the home-machine setup doesn't have.

### Recommendation

**Pick (ii).** Extend `PrMergeState` to a three-state enum and route the new branch through `sweep_one`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open,
    Merged,
    ClosedUnmerged,
}

pub struct PrLifecycleProbe {
    pub url: String,
    pub state: PrLifecycleState,
    /// CLOSED-state timestamp; `None` for `Open`. Used as the
    /// `closed_at` field on the new frontend event.
    pub closed_at: Option<String>,
}
```

`PrMergeState` and the `MergeProbe` trait are renamed accordingly (or `MergeProbe` keeps its name and the variant rides on the result type — minor; I prefer the rename for clarity). The `--json` query gains `closedAt`:

```rust
"--json", "state,mergedAt,closedAt",
"--jq",   r#"[(.state // ""), (.mergedAt // ""), (.closedAt // "")] | @tsv"#,
```

`closedAt` is set whenever `state=CLOSED` or `state=MERGED`. We use it only for the `ClosedUnmerged` variant.

`sweep_one` becomes:

```rust
match probe.state {
    PrLifecycleState::Open           => return false,                       // unchanged
    PrLifecycleState::Merged         => mark_merged(...).await,             // existing path
    PrLifecycleState::ClosedUnmerged => mark_closed_unmerged(...).await,    // NEW
}
```

The polling cadence and the `list_chores_pending_merge_check` query are unchanged. Latency budget is identical to today's merge detection — a few seconds to a minute, dictated by `interval` in `spawn_loop`.

### Why not (iii)

Webhooks are a real win for latency, but the home-machine engine is not internet-reachable. A pull-based design that already exists is good enough for this workload (PRs close on the order of a few per day). If we ever stand up a hosted engine, webhooks become a strict upgrade and the same handler signature applies.

### Network failure mode

The existing probe maps "PR doesn't exist" (404) to `merged=false` (`merge_poller.rs:90-98`). Under the new scheme that maps to `state=Open` — i.e. "do nothing this pass." Same conservative behaviour: a deleted-PR or unreachable-GitHub keeps the chore where it is and the next sweep retries. We deliberately do not treat a 404 as `ClosedUnmerged`, because that would write a misleading state for force-deleted-but-merged-elsewhere PRs (rare, but possible).

A persistent network outage means `needs_attention` transitions land late, not wrong; same residual property as merge detection has today.

---

## Design Question 3 — `pr_url` Handling on Re-Attempt

### Options

- **(α) Clear `pr_url` on close-unmerged.** Chore returns to `needs_attention` with `pr_url=NULL`.
- **(β) Keep `pr_url` pointed at the closed PR.** Treat it as a historical pointer until the next worker overwrites it.
- **(γ) Add a `prior_pr_urls` JSON array column.** Live `pr_url` always points at the current PR (or NULL between attempts); `prior_pr_urls` accumulates closed-unmerged URLs in chronological order.

### Discussion

(α) is the cheapest schema change but loses signal. A coordinator who reopens a chore can no longer link back to the rejected PR without checking history. The kanban / activity feed can't render a "previous attempt" badge because there's no data left to render.

(β) keeps the PR URL but conflates "this is the chore's current PR" (which is the load-bearing meaning of `pr_url` in `mark_chore_pr_merged`'s WHERE clause and in the auto-detect symmetry) with "this is the last PR we tried." A worker that creates a *new* PR in a re-attempt would either overwrite `pr_url` (losing the old one) or refuse to overwrite (forcing the user to clear it manually). Both are bad.

(γ) is the cheapest schema change that keeps both signals. `pr_url` keeps its current meaning (current attempt's PR, or NULL); `prior_pr_urls` is purely additive. The macOS UI can show a "tried N times before" badge that reads the array's length, and the worker spawn prompt (Q8) can include the most recent prior URL so the worker reads what didn't work.

### Recommendation

**Pick (γ).** Add `prior_pr_urls TEXT` (JSON array, default `'[]'`). On the `in_review → needs_attention` transition:

1. Append the current `pr_url` to `prior_pr_urls` (idempotent — if it's already the last element, no-op).
2. Set `pr_url = NULL`.
3. Stamp `last_status_actor = 'engine'` (matches the convention from `work-dependencies.md` Q4).
4. Update `updated_at`.

```sql
UPDATE tasks SET
    status            = 'needs_attention',
    pr_url            = NULL,
    prior_pr_urls     = json_insert(prior_pr_urls, '$[#]', ?prev_pr_url),
    last_status_actor = 'engine',
    updated_at        = ?now
WHERE id = ?id AND status = 'in_review' AND pr_url = ?prev_pr_url;
```

The guard on `status = 'in_review' AND pr_url = ?prev_pr_url` is the idempotency lock: a second sweep observing the same close-unmerged finds the row already in `needs_attention` and the UPDATE matches zero rows. No double-append.

### Re-attempt behaviour

When the chore is reopened (`needs_attention → active`), `pr_url` stays NULL. The worker spawn flow (`spawn_flow.rs`) is responsible for setting `pr_url` once the new PR exists, exactly like a fresh first attempt. The worker prompt (Q8) carries the most recent entry from `prior_pr_urls` so the worker can read the rejected PR before opening a new one.

### Schema

```sql
ALTER TABLE tasks ADD COLUMN prior_pr_urls TEXT NOT NULL DEFAULT '[]';
```

`prior_pr_urls` is JSON (`TEXT` with a `json` shape); SQLite's JSON1 functions read and write it. Existing rows default to `'[]'`; the migration is a no-op for existing data.

A more aggressive design would add a separate `chore_pr_history` table with one row per PR attempt and richer fields (`opened_at`, `closed_at`, `decision`). The user explicitly asked for "the cheapest schema change that preserves enough signal for the macOS UI to show 'tried once before'" — a column suffices, the table doesn't pay for itself yet. If activity-feed-driven UX later needs per-attempt timestamps, the table is the natural follow-up.

---

## Design Question 4 — Auto-Redispatch

### Options

- **(A) Auto-pick.** When the chore lands in `needs_attention`, the dispatcher treats it like `active` and re-spawns a worker.
- **(B) Manual pick.** `needs_attention` is explicitly outside the dispatcher's pickup pool; the user (or coordinator) runs `boss chore reopen` or `bossctl work start` to re-dispatch.
- **(C) Heuristic.** Auto-pick *only* if the chore description has been edited since the close (i.e. `tasks.updated_at > rebase_attempt's_close_timestamp`); otherwise wait for explicit start.

### Discussion

**(A) loops on bad scope.** PR #252's exact failure mode — chore conflates two layers, PR rejected — would re-spawn a worker that produces another conflated PR. The engine has no way to distinguish "the description was right; the worker's implementation was wrong" (worth retrying) from "the description was wrong" (worth editing first). Auto-pick assumes the former; the close-unmerged signal more often means the latter.

**(B) is the safest default.** The user already has to look at the chore (the `needs_attention` lane is visible, the bossctl `needs-attention` verb surfaces it for the coordinator) before disposing of it; making them flip the verb at the same moment is a single keystroke of friction in exchange for never re-looping on a bad chore.

**(C) sounds tempting but the heuristic is fragile.** "Was the description edited since the close?" requires storing the close timestamp somewhere — either we add a `last_close_unmerged_at` column on `tasks` or we read it from `prior_pr_urls`'s entry shape, which we deliberately kept simple in Q3. Even with the timestamp, the heuristic misses cases where the chore description was edited *before* the close (e.g. the user started editing during review, then the PR closed). And it auto-picks cases where the user edited the description but the edit was non-substantive (whitespace, typo). The false-positive rate is high enough that (B)'s explicit verb is friendlier than (C)'s wrong guess.

### Recommendation

**Pick (B).** `needs_attention` is the dispatcher's "do not pick up" lane. The user (or coordinator) runs `boss chore reopen <id>` to flip to `active`, at which point the auto-dispatcher picks it up like any other `active` chore.

The dispatcher's existing pickup query (`work.rs:262-280` and the `auto-dispatch` paths in `coordinator.rs`) gates on `status` so the change is one extra excluded variant: `WHERE status = 'active' AND ...` already excludes `needs_attention` by virtue of not matching. We should still audit any place the dispatcher reads `status` with a fuzzier match (e.g. "non-terminal status") and explicitly exclude `needs_attention` and `abandoned`.

### Symmetry with `boss chore move`

A user can manually `boss chore move <id> --to active` to reach the same state without using `boss chore reopen`. That's allowed — the `move` verb is the generic state setter, the `reopen` verb is sugar for the close-unmerged → active path that also clears the `needs_attention` badge in the UI cleanly. Both call the same engine path; `reopen` records `last_status_actor = 'human'` and tags an audit trail entry of "reopened from needs_attention" for the activity feed.

### Why not auto-pick on reopen?

Reopening already implies the user wants to re-dispatch — that's the whole point of the verb. So `reopen` flips to `active` *and* the auto-dispatcher picks the chore up on its next sweep, no extra action needed. This is symmetric with how `boss chore create --autostart` already auto-dispatches.

---

## Design Question 5 — Frontend Event and UI Affordance

### What the engine emits today

`merge_poller::sweep_one` calls `publisher.publish_work_item_changed(product_id, work_item_id, "pr_merged")` after a successful merge. The macOS app subscribes to `work_invalidated` / `work_item_changed` topics and refetches the affected row.

There is no typed payload for "what kind of change was this"; the `reason` parameter (`pr_merged` etc.) is a free-form string. The kanban renders the new status from the refetched `Task` row and that's the visible signal.

### Options

- **(I)** Reuse `publish_work_item_changed` with `reason = "pr_closed_unmerged"`. The Swift side keys off the reason string to render any extra UI affordance.
- **(II)** Add a typed `FrontendEvent::ChorePrClosedUnmerged { chore_id, pr_url, closed_at }` push that carries the close-unmerged context as structured data, in addition to the generic `work_item_changed` topic event.

### Recommendation

**Pick (II).** Both halves: emit the existing `work_item_changed` (so subscribers that just want the new row state get it) *and* a new `FrontendEvent::ChorePrClosedUnmerged` push (so subscribers that want the close-unmerged context — the activity feed, a future "show me what attempts I've abandoned this week" view — get a typed payload).

```rust
// tools/boss/protocol/src/wire.rs (FrontendEvent additions)
ChorePrClosedUnmerged {
    product_id: String,
    chore_id: String,
    /// The PR URL that closed without merge. Already moved into
    /// `prior_pr_urls` on the chore by the time this event lands.
    pr_url: String,
    /// GitHub-reported close timestamp.
    closed_at: String,
},
```

This event is broadcast on the same product topic the existing `WorkItemChanged` rides; subscribers picking it up replace their cached row from the accompanying `WorkItemChanged` (no separate fetch needed).

### Macos UI affordance

The kanban gets one new lane — `Needs Attention` — between `Doing` and `Review`, ordered chronologically by `updated_at DESC`. Cards in this lane render with a subtle "previous attempt" badge in the footer:

```text
┌──────────────────────────────────┐
│ Engine app RPC: bind to socket   │
│ chore_18ad…f3                    │
│ ─────────────────────────────────│
│ ↺ tried 2× before · #252         │  ← badge
│ feat-engine-app-rpc · in chore   │
└──────────────────────────────────┘
```

The badge text is generated from `prior_pr_urls.length`; click → opens the most recent prior PR in the browser (URL is pulled from the array). For `prior_pr_urls.length == 1` the badge reads `↺ tried 1× before · #N`; for empty (which shouldn't happen for `needs_attention` chores but is possible if a coordinator manually moved a chore there) the badge is omitted.

Out of scope for this design: the visual treatment itself (color, icon choice). In scope: the data contract that supports it.

### Drag-out behaviour

Dragging a `needs_attention` card to:

- **Doing** (i.e. `active`) → equivalent to `boss chore reopen`. Allowed; engine runs the same write path.
- **Review** (i.e. `in_review`) → refused with an inline warning ("a chore in needs_attention has no current PR to be in review for; reopen instead"). The UI offers a one-click "reopen instead" affordance.
- **Done** → refused with the same error a `move … --to done` already produces today (the existing path requires a `pr_url`; we extend the validation to disallow done-from-needs-attention as well).
- **Abandoned** → allowed, equivalent to `boss chore abandon`.
- **Backlog / Todo** → allowed as a "park it for later, but I haven't decided yet" path. Stays out of the dispatcher's pickup pool because `needs_attention` already was; `todo` is also out except for `autostart` chores (which are an existing fast-path).

---

## Design Question 6 — CLI Verbs

### Verbs

```text
boss chore reopen   <selector>             # needs_attention | abandoned → active
boss chore abandon  <selector> [--reason]  # any non-terminal → abandoned
boss chore move     <selector> --to needs_attention | abandoned   # generic
```

`boss chore move` is the generic state setter (already exists, already accepts the kanban targets); we widen `MoveTarget` (`tools/boss/cli/src/main.rs:578`) to include `NeedsAttention` and `Abandoned`. The `reopen` and `abandon` verbs are sugar with the disposition semantics:

- `reopen` is only valid if the chore is currently `needs_attention` or `abandoned`. Flipping a `done` chore to `active` is not a "reopen" — it's an unrelated correction and should go through `move`.
- `abandon` is valid from any non-terminal status. Abandoning a chore in `in_review` (PR still open) is allowed but unusual; the CLI prints a warning ("PR <url> is still open; consider closing it on GitHub first") but does not refuse.
- `abandon --reason "..."` writes a free-form reason to a new `abandoned_reason` column on the chore (see Schema). When omitted, the column is NULL. The macOS card detail view surfaces the reason if present.

### `boss chore status set` — the generic verb

The chore notes mention "possibly `boss chore status set` as a generic verb." We *don't* need a new generic verb — `boss chore move` already plays that role. Adding `status set` would duplicate the role with no behavioural delta. Leave it.

### Coordinator surface (bossctl)

```text
bossctl work needs-attention            # list chores in needs_attention across all products
bossctl work needs-attention --json     # same, JSON output for tooling
```

This is the chore-side counterpart of `bossctl work start` / `bossctl work cancel` (PR #240). The list is sorted by `updated_at DESC` (most-recently-attended-to first); each row includes the chore id, name, the most recent prior PR URL, and the close timestamp.

### Symmetric examples

```text
# A close-unmerged just landed. Coordinator decides to retry.
$ bossctl work needs-attention
chore_18ad6b85…f  "wire BOSS_APP_PID handling"  closed 2026-05-07T14:22Z  prior #252

$ boss chore reopen chore_18ad6b85…f
reopened chore_18ad6b85…f → active (will be picked up on the next dispatcher sweep)

# Or: coordinator decides this chore is dead.
$ boss chore abandon chore_18ad6b85…f --reason "scope conflated with the dispatch-cache work; superseded by chore_18ad…b3"
abandoned chore_18ad6b85…f. Reason stored. PR #252 was already closed on GitHub.
```

Both verbs return JSON in `--json` mode (`Task` row post-update); both write `last_status_actor = 'human'` and an audit-trail entry to the activity feed.

### Reference doc updates

`boss reference` gains a new section under chore status semantics:

> `needs_attention` is set automatically when a chore's PR is closed without merge. The chore is held out of the dispatcher's pickup pool until you run `boss chore reopen <id>` (re-dispatch) or `boss chore abandon <id>` (terminal).
>
> `abandoned` is a terminal status, symmetric with `done` but meaning "this work didn't land and isn't being reattempted." Use `abandoned` rather than `done` when a PR was rejected — `done` is reserved for PRs that merged.

---

## Design Question 7 — Interaction With Auto-Detect (Conflict Resolution)

### The hazard

The merge poller emits two transitions: `in_review → done` (on merge) and `in_review → needs_attention` (on close-unmerged). They are mutually exclusive at any single moment, but the chore can move between them over time:

- User abandons chore C with `boss chore abandon`. C is now `abandoned`, `pr_url = NULL`, the closed PR is in `prior_pr_urls`.
- Someone (coordinator, the user from the GitHub UI, a recover script) reopens the closed PR on GitHub and merges it.
- The merge poller's next sweep sees `state=MERGED` for the URL in `prior_pr_urls`. But the merge poller's pickup query (`list_chores_pending_merge_check`) only returns chores with `status='in_review' AND pr_url IS NOT NULL`. C is `abandoned` and has no live `pr_url`. So the poller never queries that URL, and never observes the merge.

So the poller's *current* shape gives `abandoned` precedence over a future merge — *because the poller can't see the merge.* But that's accidental, not designed. If a future change widened the pickup query (e.g. "any chore whose `prior_pr_urls` contains an unresolved PR"), we'd suddenly have a "what wins, abandoned-terminal or done-on-merge?" question to answer.

### Options

- **Abandoned wins.** A terminal status is terminal. If a PR somehow merges later, log it but do not flip the chore.
- **Done wins.** Treat the merge as the load-bearing signal; flip `abandoned → done`.
- **Engine refuses to flip and surfaces an attention item.** Coordinator picks.

### Recommendation

**Abandoned wins, with a logged informational event.**

The argument for "done wins" is that a merged PR is the most concrete signal — work shipped, regardless of what the human said earlier. But that overstates the merge's authority: a coordinator who explicitly ran `boss chore abandon` made a judgement call ("this work isn't being reattempted") and a later reopen-and-merge from somewhere else is more likely to be a mistake (someone resurrected the wrong PR) than the coordinator's earlier judgement being wrong. Auto-flipping `abandoned → done` without coordinator input would silently overrule them.

The "engine refuses and surfaces an attention item" option is too friction-heavy for what is going to be a near-zero-frequency event. Logging is enough.

Concretely:

- `abandoned` chores are not in `list_chores_pending_merge_check` (the WHERE excludes them by `status != 'in_review'`). The poller will never see a future merge of their PR through the existing pipeline.
- We deliberately do *not* extend the pickup query to include `abandoned`-and-prior-PR-URLs. That keeps `abandoned` truly terminal and avoids the conflict.
- If, at some point, an engine path *does* observe a merge of an abandoned chore's prior PR (e.g. the macOS app's own GitHub-watcher feature), the engine logs `tracing::warn!(chore_id=…, pr_url=…, "merge of abandoned chore's prior PR observed; not transitioning")` and emits an activity-feed entry. Status stays `abandoned`.
- If the user explicitly wants to "un-abandon" because of the merge, they run `boss chore reopen` (which moves `abandoned → active`) and then `boss chore move <id> --to done` (which writes `pr_url = <merged PR>` and `status = done`).

### Symmetric case: `needs_attention` and a future merge

A chore in `needs_attention` has `pr_url = NULL` by design (Q3). The merge poller's pickup query excludes it. So the same shape applies — a future merge of a prior PR is invisible to the poller, and the chore stays `needs_attention` until the human disposes of it.

If a worker is re-dispatched on the chore (after `reopen`), creates a new PR, and that new PR merges, the existing path works unchanged: `pr_url` is set by the worker, `status = in_review` after the worker's Stop, the merge poller picks it up, transitions to `done`.

---

## Design Question 8 — Worker Behaviour on Re-Attempt

### What the worker sees today

The boss-managed worker spawn injects a CLAUDE.md preamble into the worker session (the chore description, the workspace path, the lease id) — see `tools/boss/engine/src/spawn_flow.rs`. Today there is no signal that this is a re-attempt: a worker leasing a workspace whose branch already has commits from a prior failed attempt has no way to know the prior attempt closed-unmerged.

### Options

- **Status quo.** Worker reads no special context; if there are leftover commits on the branch, it figures it out from `jj log`.
- **Augmented prompt.** When `prior_pr_urls` is non-empty for the chore being dispatched, the spawn prompt includes the most recent prior URL plus a brief instruction.
- **Full diff.** Engine fetches `gh pr diff <prior-PR>` and embeds it in the prompt.

### Recommendation

**Augmented prompt.** Inject one extra paragraph into the spawn prompt when `prior_pr_urls` is non-empty:

```
## This is a re-attempt

A previous attempt at this chore was opened as PR <most-recent-prior-PR-url>
and closed without being merged on <closed_at>. Read the PR description and
review comments before starting:

    gh pr view <prior-PR-url>          # description
    gh pr view <prior-PR-url> --comments

If the prior PR was rejected because the chore description itself was wrong,
stop, comment on the chore (`boss chore update --description "…"` or via the
coordinator), and ask for clarification rather than producing another PR with
the same problem.

The branch may or may not contain leftover commits from the prior attempt;
run `jj log -r main..@` to see the current state. You are not obligated to
preserve those commits — start fresh from `main` if the prior approach was
wrong.
```

The `<prior-PR-url>` and `<closed_at>` tokens are filled from the chore row at spawn time. When `prior_pr_urls` is empty (i.e. this is the first attempt), the paragraph is omitted entirely.

### Why not the full diff

A full diff in the prompt eats context window for content the worker can fetch on demand with `gh pr diff` — and `gh` is already in the worker's environment. The prompt addition is a pointer; the worker fetches the data when needed. This keeps the prompt small and keeps the worker's context spent on actual work.

### Where this lives

The augmented prompt is a one-line read in `spawn_flow.rs::compose_spawn_prompt` (or whichever function builds the prompt today). The query is `SELECT prior_pr_urls FROM tasks WHERE id = ?`, parsed as JSON, last element is the most-recent prior. If the JSON is malformed (corruption), we silently omit the paragraph and log a warning — the worker still runs, just without the re-attempt signal.

---

## Design Question 9 — Migration

### Existing chores in production today

Some chores in the live engine are in `in_review` with PRs that have already closed unmerged. They are silently failed today (the case PR #252 documents). On engine startup, we should reconcile.

### Options

- **No migration.** Existing rows stay where they are; only new close-unmerged events are caught. Users notice the lingering rows and dispose of them by hand.
- **One-shot startup sweep.** On engine start, iterate `list_chores_pending_merge_check` (all `in_review`-with-`pr_url` chores) and run the new probe once. Any `ClosedUnmerged` rows transition to `needs_attention` immediately.
- **Continuous full sweep.** Same as the startup sweep, but at every poller tick, with no incremental optimisation.

### Recommendation

**Pick the startup sweep.** Idempotent (same WHERE clause as `mark_close_unmerged` so a second sweep is a no-op), bounded (at-most-N candidates where N is small in practice), and addresses the documented backfill case.

Concretely: `app.rs::start_engine` calls `merge_poller::run_one_pass(&work_db, &probe, &publisher)` once during startup, *before* the regular poller loop spawns. Today's `run_one_pass` already handles the merge case; it'll handle the close-unmerged case once Q2 lands. The startup sweep is just "call the same function once at boot," and any chores that should have been transitioned while the engine was offline catch up in one pass.

The startup sweep runs in the background — we don't block engine start on it. If `gh` is slow (cold cache, network blip), the sweep takes longer but the engine is up and serving. The first regular poller tick will overlap with the startup sweep's tail; the existing idempotency on the WHERE-guarded UPDATE makes that safe.

### Schema migration

```rust
fn migrate_chore_close_unmerged(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "prior_pr_urls")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN prior_pr_urls TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    if !table_has_column(conn, "tasks", "abandoned_reason")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN abandoned_reason TEXT",
            [],
        )?;
    }
    Ok(())
}
```

Bump `metadata.schema_version`. Both columns are nullable / defaulted, so existing rows keep their current data.

---

## Design Question 10 — Bossctl Coordinator Surface

### Verb

```text
bossctl work needs-attention                   # list, all products
bossctl work needs-attention --product <id>    # filter
bossctl work needs-attention --json            # tooling-friendly
```

Output format mirrors `bossctl work list` (which lists running executions). One row per chore, sorted by `updated_at DESC`:

```text
chore_18ad6b85b6fdd1e0_f
    name:        wire BOSS_APP_PID handling
    product:     boss
    closed_at:   2026-05-07T14:22:01Z
    prior:       2 attempts (#252, #248)
    description: <first 200 chars>
```

JSON output is a `Vec<Task>` filtered to `status = 'needs_attention'`. The same shape the existing `boss chore list --status needs_attention` produces, accessed through the bossctl-style RPC verb that the coordinator already speaks.

### Implementation

The engine already has `WorkDb::list_chores`, parameterised on status. The bossctl verb becomes a thin wrapper that calls it with `status = 'needs_attention'`, prints in human or JSON mode. The protocol gets one new request type:

```rust
// tools/boss/protocol/src/wire.rs
FrontendRequest::ListChoresNeedingAttention {
    product_id: Option<String>,
}

FrontendEvent::ChoresNeedingAttentionList {
    chores: Vec<Task>,
}
```

This is symmetric with the existing `ListChores` request shape; we could also reuse `ListChores` with a status filter argument and skip the dedicated request. I prefer the dedicated request because it has a coordinator-specific semantic ("here are things you have to look at") that warrants its own typed surface; the underlying engine path is shared.

### Why not a standing notification

The coordinator could subscribe to the `ChorePrClosedUnmerged` push (Q5) and react in real time. That's strictly better than polling — and bossctl can do this if it wants — but the *verb* (`bossctl work needs-attention`) is the synchronous "show me everything queued up right now" surface, which is the load-bearing thing for a coordinator that just woke up and wants the catch-up view.

---

## Design Question 11 — Failure Modes and Open Questions

### 11.1 GitHub unreachable when the close-unmerged signal would have arrived

The probe returns an error → `sweep_one` logs and returns false → the chore stays in `in_review`. Same residual property as today's merge detection: the next poller tick retries, the next-next retries, and so on. Persistent outage means delayed transition, not corrupt state.

If the engine is restarted during a long outage, the startup sweep (Q9) re-runs the probe and catches up once GitHub is reachable. No special handling.

### 11.2 User closes-and-reopens the PR rapidly

Sequence: PR closes; poller sweeps, observes `state=CLOSED`, transitions chore to `needs_attention` and clears `pr_url`; user reopens the PR ten seconds later; chore is in `needs_attention` with no live `pr_url`.

The chore does *not* automatically flip back to `in_review` because:

- The poller's pickup query gates on `pr_url IS NOT NULL`, so the reopened PR is invisible to the poller.
- The engine has no other pathway to discover the reopened PR (nothing watches GitHub for reopen events).
- The chore description / prior_pr_urls have already been updated; reverting silently would leave them inconsistent.

The user observes a `needs_attention` chore in the UI and a reopened PR on GitHub, with no link between them. They run `boss chore move <id> --to in_review` and `boss chore bind-pr <id> <pr-url>` (existing verb, `cli/src/main.rs:147`) to relink. We add a one-line note in the `bind-pr` reference doc covering this case.

A more aggressive design would hook the close-unmerged path to *not* clear `pr_url` and instead keep it pointed at the closed PR until a new attempt overwrites it (back to option (β) in Q3). Then a reopen-and-merge could be picked up by widening the merge poller's WHERE to include `status='needs_attention'` chores. I deliberately don't do that for v1: the close-and-reopen-rapidly case is rare, the manual `bind-pr` recovery is cheap, and (γ)'s clean separation of "current PR" vs "history" is more valuable than supporting a 1-in-N edge case that already has a recovery path.

### 11.3 Chore deleted while PR was open

A coordinator runs `boss chore delete <id>` while the chore's PR is still open. Today the delete sets `deleted_at` (soft delete) but leaves the row queryable. The poller's pickup query (`list_chores_pending_merge_check`) excludes `deleted_at IS NOT NULL` rows. So the deleted chore is invisible to the poller; if the PR later closes-unmerged, no transition fires. Status remains as it was at delete time (likely `in_review`).

This is the same shape today's merge detection has: a deleted chore with a still-live PR is silently outside the engine's awareness. We don't change it. If the user un-deletes (clears `deleted_at`), the chore re-enters the poller's pickup pool and the next sweep observes the close-unmerged.

### 11.4 PR transferred to another repo

`gh pr view <old-url>` returns a 404; the existing 404 handling (`merge_poller.rs:90-98`) treats this as `state=Open` (do nothing). Chore stays in `in_review` indefinitely. The user observes the orphaned `in_review` chore in the UI; they run `bind-pr` with the new URL (or `abandon` if they don't care). Same as 11.3 — manual recovery, no engine-side automation.

### 11.5 PR closed-unmerged, then admin re-opens and merges

Already addressed in Q7: the chore's `pr_url` is NULL after the close-unmerged transition, so the merge poller does not see the merge. The chore stays in `needs_attention` until the human disposes. If the user wants to absorb the merge, they `reopen` then `move … --to done` after `bind-pr`-ing the now-merged URL.

### 11.6 Merge poller's startup sweep collides with normal sweep

Two sweeps might run concurrently during the first poll interval after startup. Both call `mark_close_unmerged` with the same `(work_item_id, prev_pr_url)` pair; both UPDATEs run with the WHERE guard `status = 'in_review' AND pr_url = ?prev_pr_url`. The first writer wins (zero-row update on the second). No double-append to `prior_pr_urls`.

### 11.7 `prior_pr_urls` JSON gets corrupted

A direct DB edit, a botched migration, or a downgrade-then-upgrade leaves `prior_pr_urls` in a non-array shape. Engine paths that read it (Q3 transition, Q8 prompt augmentation) parse defensively: if `serde_json::from_str::<Vec<String>>(...)` errors, treat as empty array, log `tracing::warn!`, continue. Worst case: the prior URL doesn't show up in the badge or prompt; the transition still completes.

### 11.8 What if `closed_at` is missing from `gh pr view`

A PR with `state=CLOSED` should always have `closedAt`, but if GitHub returns null we fall back to "now" (`chrono::Utc::now()`) and stamp the engine's wall clock. The frontend event still fires. Inaccuracy is bounded by poller cadence (a few seconds to a minute).

### 11.9 What about projects (not chores)?

Project-level "PR closed unmerged" doesn't really exist — projects don't carry a single `pr_url`. The transitions in this doc apply to `kind='chore'` and `kind='project_task'`; a project's status moves based on its leaf tasks, not directly. The merge poller's existing pickup already handles both kinds; the close-unmerged transition lands on the same set.

---

## Sequence Diagram — Close-Unmerged Path

```
┌──────────┐  ┌────────────┐  ┌──────────┐  ┌─────────────┐  ┌───────────┐  ┌──────────┐
│ GitHub   │  │ merge_poll │  │  WorkDb  │  │  publisher  │  │ macOS UI  │  │ user /   │
│ (PR #252 │  │            │  │          │  │             │  │           │  │ coord.   │
│  closed) │  │            │  │          │  │             │  │           │  │          │
└────┬─────┘  └─────┬──────┘  └────┬─────┘  └──────┬──────┘  └─────┬─────┘  └─────┬────┘
     │              │ run_one_pass │               │               │              │
     │              │ list_chores_pending_merge_check               │              │
     │              │ ─────────────┼───────────────┼───────────────►│              │
     │              │              │  [chore C]    │               │              │
     │              │ ◄────────────│               │               │              │
     │ gh pr view   │              │               │               │              │
     │◄─────────────┤              │               │               │              │
     │ state=CLOSED │              │               │               │              │
     │ mergedAt=    │              │               │               │              │
     │   null       │              │               │               │              │
     │ closedAt=…   │              │               │               │              │
     │─────────────►│              │               │               │              │
     │              │  branch:     │               │               │              │
     │              │  ClosedUnmerged              │               │              │
     │              │ mark_close_unmerged          │               │              │
     │              │ ─────────────►               │               │              │
     │              │              │ UPDATE tasks  │               │              │
     │              │              │ SET status=needs_attention,   │              │
     │              │              │ pr_url=NULL,                  │              │
     │              │              │ prior_pr_urls += [#252],      │              │
     │              │              │ last_status_actor='engine'    │              │
     │              │              │ WHERE status='in_review'      │              │
     │              │              │   AND pr_url=#252             │              │
     │              │              │ → 1 row updated               │              │
     │              │              │ Task                          │              │
     │              │ ◄────────────│               │               │              │
     │              │ publish_work_item_changed(C, "pr_closed_unmerged")          │
     │              │ ─────────────┼──────────────►│               │              │
     │              │ publish ChorePrClosedUnmerged{chore_id, pr_url, closed_at}  │
     │              │ ─────────────┼──────────────►│               │              │
     │              │              │               │ TopicEvent    │              │
     │              │              │               │ ──────────────►              │
     │              │              │               │               │ refetch C    │
     │              │              │               │               │ render       │
     │              │              │               │               │ NeedsAttention│
     │              │              │               │               │ lane card    │
     │              │              │               │               │ with badge   │
     │              │              │               │               │ "tried 1×    │
     │              │              │               │               │ before · #252"│
     │              │              │               │               │              │
     │              │              │               │               │              │ user runs:
     │              │              │               │               │              │ boss chore reopen C
     │              │              │               │               │              │ ──────────────►
     │              │              │ UPDATE tasks  │               │               (engine receives,
     │              │              │ SET status=active,            │                writes 'active',
     │              │              │ last_status_actor='human'     │                publishes)
     │              │              │ ◄─────────────│               │              │
     │              │              │               │ work_item_changed → UI re-renders
     │              │              │               │               │              │
     │              │              │ (next dispatcher tick picks up active C,    │
     │              │              │  spawns worker with augmented prompt        │
     │              │              │  including prior PR URL)                    │
```

---

## Schema and Wire Summary

### Column adds

```sql
ALTER TABLE tasks ADD COLUMN prior_pr_urls    TEXT NOT NULL DEFAULT '[]';
ALTER TABLE tasks ADD COLUMN abandoned_reason TEXT;
```

No new tables. `last_status_actor` was added by `work-dependencies.md`'s migration; this doc relies on it.

### Status enum additions

`TaskStatus` (`tools/boss/cli/src/main.rs:569`) and `MoveTarget` (`tools/boss/cli/src/main.rs:578`) gain `NeedsAttention` and `Abandoned`. Engine validation in `WorkDb::update_task` accepts them on read/write.

Bump `metadata.schema_version`.

### Protocol additions (`tools/boss/protocol/src/types.rs`)

```rust
// Task gains:
pub prior_pr_urls: Vec<String>,
pub abandoned_reason: Option<String>,
```

### Protocol additions (`tools/boss/protocol/src/wire.rs`)

```rust
// FrontendRequest:
ListChoresNeedingAttention {
    product_id: Option<String>,
},

// FrontendEvent:
ChoresNeedingAttentionList {
    chores: Vec<Task>,
},
ChorePrClosedUnmerged {
    product_id: String,
    chore_id: String,
    pr_url: String,
    closed_at: String,
},
```

### Probe API change (`tools/boss/engine/src/merge_poller.rs`)

```rust
pub enum PrLifecycleState { Open, Merged, ClosedUnmerged }

pub struct PrLifecycleProbe {
    pub url: String,
    pub state: PrLifecycleState,
    pub closed_at: Option<String>,
}

#[async_trait]
pub trait PrLifecycleProbeTrait: Send + Sync {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe>;
}
```

The existing `MergeProbe` trait and `PrMergeState` struct are renamed (or kept as aliases for the migration window — I prefer a hard rename in one PR with the call-sites adjusted, since there are only three).

### Engine module additions (`tools/boss/engine/src/work.rs`)

```rust
impl WorkDb {
    pub fn mark_close_unmerged(
        &self,
        work_item_id: &str,
        pr_url: &str,
        closed_at: &str,
    ) -> Result<Option<Task>>;

    pub fn list_chores_needing_attention(
        &self,
        product_id: Option<&str>,
    ) -> Result<Vec<Task>>;
}
```

Both use the WHERE-guarded UPDATE / SELECT pattern from existing code; `mark_close_unmerged` is idempotent.

### Engine module additions (`tools/boss/engine/src/merge_poller.rs`)

```rust
async fn sweep_one(
    work_db: &WorkDb,
    probe: &dyn PrLifecycleProbeTrait,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) -> bool {
    let probe_result = match probe.probe(&candidate.pr_url).await { ... };
    match probe_result.state {
        PrLifecycleState::Open           => false,
        PrLifecycleState::Merged         => mark_merged_branch(...).await,
        PrLifecycleState::ClosedUnmerged => mark_close_unmerged_branch(...).await,
    }
}
```

### CLI verbs

```
boss chore reopen   <selector>
boss chore abandon  <selector> [--reason <text>]
boss chore move     <selector> --to needs_attention | abandoned    # via widened MoveTarget
```

### bossctl verbs

```
bossctl work needs-attention [--product <id>] [--json]
```

### App / UI deltas

- Models: decode `prior_pr_urls`, `abandoned_reason`, the two new statuses.
- New kanban lane: `Needs Attention`, ordered `updated_at DESC`.
- New kanban lane: `Abandoned`, hidden by default (toggle in settings, like `Done` aging).
- Card badge: `↺ tried N× before · #last` on `needs_attention` cards with non-empty `prior_pr_urls`.
- Card detail view: `Prior PRs` subsection listing each entry in `prior_pr_urls` with a click-through; `Abandon Reason` line when set.
- Drag handling: refusals on Review and Done targets from `needs_attention` (Q5).
- Subscribe to `ChorePrClosedUnmerged` for any future "what happened recently" surface; v1 ships without a dedicated activity feed.

---

## Risks

**R1 — `needs_attention` becomes a dumping ground.** Without the coordinator's discipline, chores pile up in the `needs_attention` lane and the user accumulates a backlog they can't tell apart. Mitigation: `bossctl work needs-attention` makes the list explicit; the macOS lane is visible; the badge surfaces the count of prior attempts. If pile-up turns out to be the dominant pattern in practice, a follow-up could add an aging rule (e.g. items in `needs_attention > 30 days` auto-promote to `abandoned`) — but auto-abandonment is dangerous and we should *not* default to it without the user's signal.

**R2 — User confusion between `needs_attention` and `blocked`.** Both are non-terminal-not-in-progress; the kanban shows both as cards parked outside the active flow. Mitigation: the badge ("`↺ tried N× before`") differentiates `needs_attention`; `blocked` carries the existing dependency-edge tooltip from `work-dependencies.md`. The `boss reference` updates explicitly contrast the two states.

**R3 — Worker re-spawns ignore the augmented prompt.** A worker session might skim past the "this is a re-attempt" paragraph and produce a near-identical PR to the rejected one. Mitigation: the prompt's wording (Q8) is direct ("stop, comment, ask for clarification rather than producing another PR with the same problem"); v1 trusts the worker, the second-line defence is the human reviewer of the new PR. If worker non-compliance turns out to be a pattern, a follow-up adds an explicit pre-flight "run `gh pr view <prior>` and summarise why it was closed before starting" injected as a tool call.

**R4 — Concurrent close-unmerged of two PRs for the same chore.** Cannot happen in v1: a chore has at most one live `pr_url`, and the close-unmerged path requires `pr_url = ?prev_pr_url` in the WHERE guard. If the chore's `pr_url` was somehow modified between the probe and the UPDATE, the UPDATE matches zero rows (idempotent) and the chore is left as the latest writer set it. Acceptable.

**R5 — `prior_pr_urls` grows unbounded.** A perpetually-rejected chore accumulates a long array. Mitigation: in practice we don't expect more than 2–3 entries; the column is unindexed and unbounded text in SQLite is cheap. If it becomes a problem, switch to a `chore_pr_history` table (the natural follow-up from Q3).

**R6 — Status-actor column doesn't capture the reopen intent.** Q4 says `reopen` writes `last_status_actor = 'human'`. If the user-then-edits the description before reopening, the description-edit also stamps `last_status_actor = 'human'`. So `last_status_actor` doesn't distinguish "user reopened" from "user edited then reopened" — but that's fine: both are explicit human actions. Same risk shape as in `work-dependencies.md`'s R1.

**R7 — Schema downgrade leaves `needs_attention` rows in the DB.** A user downgrading to a pre-v4 engine encounters chores with `status = 'needs_attention'` that the older code rejects. Mitigation: schema versions don't go backwards (the engine refuses to start against a `schema_version` higher than its compiled-in expectation). Same property as `work-dependencies.md`'s migration.

**R8 — CI / dispatcher tests assume only the old five statuses.** Adding two new strings to the `status` column may break tests that exhaustively `match` on `TaskStatus`. Mitigation: add a `#[non_exhaustive]` attribute on the enum (Rust) and an `@unknown default` arm on the Swift side at the same time as adding the new variants, so future additions are forward-compatible. Audit and adjust call sites in the same PR as the protocol change.

**R9 — `gh pr view` rate limits.** Adding `closedAt` to the existing `--json` query is no extra GitHub API call; same row, more fields. No rate-limit pressure beyond today's.

---

## Follow-up Implementation Chores (to enqueue once approved)

Bite-sized; each fits one worker session.

1. **Schema + migration**: add `prior_pr_urls` and `abandoned_reason` columns; bump `metadata.schema_version`. Acceptance: fresh init and migration from prior schema both yield the new columns; existing chores have `prior_pr_urls = '[]'` and `abandoned_reason = NULL`.

2. **Status enum additions**: extend `TaskStatus` (`cli/src/main.rs:569`) and `MoveTarget` (`cli/src/main.rs:578`) with `NeedsAttention` and `Abandoned`; widen `WorkDb::update_task` validation. Mirror in Swift `TaskStatus` enum (with `@unknown default`). Acceptance: `boss chore move <id> --to needs_attention` round-trips; the kanban renders the new strings.

3. **Probe API rename**: refactor `MergeProbe` / `PrMergeState` to `PrLifecycleProbe` / `PrLifecycleState { Open, Merged, ClosedUnmerged }`; extend `--json` query to include `closedAt`. Update the in-memory test stub. Acceptance: tests for all three lifecycle states; the existing merge test still passes.

4. **Engine: `mark_close_unmerged`**: idempotent WHERE-guarded UPDATE that moves `in_review → needs_attention`, clears `pr_url`, appends to `prior_pr_urls`, stamps `last_status_actor = 'engine'`. Unit tests cover idempotency, the JSON-array append, and the no-op-when-status-already-changed case. Acceptance: tests green; `merge_poller` not yet wired to call it.

5. **Engine: wire `merge_poller::sweep_one` to dispatch on `PrLifecycleState`**: route `Merged` to the existing path, `ClosedUnmerged` to the new path. Acceptance: integration test simulates a closed-unmerged probe and asserts the chore lands in `needs_attention` with the correct `prior_pr_urls` and a `ChorePrClosedUnmerged` event fired.

6. **Engine: startup sweep**: `app.rs::start_engine` invokes `merge_poller::run_one_pass` once before the regular loop spawns. Acceptance: integration test seeds a chore with a closed-unmerged PR, starts the engine, asserts the chore transitions to `needs_attention` within the startup sweep.

7. **Protocol: `FrontendEvent::ChorePrClosedUnmerged` and `FrontendRequest::ListChoresNeedingAttention`**: protocol types, serde round-trip, Swift Codable mirror. Acceptance: serde tests green; existing wire-shape tests still pass.

8. **Engine: `WorkDb::list_chores_needing_attention`**: wraps `list_chores` with the status filter. Acceptance: returns the expected rows; respects `product_id` filter.

9. **Engine: handler for `ListChoresNeedingAttention`**: routes the new request to `WorkDb::list_chores_needing_attention` and emits `ChoresNeedingAttentionList`. Acceptance: integration test issues the request and observes the response.

10. **CLI: `boss chore reopen` and `boss chore abandon`**: thin wrappers over `boss chore move` plus the abandoned-reason write. Reference doc updated. Acceptance: CLI integration test covers reopen (`needs_attention → active`), abandon (`needs_attention → abandoned`, with and without `--reason`), and reopen-from-abandoned.

11. **CLI: `MoveTarget::NeedsAttention` and `Abandoned` valid targets**: extend `boss chore move` parser; reject the targets for `boss task move` / `boss project move` (they don't apply). Acceptance: target round-trips; the reference's status-target table is updated.

12. **Bossctl: `bossctl work needs-attention`**: thin wrapper over `ListChoresNeedingAttention`. Reference doc updated. Acceptance: CLI integration test covers list-with-empty, list-with-rows, and `--product` / `--json` filters.

13. **Engine: spawn-prompt augmentation**: when `prior_pr_urls` is non-empty, inject the re-attempt paragraph into the worker spawn prompt (Q8). Defensively handle malformed JSON. Acceptance: integration test asserts the paragraph appears for a re-attempt and is omitted for a fresh attempt.

14. **macOS: `Needs Attention` lane**: kanban lane between `Doing` and `Review`, ordered `updated_at DESC`. Acceptance: snapshot tests; visual review on a fixture board with two chores in `needs_attention`.

15. **macOS: `Abandoned` lane (hidden by default)**: settings toggle, like the existing `Done` aging treatment. Acceptance: toggle persists; lane shows / hides without restart.

16. **macOS: card badge for `needs_attention`**: chain icon plus "`↺ tried N× before · #last`" label; click → opens last prior PR in the browser. Acceptance: snapshot tests with `prior_pr_urls.length` of 1 and 3.

17. **macOS: card detail Prior-PRs subsection**: list every entry in `prior_pr_urls` with click-through; show `abandoned_reason` line when set. Acceptance: snapshot tests; visual review.

18. **macOS: drag refusals**: `Needs Attention → Review / Done` refused with inline warning; `Needs Attention → Doing` allowed (treated as reopen). Acceptance: interaction tests for each direction.

19. **Documentation**: `boss reference` and `tools/boss/docs/designs/` indices updated with the new statuses, verbs, and event. Acceptance: docs build clean; the work-taxonomy / work-cli docs reference the new statuses.

---

## Out of Scope

- Per-chore aging rules (auto-abandon after N days in `needs_attention`).
- Chore-PR history as a separate table (vs. the JSON column proposed here).
- GitHub webhooks for push-based detection.
- Cross-product / cross-repo PR shape changes.
- Auto-edit of chore descriptions on close-unmerged.
- Detection of PR merge-conflicts as a distinct close reason.
- Deciding what `done` should mean for project-level work (this doc is chore-scoped; project status semantics already differ).
