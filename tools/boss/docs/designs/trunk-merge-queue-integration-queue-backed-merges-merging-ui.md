# Boss: Trunk merge queue integration — queue-backed merges + Merging UI

- **Date:** 2026-07-19
- **Project:** `proj_18c3dfc555074eb0_cbc` — Trunk merge queue integration: queue-backed merges + Merging UI
- **Execution:** `exec_18c3dfcb8b23d990_cbf` (project_design)
- **Related designs:** [`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) (P188 — conflict_watch/ci_watch), [`revision-tasks.md`](revision-tasks.md) (P654), [`unify-pr-remediation-on-revisions.md`](unify-pr-remediation-on-revisions.md), [`flunge-buildkite-pipeline-reference.md`](flunge-buildkite-pipeline-reference.md)
- **First adopter:** flunge (`brianduff/flunge`, Buildkite `flunge-ci`). Trunk Merge Queue is installed and validated there (PRs #978/#979 merged through the queue via `/trunk merge`, zero Buildkite changes). Queue-only enforcement is deliberately **not yet** enabled.

Boss's merge button currently executes one hardcoded verb: `gh pr merge --auto --squash`. Once flunge flips to queue-only enforcement (main pushes restricted to the Trunk GitHub app), that verb hard-fails there. This design makes the merge mechanism a per-product choice — `direct` (today's behavior, which also transparently covers mono's GitHub-native merge queue) vs `trunk_queue` (submit to Trunk's queue via its REST API and track the entry asynchronously to a terminal state) — and surfaces queue progress in the existing Merging UI lane, with queue eviction flowing into the existing CI-failure remediation machinery.

## Goals

- **Per-product merge mechanism.** A product-level setting selects how an approved merge is executed: `direct` (today's `gh pr merge --auto --squash`) or `trunk_queue` (Trunk REST API submit). Flunge is the first `trunk_queue` adopter; mono is unchanged.
- **Asynchronous queue-backed merge verb.** For `trunk_queue` products, the merge button submits the PR to Trunk's queue and the engine tracks the entry through `pending → testing → merged | failed | cancelled`. Merge-approval semantics upstream of the click are untouched — only the mechanics of executing an approved merge change.
- **Queue state in the Merging UI.** The existing Merging lane shows Trunk-queued PRs with their queue state and position, on freshness good enough to watch a merge land (≤ ~30 s staleness while anything is enqueued).
- **Eviction is a first-class failure signal.** A PR kicked from the queue (combined CI failed on its `trunk-merge/*` test branch) flows into the existing ci_watch → engine-triggered-revision remediation path, exactly like a red PR-branch CI run or a GitHub merge-queue rebounce, and is resubmitted once fixed.
- **Loud, honest failure semantics.** Missing/expired token, Trunk API unreachable, queue paused — every failure mode surfaces as an explicit error or attention item. The engine never silently falls back to `gh pr merge` for a `trunk_queue` product.
- **A safe enforcement flip.** A documented sequence and runbook for making flunge queue-only, including how the engine detects and reports the "enforcement is on but Boss is misconfigured" state.

## Non-goals

- **Changing merge-approval semantics.** Who may click merge, review gating (`review_required_state`), and the `InReview` lifecycle before the click are all unchanged. This design only changes what happens after the click.
- **Webhook ingestion in v1.** The engine runs on the operator's Mac with no public ingress; Trunk's Svix-delivered webhooks cannot reach it. Polling is the v1 mechanism; a webhook relay is sketched as future work only.
- **Migrating mono to Trunk.** Mono keeps GitHub's native merge queue via the existing `gh pr merge --auto` verb; no mono behavior changes in this project. "Non-goal" means out of this project's scope, not architecturally ruled out: flunge is deliberately a **trial** — if the queue proves better, mono can follow, and the design keeps that a config flip (`boss product set-merge-mechanism mono trunk_queue` plus the branch-protection change) rather than a code change. Nothing in the merge path, poller, UI, or remediation hardcodes flunge: `merge_mechanism` is a per-product column with no hardcoded product list, `TrunkQueueProbe` keys on `(repo, target_branch)`, repo coordinates derive from `repo_remote_url`, and eviction rides the shared remediation substrate. The prerequisites for a later mono switch are: multi-org token support (task 14) if mono lands in a separate Trunk org (until then the single-token blast radius in the risks section covers both repos), 429/`Retry-After` handling at mono's request volume (built into `trunk_client` from day one), and gating the GitHub-native `mergeQueueEntry`/rebounce observation on `merge_mechanism != trunk_queue` so a repo can never be observed in both queues at once (built in v1 — see the coordination rules).
- **Queue administration from Boss.** Creating/deleting/pausing queues, changing concurrency/batching modes, and `setImpactedTargets` optimization stay in the Trunk web app. Boss reads queue state; it only ever writes `submitPullRequest` and `cancelPullRequest`.
- **Driving Buildkite.** Trunk triggers CI on `trunk-merge/*` branches and Buildkite already builds all branches (validated in the flunge trial). Boss does not orchestrate CI for queue runs; it only reads failure evidence when an eviction happens.
- **ETA prediction in v1.** `getMergeQueueMetrics` returns Prometheus text suitable for dashboards; deriving a per-PR ETA from it is speculative and deferred (`future / not a v1 blocker` in the task list).
- **Priority UI in v1.** The API accepts a `priority` on submit and the client will plumb it, but the merge button submits at default priority. A priority picker is future work.

## Background: how merging works today (read-only map)

Anchors for the implementer; line numbers will drift.

- **The merge verb.** `tools/boss/engine/core/src/merge_when_ready.rs:51` — `gh_merge_when_ready(pr_url)` runs `gh pr merge --auto --squash <pr_url>` via `boss_engine_gh_invocation::gh_output`. It then re-probes (`gh pr view --json state`, `pr_in_merge_queue()`) and classifies into `MergeAction::{Enqueued, AutoMergeEnabled, Merged}`. Fire-once; it does not itself transition the task.
- **The RPC boundary.** App `ChatViewModel.mergeWhenReady(for:)` (`tools/boss/app-macos/Sources/ChatViewModel.swift:452`) → `FrontendRequest::MergeWhenReady { work_item_id }` (`tools/boss/protocol/src/wire.rs:1157`) → `review::handle_merge_when_ready` (`tools/boss/engine/core/src/app/review.rs:10`). Guards: item is Task/Chore, `status == InReview`, non-empty `pr_url`; failure replies `FrontendEvent::WorkError`, success replies `MergeWhenReadyAccepted { work_item_id, pr_url, action }`.
- **No "Merging" task status.** The task stays `InReview` the whole time. Queue/auto-merge sub-state lives in two `Task` columns (`tools/boss/protocol/src/types/task.rs:538-583`): `merge_queue_state: Option<String>` (`"queued"` | `"auto_merge_enabled"`; any `Some` moves the card into the app's Merging section) and `merge_queue_detail: Option<String>` (JSON `{position, state, enqueued_at, section_order}` with GitHub's raw `mergeQueueEntry.state`). The merge poller writes both (`merge_queue_state_str()`, `merge_poller.rs:3454`).
- **The Merging UI already exists** as a collapsible section inside the **Done column** (`mergingSection()` in `ChatViewModel+WorkBoardSections.swift`, section id `done-merging`): `WorkTask.boardColumn` (`Models.swift:348`) routes `in_review` cards with a live `merge_queue_state` into Done. Cards there render `MergeQueueBadge` (`ContentView.swift:~5210` — position `#n` + readiness glyph, backed by `MergeQueueDetail.parse`), which replaces the `PrCiIndicator` CI icon. The merge button is on Review cards only (`arrow.triangle.merge` + confirmation dialog), wired only when the card has a `pr_url` and `merge_queue_state == nil`; it self-hides once queued. The app receives all of this as pushed `work_item_changed` events over the engine's unix socket — the app never polls.
- **Terminal detection.** The merge poller (`merge_poller.rs`; 60 s global sweep spawned in `app/server.rs`, adaptive per-PR `PollTier::{Hot 15s, Cold 180s}`, plus `PrReconcilerKick`/targeted-kick notifies so RPC handlers can force a prompt reconcile) runs a batched GraphQL probe (`probe_batch_via_graphql`, `merge_poller.rs:783`). Observing the PR merged → `mark_merged()` (`merge_poller.rs:3818`) → `mark_chore_pr_merged` (task → `done`), scheduler kick for dependents, design-doc auto-population. It also computes Merging-section ordering (`renumber_merge_queue`, `merge_poller.rs:3747`).
- **GitHub-native merge queue (mono).** Handled implicitly: `gh pr merge --auto` enqueues when a queue exists, and the probe reads `mergeQueueEntry`/`autoMergeRequest`. `FAILED_CHECKS` dequeue events are polled via GraphQL and trigger `check_merge_queue_rebounce()` (`merge_poller.rs:2738`) → `ci_watch::on_merge_queue_rebounce_detected`.
- **CI-failure remediation.** `ci_watch::on_ci_failure_detected` (`tools/boss/engine/core/src/ci_watch.rs:227`) spawns an **engine-triggered revision task** (`kind=revision`, `created_via="ci-fix:<crm_id>"`) via the shared revision substrate. `ci_remediations` side-table rows are keyed `(work_item_id, head_sha_at_trigger, attempt_kind)` with `failure_kind ∈ {pr_branch_ci, merge_queue_rebounce}` and `before_commit_sha` for rebounce. Budget: `tasks.ci_attempts_used` vs task/product `ci_attempt_budget` (default 3); exhaustion → `blocked: ci_failure_exhausted` + attention item. Opt-out label `boss/no-auto-rebase`.
- **Per-product config.** `Product` struct (`tools/boss/protocol/src/types/product.rs:95`); DB columns are added as guarded `ALTER TABLE` migrations in `tools/boss/engine/core/src/work/migrations_b.rs`. Some product knobs are DB-only (`ci_attempt_budget`, `auto_pr_maintenance_enabled`); the template for a per-product setting with protocol surface is `default_driver`/`worker_branch_prefix`.
- **Secrets.** GitHub token lives in the macOS Keychain (`KEYCHAIN_SERVICE = "dev.spinyfin.boss.github"`, `tools/boss/github_tracker/src/github_oauth.rs:957`) via the `keyring` pattern, obtained by OAuth device flow, with fallback to ambient `gh` auth. The Anthropic key is env-only. There is no engine `config-secrets.toml`.

One repair this project should carry: `tools/boss/engine/core/tests/no_gh_pr_merge_test.sh:8` greps `engine/src` (the crate moved to `engine/core/src`), so the guard silently passes and the invariant it enforced ("the engine never merges") is both dead and no longer true. The stale claims in `boss-ci-buildkite-pipeline-mirroring-flunge.md` should be corrected at the same time.

## Trunk API surface (verified 2026-07-19 against docs.trunk.io / openapi.json)

- Base URL `https://api.trunk.io/v1`, auth header `x-api-token: <org token>`, JSON bodies, all endpoints POST except metrics.
- `POST /v1/submitPullRequest` — `{repo:{host,owner,name}, pr:{number}, targetBranch, priority?, noBatch?}` → `200 {}`. `priority` is int-or-string, optional.
- `POST /v1/getSubmittedPullRequest` — `{repo, pr, targetBranch}` → `{id, state, readiness, stateChangedAt, priorityValue, priorityName, prNumber, prTitle, prSha, prBaseBranch, prAuthor}`.
- `POST /v1/listPullRequests` — `{repo, targetBranch, state?, since?, cursor?, take? (1-100, default 50)}` → `{pullRequests[], nextCursor?}`. `since` filters concluded PRs by timestamp — the reconciliation backstop.
- `POST /v1/getQueue` — `{repo, targetBranch}` → `{state, branch, concurrency, testingTimeoutMinutes, mode, batch, enqueuedPullRequests[]}`. One call returns every enqueued PR — the cheap bulk probe.
- `POST /v1/cancelPullRequest`, `POST /v1/restartTestsOnPullRequest` — dequeue / re-test verbs.
- `GET /v1/getMergeQueueMetrics` — Prometheus text (dashboards; not machine-friendly per-PR ETA).
- **PR states:** `not_ready`, `pending`, `testing`, `tests_passed`, `merged`, `failed`, `cancelled`, `pending_failure`.
- **Queue states:** `RUNNING`, `PAUSED`, `DRAINING`, `SWITCHING_MODES`.
- No documented rate limits; bodies capped at 20 MiB. Webhooks exist (Svix-delivered, `pull_request.*` and `pull_request_batch.*` families) but require a public HTTPS endpoint.
- Non-API submit paths (`/trunk merge` comment, enqueue label, Chrome extension, `trunk merge` CLI) exist and were used in the flunge trial; humans may keep using them alongside Boss.

## Alternatives considered

### A. Keep the comment command: engine posts `/trunk merge` on the PR

The trial mechanism. Rejected as the engine's verb: the comment is fire-and-forget with no structured acknowledgement (a typo'd or unauthorized comment fails silently), attribution is "whoever's GitHub token posted a comment" rather than an org API token, there is no priority control, and the engine would still need the REST API anyway to read queue state. The REST `submitPullRequest` is attributable, synchronous-acknowledged, and priority-capable. (Humans commenting `/trunk merge` by hand remains supported — the poller treats externally-submitted entries as observed state, see coordination rules.)

### B. Model Trunk as a third arm of the existing GitHub merge-queue observation (no mechanism setting)

Teach the merge poller to always probe Trunk for every PR and treat "a Trunk queue exists for this repo" as the routing signal, avoiding a product-level setting. Rejected: it makes merge routing an emergent property of remote state rather than declared intent — a Trunk org token outage would silently degrade routing decisions, and pre-enforcement (when both `gh pr merge` and the queue work) the choice would be ambiguous. A declared per-product mechanism is inspectable, testable, and fails loudly when its prerequisites are missing.

### C. Synchronous merge verb: submit and block until terminal state

Keep the merge action synchronous by having the handler wait for the queue outcome. Rejected outright: queue runs take as long as a full CI cycle (tens of minutes on flunge), batching/bisection can multiply that, and the engine's RPC handlers must not hold work-item state hostage to a remote CI pipeline. The whole point of the queue is that "merge" becomes a tracked asynchronous process; Boss already has exactly the right shape for this (fire the verb, let the poller observe the outcome) in the GitHub-native path.

### D. A third mechanism value for mono's GitHub-native queue

Considered making the setting three-valued: `direct` | `github_queue` | `trunk_queue`. Rejected: `gh pr merge --auto --squash` is literally the same verb whether the repo has no queue (merges/arms auto-merge) or a GitHub-native queue (enqueues) — GitHub decides, and the poller already observes `mergeQueueEntry` either way. A `github_queue` value would add config surface with zero behavioral difference and create a misconfiguration class (mono set to `direct` vs `github_queue` behaving identically but reading differently). Mono stays under `direct` semantics; the docs for the setting say "direct = GitHub-native merge verb, including GitHub's own merge queue where enabled."

## Chosen approach

### Per-product merge mechanism

New nullable TEXT column `products.merge_mechanism`, values `'direct'` (NULL ⇒ `direct`) and `'trunk_queue'`, added via a guarded `ALTER TABLE` in `migrations_b.rs`, surfaced on the protocol `Product` struct (`#[serde(default, skip_serializing_if = "Option::is_none")]`, builder-pattern field per repo convention) and settable via `boss product set-merge-mechanism <product> (direct|trunk_queue)` plus the app's product settings. Parsed engine-side into:

```rust
pub enum MergeMechanism {
    Direct,               // gh pr merge --auto --squash (covers GH-native queue)
    TrunkQueue { target_branch: String /* default "main" */ },
}
```

`trunk_queue` needs the repo coordinates Trunk uses (`host`, `owner`, `name`); these derive from the product's existing `repo_remote_url` (flunge → `github.com` / `brianduff` / `flunge`) — no new columns. The target branch defaults to `main`; a `products.trunk_target_branch` override column is deliberately deferred until a product needs it.

There is no per-task override. Merge mechanism is a property of how a repo integrates with its trunk, not of individual work items.

### `trunk_client` crate

Per the crates-over-modules convention (precedent: the `claude_client` extraction, PR #1702), the REST client is a new crate `tools/boss/trunk_client`:

- Typed request/response structs for `submitPullRequest`, `getSubmittedPullRequest`, `listPullRequests`, `getQueue`, `cancelPullRequest`, `restartTestsOnPullRequest`; a `TrunkPrState` enum for the eight PR states and `TrunkQueueState` for the four queue states, both with `#[serde(other)]`-style unknown-variant tolerance so new Trunk states degrade gracefully.
- Takes the token as a constructor parameter (a `TrunkTokenProvider` trait or plain `SecretString`); the crate knows nothing about keychains or engine config. Edges stay one-directional: `engine → trunk_client`, never the reverse.
- Reuses the repo's existing HTTP stack (whatever `claude_client` uses) with timeouts, bounded retries with jittered backoff on 5xx/transport errors, and no retry on 4xx. 401/403 is surfaced as a distinct `TrunkAuthError` so callers can emit the "token missing/expired" attention.
- Errors are structured (`Auth`, `NotFound` — PR not in queue, `QueueUnavailable`, `Transport`) because the merge verb and poller branch on them.

