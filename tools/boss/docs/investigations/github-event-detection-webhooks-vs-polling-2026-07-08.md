# Investigation: GitHub event detection — webhooks vs polling sweeps for a laptop-hosted engine

**Date:** 2026-07-08
**Execution:** `exec_18c029fe4da7a9b0_61`
**Status:** Investigation writeup. No code changes. Deliverable is the option space for how Boss detects GitHub state changes, a baseline grounded in today's engine sweeps, and a staged recommendation. Follow-up code changes are listed at the end for the operator to file separately.
**Related:** [`oauth-device-flow-scopes-vs-issue-sync`](oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md) · [`external-issue-tracker-sync-github-projects`](../designs/external-issue-tracker-sync-github-projects.md) · [`comment-triggered-document-revisions`](../designs/comment-triggered-document-revisions.md) · [`unify-pr-remediation-on-revisions`](../designs/unify-pr-remediation-on-revisions.md) · [`merge-conflict-handling-in-review`](../designs/merge-conflict-handling-in-review.md)

---

## TL;DR — recommendation

**Do smarter polling now; add an outbound-connection relay later; keep a reconciliation sweep as the permanent backstop.** Concretely, staged:

1. **Now (no new infra):** make the existing sweeps cheaper and more responsive — conditional requests (ETag/`If-None-Match`, whose `304`s are free of REST quota), one GraphQL probe per pass instead of N per-PR `gh` shellouts, and adaptive intervals (poll fast only while a PR is in an active state). This alone takes the latency floor from 60 s toward ~10–15 s and cuts steady-state API traffic on unchanged PRs to near zero, with **zero reachability problems**. It is the do-nothing-radical baseline and it gets us most of the way.

2. **Later (when sub-10 s latency or high-fanout signals like per-PR comments actually matter):** stand up a **tiny always-on cloud relay** that receives GitHub webhooks and holds them in a queue; the engine keeps a single **outbound** WebSocket/SSE/long-poll to it. The laptop never needs to be reachable from GitHub — it dials out, exactly like `cloudflared` and `smee.io` already do. This is the only option that both survives NAT/sleep/network-switching _and_ delivers seconds-latency for an open-ended set of event types.

3. **Always:** keep the periodic reconciler sweep running as the correctness floor. Webhook delivery is best-effort (GitHub retains deliveries for redelivery only **3 days**, does **not** auto-redeliver, and drops on a >10 s / offline endpoint). Push is the _fast path_; poll is the _catch-up path_. Both must feed **one** engine-owned reconciler — which is exactly Boss's existing design stance ("the engine owns reconciliation, the UI is a thin renderer") and maps cleanly onto the merge poller's **already-existing `kick` seam**.

**Auth, orthogonally:** move Boss's GitHub reconciler from the ambient `gh` login onto a **GitHub App**. App-level webhooks, fine-grained per-repo permissions, a real webhook secret, and a higher/scalable rate limit are all worth it regardless of which transport we pick — and Boss **already ships a GitHub App** (the `boss shake` app), so the credential machinery mostly exists.

The single most important framing in this doc: **the real axis is not "webhook vs poll" but "who holds the connection."** Every option that needs GitHub to reach _inbound_ to the laptop (raw webhooks, ngrok, Tailscale Funnel) fights the operator's constraint. Every option where the laptop dials _outbound_ (smarter polling, `cloudflared`, a relay + outbound socket, GitHub Actions → relay) sidesteps it. Pick from the outbound column.

---

## 1. Why this investigation

Today Boss learns about GitHub state changes — PR opened/merged/closed, CI pass/fail, mergeability, review decisions — by **polling GitHub on timers from the engine**. The operator wants to know whether webhook-style push notifications could replace or augment that, with one hard constraint, in the operator's words:

> "what the webhook needs to call back is my local laptop, and I'm not sure if it's reachable from GitHub."

Two pressures make this worth investigating now rather than later:

- **We will want more signals over time.** PR review comments and issue comments are already a planned producer (see §5 and the [comment-triggered revisions design](../designs/comment-triggered-document-revisions.md), which explicitly names "deferred GitHub PR-comment triage" as a future source). Reviews and individual check runs are close behind. Comment-level signals are the case where polling scales worst: there is no cheap "did any comment change on any of my PRs" query, so naive polling means many calls per PR per interval.
- **Detection should be fast and efficient.** Fast = seconds, not a sweep interval. Efficient = not burning API quota re-fetching unchanged state.

This doc inventories what we poll today (§2), states the targets and the rate-limit facts that bound the design (§3), walks the option space (§4), answers the cross-cutting questions — laptop realities, security, auth model, future signals, ops burden (§5–§6) — sketches the engine-side event-ingestion boundary that lets push and poll coexist (§7), and ends with a compared, staged recommendation (§8).

---

## 2. Current state — the polling baseline

Line numbers below are as of writing and will drift; module and function names are the durable anchors.

### 2.1 The GitHub-polling sweeps that exist today

