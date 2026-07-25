# Boss: GitHub webhook ingress — push-based GitHub events for Boss engines

- **Status:** design proposal (not implemented). This is a `kind=design` deliverable — architecture, quota analysis, and migration plan. No code.
- **Project:** `proj_18c56b15d3ae6e38_102` — "GitHub webhook ingress: push-based GitHub events for Boss engines".
- **Provenance:** design execution `exec_18c56cb4c4596e10_1bb`, design task T3557.
- **Code citations:** all against `spinyfin/mono` @ `2c618cb36bb75f197ce1f882f1f52fa8a5bff731` unless stated. The task brief's pre-gathered inventory cites `e7fb0b11f271efc75771ff4154ef53f21dfafb68`; every claim reused from it was re-verified at the newer sha and line numbers below are the newer ones.
- **Related design docs:** [`engine-event-bus-…`](./engine-event-bus-event-driven-reconcilers-via-an-in-process-message-queue.md) (the landing zone this design produces onto; scoped webhooks out and named them a follow-up — this is that follow-up); [`engine-counter-metrics-framework.md`](./engine-counter-metrics-framework.md) (the counter surface used for shadow mode); [`trunk-merge-queue-integration-…`](./trunk-merge-queue-integration-queue-backed-merges-merging-ui.md) (the credential-storage precedent this reuses verbatim); [`maintenance-tasks.md`](./maintenance-tasks.md) (catalogues the sweeps reclassified here); [`auto-populate-project-tasks-on-design-pr-merge.md`](./auto-populate-project-tasks-on-design-pr-merge.md) (consumer of this doc's task-breakdown section).
- **Related work items:** T3537 (instrumentation — hard prerequisite, `active`, PR #2349), T3538 (merge-poller drain — overlapping, `blocked` on T3537), T3540 (`boss pr status`), T3196 (GitHub App installation token, `todo`), P856 (PR/CI/conflict reconciliation domain), P3246 (engine event bus, doc merged).
- **Supersedes:** `tools/boss/docs/investigations/github-event-detection-webhooks-vs-polling-2026-07-08.md` — see [Superseding the missing investigation](#superseding-the-missing-investigation).

## TL;DR

Boss burns **3,900–6,900 GraphQL points/hour against a 5,000/hour budget** (measured, T3537 `[measured-baseline]`), and roughly two thirds of that is the merge poller's un-batched per-PR adaptive timer asking GitHub whether anything changed on PRs where nothing did. This design replaces the _trigger_ for that timer — not the fetch it performs — with GitHub webhook deliveries relayed to the engine over an outbound-only channel, so the engine fetches when something actually happened instead of on a clock.

A small Rust relay hosted on **Cloudflare Workers + Durable Objects** receives GitHub deliveries, verifies their HMAC, normalizes them into a tiny envelope, and buffers them per repository. Engines **long-poll** the relay (the shape the Buildkite agent actually uses), so there is no inbound port, no tunnel, no NAT work, and — importantly — **no HTTP server added to the engine**, which today accepts no network connection anywhere. Each delivery becomes a `bus.publish(Event::PrReconcileRequested { pr_url })`, which the merge poller already subscribes to and handles (`app/server.rs:940`, `merge_poller/schedule.rs:463-522`).

**The win is real but it is not "no more polling."** Webhook payloads are used for _triage only_ — never written to the DB — so the GraphQL probe still runs. The saving comes from firing it ~15 times over a PR's whole life instead of ~90 times an hour. With the reconciliation sweep left completely untouched at 60s, spend drops to **~1,600 points/hour (59–77% reduction)**. Relaxing the sweep to 600s afterwards, gated on shadow-mode evidence, takes it to **~200 points/hour (95–97%)**. The sweep is never removed; it is the correctness floor and the automatic fallback when the relay is unreachable.

**One verified defect blocks all of this and must be fixed first:** the merge poller's `PrReconcileRequested` arm _drops_ an event that arrives within 15s of the last full sweep rather than deferring it (`merge_poller/schedule.rs:466-475`). At a 60s sweep cadence that silently discards a quarter of all push notifications.

## Goals

- **Cut GitHub GraphQL spend by at least 50% without lengthening worst-case transition-detection latency**, measured with T3537's per-subsystem counters, against the measured 3,900–6,900 pts/hr baseline.
- **Cut median transition-detection latency** for the PR lifecycle signals Boss cares about (CI red/green, review submitted, merged, merge-queue dequeue) from ~20s (half a 40s Hot tier) to **under 5s**.
- **Introduce no inbound network surface on the operator's machine.** No listening TCP port, no tunnel, no DNS, no NAT traversal, no firewall change. The engine dials out and only ever dials out.
- **Introduce no new failure mode that can strand a PR.** Every push-driven path degrades to today's polling behaviour, automatically, without operator action.
- **Give the engine a real identity for the first time**, in a way that is enrollable, rotatable, revocable, and stored the way every other Boss secret already is.
- **Keep the relay dumb.** It holds no GitHub credential, makes no GitHub API calls, and knows nothing about tasks, projects, or work items. Its blast radius on compromise is "can forge or withhold hints", which the sweep bounds.

## Non-goals

- **Eliminating the reconciliation sweep.** Explicitly forbidden by the brief and correct on the merits. The sweep is the correctness floor (`merge_poller/schedule.rs:203-208`) and the thing that makes a best-effort push channel safe. It may be _relaxed_, gated on measured evidence; it is never deleted.
- **Trusting webhook payloads as authoritative state.** See [The payload-trust decision](#the-payload-trust-decision). Nothing GitHub pushes is written to the `tasks` row.
- **Batching or trimming the poller's own queries.** That is T3538's scope. See [Relationship to T3537 and T3538](#relationship-to-t3537-and-t3538).
- **Fixing the shared-personal-token problem.** The webhook secret is a different credential axis from the polling token. This design neither fixes nor entrenches it; T3196 owns that.
- **Assuming T3196's GitHub App exists.** It is `todo` and opt-in. v1 ships against a repo-level webhook and treats the App as a later, separately-decided migration.
- **Event-driving the external issue tracker (Projects V2 / Issues).** Deferred; its 120s cadence (`app/server.rs:1369`) is not implicated in the measured drain.
- **Buildkite webhook ingestion.** For mono, CI _truth_ is Buildkite, not GitHub — but GitHub's `status` events already carry the verdict Boss consumes, and Buildkite's own webhooks would be a second ingress with a second secret. Out of scope; noted as deferred.
- **Cross-engine coordination, leader election, or a shared engine state store.** Engines stay independent. The relay fans out; it does not arbitrate.
- **Replacing the frontend or events Unix sockets.** Both stay exactly as they are.

## Background: what exists today

### The landing zone is already built and has no producer

`Event::PrReconcileRequested { pr_url }` is a live event-bus topic (`engine/event-bus/src/event.rs:31`). The merge poller subscribes to it (`engine/core/src/app/server.rs:940`) and handles it in its `select!` loop (`engine/core/src/merge_poller/schedule.rs:463-522`), calling `reconcile_one` on just that PR. Outside tests there is **no publisher** — `grep -rn "PrReconcileRequested" tools/boss --include='*.rs'` returns 20 hits, of which the only `publish` calls are in `merge_poller/tests/schedule_tests.rs:222` and `event-bus/src/tests.rs:72`.

The engine-side integration of this whole project is therefore, in the limit, one `publish` call. Everything hard is on the other side of it. This is not an engine refactor project.

### The verified defect: the quiesce window drops, it does not defer

`merge_poller/schedule.rs:463-522`:

```rust
event = pr_reconcile_requests.recv(), if !pr_requests_closed => {
    match event {
        Some(Event::PrReconcileRequested { pr_url }) => {
            let since_last = last_run_at.elapsed();
            if since_last < quiesce_window {
                tracing::debug!(/* "…within quiesce window, absorbing" */);
            } else {
                let (outcome, tier) = reconcile_one(/* … */).await;
```

`quiesce_window` is `Duration::from_secs(15)` (`schedule.rs:257`) and `last_run_at` tracks the last **full sweep**, not the last reconcile of this PR. So a `PrReconcileRequested` arriving in the 15 seconds after any full sweep is logged and thrown away — the PR is not reconciled, not deferred, and not rescheduled. With the sweep at 60s (`app/server.rs:950`) that is **25% of wall-clock time**. This is harmless today because there are no producers. It is a correctness bug the moment there are.

The adjacent broad-kick arm (`schedule.rs:445-461`) has the same shape and the same issue, but its consequence is milder — it only delays a sweep that is about to happen anyway.

### The measured cost, and a model that reproduces it

`PR_PROBE_FIELDS` (`merge_poller/probe.rs:370-379`) costs 81 GraphQL nodes per PR: `labels(first: 30)` = 30, `reviews(last: 20)` = 20, `commits(last: 1)` = 1, its nested `contexts(first: 30)` = 30. The dequeue-events probe (`merge_poller/merge_queue.rs:61-62`) costs 20 nodes per PR. GitHub bills `ceil(nodes / 100)` with a floor of 1 point per query.

At N ≈ 25 tracked open PRs:

| Path                                                                                                                 | Arithmetic            | Cost                                   |
| -------------------------------------------------------------------------------------------------------------------- | --------------------- | -------------------------------------- |
| Batched sweep — lifecycle probe                                                                                      | `ceil(81 × 25 / 100)` | 21 pts                                 |
| Batched sweep — dequeue events                                                                                       | `ceil(20 × 25 / 100)` | 5 pts                                  |
| Batched sweep — `{rateLimit{remaining}}` (`metrics.rs:247-248`)                                                      | free                  | 0 pts                                  |
| **Sweep total, once per 60s**                                                                                        | 26 pts × 60/hr        | **1,560 pts/hr (26 pts/min)**          |
| Adaptive per-PR path — `probe_batch(&vec![one])` (`sweep.rs:393`) + `run_merge_queue_rebounce_pass` (`sweep.rs:418`) | 2 pts × reconciles    | **2,400–5,400 pts/hr (40–90 pts/min)** |
| **Total**                                                                                                            |                       | **3,960–6,960 pts/hr**                 |

That model lands squarely on the measured 3,900–6,900 pts/hr (five clean windows: 64.7 / 68.1 / 95.7 / 71.4 / 114.6 pts/min), which is the basis for trusting the after-numbers below. The adaptive residual is consistent with a Hot 40s / Cold 180s mix over 25 PRs (`schedule.rs:36-37`).

**The adaptive path is ~60–78% of total spend, and almost all of it is asking about PRs where nothing changed.** That is precisely what a push notification fixes.

### Inbound network posture: entirely outbound

Nothing in Boss accepts a network connection. Searching `tools/boss` for `TcpListener`, `SocketAddr`, `axum`, `hyper`, `warp`, `actix`, `tonic`, and `tokio-tungstenite` across both `*.rs` and every `Cargo.toml` returns **zero hits**. `reqwest` (client-only) is the sole HTTP dependency, present in six crates. The only `bind()` calls are `UnixListener::bind` — the frontend socket and the events socket (`engine/core/src/events_socket.rs:123`, chmod `0600` at `:125-129`).

The events socket is the existing external-event ingress precedent: worker `boss-event` shims write to it, peer identity comes from `LOCAL_PEERPID`/`SO_PEERCRED` (`events_socket.rs:24-29`), and remote hosts reach it via `ssh -O forward -R` reverse Unix-socket forwards (`engine/ssh-transport/src/lib.rs:357-370`). That is already a dial-out-and-hold-a-channel design; it is worth studying and worth rejecting for this purpose ([see alternatives](#alternative-c-ssh--r-reverse-forward-from-a-jump-host)).

The frontend socket has a deliberately weaker posture — `engine_control.rs:1-10`: _"The frontend socket sits on a well-known path that any process on the same user/machine can dial."_ **No webhook-derived data lands on the frontend socket.** The relay client is an in-process component of the engine that publishes directly onto the in-process event bus; there is no new IPC hop and therefore nothing new to secure at the IPC layer.

### Auth today, and what is missing

Three credential paths exist; the polling path uses the weakest. `gh_output` (`github/src/gh_runner.rs:99-108`) sets no environment variable at all — `gh` resolves its own ambient, per-machine, per-OS-user personal OAuth token, which is the 5,000/hr budget everything drains. The OAuth device flow stores a token in the macOS keychain under `dev.spinyfin.boss.github` (`github_tracker/src/github_oauth.rs:957`) but serves only the external tracker. GitHub App JWT machinery exists and works (`github/src/lib.rs:131-154`, RS256, 9-minute TTL) with credentials compile-time-embedded via Bazel `rustc_env` (`github/src/lib.rs:56-58`) — but **no engine code path calls `embedded_config()`**; it serves `boss shake` only.

**There is no engine identity primitive.** `engine_control.rs` mints a random 32-byte token in a 0600 file for the local shutdown RPC, but it does not survive a restart and is not addressable by any backend. Engine→relay authentication is genuinely greenfield.

The storage pattern to copy is not greenfield, though. `trunk_auth/src/lib.rs:1-24` is exactly the right shape: `boss_keychain::KeychainStore` (macOS Data Protection Keychain in release builds, 0600 file fallback in ad-hoc dev builds), an env override for local testing, a `TokenSource` enum so status output can say where the credential came from, and a `boss engine trunk` CLI surface. This design reuses it verbatim.

### What mono's CI actually emits — and why it matters

`.buildkite/REQUIRED_CHECKS.md` is authoritative: mono's branch protection requires exactly **three** checks — `buildkite/mono/bazel-build-test`, `buildkite/mono/mac-app-build`, `buildkite/mono/checks` — each emitted by one explicit `notify: github_commit_status` block in `.buildkite/pipeline.yml`. They are **GitHub commit statuses**, not check runs.

This is load-bearing in two directions. It means mono's per-push webhook volume is small (~6 `status` deliveries per push: three contexts × pending + final), so mono alone would tolerate a naive design. It also means Boss's other tracked repos, if any use GitHub Actions with a job matrix, emit `check_run.created` + `check_run.completed` per job — dozens per push. The design must handle both event families and must not be sized against mono's friendly numbers.

## Scope: what this replaces, and what it provably cannot

Every row of the engine's periodic GitHub surface, classified. "Replaced" means the fetch goes away. "Cadence-reduced" means the fetch still happens but is triggered by an event instead of a clock. "Untouched" means this design does nothing for it.

| Polling site                                                                                                                   | Verdict                              | Why                                                                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Batched PR lifecycle probe — full sweep (`probe.rs:370-379,430-438`; 60s at `app/server.rs:950`)                               | **cadence-reduced (v2)**             | Stays as the backstop. Relaxed 60s → 600s only after shadow mode proves parity. Never removed.                                                                      |
| Batched PR lifecycle probe — adaptive Hot 40s / Cold 180s (`schedule.rs:36-37,419-443`)                                        | **replaced**                         | The timer is what goes away, for PRs whose repo has a healthy relay channel. The probe it invoked still runs, on event trigger. This is the entire quota win.       |
| Merge-queue dequeue events (`merge_queue.rs:61-62`)                                                                            | **cadence-reduced**                  | `merge_group` and `pull_request.dequeued` events trigger it. The `REMOVED_FROM_MERGE_QUEUE_EVENT` timeline query is GraphQL-only and still runs.                    |
| Free budget read `{rateLimit{remaining}}` (`metrics.rs:247-248`)                                                               | **untouched**                        | 0 points. Keep exactly as is; it is how throttling stays honest.                                                                                                    |
| Legacy combined status, ETag-conditional (`probe.rs:790-828`)                                                                  | **cadence-reduced**                  | Rides the same reconcile. Its `304`s are already free (`probe.rs:253-260`); fewer reconciles simply means fewer conditional requests.                               |
| Failing check runs on a merge queue's synthetic commit (`merge_queue.rs:212` → `github/src/check_runs.rs`)                     | **cadence-reduced**                  | Fires per dequeue, and dequeue is now event-driven.                                                                                                                 |
| PR state for stalled reviewers >10 min (`sweep.rs:420-437` → `work/pr_state.rs`)                                               | **untouched**                        | The trigger is a Boss-side elapsed-time predicate — "a reviewer has been silent for 10 minutes". GitHub emits no event for _nothing happening_. Stays on the sweep. |
| Branch-keyed PR detection, suffix-scan fallback, diff stats (`completion.rs`)                                                  | **partly replaced — unexpected win** | See [The `pr_url` capture win](#the-pr_url-capture-win). Diff stats still need a fetch.                                                                             |
| Branch/PR existence, default branch, PR create (`abandoned_branch_pr_sweep.rs:118` = 5 min)                                    | **untouched**                        | Asks "does a pushed branch exist with _no_ PR" — a negative. No webhook announces absence. A `push`-event-fed branch registry could seed it; deferred.              |
| PR state for review recovery (`pr_review_recovery.rs`, 60s)                                                                    | **cadence-reduced**                  | `pull_request_review.submitted` / `.dismissed` are exact triggers.                                                                                                  |
| GitHub Projects V2 + Issues import (`github_tracker/src/github.rs`, 120s at `app/server.rs:1369`)                              | **untouched in v1**                  | Needs `issues` / `issue_comment` / `projects_v2_item` subscriptions and a different consumer. Not implicated in the measured drain. Deferred to v2.                 |
| Changed-file sets for stacking pairs (`stacked_pr_structuring.rs`, 30 min, flag default OFF at `feature-flags/src/lib.rs:222`) | **untouched**                        | Webhook PR payloads carry no file list.                                                                                                                             |
| CI log tail `gh run view --log-failed` (`ci-log-reader/src/lib.rs`)                                                            | **untouched**                        | GitHub Actions only; mono's log tail is Buildkite via `bk` (`ci_watch.rs`).                                                                                         |

### What no webhook stream can supply

These need a fetch or a local computation no matter what ships. A design that claims to eliminate polling is wrong; the honest frame is **cadence and latency, not elimination**.

- **`mergeStateStatus` is GraphQL-only, and `mergeable` is computed lazily.** GitHub returns `null` for `mergeable` while it recomputes asynchronously, and webhook `pull_request` payloads carry REST's `mergeable_state` (frequently `"unknown"`), which is not the same enum. This one field drives `pr_mergeable_state` via `mergeable_state_str` (`sweep.rs:1382-1395`), **all** conflict detection, and the merge-queue lane. **Conflict detection genuinely cannot be event-driven** — and critically, when `main` moves and a PR becomes conflicted, GitHub emits _no event for that PR at all_. The `push`-to-default-branch trigger below is the closest available proxy and it is a trigger for a fetch, not a substitute for one.
- **`conflict_resolutions.conflict_diagnosis`** — produced by running `git merge-tree` locally in a leased cube workspace (`conflict-diagnosis/src/lib.rs`).
- **`ci_remediations.log_excerpt`** — last 100 lines of a Buildkite job log via `bk` (`ci_watch.rs`). A GitHub `status` event carries a `target_url`, not a log.
- **`trunk_merge_intents.last_trunk_state`** — Trunk.io's API (`trunk_client/src/client.rs`), a different vendor, interleaved with engine sentinels.
- **PR changed-file lists** and **compare/diff results** and **file contents at a ref** — none are in any webhook payload.
- **Point-in-time execution baselines** — `pr_head_before`, `pr_body_before`, `revision_stop_contributed_head`. GitHub has no notion of a Boss execution.

### `ci_watch` and `conflict_watch` get the win for free

Both make zero direct GitHub calls; both are pure consumers of the poller's probe result. All GitHub-derived CI and conflict latency collapses to the merge-poller cadence, so event-driving the poller event-drives them **with no code change in either module**. There is no separate CI poller to webhook-ify.

### The `pr_url` capture win

`pr_url` is not GitHub-sourced today. It comes from a regex scrape of the worker's `gh pr create` stdout in the PostToolUse hook, staged in an in-memory `HashMap<execution_id, pr_url>` that is **lost on engine restart** (`pr_url_capture.rs:1-33`, which documents this explicitly and keeps the `jj`-plus-GitHub reconstruction path as the cold-path fallback).

A `pull_request.opened` delivery carries `html_url` and `head.ref` authoritatively, from GitHub, durably. Wiring it in means an engine restart between a worker's push and its Stop hook no longer needs the fragile reconstruction path. **This is a reliability win the project did not set out to get, and it should be claimed** — but as a _second_ consumer of the same channel, sequenced after the merge-poller path proves out, not folded into v1.

The corollary constrains routing: the `pr_url` → work-item mapping exists **only in each engine's DB**. The relay cannot know which engine owns a PR. It must route by **repository**, and each engine resolves locally.

## Quota: prove the win

### The naive design, and where it actually goes wrong

"Publish `PrReconcileRequested` on every delivery" costs 2 GraphQL points per delivery (`sweep.rs:393` + `sweep.rs:418`, both flooring to 1 point). Whether that is better or worse than today depends entirely on the repo's check topology:

| Repo shape                              | Deliveries per PR lifetime                                           | Naive cost  | Today's adaptive cost over the same window      | Verdict           |
| --------------------------------------- | -------------------------------------------------------------------- | ----------- | ----------------------------------------------- | ----------------- |
| **mono** — 3 commit statuses, ~3 pushes | ~24 (`status` ×18, `pull_request` ×3, review ×2, close ×1)           | 48 pts      | ~135 pts over a 45-min active window at Hot 40s | marginally better |
| **A GHA repo, 40-job matrix**           | ~240 (`check_run` created+completed per job × 3 pushes, plus suites) | **480 pts** | ~135 pts                                        | **~3.5× worse**   |

So the brief's warning is correct and the mechanism is specific: **burst amplification on check-heavy repos**. Three near-simultaneous `status` deliveries when a build finishes produce three targeted reconciles where one suffices — 6 points instead of 2 — and on a matrix workflow that multiplies by the job count. Compounding it, the existing quiesce window would _drop_ most of the burst rather than coalesce it (`schedule.rs:466-475`), so the naive design is simultaneously wasteful and lossy.

**Debounce is therefore mandatory, not an optimization.** The design below applies a per-PR minimum reconcile interval of 40s — deliberately identical to today's Hot tier — which makes the worst case _provably no worse than today_ for any repo, whatever GitHub's event volume, while the common case collapses to near zero.

### Before and after

Assumptions, stated so they can be checked: N ≈ 25 tracked open PRs (T3538); Boss opens and merges ~20 PRs/day on a busy day; ~20 merges/day to `main`; per-PR debounce W = 40s. Steady state, per hour.

| Path                                                        | Before                     | After — v1 (sweep untouched)                            | After — v2 (sweep at 600s) |
| ----------------------------------------------------------- | -------------------------- | ------------------------------------------------------- | -------------------------- |
| Full reconciliation sweep                                   | 26 pts × 60/hr = **1,560** | 26 pts × 60/hr = **1,560**                              | 26 pts × 6/hr = **156**    |
| Adaptive per-PR timer                                       | **2,400–5,400**            | **0** — replaced by event trigger                       | **0**                      |
| Event-driven targeted reconciles                            | —                          | ~15 debounced bursts × 2 pts × 20 PRs/day ÷ 24 = **25** | **25**                     |
| `push` to `main` → batched re-probe of that repo's open PRs | —                          | 26 pts × 20/day ÷ 24 = **22**                           | **22**                     |
| Free `{rateLimit}` read                                     | 0                          | 0                                                       | 0                          |
| **Total pts/hr**                                            | **3,960–6,960**            | **~1,607**                                              | **~203**                   |
| **vs 5,000/hr budget**                                      | **79–139%**                | **32%**                                                 | **4%**                     |
| **Reduction**                                               | —                          | **59–77%**                                              | **95–97%**                 |

Two things to read off this table.

First, **v1 clears the goal without touching the sweep at all.** That matters because relaxing the correctness backstop is the one irreversible-feeling decision here, and it should be gated on shadow-mode evidence rather than bundled into the cutover.

Second, **after v1 the backstop sweep is 97% of remaining spend.** The sweep cadence becomes the single dial that controls Boss's GitHub bill, which is a much better place to be than today's "it depends on how many PRs happen to be Hot."

Both after-columns leave the 1,500-point reserve (`metrics.rs:161`) untouched with room to spare, which is what protects the `boss-release` job and ad-hoc `gh` from the starvation documented at `metrics.rs:145-160`.

### The payload-trust decision

**Decision: trust webhook payloads for triage only. Never write payload data to the `tasks` row.**

The relay forwards a normalized envelope carrying `delivery_id`, `repo`, `event`, `action`, `pr_number`, `pr_url`, `head_sha`, `base_sha`, and `received_at`. The engine uses those fields to answer two questions — _should I reconcile this PR at all?_ and _how urgently?_ — and nothing else. The only path that writes GitHub-derived state remains `WorkDb::update_task_pr_poll_state` (`work/pr_flow.rs`) fed by `probe_batch`, exactly as today.

This is the reading that survives the event bus's contract (`event-bus/src/event.rs:1-5`: _"Events are hints, not commands — a subscriber re-reads authoritative state from the DB before acting"_) rather than fighting it. It is also the only reading that preserves the four engine divergences by construction:

1. **`merge_queue_state` demotion on red CI** (`sweep.rs:1455-1474`, mono#2023 — _"Demote the lane signal only — never GitHub's own arming"_). Computed from `ci_state` and `raw_merge_queue_state` inside `sweep_one`. A payload-trusting write path would bypass it and bounce cards out of the Merging lane.
2. **`preserve_merge_queue_state` for trunk products** (`sweep.rs:1486`), whose `"queued"` value GitHub _never reported_ — it was written optimistically by `handle_trunk_queue_merge`. A payload write would wipe it within one delivery.
3. **The per-org review overlay** (`merge_poller/classify.rs:108-124`, `apply_review_signal`), which forces `Required` / `ChangesRequested` over GitHub's own `reviewDecision`. `pull_request_review` payloads carry a single review's state and no aggregate at all, so a payload write would both lose the overlay and misreport the base.
4. **`section_order`**, a UI sort key baked into `merge_queue_detail` (`merge_queue.rs:264-308`). Not a GitHub concept in any form.

Because payload data never reaches the write path, **all four are protected without a single line of defensive code.** That is the argument for this choice: the alternative — trusting payloads and re-deriving the divergences on a second write path — buys perhaps 20 points/hour and puts four hard-won behaviours in permanent double jeopardy.

The corollary is that this design does **not** save the GraphQL call. It retimes it, and the retiming is worth 59–97%.

### The coalescing windows — there are two, and they are different

The brief flags the existing 15s quiesce as "the natural coalescing primitive". It is the wrong primitive as written (it drops rather than defers, and it keys off the last _sweep_ rather than the last _reconcile of this PR_), but the instinct is right. There are two distinct windows and both are needed:

- **Per-PR debounce, W = 40s, owned by this design.** Input: webhook deliveries. Effect: at most one reconcile per PR per 40s. This is what bounds burst amplification and makes the worst case equal to today's Hot tier.
- **Cross-PR batch window, ±5s, owned by T3538.** Input: the due set. Effect: PRs becoming due within 5s of each other go through `probe_batch` in **one** GraphQL query, `ceil(81N/100) + ceil(20N/100)` instead of `2N`.

They compose: the debounce decides _when_ a PR becomes due, the batch window decides _how many go in one query_. Neither subsumes the other. If both ship, the "event-driven targeted reconciles" row above drops further, from 25 pts/hr toward 12.

## Alternatives considered

### Alternative A: an engine-side HTTP listener plus a tunnel

Run an HTTP server in the engine and expose it with Cloudflare Tunnel, ngrok, or a static public address. GitHub delivers straight to the laptop. No relay service, no fan-out layer, no new wire protocol.

**Rejected.** It requires a tunnel daemon per machine, an inbound path into a laptop that today accepts zero network connections, and a public hostname per engine registered with GitHub — so adding a second engine means a second webhook configuration and a second tunnel. It also drags an entire HTTP server framework into a codebase whose only HTTP dependency is client-side `reqwest`. Worst, it puts the HMAC-verification boundary on the operator's machine: a misconfigured tunnel exposes the engine itself, not a stateless relay. The operator's framing explicitly rules this shape out and the framing is right.

### Alternative B: poll a queue instead of relaying

Have GitHub deliver into a hosted queue (SQS, Cloudflare Queues, Azure Service Bus) and have engines poll the queue.

**Rejected, but narrowly** — this is genuinely close to the chosen design, and the chosen design is best understood as this one with a purpose-built consumer API. A raw managed queue gives fan-out to multiple consumers awkwardly (most are competing-consumer by default, which is exactly wrong when two engines both want every event for a repo), gives no per-repo routing without either one queue per repo or client-side filtering of everything, and gives no place to put HMAC verification, delivery-id dedup, or engine authentication. Adding all of that means writing a service anyway — at which point the service should own the buffer rather than proxy someone else's.

### Alternative C: `ssh -R` reverse forward from a jump host

The precedent exists in-tree and works: `add_reverse_unix_forward` (`ssh-transport/src/lib.rs:357-370`) has remote `sshd` create a Unix socket that tunnels back to the engine's `events.sock`, riding a persistent `ControlMaster`. Dial-out, filesystem-scoped, no TCP port. Point the same mechanism at a small public jump host running the webhook receiver.

**Rejected on the record.** It needs a persistent public host to be the SSH endpoint — the operational cost this design is trying to avoid — plus an SSH key per engine as the identity primitive (not rotatable without host access, not revocable from a control plane), plus a webhook receiver on that host anyway. The connection is also brittle in exactly the wrong conditions: `ControlMaster` recovery across laptop sleep and network changes is the thing this design most needs to be boring. It is worth recording because its _shape_ — dial out, hold a channel, no inbound port — is the shape being adopted; only the mechanism differs.

### Alternative D: WebSocket over Durable Objects

The relay holds a WebSocket per engine using Cloudflare's Durable Object WebSocket Hibernation API; deliveries push down the socket with sub-second latency.

**Rejected for v1, retained as the upgrade path.** It is the "correct" answer for high volume and low latency, and if measured latency disappoints it is where this goes. But it adds a WebSocket client dependency to an engine that has none, adds reconnect/ping/pong/backpressure state machines on both ends, and buys latency the workload does not need: a long-poll with a request already in flight delivers with the _same_ latency as a WebSocket, and the workload here is tens of events per hour, not thousands per second. Notably, **the Buildkite agent — the operator's stated model — long-polls over HTTP.** Long-poll _is_ the agent shape; it is not a lesser approximation of it.

### Alternative E: Azure Container Apps running a plain `axum` service

A conventional Rust binary in a container: `axum` for HTTP, native WebSocket/SSE support, a real long-lived process, built by bazel `rules_oci`, pushed to ACR, deployed with `az containerapp update`.

**Rejected, and this is the closest call in the document.** It is the _only_ option where "a simple Rust HTTP service" means exactly what it sounds like, with no wasm, no `wasm-bindgen`, and no toolchain novelty. It loses on cost and ops: holding client connections is incompatible with scale-to-zero, so it needs `min-replicas: 1`, which is roughly $10–20/month of always-on compute for a service handling tens of requests per hour — against $0 on Cloudflare's free tier. It also adds a container registry, an image build, and a second cloud account's credentials in mono's CI. See [Hosting](#hosting-cloudflare-vs-azure) for the full trade, and the [attentions manifest](#risks--open-questions) — this is flagged for an operator decision, because the argument turns on how much the Rust-on-wasm friction is worth.

## Chosen approach

### Architecture

```
GitHub ──HTTPS POST──▶  Relay (Cloudflare Worker)
  webhook               │  · HMAC-SHA256 verify (X-Hub-Signature-256)
                        │  · delivery-id dedup (X-GitHub-Delivery)
                        │  · normalize → DeliveryEnvelope
                        ▼
                  Durable Object, one per repo
                        │  · bounded ring buffer (512 envelopes / 1 h)
                        │  · per-engine cursor
                        ▲
                        │  HTTPS long-poll GET /v1/subscribe?cursor=…
                        │  Authorization: Bearer <engine_secret>
                        │  (outbound only — engine dials, relay never dials back)
                        │
                  Boss engine ── relay_client
                        │
                        ▼
                  per-PR debounce (W = 40 s)
                        │
                        ▼
                  bus.publish(Event::PrReconcileRequested { pr_url })
                        │
                        ▼
                  merge poller ── reconcile_one ── gh api graphql
                        (the existing, unchanged write path)
```

Nothing to the right of `bus.publish` changes. The merge poller's subscription, `reconcile_one`, `sweep_one`, and `update_task_pr_poll_state` are all as they are today.

### Repo placement

Operator-decided and not re-litigated: the service lives in `spinyfin/mono` under `tools/boss/`, sibling to `engine/`. Three consequences are inherited and worth naming rather than discovering later.

- **A deployed network service now lives in the monorepo.** `checkleft`, `file/size`, and the bazel lint/format checks apply to it like any other crate, which is good. But mono's `CHECKS.yaml` and CI were written for tools and an app, not for something with a production deployment; the deploy step is new surface in `.buildkite/pipeline.yml`.
- **Deploy cadence couples to mono.** There is no independent release train for the relay. A relay hotfix rides a mono PR through the same three required checks. Given the relay's failure mode is "engines fall back to polling", this is acceptable — but it means the relay must never be on a path where minutes matter.
- **Hosted-service secrets live in mono's CI.** The webhook secret and the Cloudflare API token become Buildkite secrets. That widens what a compromised CI agent reaches. Mitigation: the deploy step runs only on `main`, the API token is scoped to a single Worker, and neither secret grants any GitHub access — see [Blast radius](#blast-radius).

Crates: `tools/boss/relay_protocol/` (wire types, no I/O — the reason placement in mono was chosen), `tools/boss/relay/` (the service), `tools/boss/relay_client/` (engine-side client), `tools/boss/relay_auth/` (engine-side credential storage, mirroring `trunk_auth`). Each with minimal bazel visibility (`//tools/boss:__subpackages__`), following `tools/boss/http_retry/BUILD.bazel`.

### The wire format

`relay_protocol` owns one envelope, versioned, deliberately small:

```rust
pub struct DeliveryEnvelope {
    pub protocol_version: u16,     // relay and engine both reject a mismatch loudly
    pub seq: u64,                  // per-repo monotonic; the cursor
    pub delivery_id: String,       // X-GitHub-Delivery, for dedup and tracing
    pub repo: String,              // "owner/name" — the routing key
    pub event: String,             // "pull_request" | "status" | "check_run" | …
    pub action: Option<String>,    // "opened" | "synchronize" | …
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,    // canonical https://github.com/o/r/pull/N
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub received_at: i64,          // relay-side epoch seconds
}
```

**The relay normalizes; it does not forward raw payloads.** Raw forwarding would put GitHub's payload schema in the engine's parsing surface and move kilobytes per delivery for a handful of used fields. Normalizing costs one relay deploy when GitHub changes a payload shape, and is the concrete reason the shared-protocol-crate placement rationale pays off.

Deliberately **not** in the envelope: labels, PR state, mergeable flags, review decisions, check conclusions. Per [the payload-trust decision](#the-payload-trust-decision) the engine would not be allowed to write them, and carrying them would invite exactly the write path that regresses the four divergences. If a future signal genuinely needs one, adding a field is a protocol bump and a design decision, not an accident.

### Transport: HTTP long-poll

```
GET /v1/subscribe?repos=spinyfin/mono&cursor=<seq>&timeout=25
Authorization: Bearer <engine_secret>
```

- **200** with `{envelopes: [...], cursor: <new seq>}` when deliveries are pending or arrive within the window.
- **204** on timeout with no deliveries — this doubles as the liveness heartbeat, so there is no separate ping.
- **409** `cursor_expired` when the requested cursor has fallen out of the ring buffer. The engine responds by resuming from head **and** firing one immediate broad kick so the full sweep re-establishes ground truth. This is the designed degradation path, not an error case.
- **401** on a bad or revoked credential. The engine stops dialling, marks the channel unhealthy, and files an attention.

The engine loop reuses `boss_http_retry`'s `RetryPolicy` and shared `reqwest::Client` (`http_retry/src/lib.rs:1-16`), which is precisely the crate boundary it documents for outbound clients. **Zero new engine dependencies. Zero new engine listeners.**

Latency: with a request in flight, delivery is a chunked write on an open connection — the same latency as a WebSocket. Between responses it is one RTT to re-dial, which given a 25s window and ~20 deliveries/hour is a rare coincidence. Median detection latency lands well under the 5s goal; the p99 is bounded by one RTT.

### Routing and fan-out

- **Route by repository.** `repository.full_name` from the delivery is the only routing key. The relay has no view of tasks, projects, or work items, because that mapping exists only in each engine's DB (`pr_url_capture.rs`, `work/proposal_apply.rs`).
- **Each engine declares its repo set** on every `/v1/subscribe` request, derived from its `products` table. No server-side subscription state to drift.
- **Two engines claiming the same repo both receive every delivery.** No leader election, no partitioning. This is correct and it is cheap: an engine with no work item for a PR pays _nothing_, because `reconcile_one` early-returns before it probes — `sweep.rs:384-392`, _"reconcile_one found no live candidate for this PR; skipping until next full sweep"_. Filtering is free and already implemented.
- **Offline engine:** the per-repo Durable Object retains a ring buffer of the last 512 envelopes or 1 hour, whichever is smaller. Reconnect inside the window replays from the cursor; outside it, `409 cursor_expired` and the degradation above.
- **Delivery contract: at-least-once inside the buffer window, best-effort outside it.** Duplicates are free — `reconcile_one` is idempotent by construction, since it re-reads the DB and re-probes GitHub, and the debounce collapses repeats anyway.

**Dropping is acceptable, and here is the argument.** A dropped delivery costs latency, never correctness, because the full sweep re-discovers any PR the targeted path missed — that is what `schedule.rs:203-208` is for and what the event-bus design leans on for the same reason (_"a bug in an in-memory best-effort bus can at worst fall back to the sweep cadence we have today"_). The alternative — durable per-engine delivery with acks — buys a bounded latency improvement in a rare failure mode and costs persistent server-side state, ack protocol, redelivery semantics, and poison-message handling. That trade is only worth taking if the sweep is removed, and the sweep is not being removed.

### Auth, end to end

Three credential axes, kept strictly separate.

**GitHub → relay.** Standard webhook HMAC. GitHub signs the raw request body with a shared secret and sends `X-Hub-Signature-256: sha256=<hex>`; the relay recomputes with HMAC-SHA256 and compares in constant time, rejecting with `401` and no body echo. Replay protection is the `X-GitHub-Delivery` UUID against a bounded dedup set (last 4096 ids or 10 minutes, per repo DO); a duplicate returns `200` and is discarded, because GitHub retries deliveries and must not be discouraged. `X-GitHub-Hook-ID` is checked against the configured hook. **GitHub does not sign a timestamp**, so replay resistance rests on the delivery-id set plus secret secrecy, not on a freshness window — stated plainly because it is a real limit.

**v1 uses a repo-level webhook on `spinyfin/mono`**, not a GitHub App. It is configurable in one GitHub UI screen, revocable in one click, scoped to exactly one repo, and needs no App registration, no installation, and no private key. A GitHub App is the better long-term owner — one subscription for the whole org, and the natural place to unify with T3196's installation-token work — but T3196 is `todo` and explicitly opt-in, so v1 must not depend on it. The App migration is a tracked, deferred entry in the breakdown. If the App does land, note that it should be a **new** App, not `boss-shake`: shake's credentials are compile-time-embedded into a distributed binary (`github/src/lib.rs:56-58`), which is the last place a webhook secret should live.

**Engine → relay.** Greenfield, so it is specified end to end.

- _Enrollment._ The operator runs `boss engine relay enroll --code <one-time-code>`, where the code is minted by an admin key held as a Worker secret. The relay issues an `engine_id` (opaque) and a 32-byte `engine_secret`, returns them once, and stores only a hash of the secret.
- _Storage._ `boss_relay_auth`, a near-copy of `trunk_auth/src/lib.rs`: `boss_keychain::KeychainStore` under service `dev.spinyfin.boss.relay`, account `engine-credential@<relay-host>`, 0600 file fallback for ad-hoc dev builds, `BOSS_RELAY_CREDENTIAL` env override for local testing, and a `TokenSource` enum so `boss engine relay status` can report where the credential came from. Never in `state.db`, never in a repo file, never logged.
- _Per-request auth._ `Authorization: Bearer <engine_secret>` over TLS. Not request signing: TLS already provides transport integrity, the secret never leaves the keychain→memory→TLS path, and a signing scheme would add clock-skew handling and canonicalization bugs for no threat this model faces.
- _Rotation._ `boss engine relay rotate` issues a new secret with the old one valid for a 10-minute grace window, then revoked. `boss engine relay revoke --engine <id>` is the lost-laptop path and takes effect on the next request.
- _Reconnect identity._ The same `engine_id` and secret. The cursor is held server-side keyed by `engine_id`, so a reconnect resumes where it left off without the engine persisting anything but its credential.

**Relay → GitHub: nothing.** The relay holds no GitHub token, no App private key, and no installation credential, and makes no outbound GitHub call of any kind. It cannot read a private repo, cannot post a status, cannot open or merge a PR. This is a deliberate constraint, not an omission — it is what bounds the blast radius to "can forge or withhold hints".

**The asymmetry worth naming:** the webhook secret is a different credential axis from the polling token. This project does not fix the shared-personal-token problem (T3196's job) but it does not entrench it either — nothing here makes the ambient `gh` login more load-bearing than it already is, and the relay adds no new consumer of it.

### The backstop, and detecting a silently dead push path

**The full sweep is never removed.** In v1 its cadence does not change at all. In v2, gated on shadow-mode evidence, it relaxes from 60s to 600s — slower, never absent.

Silent death is the failure mode that matters, and it is detected on both sides:

- **Engine side.** The relay client tracks `last_successful_cycle_at`, updated on any `200` or `204`. If it exceeds `3 × timeout` (75s), the channel is marked unhealthy. Unhealthy has three automatic effects: the sweep cadence reverts to its compiled default even if the relaxed-sweep flag is on, the per-PR adaptive timer is re-armed for all tracked PRs, and an attention is filed. **No operator action is required for the fallback**; the attention exists so a degraded state is not silently permanent.
- **Relay side.** A `204` on every timeout means silence is affirmative, not ambiguous. A repo DO that has received no GitHub delivery in 24 hours emits a counter that surfaces a misconfigured or deleted webhook — the failure a delivery-driven system cannot otherwise see.
- **Shadow mode is the third detector**, and the only one that catches _partial_ death: a channel that is up but not delivering, say, `status` events. See [Cutover](#cutover).

### Hosting: Cloudflare vs Azure

**Recommendation: Cloudflare Workers + Durable Objects, with the relay in Rust via `workers-rs`.**

|                     | Cloudflare Workers + DO                                                                                                                                               | Azure Container Apps                                                                   |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Rust story          | `workers-rs` → `wasm32-unknown-unknown` + `wasm-bindgen` + `worker-build`. **Awkward under bazel.**                                                                   | Plain `axum` binary in a container. Native, no friction.                               |
| Bazel integration   | Toolchain already carries `wasm32-unknown-unknown` (`MODULE.bazel:134`), but `worker-build` is a non-bazel glue step; needs an `sh_binary` wrapper around `wrangler`. | `rules_oci` image → ACR push → `az containerapp update`. Conventional.                 |
| Connection state    | Durable Object per repo. Wakes in ms, no cold-start penalty worth naming.                                                                                             | Real process; must pin `min-replicas: 1` because scale-to-zero drops held connections. |
| Cost at this volume | **$0** (free tier: 100k req/day; 5 engines long-polling at 25s ≈ 17k/day).                                                                                            | ~$10–20/month always-on.                                                               |
| Ops surface         | One `wrangler deploy`, two secrets.                                                                                                                                   | Container registry, image build, revision management, two clouds' credentials in CI.   |
| Availability        | Anycast edge; no region to lose.                                                                                                                                      | Single region unless multi-region is configured.                                       |

The decisive argument is that **relay availability is a soft requirement**: when it is down, engines fall back to polling and nothing breaks. Paying for always-on compute and carrying a container pipeline to protect a property that degrades gracefully is the wrong trade. Cost and ops win, so Cloudflare wins.

The honest cost is the `workers-rs` friction. It is bounded — one crate, one wasm target already in the toolchain, one non-bazel glue step behind an `sh_binary` — but it is real, and the operator asked for "a simple Rust HTTP service", which on Cloudflare means Rust compiled to wasm with a JS shim rather than a Rust binary serving HTTP. **This is flagged for an operator decision** in the attentions manifest, with the fallback spelled out: if `workers-rs` under bazel proves worse than budgeted during the deploy task, the choice is between a TypeScript Worker with the wire format still owned by the Rust `relay_protocol` crate (keeps $0 and the ops story, loses "it's Rust"), and Azure Container Apps (keeps Rust, costs money and a container pipeline). Deciding that now, in the abstract, would be guessing.

### Blast radius

If the relay is fully compromised, the attacker can: forge `PrReconcileRequested` hints (cost: wasted GraphQL points, bounded by the debounce and the rate-limit throttle at `metrics.rs:172-181`), withhold deliveries (cost: latency, bounded by the sweep), and read envelope metadata — repo names, PR numbers, commit shas. All of that is already public for public repos and is metadata, not content, for private ones.

The attacker **cannot**: read repository content, act on GitHub in any way, reach the engine (the channel is engine-initiated and carries no commands), reach `state.db`, or influence what the engine writes about a PR — because the engine re-probes GitHub and re-reads its own DB before every write.

If the _webhook secret_ leaks, an attacker can inject forged deliveries: same bounded cost, and rotating the secret is one GitHub UI field plus one `wrangler secret put`. If an _engine credential_ leaks, the holder can read that engine's repo deliveries; `boss engine relay revoke` is immediate.

Public-endpoint abuse: the `/v1/webhook` path rejects anything failing HMAC before doing work, Cloudflare's platform absorbs volumetric traffic, and per-IP rate limiting caps unsigned-request floods. Unauthenticated requests never touch a Durable Object, so a flood cannot exhaust per-DO resources.

### Observability

Relay-side counters: `relay_deliveries_received_total{repo,event}`, `relay_deliveries_rejected_total{reason}` (`bad_signature` / `duplicate` / `unknown_repo`), `relay_deliveries_fanned_out_total{repo}`, `relay_subscribe_requests_total{outcome}`, `relay_cursor_expired_total{engine}`, `relay_repo_silent_24h`. Structured logs carry `delivery_id` end to end so one GitHub delivery is traceable from GitHub's own delivery log through the relay to the engine's reconcile.

Engine-side, on the existing registry ([`engine-counter-metrics-framework.md`](./engine-counter-metrics-framework.md), the same surface `bossctl metrics` reads): `relay_envelopes_received_total{event}`, `relay_reconciles_published_total`, `relay_deliveries_debounced_total` (the amplification-avoided number, and the direct evidence the debounce is earning its place), `relay_channel_healthy` (gauge), `relay_cursor_expired_total`. Plus `boss engine relay status`, mirroring `boss engine trunk status`, reporting credential source, channel health, cursor position, and last delivery time.

**"Are deliveries arriving and being fanned out?"** is answered by `relay_deliveries_received_total` rising, `relay_envelopes_received_total` rising in step, and `relay_repo_silent_24h` staying at zero.

## Cutover

**T3537 is the gate.** Without its per-call, per-subsystem GraphQL counters, the before/after table above is arithmetic rather than measurement, and the cutover's effect is unfalsifiable. Nothing here ships before T3537's instrumentation is landed and has produced a baseline. That is a hard cross-project dependency, not a preference.

**Two flags, not one**, because these are two independently reversible decisions:

- `github_webhook_ingress` — **default OFF.** Off means the engine never dials the relay and every cadence is exactly today's. Following the default-OFF precedent at `feature-flags/src/lib.rs:222` (`stacked_pr_auto_structuring`).
- `github_webhook_relaxed_sweep` — **default OFF.** Gates _only_ the backstop cadence change. Webhooks can run for weeks at the 60s sweep before this is ever touched.

**Phase 0 — shadow.** `github_webhook_ingress` on, but deliveries drive **nothing**. Each envelope is recorded, and when the poller next reconciles that PR the two are compared. Three counters: `webhook_shadow_predicted_total` (webhook announced it, poll confirmed a transition — the good case), `webhook_shadow_poll_only_total` (**poll found a transition no webhook announced — a missed delivery, the number that must be ~0**), `webhook_shadow_webhook_only_total` (webhook fired, poll found nothing — harmless noise, but a sanity check on subscription scope). Run for one week minimum. Adaptive timer untouched throughout, so Phase 0 costs a little extra spend and risks nothing.

**Phase 1 — drive, sweep untouched.** Deliveries publish `PrReconcileRequested`; the adaptive per-PR timer is disabled for repos with a healthy channel. The 60s sweep does not move. Expected: ~1,600 pts/hr, a 59–77% reduction, measured on T3537's counters. Rollback is flipping one flag.

**Per-signal rollout order within Phase 1**, cheapest-to-verify first: `pull_request` (opened/synchronize/closed — the clearest transitions and the ones with existing tests), then `status` and `check_run`/`check_suite` (the CI verdicts, highest volume, where the debounce is load-bearing), then `pull_request_review`, then `merge_group`, then `push`-to-default-branch (last, because it triggers a whole-repo re-probe and so has the largest single-event cost).

**Phase 2 — relax the sweep.** Only if Phase 1 held for two weeks with `webhook_shadow_poll_only_total` at zero. 60s → 600s behind `github_webhook_relaxed_sweep`. Expected: ~200 pts/hr, 95–97%. This is the phase that trades backstop latency for quota, so it gets its own flag, its own soak, and its own decision.

**Kill switches, in escalating order:** flip `github_webhook_relaxed_sweep` off (sweep returns to 60s instantly); flip `github_webhook_ingress` off (engine stops dialling, all cadences return to today's); disable the webhook in GitHub's UI (deliveries stop, engines see silence, dead-channel detection reverts them automatically within 75s). Every level is one action and none requires a rebuild or a deploy.

**The metric that says this is working:** GraphQL points/hour attributable to the merge poller, from T3537's counters, at or below 1,600 — **with** median transition-detection latency at or below today's and `webhook_shadow_poll_only_total` at zero. All three, or it is not working. The latency and missed-delivery terms are what stop "spend less" from being satisfiable by simply polling less.

## Relationship to T3537 and T3538

**T3537 (instrumentation) — hard prerequisite, no overlap.** It measures; this changes what is measured. Nothing here ships first.

**T3538 (merge-poller drain) — overlapping, and the split is proposed explicitly here.** T3538's description reserves event-driving (_"where GitHub can tell us (webhooks…), react to the event and drop the corresponding poll"_) while flagging the blocker (_"the engine runs locally, so webhook delivery is a real infrastructure problem (no public URL)… If that turns out to be the bulk of the work, raise an attention rather than absorbing it silently — it may deserve its own design."_). It did, and this is that design.

Proposed split, confirming the brief's:

- **T3538 owns:** batching the adaptive path, the ±5s cross-PR coalescing window, the due-set size distribution measurement, and `PR_PROBE_FIELDS` trimming (`labels(first: 30)` → 10, `reviews(last: 20)` → 5). All inside the poller. **These stay valuable whether or not webhooks ever ship** — they are what determines the quality of the fallback when the relay is unreachable, and they make the backstop sweep cheaper, which is 97% of post-cutover spend.
- **P3556 (this project) owns:** the relay service, the transport, engine enrollment and identity, the webhook → `PrReconcileRequested` producer, the defer-not-drop fix, the per-PR debounce, shadow mode, and the sweep-cadence relaxation.
- **The contested seam is the coalescing window, and it is not actually contested** once split correctly: T3538's ±5s window batches the _due set_ (input: timers), this design's 40s debounce bounds _per-PR reconcile frequency_ (input: deliveries). Different inputs, different purposes, and they compose. Both are needed.
- **File overlap is real.** Both projects edit `merge_poller/schedule.rs` and `merge_poller/sweep.rs`. Serialize at the file level: the defer-not-drop fix lands first (it is small and independently correct), then whichever project reaches the debounce/batching work forward-ports the other's changes preservingly.

**Renegotiation requested:** T3538's "Event-driving" paragraph should be **struck from T3538** and treated as absorbed here, leaving T3538 as "scope + batch". Flagged in the attentions manifest as an operator decision, because retitling another active work item is not this design's call to make unilaterally.

**T3540 (`boss pr status` / `boss pr body`)** — complementary. Webhooks make its stored state fresher, and its `observed_at` contract matters _more_ under a push model, where staleness is bursty rather than uniform.

**P856 (PR/CI/conflict reconciliation)** — owns the reconciliation _domain_; this owns _ingress and transport_ into it. The event-bus doc flagged this seam as unconfirmed (`:248`, _"Want a reviewer/coordinator to confirm the split"_). This design does not resolve it and does not need to: it produces onto `PrReconcileRequested` and touches nothing downstream of the publish.

## Superseding the missing investigation

`tools/boss/docs/investigations/github-event-detection-webhooks-vs-polling-2026-07-08.md` does not exist at any sha in the checkout (`ls tools/boss/docs/investigations/` lists eight files; none matches). It is cited from **five** live code sites — `merge_poller/schedule.rs:99`, `schedule.rs:207`, `probe.rs:259`, `probe.rs:795`, `sweep.rs:346` — and repeatedly from the event-bus design doc, which recovered its conclusions inline and asked whether it should be rewritten before this project starts (`:377`).

**Decision: supersede it, do not attempt recovery.** Its recoverable conclusions are all reflected here and all still hold — §8's "keep the full sweep as the correctness backstop" is [the backstop section](#the-backstop-and-detecting-a-silently-dead-push-path); §9 item 3's adaptive timer plus targeted reconcile has already shipped (`PrPollSchedule`, `reconcile_one`); §9.2's ETag conditional requests shipped at `probe.rs:790-828`. Rewriting a July investigation from its own citations would produce a document less accurate than this one, which has measured data the original did not.

The five code citations should be repointed at this document. That is a separate small PR ([task 1](#proposed-implementation-task-breakdown)) so the design PR stays doc-only.

## Risks / open questions

- **`workers-rs` under bazel is the largest unknown.** Budgeted as one `sh_binary` wrapper around `wrangler`/`worker-build`. If it exceeds that, the fallback is a TypeScript Worker (keeps $0 and the ops story, loses "it's Rust") or Azure Container Apps (keeps Rust, costs ~$15/month and a container pipeline). **Operator decision — in the attentions manifest.** Recommendation: attempt `workers-rs`, time-box it inside the deploy task, and escalate rather than absorb.
- **The before/after numbers assume N ≈ 25 PRs and ~20 PRs/day.** The sweep term scales linearly in N; the event term scales in PR _throughput_, not count. If Boss's throughput doubles, v1 goes to ~1,650 pts/hr (the sweep still dominates) — the model is robust in the direction that matters. T3537's counters are what confirm it.
- **`mergeStateStatus` still gates conflict detection, and no event announces it.** The `push`-to-default-branch trigger is a proxy: it catches conflicts caused by `main` moving, which is the dominant cause, but not conflicts surfaced by GitHub's async recomputation landing later. Under the relaxed v2 sweep, worst-case conflict-detection latency becomes **10 minutes** rather than 60 seconds. That is the single real regression in this design. It is why Phase 2 is separately flagged, separately soaked, and separately reversible — and it may be that the right long-term answer is a relaxed sweep for everything _except_ a cheap mergeability-only probe kept at a faster cadence. Not proposed for v1; flagged.
- **The 40s debounce is a guess calibrated to today's Hot tier.** It guarantees no-worse-than-today but may be conservative — if `webhook_shadow_poll_only_total` is zero and latency is dominated by the debounce rather than the network, lowering it to 10s is nearly free. Tune on Phase 1 data, not now.
- **Repo-level webhook vs GitHub App for v1.** Recommended repo-level for reversibility and zero new credential machinery, with the App as a tracked follow-up that should unify with T3196. **Operator decision — in the attentions manifest**, since it interacts with T3196's sequencing.
- **Two engines on the same repo has never been exercised.** The design says both receive everything and non-owners filter for free (`sweep.rs:384-392`), which is sound on inspection, but a validation task exists precisely because "sound on inspection" is not evidence.
- **Cursor durability across a Durable Object migration.** Cloudflare can relocate a DO; storage is durable but a migration window could drop in-flight long-polls. Engines handle this as an ordinary reconnect, but it should be exercised deliberately rather than discovered.
- **`boss shake`'s App must not be reused** if the App migration happens. Its credentials are compile-time-embedded in a distributed binary (`github/src/lib.rs:56-58`). A fresh App, with the webhook secret held only as a Worker secret.
- **`is_rate_limit_error` is a bare substring match** (`metrics.rs:265-267`: `stderr.to_ascii_lowercase().contains("rate limit")`) and misses a bare `HTTP 429`. Not caused by this design and not fixed by it, but it becomes more visible when total spend drops and each remaining rate-limit event carries more diagnostic weight. Worth a line in T3537 or T3538 rather than a task here.

## Proposed implementation task breakdown

Tasks are PR-sized: one subsystem, one worker, one session. "Depends on" names gating entries by name. Parallelism is noted per depth, weighing **file** overlap and not just functional independence.

**Cross-project gate:** every entry from _Engine: publish `PrReconcileRequested` from relay deliveries_ onward is blocked on **T3537** (instrumentation) landing, because the cutover's effect is unmeasurable without it. Entries before that point may proceed in parallel with T3537.

### 1. Repoint the missing-investigation citations at this design

**Scope:** Update the five live code comments that cite the absent `github-event-detection-webhooks-vs-polling-2026-07-08.md` — `merge_poller/schedule.rs:99`, `schedule.rs:207`, `probe.rs:259`, `probe.rs:795`, `sweep.rs:346` — to cite this design doc instead, preserving each site's section reference by mapping it to the equivalent section here. Comment-only; no behaviour change.

**Effort:** `trivial`

**Depends on:** none.

**Scope: in-scope**

### 2. `relay_protocol` crate — wire types

**Scope:** New crate `tools/boss/relay_protocol/` holding `DeliveryEnvelope`, `SubscribeRequest`, `SubscribeResponse`, the cursor type, the protocol-version constant, and the error enum. Serde derives, round-trip tests, no I/O and no dependency on the engine. Minimal bazel visibility (`//tools/boss:__subpackages__`) following `tools/boss/http_retry/BUILD.bazel`.

**Effort:** `small`

**Depends on:** none.

**Scope: in-scope**

### 3. Fix the `PrReconcileRequested` quiesce: defer, do not drop

**Scope:** In `merge_poller/schedule.rs:463-522`, replace the drop-on-quiesce behaviour with a pending set plus a short defer timer, so an event arriving inside the window is reconciled when the window closes rather than discarded. Key the window off the last reconcile _of that PR_ rather than the last full sweep. Regression test: publish inside the window, assert the PR is reconciled rather than logged-and-lost. Independently correct today; a hard prerequisite for anything push-driven.

**Effort:** `small`

**Depends on:** none.

**Scope: in-scope**

### 4. Per-PR debounce for event-driven reconciles

**Scope:** Add a debounce keyed by `pr_url` enforcing a minimum 40s interval between event-driven reconciles of the same PR, with a `relay_deliveries_debounced_total` counter. Bounds burst amplification and makes the event-driven worst case provably equal to today's Hot tier. Engine-core only; no relay dependency, testable with synthetic bus publishes.

**Effort:** `medium`

**Depends on:** Fix the `PrReconcileRequested` quiesce. **Edits the same files as T3538's batching work (`schedule.rs`, `sweep.rs`)** — land after entry 3, and forward-port T3538's changes preservingly if it lands first.

**Scope: in-scope**

### 5. Relay: webhook receive, HMAC verify, delivery dedup

**Scope:** New crate `tools/boss/relay/`. HTTP handler for `POST /v1/webhook`: constant-time `X-Hub-Signature-256` HMAC-SHA256 verification over the raw body, `X-GitHub-Hook-ID` check, `X-GitHub-Delivery` dedup against a bounded set, and normalization of `pull_request` / `status` / `check_run` / `check_suite` / `push` / `merge_group` payloads into `DeliveryEnvelope`. In-memory buffer only — no fan-out, no persistence, no deploy. Table-driven tests over captured GitHub payload fixtures.

**Effort:** `medium`

**Depends on:** `relay_protocol` crate.

**Scope: in-scope**

### 6. Relay: per-repo Durable Object, ring buffer, and long-poll subscribe

**Scope:** Add the per-repo Durable Object holding a bounded ring buffer (512 envelopes / 1 hour) and per-engine cursors, plus `GET /v1/subscribe` implementing the long-poll contract: `200` with envelopes, `204` on timeout, `409 cursor_expired` when the cursor has aged out. Tests for replay-from-cursor, timeout, and expiry.

**Effort:** `medium`

**Depends on:** Relay: webhook receive, HMAC verify, delivery dedup.

**Scope: in-scope**

### 7. Relay: engine enrollment, credential issue, rotate, revoke

**Scope:** Admin-key-gated enrollment endpoint issuing `engine_id` + 32-byte `engine_secret` (storing only a hash), bearer-token verification on `/v1/subscribe`, rotation with a 10-minute grace window, and immediate revocation. Tests for rotation overlap and post-revocation rejection.

**Effort:** `medium`

**Depends on:** Relay: per-repo Durable Object, ring buffer, and long-poll subscribe.

**Scope: in-scope**

### 8. Relay: deploy pipeline and secret management

**Scope:** `wrangler.toml` with the Durable Object binding, a bazel `sh_binary` deploy target wrapping `worker-build`/`wrangler`, a `main`-only Buildkite deploy step, and the webhook secret plus Cloudflare API token as Buildkite secrets. **Time-box the `workers-rs`-under-bazel integration inside this task and escalate rather than absorb** if it exceeds budget — the fallback fork is an operator decision recorded in this design's open questions.

**Effort:** `medium`

**Depends on:** Relay: engine enrollment, credential issue, rotate, revoke.

**Scope: in-scope**

### 9. `relay_auth` crate + `boss engine relay` CLI

**Scope:** New crate `tools/boss/relay_auth/` mirroring `trunk_auth/src/lib.rs`: `boss_keychain::KeychainStore` under `dev.spinyfin.boss.relay`, 0600 file fallback for ad-hoc dev builds, `BOSS_RELAY_CREDENTIAL` env override, and a `TokenSource` enum. Plus `boss engine relay enroll | status | rotate | revoke`, mirroring `boss engine trunk`. Storage and CLI only — no network client.

**Effort:** `small`

**Depends on:** `relay_protocol` crate. Parallel with entries 5–8 (different crates, no shared files).

**Scope: in-scope**

### 10. `relay_client` crate — the engine-side long-poll loop

**Scope:** New crate `tools/boss/relay_client/` built on `boss_http_retry`'s shared client and `RetryPolicy`: the long-poll loop, exponential backoff with jitter, cursor tracking, `409 cursor_expired` handling, `401` handling, and `last_successful_cycle_at` health tracking. Returns envelopes to a caller-supplied sink; publishes nothing itself. Tests against a mock HTTP server.

**Effort:** `medium`

**Depends on:** `relay_protocol` crate; `relay_auth` crate + `boss engine relay` CLI.

**Scope: in-scope**

### 11. Engine: publish `PrReconcileRequested` from relay deliveries, flag-gated

**Scope:** Wire `relay_client` into the engine: derive the repo subscription set from the `products` table, run the client as a supervised task, map envelopes through the debounce to `bus.publish(Event::PrReconcileRequested { pr_url })`, and disable the adaptive per-PR timer for repos with a healthy channel. Behind `github_webhook_ingress`, **default OFF**, following `feature-flags/src/lib.rs:222`. **Blocked on T3537.**

**Effort:** `medium`

**Depends on:** `relay_client` crate; Fix the `PrReconcileRequested` quiesce; Per-PR debounce for event-driven reconciles; **T3537 (cross-project)**.

**Scope: in-scope**

### 12. Engine: shadow mode — record deliveries, compare against poll outcomes

**Scope:** Record each envelope and, on the next poll of that PR, compare prediction against observed transition. Emit `webhook_shadow_predicted_total`, `webhook_shadow_poll_only_total`, `webhook_shadow_webhook_only_total` on the existing registry. Runs with `github_webhook_ingress` on but before deliveries drive behaviour — this is the Phase 0 instrument and the gate on every later phase.

**Effort:** `medium`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries.

**Scope: in-scope**

### 13. Engine: dead-channel detection and automatic cadence revert

**Scope:** When `last_successful_cycle_at` exceeds `3 × timeout` (75s), mark the channel unhealthy, revert the sweep cadence to its compiled default, re-arm the adaptive per-PR timer for all tracked PRs, and file an attention. Recover automatically when the channel returns. Test the full degrade-and-recover cycle. This is what makes relay unavailability a non-event.

**Effort:** `small`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries. Parallel with shadow mode, but **both touch the relay-client health surface** — land shadow mode first and forward-port.

**Scope: in-scope**

### 14. Engine: `push` to default branch triggers a batched whole-repo re-probe

**Scope:** On a `push` envelope for a repo's default branch, enqueue all tracked open PRs in that repo for a single batched `probe_batch` re-probe rather than N individual reconciles. This is the conflict-detection trigger — GitHub emits no per-PR event when `main` moves — and its whole-repo cost is why it rolls out last in the per-signal order.

**Effort:** `small`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries.

**Scope: in-scope**

### 15. Relay and engine observability wiring

**Scope:** Add the relay-side counters (`relay_deliveries_received_total`, `..._rejected_total{reason}`, `..._fanned_out_total`, `relay_subscribe_requests_total{outcome}`, `relay_cursor_expired_total`, `relay_repo_silent_24h`) and the engine-side counters (`relay_envelopes_received_total`, `relay_reconciles_published_total`, `relay_channel_healthy`), propagate `delivery_id` through structured logs end to end, and extend `boss engine relay status` to report channel health, cursor position, and last delivery time.

**Effort:** `small`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries; Relay: deploy pipeline and secret management.

**Scope: in-scope**

### 16. Validation: two engines on one repo

**Scope:** Stand up a second engine (isolated `--socket-path` fixture per the worker rules), enroll it against the same repo, and verify both receive every delivery, the engine without a matching work item pays nothing via `reconcile_one`'s early return (`sweep.rs:384-392`), cursors advance independently, and one engine going offline and reconnecting replays correctly from its own cursor. A validation campaign, listed after the implementation it validates — not folded into it.

**Effort:** `small`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries; Engine: dead-channel detection and automatic cadence revert.

**Scope: in-scope**

### 17. Relax the backstop sweep behind `github_webhook_relaxed_sweep`

**Scope:** Make the full-sweep interval configurable (it is a hardcoded `Duration::from_secs(60)` at `app/server.rs:950`) and gate 60s → 600s behind a second flag, **default OFF**, separate from `github_webhook_ingress`. Ships only after Phase 1 holds for two weeks with `webhook_shadow_poll_only_total` at zero. This is the phase that trades worst-case conflict-detection latency for quota, so it is separately flagged and separately reversible.

**Effort:** `small`

**Depends on:** Engine: shadow mode; Engine: dead-channel detection and automatic cadence revert; Validation: two engines on one repo.

**Scope: in-scope**

### 18. Engine: adopt `pull_request.opened` as the authoritative `pr_url` source

**Scope:** Use the `pull_request.opened` envelope's `pr_url` to populate the work item's `pr_url` directly, complementing the in-memory `StagedPrUrlCache` that is lost on engine restart (`pr_url_capture.rs:1-33`). Keep the existing regex-scrape hot path and the `jj`-plus-GitHub reconstruction cold path unchanged; this is a third, durable source, not a replacement. A reliability win the project did not set out to get.

**Effort:** `small`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries.

**Scope: deferred (future / not a v1 blocker)** — orthogonal to the quota goal; land after the merge-poller path has soaked.

### 19. Migrate from a repo-level webhook to a GitHub App

**Scope:** Register a new GitHub App (explicitly **not** `boss shake`'s, whose credentials are compile-time-embedded in a distributed binary at `github/src/lib.rs:56-58`), move the webhook subscription to it, and evaluate unifying its installation token with T3196's opt-in polling-auth work so one App owns both delivery and polling. Gives org-wide subscription instead of per-repo configuration.

**Effort:** `medium`

**Depends on:** Relay: deploy pipeline and secret management; **T3196 (cross-project, currently `todo`)**.

**Scope: deferred (future / not a v1 blocker)** — v1 deliberately uses a repo-level webhook for one-click reversibility, and T3196 is `todo` and opt-in so v1 must not depend on it.

### 20. Event-drive the external issue tracker (Issues / Projects V2)

**Scope:** Add `issues`, `issue_comment`, and `projects_v2_item` to the webhook subscription and route them to the external tracker, replacing or relaxing its 120s cadence (`app/server.rs:1369`, `github_tracker/src/github.rs`). Needs its own envelope fields and its own consumer; shares only the transport.

**Effort:** `medium`

**Depends on:** Engine: publish `PrReconcileRequested` from relay deliveries.

**Scope: deferred (future / not a v1 blocker)** — not implicated in the measured GraphQL drain; a separate consumer with a separate correctness argument.

### 21. Branch registry from `push` events for the abandoned-branch sweep

**Scope:** Maintain a local registry of pushed branches from `push` envelopes so `abandoned_branch_pr_sweep.rs` (5-minute cadence, `:118`) can consult it instead of asking GitHub whether each branch exists. Reduces REST spend on a sweep that today asks a _negative_ question no webhook answers directly.

**Effort:** `medium`

**Depends on:** Engine: `push` to default branch triggers a batched whole-repo re-probe.

**Scope: deferred (future / not a v1 blocker)** — REST budget is separate from the exhausted GraphQL budget, so the payoff does not address the measured incident.

### 22. WebSocket transport upgrade over Durable Object hibernation

**Scope:** Replace long-poll with a WebSocket held via Cloudflare's Durable Object WebSocket Hibernation API, adding a WebSocket client to `relay_client` with ping/pong, reconnect, and backpressure handling. Removes the sub-RTT reconnect gap between long-poll cycles.

**Effort:** `medium`

**Depends on:** Relay: deploy pipeline and secret management; `relay_client` crate.

**Scope: deferred (future / not a v1 blocker)** — long-poll is what the Buildkite agent actually does and meets the sub-5s latency goal; revisit only if measured latency disappoints.

### 23. Buildkite webhook ingestion for CI verdicts and log tails

**Scope:** Receive Buildkite's own webhooks at the relay (a second HMAC secret, a second normalization path) to carry mono's authoritative CI verdict and job metadata, rather than inferring it from GitHub's `status` mirror. Would also give `ci_remediations.log_excerpt` an event trigger instead of a poll.

**Effort:** `large`

**Depends on:** Relay: deploy pipeline and secret management.

**Scope: deferred (future / not a v1 blocker)** — GitHub's `status` events already carry the verdict Boss consumes (`.buildkite/REQUIRED_CHECKS.md`), so this is a second ingress with a second secret for a marginal gain.

### Dependency graph and parallelism

- **Depth 0 (all parallel — disjoint files):** 1, 2, 3.
- **Depth 1 (parallel):** 5 and 9, after 2. Also 4, after 3 — but 4 shares `schedule.rs` / `sweep.rs` with T3538's batching work; serialize at the file level and forward-port preservingly.
- **Depth 2:** 6 after 5; 10 after 9 and 2. Parallel with each other.
- **Depth 3:** 7 after 6.
- **Depth 4:** 8 after 7. Parallel with 11 once T3537 has landed.
- **Depth 5:** 11, gated on 10, 3, 4, and T3537. Single-threaded — everything downstream depends on it.
- **Depth 6:** 12, 13, 14, 15 after 11. 14 and 15 are parallel with everything; **12 and 13 both touch the relay-client health surface — land 12 first, then 13 forward-ports.**
- **Depth 7:** 16 after 11 and 13.
- **Depth 8:** 17 after 12, 13, and 16. This is the Phase 2 gate.
- **Deferred, unsequenced:** 18, 19, 20, 21, 22, 23.