### Auth: the Trunk org API token

- **Provisioned** by the operator from the Trunk web app (org-level API token) and handed to Boss via `boss engine trunk set-token` (reads from stdin/prompt, never argv) or the app's settings pane. A `boss engine trunk status` verb reports token presence + a live `getQueue` smoke check against the first `trunk_queue` product.
- **Stored** in the macOS Keychain, mirroring the GitHub OAuth pattern: service `dev.spinyfin.boss.trunk`, account `api-token@trunk.io`, accessibility `AfterFirstUnlockThisDeviceOnly`. Env override `BOSS_TRUNK_API_TOKEN` for development. **Never** in any repo, never in the DB, never logged.
- **Scoped:** one token per Trunk org; today one org covers flunge. Multi-org (per-product tokens) is future work; the keychain account naming leaves room (`api-token+<org>@trunk.io`).
- **Failure semantics (loud, no fallback):**
  - Merge click on a `trunk_queue` product with no token → the handler replies `WorkError("Trunk merge queue is configured for <product> but no Trunk API token is set — run `boss engine trunk set-token`")`. No task state change, no fallback to `gh pr merge`.
  - Token rejected (401/403) at submit or poll time → same loud path, plus a deduplicated attention item ("Trunk API token rejected — merges for flunge are stalled"), since post-enforcement a dead token means nothing can merge.
  - Trunk unreachable → submit fails loudly with retry advice; the poller backs off exponentially (cap ~5 min) and emits an attention item if unreachable for > 15 min while entries are being tracked.