Boss runs a family of periodic loops, but only two of them poll GitHub. The rest (dead-PID reaper, lost-workspace sweep, pool-claim sweep, host reconcile, cube-lease heartbeat, DB backup — all `Duration::from_secs(60)`-class loops in `engine/core/src/*_sweep.rs` / `*_reconcile.rs`) talk to cube or the local process table, **not** GitHub, and are out of scope.

#### (A) The PR reconciler — a.k.a. the "merge poller"

This is the primary GitHub sweep and the one the operator is really asking about.

| Property                         | Value                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Spawn site                       | `engine/core/src/app/server.rs:668` (`spawn_merge_poller`)                                                                                                                                                                                                                                                                                                                                                                                 |
| Loop body                        | `engine/core/src/merge_poller.rs:2789` (`spawn_loop` → `run_one_pass`)                                                                                                                                                                                                                                                                                                                                                                     |
| Interval                         | **60 s** (`Duration::from_secs(60)`, `server.rs:676`)                                                                                                                                                                                                                                                                                                                                                                                      |
| Out-of-band trigger              | An `Arc<Notify>` **`kick`** (`server.rs:678`, wired as `pr_reconciler_kick`), with a **15 s quiesce window** (`merge_poller.rs:2802`) so bursts collapse to one pass                                                                                                                                                                                                                                                                       |
| Candidate set per pass           | `list_chores_pending_merge_check` (`work/pr_flow.rs:394`) — every task with `status = 'in_review' AND pr_url IS NOT NULL` — **plus** `list_chores_blocked_on_merge_conflict` and `list_chores_blocked_on_ci_failure`                                                                                                                                                                                                                       |
| Per-PR probe                     | `CommandMergeProbe::probe` (`merge_poller.rs:465`) shells out to `gh pr view <url> --json state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,headRefOid,headRefName,baseRefName,labels,statusCheckRollup,reviewDecision,reviews` (`:482`), then a **second** GraphQL probe `pr_in_merge_queue`, and **occasionally** a third REST call (`fetch_commit_combined_state_for_empty_rollup`, only when `statusCheckRollup` is empty) |
| Transport / creds                | The **`gh` CLI**, inheriting the engine-local **ambient `gh auth` login** (a user PAT/OAuth token). `gh pr view` runs as GraphQL under the hood                                                                                                                                                                                                                                                                                            |
| Signals extracted from one probe | PR open/merged/closed, mergeability + merge-state-status, **CI rollup**, **review decision + reviews**, merge-queue membership                                                                                                                                                                                                                                                                                                             |

The important structural fact: **this one probe is Boss's entire GitHub read pipeline for PRs.** CI-failure detection is not a separate sweep — `ci_watch.rs` (`on_ci_failure_detected` / `on_ci_resolved`) is invoked _from inside_ `merge_poller::sweep_one`, reading the `statusCheckRollup` the probe already fetched; conflict detection (`conflict_watch.rs`) likewise reuses the same fetched probe with no extra round-trip. Merge detection, CI detection, conflict detection, and review indicators all ride the same 60 s probe. That is good news for a push design: **there is already a single choke point** to feed, not five scattered pollers.

Roughly **2–4 GitHub API calls per in-review PR per pass**: the `gh pr view` GraphQL query (1), the merge-queue-membership GraphQL query `pr_in_merge_queue` (1), a per-candidate merge-queue dequeue-timeline GraphQL query (1), and an occasional REST commit-status call when the check rollup is empty (1). Almost all of it is **GraphQL** — which matters for §3.2, because GraphQL cannot use conditional requests. And critically: when **nothing** is in review, the candidate set is empty and the pass makes **zero** GitHub calls (`merge_poller.rs:1398`), so the poller is already idle-efficient — it only spends quota when there is live work.

#### (B) The external-tracker issue-sync reconciler

| Property          | Value                                                                                                                                                                                                                                                                                                         |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Spawn site        | `engine/core/src/app/server.rs:993` (`external_tracker::reconcile::spawn_loop`)                                                                                                                                                                                                                               |
| Interval          | **120 s** (`Duration::from_secs(120)`), fires immediately on boot to catch offline drift                                                                                                                                                                                                                      |
| What it reads     | GitHub Projects V2 items + nested issue content via **one paginated GraphQL query** (`fetch_items`), plus targeted REST issue reads/writes                                                                                                                                                                    |
| Transport / creds | The `gh` CLI via a `GhRunner` abstraction (`external_tracker/github.rs`), ambient `gh auth` today; an **OAuth device-flow** token path is being built (`external_tracker/github_oauth.rs`) to get least-privilege, per the [OAuth scopes investigation](oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md) |
| Status            | **Opt-in** (only runs when a tracker is configured), cross-repo within the org                                                                                                                                                                                                                                |

Roughly **1 GraphQL query per pass** when nothing changed (writes only fire on a detected delta).

#### (C) On-demand `gh` calls (not sweeps, for completeness)

`coordinator.rs` fetches a PR's head OID via `gh` at merge time; `pr_url_capture.rs` scrapes PR URLs from worker hook output. These are event-driven one-shots, not timers, and impose no steady-state polling load.

### 2.2 Baseline API-traffic table

Assuming the merge poller's 60 s cadence with no kicks, ~3 calls per in-review PR per pass (the 2–4 range above), and the issue-sync reconciler on at 120 s:

| Scenario                   | In-review PRs | Merge-poller calls/hr | Issue-sync calls/hr | Total/hr   | vs. 5,000/hr |
| -------------------------- | ------------- | --------------------- | ------------------- | ---------- | ------------ |
| Light (typical single-dev) | 3             | ~540                  | ~30                 | **~570**   | 11%          |
| Busy                       | 10            | ~1,800                | ~30                 | **~1,830** | 37%          |
| Heavy fan-out              | 25            | ~4,500                | ~30                 | **~4,530** | 91%          |

**Read this table carefully, because it reframes the problem.** Two caveats first: (i) almost all of these calls are **GraphQL**, so they draw on GitHub's separate **5,000-_point_/hr** GraphQL budget rather than the REST **5,000-_request_/hr** limit — the "vs 5,000/hr" column is a request-count proxy, and a `gh pr view` costs a few points, so the effective headroom on the GraphQL budget is _tighter_ than raw request counts suggest; (ii) the counts scale linearly with in-review PR count. At light-to-busy single-developer volumes Boss is **not** quota-constrained. But at heavy fan-out the poller already brushes the ceiling — 25 in-review PRs at the upper (4-call) end of the range is ~6,000 calls/hr, **over** the request limit and well into GraphQL-point pressure. So the current pain is _mostly_ not "we're burning quota" today, with an asterisk that heavy fan-out is already close. The dominant deficiencies are:

1. **Latency:** the floor is the poll interval — up to 60 s to notice a merge or a CI flip (120 s for issue state). The `kick` helps when the _app_ is focused, but a PR that merges while the laptop is asleep or the app is backgrounded is invisible until the next tick after wake.
2. **Scaling — with PR count _and_, worse, with signal _types_.** The linear-in-PRs cost is tolerable at low volume but, as the heavy row shows, already brushes the ceiling at 25 PRs. What breaks it decisively is adding **per-comment** or **per-review** polling: those have no cheap aggregate query and no ETag story, so each new signal type multiplies the per-PR call count on top of the PR-count scaling — and _that_ is what collides hard with the quota, precisely the future the operator flagged.

So the driver for a push channel is **latency + future high-fanout signals** (with heavy-fanout quota pressure as a near-term third), not comfortable present-day headroom. That materially changes the recommendation: the cheap wins (conditional requests, batching, adaptive intervals) buy down all three far enough that the expensive push infrastructure can be deferred until the fanout signals actually land.

---

## 3. Targets and the facts that bound the design

### 3.1 Targets

- **Latency:** seconds, not the sweep interval. A merged PR or a red CI check should move the kanban card in ~1–10 s.
- **Efficiency:** steady state on _unchanged_ PRs should cost ~zero quota. Adding a new signal type should not multiply the poll cost.
- **Reachability:** must not depend on GitHub being able to open an inbound connection to the laptop. This is the operator's hard constraint and it eliminates a whole column of options.
- **Ops burden:** it has to be runnable 24/7 by one person, survive laptop sleep / Wi-Fi switching / coffee-shop NAT, and not require babysitting.

### 3.2 GitHub rate-limit and delivery facts (verified 2026-07)

These numbers are load-bearing for the options, so they are stated explicitly:

- **REST primary limit:** 5,000 requests/hr for a user token (PAT or user-to-server). A **GitHub App installation token** also starts at 5,000/hr but **scales** with installation size (+50/hr per repo over 20, +50/hr per user over 20, cap **12,500/hr**), and is **15,000/hr** on GitHub Enterprise Cloud. ([GitHub: REST API rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api))
- **Conditional requests are the free lunch:** a REST request carrying `If-None-Match: <etag>` (or `If-Modified-Since`) that yields **`304 Not Modified` does not count against the primary rate limit**, provided the request was authorized. ([GitHub: REST API best practices](https://docs.github.com/rest/guides/best-practices-for-using-the-rest-api)) Caveats: ETags are cached **per token**, and an installation token's 1 h TTL invalidates the cache on rotation; ETags are **per page**, not per collection.
- **GraphQL** has a **separate** budget: **5,000 points/hr**, where a point cost is computed from the connections/nodes touched. `gh pr view` costs only a few points, but GraphQL has **no ETag/conditional-request equivalent** — you cannot get a free "unchanged" answer from it the way you can from REST.
- **Webhook delivery is best-effort:** GitHub retains webhook deliveries for **redelivery for only 3 days**, **does not automatically redeliver** failures (you script it against the deliveries API), and marks a delivery failed if your endpoint is **down or takes >10 s** to respond. ([GitHub: redelivering webhooks](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks)) This is why _any_ push design needs a poll backstop.

The single most actionable fact here: **Boss's PR reconciler probe is mostly GraphQL** (`gh pr view` + merge-queue), which cannot use conditional requests. A big chunk of the "smarter polling" win in §4 Option 3 comes from **restructuring the probe to lean on REST conditional requests where the data is available over REST**, and batching the rest into one GraphQL call per pass instead of one per PR.

---

## 4. The option space

For each option: how it works, the reachability verdict (the operator's constraint), setup/ops burden, security surface, cost, and how it copes with laptop sleep/offline. Options are grouped by the **who-holds-the-connection** axis.

### Column 1 — GitHub reaches _inbound_ to the laptop (fights the constraint)

#### Option 1: GitHub webhooks + a tunnel to the laptop

GitHub POSTs each event to a public URL; a tunnel forwards that URL to a local port the engine listens on. Candidate tunnels differ _sharply_ on the reachability axis, so they must be split:

**1a. Raw public endpoint / port-forward.** Requires a static public IP and an open inbound port on the laptop. **Dead on arrival** for a laptop behind NAT/DHCP on changing networks. Not considered further.

**1b. ngrok.** A tunnel agent on the laptop dials out to ngrok's edge; ngrok gives a public URL that forwards inbound. Setup is trivial (`ngrok http 8080`). But the free tier hands out **random URLs that change on restart** (so GitHub's configured webhook URL goes stale on every laptop reboot/sleep-wake unless you script the webhook-config update), and a stable domain is a **paid** plan. Ops burden: medium-to-annoying. Security: a public URL that anyone can POST to, so webhook-secret verification (§6.2) is mandatory.

**1c. Tailscale Funnel.** If the laptop already runs Tailscale, `tailscale funnel 8080` publishes the local service on a **stable** `https://<node>.<tailnet>.ts.net` URL with auto-provisioned TLS, on ports 443/8443/10000 only, and the laptop's real IP is never exposed (Tailscale's ingress relays front it). ([Tailscale Funnel docs](https://tailscale.com/docs/features/tailscale-funnel)) This is the _best_ of the inbound options: stable URL, free, no separate account if Tailscale is already in use. **But it is still fundamentally inbound** — the funnel only works while the laptop is awake, online, and running `tailscaled` with funnel enabled; a webhook fired during a sleep window hits a dead ingress and is lost (subject only to the 3-day manual redelivery). It also publishes a genuinely public endpoint, so secret verification is still required.

**Verdict on Option 1:** 1c (Funnel) is viable and low-effort _if Tailscale is already deployed_, but none of the inbound variants solve the "events fired while the laptop is unreachable" problem any better than the outbound options do — and they all expose a public endpoint. The tunnel buys latency but not resilience.

### Column 2 — the laptop dials _outbound_ (satisfies the constraint)

#### Option 2: Webhook → tiny cloud relay → laptop over an outbound connection

A minimal always-on hosted receiver accepts GitHub webhooks (it _is_ reachable from GitHub — that's its whole job), validates the secret, and **enqueues** events. The engine holds a single **outbound** connection to the relay (WebSocket, SSE, or long-poll) and drains the queue. The laptop is never a server; it is a client that dials out, so **NAT, dynamic IP, Wi-Fi switching, and firewalls are all irrelevant** — the same reason `git fetch` works from a coffee shop.

This is the option that most precisely satisfies the operator's constraint, and it is a well-trodden pattern:

- **`smee.io`** is literally this, and it is **GitHub's own recommended tool for local webhook development**: a hosted channel receives the webhook, and the `smee` client on the laptop holds an outbound SSE connection and replays events to localhost. Zero-cost, zero-infra to _try_. The catch for production: public smee channels are **unauthenticated and shared-by-URL** (anyone who learns the channel URL sees your events and can inject), and smee.io is a **free community service with no delivery/uptime guarantee** — fine for a spike, not for a 24/7 control loop.
- **Self-hosted relay** removes both smee caveats. Hosting options, cheapest first:
  - **Cloudflare Worker + a queue/Durable Object.** The Worker is the webhook endpoint; a Durable Object or Queue buffers events; the engine holds an outbound WebSocket to the Worker. Effectively free at Boss's volume (well within the Workers free tier), globally reachable, nothing to patch or keep alive. Highest fit, slightly more code to write.
  - **fly.io / a $5 VPS** running a ~200-line service (an HTTP handler that verifies the HMAC and pushes onto an in-memory or Redis queue, plus a WebSocket/SSE endpoint the engine subscribes to). Dead simple, but now you own a box to patch and monitor.

**Note this collapses Option 1c and Option 2 into a spectrum.** `cloudflared` (Cloudflare Tunnel) sits exactly on the seam: the `cloudflared` daemon on the laptop makes an **outbound-only, QUIC** connection to Cloudflare's edge, and Cloudflare — which _is_ reachable from GitHub — proxies the webhook _down that existing outbound tunnel_. ([Cloudflare Tunnel docs](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/)) So `cloudflared` is a "tunnel" (Option 1 ergonomics: point GitHub at a stable `*.trycloudflare.com` or custom-domain URL) that has the **outbound-connection property of Option 2** (no inbound port, survives NAT). It is free, resilient on flaky networks, and requires no relay code. **`cloudflared` is the pragmatic sweet spot** if we want webhooks _fast_ without writing a relay: it gives outbound reachability with near-zero build cost. The only thing it does _not_ give that a custom relay does is **server-side buffering across laptop-offline windows** — if the laptop is asleep, the tunnel is down and Cloudflare returns an error to GitHub (→ 3-day manual redelivery), whereas a Worker+queue relay would hold the event until the laptop reconnects.

**Reachability verdict:** ✅ solved. **Ops:** low (`cloudflared`/`smee`) to medium (self-hosted relay). **Security:** the relay/tunnel endpoint is public, so HMAC secret verification is mandatory; a self-hosted relay can additionally require the engine to authenticate its outbound subscription. **Offline resilience:** only the _buffering_ relay variant (Worker+queue, VPS+Redis) actually holds events across sleep windows — `cloudflared`/`smee` do not.

#### Option 3: No new infra — smarter polling (the do-nothing-radical baseline)

Keep polling, but make it cheap and responsive. Three levers, in decreasing order of payoff for Boss:

- **Conditional requests (ETag / `If-None-Match`).** For every signal reachable over **REST**, store the ETag and send it next time; unchanged resources return `304` and **cost no quota** (§3.2). This turns "re-poll 10 unchanged PRs every minute" into "10 free 304s every minute." The blocker: Boss's probe is currently **GraphQL** (`gh pr view`), which can't do conditional requests. Capturing this win means **moving the PR-state / CI-status reads to REST endpoints that support ETags** (e.g. `GET /repos/{o}/{r}/pulls/{n}`, `GET /repos/{o}/{r}/commits/{sha}/check-runs`), or accepting that the GraphQL path stays quota-charged.
- **GraphQL batching.** Where GraphQL is the right tool, fetch **all** in-review PRs' state+CI+reviews in **one** query per pass (aliased sub-queries or a `search`/`nodes(ids:)` batch) instead of one `gh pr view` per PR. This cuts per-pass calls from _N_ to _1_ and is a strict improvement even without changing the interval — it directly flattens the §2.2 table's linear-in-PRs growth.
- **Adaptive intervals.** Poll fast (say 10 s) only while a PR is in an actively-changing state (CI running, review requested, merge pending), and back off aggressively (minutes) for PRs parked awaiting a human. Boss already has the state to drive this — the candidate query (`pr_flow.rs:394`) knows each PR's status; the loop just needs a per-PR next-poll-time instead of one global 60 s tick.

**How close does this get to "fast and efficient" with zero reachability risk?** Latency floor drops from 60 s to ~10 s for active PRs; steady-state quota on unchanged PRs drops toward zero (conditional 304s) or at least to 1 batched call/pass (GraphQL). It does **not** hit sub-second, and it does **not** gracefully absorb high-fanout comment polling (comments have no cheap aggregate/ETag story). But it is **free, needs no public endpoint, and has literally zero reachability exposure** — and it closes enough of the gap that it should be done _first_ regardless of whether push ever lands. This is the strongest do-now option.

#### Option 4: GitHub Actions as the push channel

A workflow triggered on `pull_request`, `issue_comment`, `pull_request_review`, `check_run`, etc. runs a step that pings the relay (or writes an event to a queue / repository dispatch). It is "push" without a GitHub App: the workflow _is_ the outbound notifier.

- **Reachability:** ✅ — the Actions runner is GitHub-hosted and reaches the relay outbound; the laptop still just drains the relay. So this is really "Option 2's relay, fed by Actions instead of a webhook."
- **Latency:** worse than native webhooks. A workflow has queue + runner-startup overhead of **~5–30 s** before the notify step even runs, versus a webhook's sub-second POST. So Actions-as-notifier is _slower_ than the very thing it's replacing polling for.
- **Per-repo setup:** a workflow file must be committed to **every** repo Boss watches, and kept in sync — versus a webhook/App configured **once** at the org or app level.
- **Cost:** Actions **minutes are metered** (2,000 free min/month on private repos; unlimited on public). A notify job is short, but every event spends minutes; high-fanout comment events could nibble the budget.
- **Verdict:** a **fallback**, not a primary. Its only real edge is that it needs no GitHub App and no webhook config — useful if App creation is blocked. Otherwise it is strictly worse than a native webhook on latency, setup, and metering.

#### Option 5: Hybrid — push fast path + poll backstop (the realistic answer)

Not a distinct transport but the **architecture** that ties them together, and almost certainly what ships. Webhooks/push (via Option 2's outbound relay) are the **fast path**; a **slow reconciliation sweep** (Option 3's cheapened poller) stays as the **backstop**. This is mandatory, not optional, because:

- Webhook delivery is best-effort and GitHub only keeps 3 days of redeliverable history (§3.2). A laptop offline for a weekend **will** miss events past the redelivery window.
- Even the relay's server-side buffer can be evicted, and the relay itself can have downtime.

So the engine must be able to **reconcile from scratch** at any time by polling — and treat every webhook as merely an _early nudge to reconcile that PR now_, never as the sole source of truth. That is precisely Boss's existing stance that **the engine owns reconciliation** (the UI is a thin renderer, not a decision-maker), and it maps directly onto the merge poller's existing `kick` seam (§7). **This is the recommended architecture.**

---

## 5. Cross-cutting: laptop realities and catch-up

The operator's world is a laptop that sleeps, switches networks, and goes offline for hours. What happens to an event fired during an unreachable window, per option, and what catch-up story each needs:

| Option                                            | Event fired while laptop asleep/offline                                                                       | Catch-up story needed                                                                                                                             |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| **1b/1c tunnel** (ngrok/Funnel)                   | **Lost** — tunnel is down, GitHub's POST fails; recoverable only via manual redelivery within 3 days          | Must still run a reconciliation poll on wake; tunnel adds latency only, not resilience                                                            |
| **2 relay, non-buffering** (`cloudflared`/`smee`) | **Lost** at the tunnel, same as above                                                                         | Same — poll on wake                                                                                                                               |
| **2 relay, buffering** (Worker+queue / VPS+Redis) | **Held** in the relay queue; drained when the engine's outbound socket reconnects on wake                     | Relay retention window must exceed expected offline time; still want a poll backstop for events older than that or lost to relay downtime         |
| **3 smarter polling**                             | Nothing to lose — there is no delivery; the next poll after wake simply observes current state                | Inherent: the poll _is_ the catch-up. Just needs a "reconcile everything on wake/boot" pass (the issue-sync reconciler already does this on boot) |
| **4 Actions → relay**                             | Buffered iff the relay buffers (same as Option 2); the Actions run itself succeeds regardless of laptop state | Same as Option 2                                                                                                                                  |

**The through-line:** only a **buffering outbound relay** turns an offline window into "catch up when you reconnect" rather than "lose the event." Everything else — including every tunnel — degrades to "you missed it, hope the next poll catches the resulting state." And because _state-based reconciliation via poll is the only universally reliable catch-up_, the poller can never be removed. This is the concrete, laptop-grounded reason §4 Option 5 (hybrid) is not optional.

A subtlety worth calling out: Boss cares mostly about **state**, not **event history**. Missing the `pull_request.synchronize` event is harmless if the next reconcile sees the new head SHA and correct CI state; the _outcome_ is idempotent. The events Boss would genuinely lose by missing are the **append-only** ones — a **comment** posted and then edited, a review submitted — where the current-state poll can still recover the final text but loses the "a new comment arrived at T" timing. For comments specifically (§6.4), that timing is the trigger, so the buffering relay earns its keep there more than it does for merge/CI state.

---

## 6. Cross-cutting: security, auth, future signals, ops

### 6.1 What surface each option exposes

- **Inbound options (1b/1c):** a genuinely **public HTTP endpoint** that anyone on the internet can POST to. Mitigation is mandatory HMAC secret verification (§6.2) plus rate-limiting the listener. Tailscale Funnel additionally never exposes the laptop's real IP, which is a real plus.
- **Outbound relay (2):** the _relay_ is the public endpoint, not the laptop. The laptop exposes **nothing** inbound. The relay must (a) verify the GitHub secret before enqueuing, and (b) authenticate the engine's outbound subscription so a stranger can't drain your event queue. Blast radius of a compromised relay is limited to _read_ visibility of GitHub metadata events plus the ability to inject fake events — which the engine defends against by treating every event as "reconcile this PR" and then **confirming state via an authenticated API read** before acting (never trusting the webhook payload's claims directly).
- **Smarter polling (3):** exposes **nothing**. No endpoint, no secret, no relay. Lowest security surface of any option, by a wide margin.

### 6.2 Webhook secret verification

GitHub signs each webhook with `X-Hub-Signature-256` (HMAC-SHA256 of the raw body under a shared secret). Any receiver — tunnel-fronted local listener or cloud relay — must compute and constant-time-compare this before trusting a payload. Boss should verify at the _first_ trust boundary (the relay, if there is one) **and** treat the payload as a hint even after verification, re-reading authoritative state via the API. Where the secret lives: for a self-hosted relay, in the relay's env/secret store; the laptop engine never needs the webhook secret at all (it only needs to authenticate _to the relay_).

### 6.3 Auth model — PAT vs GitHub App (worth doing regardless of transport)

Boss's GitHub identity story today is fragmented (documented in the [OAuth scopes investigation](oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md)):

- The **merge poller and issue-sync reconciler** run on the **ambient `gh auth` login** — whatever scopes the user's `gh login` happened to carry, invisible and unenforced from Boss's side.
- A **GitHub App already exists** — the `boss shake` app (`tools/boss/github/src/lib.rs`) signs a JWT with an RSA private key, exchanges it for a short-lived installation token, and files issues + adds them to a Project. Credentials are embedded at build time (`BOSS_SHAKE_*`). This is real, working App-auth machinery **already in the tree.**
- An **OAuth device flow** is being built for issue-sync to get least-privilege without an App.

A GitHub App is the right long-term home for the reconciler's GitHub access, and it is worth doing **independent of** whether we ever adopt webhooks, because it wins on four axes at once:

1. **Webhooks are an App-level feature.** A GitHub App can subscribe to events **once, at the app level**, across every repo it's installed on — no per-repo webhook config. This is strictly better than per-repo webhooks or Option 4's per-repo workflow files.
2. **Fine-grained permissions.** An App requests exactly `pull_requests:read`, `checks:read`, `issues:read`, etc., scoped to selected repos — versus classic PAT/OAuth `repo` scope, which is all-or-nothing across every private repo the user can see (the core finding of the OAuth investigation).
3. **Higher, scalable rate limit.** Installation tokens start at 5,000/hr and **scale** with installation size up to 12,500/hr (15,000 on Enterprise), versus a PAT's flat 5,000/hr (§3.2).
4. **A real secret and identity.** App webhooks come with a managed secret and a stable app identity for audit, rather than a user's personal token.

**Recommendation:** converge the reconciler onto a GitHub App (extending the existing `boss shake` app or a sibling), and if/when webhooks land, subscribe at the app level. The one cost is that App setup is more involved than `gh auth` (private key, installation, permissions) — but Boss has already paid most of that cost once.

### 6.4 Future signals — does the recommended path cover them?

The whole reason to invest here is the _next_ signals, so this must be checked explicitly:

| Signal                  | Webhook event                          | Covered by App-level webhook? | Poll-only cost                           |
| ----------------------- | -------------------------------------- | ----------------------------- | ---------------------------------------- |
| PR merged/closed        | `pull_request`                         | ✅                            | cheap (already done)                     |
| CI status               | `check_run` / `check_suite` / `status` | ✅                            | moderate (rollup per PR)                 |
| Review submitted        | `pull_request_review`                  | ✅                            | moderate                                 |
| **PR review comments**  | `pull_request_review_comment`          | ✅                            | **expensive** — no cheap aggregate query |
| **Issue / PR comments** | `issue_comment`                        | ✅                            | **expensive** — per-issue/PR polling     |

Every future signal Boss has on its roadmap — including the [comment-triggered revisions](../designs/comment-triggered-document-revisions.md) work, whose design already names "deferred GitHub PR-comment triage" as a producer — is a **first-class webhook event type**, and a single App-level subscription covers all of them with one config. The poll-only column shows why: comments are exactly where polling scales worst (no aggregate "any new comments?" query, no useful ETag story), so they are the signal that most _justifies_ the eventual push channel. The recommended staged plan lines up with this: smarter polling handles today's merge/CI/review signals comfortably; the relay+App push channel is what unlocks comment/review-comment signals affordably.

### 6.5 Ops burden for a single-developer 24/7 tool

Ranked by realistic "can one person run this forever without babysitting it":

1. **Smarter polling (3):** nothing new to run. Lowest burden, full stop.
2. **`cloudflared` tunnel (2, non-buffering):** one daemon on the laptop, auto-reconnects, free, resilient on flaky networks. Low burden, but no offline buffering.
3. **Cloudflare Worker + queue relay (2, buffering):** serverless — nothing to patch or keep alive, free at this volume, and it buffers across sleep. Low ongoing burden once written; the cost is the up-front code.
4. **`smee.io` (2):** zero setup, but a free community service with no SLA and an unauthenticated shared channel — fine for a spike, risky for a control loop.
5. **VPS relay (2):** a box to patch, monitor, and pay ~$5/mo for. Highest ongoing burden of the outbound options.
6. **ngrok/Funnel inbound (1):** a daemon on the laptop _and_ a public endpoint to secure, with URL churn (ngrok free) or a Tailscale dependency (Funnel), and no better offline story than polling. Effort without the resilience payoff.

---

## 7. The engine-side event-ingestion boundary (push + poll → one reconciler)

The good news is that Boss's engine is **already shaped for this**. The merge poller does not need to be rewritten to accept push — it needs a second way to fire the pass it already runs.

Today:

```
                 ┌─ 60 s interval ─────────┐
                 │                          ▼
   window focus ─┴─ pr_reconciler_kick ─► run_one_pass ─► probe each in-review PR ─► reconcile
     (engine_meta.rs:374, review.rs:77)     (15 s quiesce)     (gh pr view)         (merge/CI/review)
```

The `kick` (`Arc<Notify>`) is **already an out-of-band ingestion seam** — it's fired today by app-focus and review actions to get a fresh sweep without waiting for the interval. A push channel is just **one more producer of that kick**, ideally a _targeted_ one:

```
   webhook ─► relay queue ─► engine outbound socket ─► kick(pr_url) ─┐
                                                                      ├─► reconcile that PR now
   60 s interval / wake ────────────────────────────────────────────┤   (authoritative API read,
   adaptive per-PR timer ────────────────────────────────────────────┘    never trust payload)
```

Design properties this boundary must hold — all of which match existing Boss stances:

- **One reconciler, many triggers.** Push, timer, wake, and manual kick all funnel into the _same_ `run_one_pass` (or a targeted `reconcile_one(pr_url)` variant worth adding — see follow-ups). The engine owns the reconciliation; the transport only decides _when_.
- **Events are hints, not truth.** A webhook says "PR #123 may have changed" → the engine does an authoritative API read of #123 and acts on _that_. This makes injected/forged/stale events harmless and keeps the push and poll paths from diverging.
- **Idempotent reconciliation.** Because Boss reconciles _state_, replaying the same event twice (redelivery, poll+push overlap) is a no-op. This is what lets push and poll coexist without coordination.
- **The poll never leaves.** It is the catch-up path for everything the push path missed (§5). Its interval can _lengthen_ once push is trusted, but it stays.

Adding a targeted single-PR kick (`kick(pr_url)` rather than "sweep everything") is the one engine change that most improves both the push path (react to exactly the PR that changed) and the smarter-polling path (adaptive per-PR timers). It is noted as a follow-up.

---

## 8. Recommendation — staged, with comparison

**Stage 1 (now): smarter polling.** Restructure the PR reconciler probe to (a) batch all in-review PRs into one GraphQL query per pass instead of one `gh pr view` per PR, (b) use REST + ETag conditional requests for the sub-signals available over REST so unchanged PRs cost no quota, and (c) drive per-PR adaptive intervals off the status Boss already tracks. Zero new infra, zero reachability exposure, and it takes latency from 60 s to ~10 s while flattening the quota curve. Do this regardless of what comes next.

**Stage 2 (when comment/review-comment signals land, or sub-10 s latency is required): outbound relay + App webhooks.** Stand up a buffering cloud relay (Cloudflare Worker + queue is the lowest-ops fit), subscribe to webhooks at the **GitHub App** level (extend the existing `boss shake` app), and have the engine hold one outbound socket to the relay that fires a targeted `kick(pr_url)`. The laptop stays a pure outbound client — the operator's constraint is satisfied by construction. A `cloudflared` tunnel is the acceptable **quick-start** substitute if we want webhooks before writing the relay, trading away offline buffering.

**Always: keep the reconciliation sweep** as the backstop, lengthened once push is trusted, feeding the same reconciler.

**Orthogonal, do alongside Stage 1 or 2: move the reconciler onto a GitHub App** for fine-grained scopes, higher rate limits, and app-level webhook eligibility.

### Comparison against the current-sweeps baseline

| Option                                  | Latency                  | API-quota efficiency                                            | Reachability                              | Offline catch-up                             | Cost            | Complexity                        |
| --------------------------------------- | ------------------------ | --------------------------------------------------------------- | ----------------------------------------- | -------------------------------------------- | --------------- | --------------------------------- |
| **Current sweeps (baseline)**           | 60 s / 120 s             | Re-polls unchanged state; OK at low PR counts, bad for comments | ✅ (outbound polls)                       | ✅ inherent (poll on wake)                   | $0              | Low (exists)                      |
| **1c Tailscale Funnel + webhooks**      | ~1 s                     | Eliminates polling for covered signals                          | ⚠️ inbound; needs Tailscale, laptop awake | ❌ lost during sleep (3-day redelivery only) | $0              | Medium (public endpoint + secret) |
| **2 `cloudflared` + App webhooks**      | ~1 s                     | Eliminates polling for covered signals                          | ✅ outbound tunnel                        | ❌ not buffered (tunnel down when asleep)    | $0              | Medium                            |
| **2 Worker+queue relay + App webhooks** | ~1–3 s                   | Eliminates polling for covered signals                          | ✅ outbound socket                        | ✅ buffered across sleep                     | ~$0 (free tier) | High (write relay)                |
| **3 Smarter polling**                   | ~10 s (active)           | ✅ 304s free + 1 batched call/pass                              | ✅ outbound polls                         | ✅ inherent                                  | $0              | Medium (probe rework)             |
| **4 Actions → relay**                   | ~5–30 s                  | No polling, but Actions minutes metered                         | ✅ outbound (via relay)                   | ⚠️ buffered iff relay buffers                | Actions minutes | High (per-repo workflows)         |
| **5 Hybrid (3 now + 2 later)** ⭐       | ~10 s now → ~1–3 s later | ✅ best of both                                                 | ✅ outbound throughout                    | ✅ relay buffer + poll backstop              | ~$0             | Staged (incremental)              |

The staged hybrid (⭐) is the recommendation: it never depends on the laptop being reachable inbound, delivers a real latency + efficiency win immediately for zero infra, and leaves a clean, already-scaffolded upgrade path to seconds-latency push exactly when the high-fanout comment signals make it worth the relay.

---

## 9. Follow-up code changes (out of scope for this doc — file separately)

This is an investigation; per the worker rules it touches no code. The concrete engine changes the recommendation implies, for the operator to file as their own tasks:

1. **Batch the PR reconciler probe into one GraphQL query per pass** (replace per-PR `gh pr view` with an aliased/`nodes(ids:)` batch). Biggest single efficiency win, no reachability change. (Stage 1)
2. **Add REST + ETag conditional-request paths** for the sub-signals available over REST, so unchanged PRs cost no primary quota. (Stage 1)
3. **Adaptive per-PR poll intervals** driven by task status, replacing the single 60 s global tick. Requires a per-PR next-poll-time and a targeted `reconcile_one(pr_url)`. (Stage 1)
4. **Add a targeted `kick(pr_url)`** to the merge poller alongside the existing broad `kick`, so both push events and adaptive timers can reconcile exactly one PR. (Stage 1/2 enabler)
5. **Migrate the reconciler's GitHub auth to a GitHub App** (extend `boss shake` or a sibling app) for fine-grained scopes, scalable rate limits, and app-level webhook eligibility. (Orthogonal)
6. **Build the outbound relay + engine subscriber** (Cloudflare Worker + queue), with App-level webhook subscription and HMAC verification at the relay, feeding the targeted kick. (Stage 2)