### The merge verb: submit + standing merge intent

`handle_merge_when_ready` keeps all existing guards, then branches on the product's mechanism. `Direct` is byte-for-byte today's path. `TrunkQueue`:

1. **Record a standing merge intent** — a new side-table row (`trunk_merge_intents`: `work_item_id`, `pr_url`, `pr_number`, `repo`, `target_branch`, `created_at`, `status ∈ {active, merged, cancelled, exhausted}`, `last_trunk_state`, `last_trunk_state_at`, `submit_count`). The intent is the poller's tracking anchor and the memory that the operator approved this merge — it is what authorizes automatic resubmission after an eviction is fixed (see below). UNIQUE on (`work_item_id`) while `active`; a second merge click on an already-active intent is a no-op that re-reports current queue state.
2. **Submit**: `POST submitPullRequest` with the PR number and target branch, default priority. On 200, optimistically set `merge_queue_state = "queued"` and `merge_queue_detail = {source:"trunk", state:"pending"}` so the card moves to Merging immediately, and reply `MergeWhenReadyAccepted` with a new `action` value `trunk_enqueued` (extends the existing `MergeAction` wire vocabulary; the app already switches on `action`).
3. On failure, no intent row survives (or it is marked failed), the card stays in Review, and the error is loud per the auth section.

The task itself stays `InReview` throughout, exactly like the GitHub-native queue path — Merging is a presentation of `merge_queue_state`, not a `TaskStatus`. This keeps every downstream consumer (dependency cascade, design-doc population, revision gating on "chain root PR is Open") working unmodified.

**Entry state machine** (engine-side view of one intent):

```
                    submit
  [active] ──────────────────────▶ pending / not_ready ──▶ testing ──▶ tests_passed
                                        │                    │              │
                                        │                    ▼              ▼
                                        │                  failed ◀── pending_failure
                                        │                    │
                                        ▼                    ▼
                                    cancelled          eviction → ci_watch rebounce
                                        │                    │ (fix lands, CI green)
                                        ▼                    ▼
                                 intent cancelled      auto-resubmit (budget-capped)

  terminal: trunk state = merged  → observed by existing merge poller (PR merged on GitHub) → task done
            intent cancelled      → card back to Review + attention item
            budget exhausted      → blocked: ci_failure_exhausted (existing state) + attention item
```

Trunk's `merged` state is deliberately **not** the engine's merge-completion trigger: the existing GitHub-side probe already detects the PR's merged state and runs the whole `mark_merged()` cascade. Trunk polling only feeds the intermediate states and the failure signal. This means a human who enqueues via `/trunk merge` without touching Boss still gets correct terminal behavior today — Trunk state tracking is additive.

### Queue state ingestion: polling

**Where:** inside the existing merge poller sweep (`merge_poller.rs`), as a `TrunkQueueProbe` sibling of `CommandMergeProbe`, not a new free-running loop — it inherits the sweep's 60 s cadence, the per-PR `PollTier` schedule, `rate_limit`-style throttling, error accounting, and the publisher plumbing that pushes `work_item_changed` events to the app. The merge-when-ready handler's existing `PrReconcilerKick` gives an immediate first probe right after submit, so the card's queue position appears within seconds of the click. The `trunk_client` crate does the transport.

**What:** per `(repo, target_branch)` with ≥ 1 active intent, one `getQueue` call per cycle — it returns state + every enqueued PR in one response, covering position and per-PR state for all tracked entries at once. For any active intent whose PR is absent from `enqueuedPullRequests` (it left the queue), one `getSubmittedPullRequest` call resolves its terminal state (`merged` / `failed` / `cancelled`). `listPullRequests since=<last sweep>` runs as a low-frequency (every ~10 min) reconciliation backstop for transitions the point probes missed.

**Cadence and freshness:** riding the per-PR tier schedule — Hot (15 s) while any intent is `testing`, 30 s while entries are merely `pending` (position churn is slower than test progress), no Trunk polling at all when no intent is active. That is 2–5 requests/min against an API with no documented rate limit — comfortably conservative — and gives the Merging UI ≤ 30 s staleness, matching the existing Hot-tier GitHub probe. All failures back off exponentially (base 30 s, cap 5 min) without affecting the GitHub probe.

**What it writes:** the same two columns the UI already consumes — `merge_queue_state = "queued"` (any live Trunk state) and `merge_queue_detail = {source: "trunk", state: <trunk state>, position: <index in enqueuedPullRequests>, enqueued_at, queue_state: <RUNNING|PAUSED|...>}` — plus `trunk_merge_intents.last_trunk_state`. Queue-level anomalies (`PAUSED`/`DRAINING` while intents are active) emit a deduplicated attention item ("Trunk queue for flunge is PAUSED — 2 merges waiting") rather than per-card noise.

**Webhooks (future, explicitly not v1):** if freshness ever needs to be sub-poll-interval, a small relay (Cloudflare Worker/Tailscale Funnel endpoint receiving Svix deliveries and exposing a pull queue the engine drains) can feed the same reconciliation entry points. The poller remains the source of truth even then — webhooks would only be a cache-invalidation hint, so nothing about this design's state model changes.

### Eviction: a first-class failure signal

Trunk state `failed` (reached directly or via `pending_failure`) means the PR was kicked from the queue because combined CI failed on its `trunk-merge/*` test branch. This is the moral twin of the existing GitHub merge-queue `FAILED_CHECKS` rebounce, and it reuses that machinery end-to-end:

1. The poller detects `failed` on an active intent and calls a new `ci_watch::on_trunk_queue_eviction_detected`, a sibling of `on_merge_queue_rebounce_detected`.
2. A `ci_remediations` row is inserted with a new `failure_kind = 'trunk_queue_eviction'`. Failure evidence: the `trunk-merge/*` branch's head commit (from Buildkite via the existing `bk`-based `ci_log_reader` path — flunge CI is Buildkite, and the trial confirmed statuses/builds exist for `trunk-merge/*` branches) feeds the log excerpt; the Trunk entry id and `stateChangedAt` are recorded for provenance. Like the GH rebounce, the failing evidence lives on a synthetic/ephemeral commit, so it inherits the same rule from the unification design: a green head-branch probe cannot retroactively validate the attempt.
3. The parent flips `in_review → blocked: ci_failure` and the standard engine-triggered **revision** is spawned (`created_via = "ci-fix:<crm_id>"`), consuming one slot of the existing shared `ci_attempt_budget` (default 3, product/task override already exists). No new budget knob.
4. The intent stays `active` with `last_trunk_state = failed`. When the revision lands and the PR's own head-branch CI is green again (the existing clears-on-green observation), the engine **auto-resubmits**: `submitPullRequest` again, `submit_count += 1`, card returns to Merging. The operator's original merge click is the authorization; re-clicking merge after every eviction fix would make the async merge verb pointless. Auto-resubmit is bounded by the same budget — when it exhausts, the intent is marked `exhausted`, the parent goes `blocked: ci_failure_exhausted`, and the existing attention + `boss engine ci retry` reset path applies.
5. `cancelled` (a human ran `/trunk cancel`, the queue was drained, or Boss's own `cancelPullRequest` ran) retires the intent **without** remediation: card back to Review, attention item "PR was removed from the Trunk queue", no revision spawned. Cancellation is a human decision, not a failure.

Flake handling composes for free: `classify_pre_triage()` already distinguishes infra-flavored failures → `retrigger` (budget-free). For a `trunk_queue_eviction`, the retrigger action is resubmission itself (or `restartTestsOnPullRequest` if the entry is somehow still live) rather than a Buildkite rebuild — the queue re-runs combined CI on resubmit by construction.

### Coordination with conflict_watch / ci_watch (don't misread queue states)

Rules preventing the queue's own churn from being misread as PR pathology:

- **While an intent is live** (`pending`/`testing`/`tests_passed`), the PR's head branch is untouched — Trunk tests a `trunk-merge/*` construction branch. The existing probes keep watching the head branch and can legitimately fire (e.g. main moved and the PR went `CONFLICTING`). A conflict detected mid-queue is real — Trunk will also fail it — so conflict_watch proceeds normally, but the poller first calls `cancelPullRequest` and marks the intent's queue exit as `superseded_by_conflict` (no eviction remediation — the conflict resolver owns the slot, matching the existing conflict-pre-empts-CI precedence). After the conflict revision lands green, the same auto-resubmit rule applies.
- **Never double-fire.** A single queue failure must produce exactly one remediation. The eviction path keys its `ci_remediations` row on the Trunk entry id + `stateChangedAt` (idempotency across sweeps), and while a `trunk_queue_eviction` remediation is open, `on_ci_failure_detected` for the same head SHA is suppressed — the head-branch CI redness that follows an eviction fix push is the _remediation's own_ in-flight signal, already covered by `ci_inflight_observations`.
- **`merge_mechanism` gates observation, not just submission.** For `trunk_queue` products the GitHub-side probe stops writing `merge_queue_state`/`merge_queue_detail` and skips `check_merge_queue_rebounce()`; the Trunk probe owns those columns and the eviction signal. Everything else about the GitHub probe — CI/review state, and the merged-terminal detection that drives `mark_merged()` — is unchanged, which is what keeps Trunk's `merged` state handling GitHub-side. This rule is required for v1 correctness, not just mono-future-proofing: the GitHub probe recomputes the queue columns from `mergeQueueEntry`/`autoMergeRequest` on every sweep (`merge_poller.rs:3559`), both absent for a Trunk-queued PR, so an ungated sweep would clear the Trunk-written state within one cycle. It also structurally removes the eviction-vs-GH-rebounce double-fire class before any repo can ever be in both queues (see risks).
- **`not_ready` is not a failure.** It means GitHub-side readiness (approvals, non-queue required checks) is missing. The card shows "queued — waiting on readiness"; if `not_ready` persists > 30 min an attention item nudges the operator (usually a review requirement drifted after the merge click).
- **The existing auto-merge/red-CI override** (`merge_queue_state` forced to `None` when `ci_required_state == "fail"` under `auto_merge_enabled`, task.rs:549) applies only to the GitHub `auto_merge_enabled` state and is untouched; Trunk entries are governed by the intent state machine above.

### Merging UI

The app already has a Merging lane: the collapsible `done-merging` section rendered above the recency buckets in the Done column, populated by `WorkTask.boardColumn`'s `in_review where isInMergingSection → .done` routing and sorted by the engine-computed `section_order`. Trunk entries reuse the whole chain — `merge_queue_state` → `boardColumn` → `mergingSection()` → `MergeQueueBadge`:

- **Placement:** any card with `merge_queue_state == "queued"` lands in the Merging section with **no app-side changes** — validated against the app code: `isInMergingSection` (`Models.swift:393`) requires only `status == "in_review"` and a non-nil `mergeQueueState`, `boardColumn` routes such cards to Done/Merging, and `mergingSection()` orders by `MergeQueueDetail.parse(...)?.sectionOrder ?? .max`, whose parser ignores the unknown keys in the extended Trunk detail JSON. Two engine-side qualifications keep this honest — and explain why the flunge trial's queue-merged PRs (correctly) never appeared in Merging: **(a)** nothing writes the column for a Trunk-queued PR until tasks 4–5 land — the GitHub probe derives it exclusively from `mergeQueueEntry`/`autoMergeRequest`, both absent under Trunk — so "day one" means day one of the engine writing the column, not today; **(b)** the GitHub probe unconditionally recomputes and rewrites `merge_queue_state`/`merge_queue_detail` from its own raw probe on every sweep (`update_task_pr_poll_state`, `merge_poller.rs:3559`), which evaluates to `None` for a Trunk-queued PR — without the write-ownership gate in the coordination rules, the next sweep would clear the Trunk-written state and bounce the card out of Merging within one poll cycle. With those two pieces in place, day-one rendering is degraded-but-sane: `MergeQueueBadge` maps unknown states to the clock glyph, so a Trunk card shows position + clock until task 8 decodes the extended detail JSON into real Trunk chrome.
- **Card chrome:** `MergeQueueDetail.parse` (`ContentView.swift:~5154`) learns the new fields (`source`, `queue_state`, Trunk state strings). For `source == "trunk"`, the `MergeQueueBadge` capsule maps Trunk states — `pending` → "#3" + clock glyph, `testing` → "Testing" + clock, `tests_passed` → "Merging…" + green check, `not_ready` → "Waiting on readiness", `failed`/`pending_failure` never render (the card has already left Merging by then). Unknown states render the raw string rather than hiding the card.
- **Queue banner:** when the detail carries `queue_state != RUNNING`, the Merging section header shows "Trunk queue paused/draining" so a stalled queue is visible without opening Trunk.
- **Failure surfacing** follows the existing bifurcation: mechanical CI-shaped failures (eviction) go through the block-reason + on-card badge channel — the card leaves Merging, snaps back to Review with the existing `blocked: ci_failure` treatment (`CiFailureBadge` with `used/budget`, revision card in Doing) — while needs-a-human conditions (token dead, queue paused, cancelled entry, `not_ready` stall) go to attention items in `AttentionsView`. No new surfaces are invented.
- **The merge button** is unchanged in the app: same Review-card gate (`pr_url` present, `merge_queue_state == nil`), same confirmation dialog, same `sendMergeWhenReady` RPC; the engine routes by product. The `trunk_enqueued` action value in `MergeWhenReadyAccepted` lets the confirmation feedback say "Submitted to Trunk merge queue" instead of the generic message.
- **Not in scope:** promoting Merging from a Done-column section to a top-level `WorkBoardColumnKey` column. The plumbing makes that a small, self-contained later change (add `.merging`, retarget the `boardColumn` special case), listed as future work.
- The UI stays a thin client throughout: it renders state the engine reconciles; no Trunk API calls from the app.

### Misconfiguration detection and enforcement flag-day

**The flip sequence for flunge** (operator-executed, after the engine path has merged N ≥ 1 successful queue-backed merges):

1. Confirm `boss engine trunk status` is green and flunge's `merge_mechanism = trunk_queue` has been live through at least one full merge and one eviction-remediation cycle.
2. In GitHub branch protection for `main` on `brianduff/flunge` (per Trunk's enforcement docs): restrict pushes to `main` to the Trunk GitHub app; **disable** "require branches to be up to date before merging" (the queue owns freshness — leaving it on double-serializes); exclude `trunk-temp/**` and `trunk-merge/**` from branch-protection rules so Trunk can manage its working branches.
3. Verify: a manual `gh pr merge` on a throwaway PR now fails with a push-restriction error; a Boss merge-button merge succeeds through the queue.
4. **Emergency-bypass runbook** (documented in the ops doc this task list produces): repo admin temporarily lifts the push restriction (or adds themselves to the allowlist) in branch-protection settings, merges the emergency change, restores the restriction, and — if Boss tracked that PR — expects the normal merged-observation path to reconcile it (it does: terminal detection is GitHub-side). Pausing the Trunk queue first (`PAUSED` via the web app) avoids racing the queue during the bypass.
5. Rollback is symmetric: remove the push restriction and set `merge_mechanism` back to `direct`; nothing in the engine assumes enforcement.

**Misconfiguration detection** (enforcement on, Boss wrong): if a product is still `direct` and enforcement is enabled, `gh pr merge` fails — the handler already surfaces `gh` stderr as `WorkError`, and a new pattern match on push-restriction/rule-violation errors upgrades it to an attention item that names the fix ("flunge appears to be queue-enforced; set merge_mechanism=trunk_queue"). Conversely `trunk_queue` without a working token is covered by the auth section. Both states fail loudly on first use; neither can silently merge around the queue.

## Risks / open questions

- **Auto-resubmit authorization.** This design treats the original merge click as standing authorization to resubmit after each eviction fix (budget-capped). If the operator instead wants a fresh human click per resubmission, the intent machinery still works — the resubmit step just becomes an attention item + Review-lane affordance. Flagged in the attentions manifest.
- **Trunk entry idempotency fields.** The eviction dedup key assumes `getSubmittedPullRequest.id` + `stateChangedAt` are stable per queue episode; if `id` turns out to be per-PR rather than per-submission, the key falls back to `(pr, prSha, stateChangedAt)`. Verify during implementation with a deliberate eviction on a scratch flunge PR.
- **`tests_passed` → `merged` gap.** Between Trunk merging the construction branch and GitHub reporting the PR merged, the card says "merging" while GitHub still says open. The existing Hot-tier probe closes this within ~15 s; no action unless observed otherwise.
- **Buildkite log access for `trunk-merge/*` failures.** The trial confirmed builds run and statuses publish; the assumption that `ci_log_reader`'s Buildkite path can fetch logs by branch/commit for the excerpt needs a one-time verification (it queries by commit SHA today, which the Trunk PR object supplies via `prSha`/queue branch head).
- **Rate limits are undocumented.** The cadence chosen (≤ 5 req/min active, 0 idle) is conservative, but if Trunk introduces limits, the poller's existing backoff absorbs 429s; the client should treat 429 like a transport error with `Retry-After` respect from day one.
- **Single-token blast radius.** One org token in one keychain entry gates all queue merges. Mitigated by the `trunk status` smoke check and the loud auth-failure attention; accepted for a single-operator deployment.
- **Double observation if a product ever moves between queues.** The never-double-fire rule covers eviction vs head-SHA CI; the eviction-vs-GitHub-rebounce pair is impossible today only because no product sits in both queues. A later mono switch to Trunk (see non-goals — flunge is a trial) would expose exactly that pair: one queue failure producing both a GH-rebounce remediation and a Trunk-eviction remediation. v1 removes the class structurally rather than by runbook — `merge_mechanism = trunk_queue` disables the GH-native `mergeQueueEntry`/rebounce observation for the product (coordination rules), so at most one queue observer is ever live per repo. The remaining mono-switch prerequisites (multi-org tokens, 429 handling at mono volume) are listed in non-goals.
- **Humans and Boss sharing the queue.** `/trunk merge` comments and the enqueue label keep working. Boss only tracks entries it has intents for; an externally-enqueued Boss-tracked PR merges via the normal GitHub-side observation. The only mixed-mode oddity: an external `/trunk cancel` on a Boss-submitted entry reads as `cancelled` → intent retired + attention, which is the desired behavior.

## Proposed implementation task breakdown

Entries are PR-sized, one worker-session each. Depth annotations: tasks at the same depth with no shared files may run in parallel.

1. **`trunk_client` crate** — New crate `tools/boss/trunk_client`: typed models for the six queue endpoints, `TrunkPrState`/`TrunkQueueState` enums with unknown-variant tolerance, token-as-parameter auth, structured errors (`Auth`/`NotFound`/`QueueUnavailable`/`Transport`), bounded retry/backoff, unit tests against canned JSON fixtures. No engine dependency; minimal visibility. Effort: **medium**. Dependencies: none.
2. **Product `merge_mechanism` setting** — `products.merge_mechanism` migration in `migrations_b.rs`, protocol `Product`/`CreateProductInput` field (builder pattern), engine `MergeMechanism` parse, `boss product set-merge-mechanism` CLI verb, read plumbing in `app/products.rs`. No behavior change yet. Effort: **small**. Dependencies: none (parallel with 1).
3. **Trunk token provisioning + status** — Keychain store (`dev.spinyfin.boss.trunk`, mirroring `KeychainTokenStore`), `BOSS_TRUNK_API_TOKEN` env override, `boss engine trunk set-token` / `trunk status` CLI verbs (status does a live `getQueue` smoke check). Effort: **small**. Dependencies: 1 (client types for the smoke check). Parallel with 2.
4. **Merge verb routing + merge intents** — `trunk_merge_intents` table + DAO; `handle_merge_when_ready`/`gh_merge_when_ready` branch on mechanism; `submitPullRequest` on click; optimistic `merge_queue_state` write; `trunk_enqueued` wire action; loud no-token/auth-failure `WorkError` paths; duplicate-click no-op. Engine + protocol only (app renders the new action in task 8). Effort: **medium**. Dependencies: 1, 2, 3.
5. **Trunk queue poller** — `TrunkQueueProbe` in the merge-poller sweep: per-repo `getQueue`, per-missing-entry `getSubmittedPullRequest`, cadence tiers (15 s testing / 30 s pending / idle off), exponential backoff + unreachable-attention, writes `merge_queue_state`/`merge_queue_detail {source:"trunk", …}` and intent state, low-frequency `listPullRequests since=` reconciliation, `PAUSED`/`DRAINING` attention items, terminal `cancelled` → intent retirement + Review snap-back. Includes the write-ownership gate: for `trunk_queue` products the GitHub probe's `merge_queue_state`/`merge_queue_detail` writes and `check_merge_queue_rebounce()` are skipped (coordination rules) — without it the GH sweep clears the Trunk-written state within one cycle. Metrics counters via the existing counter framework. Effort: **large**. Dependencies: 4.
6. **Eviction → ci_watch integration** — `on_trunk_queue_eviction_detected`: `failure_kind='trunk_queue_eviction'` in `ci_remediations`, Trunk-entry-keyed idempotency, Buildkite log excerpt for the queue-branch failure, parent flip to `blocked: ci_failure`, revision spawn via the existing substrate, shared budget accounting, suppression rules vs `on_ci_failure_detected` for the same head SHA. Effort: **medium**. Dependencies: 5.
7. **Auto-resubmit + conflict coordination** — Resubmit-on-green for intents whose eviction/conflict revision landed (`submit_count`, budget-capped, `exhausted` handling); conflict-during-queue rule (`cancelPullRequest` + `superseded_by_conflict`, no eviction remediation, resubmit after conflict revision). Effort: **medium**. Dependencies: 6. (Co-edits `ci_watch.rs`/poller files with 6 — sequenced after it, forward-porting its changes.)
8. **Merging UI: Trunk queue chrome** — App-side: extend `MergeQueueDetail.parse` and `MergeQueueBadge` (`ContentView.swift`) for `source`/`queue_state`/Trunk states, queue-paused banner in `mergingSection()` (`ChatViewModel+WorkBoardSections.swift`), `trunk_enqueued` action handling in `EngineClient`/`ChatViewModel+EventHandling`; extend `MergingSectionKanbanTests`/`MergeQueueDetailTests`. Swift only; engine columns already flow. Effort: **medium**. Dependencies: 5 (detail schema settled). Parallel with 6/7 (disjoint files: app-macos vs engine).
9. **Misconfiguration detection + enforcement runbook** — Push-restriction error pattern → attention ("set merge_mechanism=trunk_queue"); ops doc with the flag-day sequence, verification steps, and emergency-bypass runbook; fix the dead `no_gh_pr_merge_test.sh` path and the stale "engine never merges" doc claims. Effort: **small**. Dependencies: 4 (error path exists); runbook content finalizes after 7.
10. **Flunge enforcement flip + end-to-end validation** — Operator-assisted: run one merge and one deliberate eviction through the full path pre-enforcement, then execute the flip sequence (push restriction, up-to-date toggle off, `trunk-temp/**`/`trunk-merge/**` exclusions), verify direct-merge hard-fail and queue-merge success post-flip. Effort: **small**. Dependencies: 5, 6, 7, 8, 9.
11. **Priority support in the merge flow** — `future / not a v1 blocker`: plumb submit priority from a UI affordance; API/client support ships in 1. Dependencies: 8.
12. **Per-PR ETA from queue metrics** — `future / not a v1 blocker`: derive expected-merge estimates from `getMergeQueueMetrics`/position history. Dependencies: 5.
13. **Webhook relay ingestion** — `future / not a v1 blocker`: hosted Svix receiver + engine drain as a freshness hint over the poller. Dependencies: 5.
14. **Multi-org token support** — `future / not a v1 blocker`: per-product Trunk org tokens if a second Trunk org ever appears. Dependencies: 3.
15. **Promote Merging to a top-level board column** — `future / not a v1 blocker`: add `.merging` to `WorkBoardColumnKey`, retarget the `boardColumn` `in_review where isInMergingSection` case, retire/restyle `mergingSection()`. Dependencies: 8.

Parallelism summary: 1 ∥ 2 at depth 0; 3 after 1 (∥ 2); 4 joins them; 5 is the spine; 6 → 7 sequenced (shared files), both ∥ 8; 9 can start any time after 4; 10 is the terminal validation gate.
