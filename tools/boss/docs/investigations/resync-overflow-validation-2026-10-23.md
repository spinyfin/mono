# Resync overflow validation, 2026-10-23

Found in post-merge review of [PR #3256](https://github.com/spinyfin/mono/pull/3256). These are live measurements, not counts inferred from a request recorder or the backoff formula.

## Method

Built three macOS app variants with `bazel build //tools/boss/app-macos:Boss`: app sources from `bb542b77d66a` (before #3256), `8b04af4d1bb6` (merged #3256), and this follow-up. The engine/protocol stayed at the current revision to isolate the app change. Extracted each Bazel-produced app archive into a separate local directory and used its bundled engine and CLI. The root `bazel-bin` engine was an older, different configuration, so it was not used for these measurements.

Each run used a fresh engine socket under `/tmp`, `BOSS_EVENTS_SOCKET` unset, `BOSS_ENGINE_AUTOSTART=0` for the app, and an isolated diagnostics directory. The CLI seeded five products with twelve projects each, with automatic design tasks disabled. No workers or product tasks were required. A transparent socket proxy recorded each app request and inbound envelope with monotonic timestamps; `metrics_list_live` snapshots independently counted decoded requests and push enqueue outcomes. The app ran quietly with `--capture-metrics --capture-to`, without showing a window.

Ordinary project edits coalesce by topic and cannot themselves fill 256 queue slots on a five-product board. The forced-overflow producer therefore used real `boss attention create` mutations: eight waves of 600 distinct questions on a project in the selected product, 32 CLI processes at a time, with 8 KiB prompts. The proxy drained 4 KiB every 20 ms during each wave, then resumed normal draining and paused three seconds before the next wave. These non-coalesced `attention_created` pushes exercise the production queue's eviction and resync path. There was no synthetic `resync_required` injection or queue-size change.

The committed [audit driver](resync_traffic_audit.py) reproduces seeding, proxying, mutation workloads, captures, raw traffic, and counter snapshots. Its `refresh` mode uses `--capture-refresh-work`, which calls the actual `ChatViewModel.refreshWork()` action ten seconds into the isolated capture. The other modes are `cold`, `reconnect` (close the proxy connection at ten seconds), `busy` (20 project edits on a three-second cadence), and `overflow`. An optional last argument changes the pause between overflow waves.

```sh
audit_repo="$(pwd)"
bazel build //tools/boss/app-macos:Boss
audit_zip="$(bazel cquery //tools/boss/app-macos:Boss --output=files)"
ditto -x -k "$audit_repo/$audit_zip" "$audit_repo/.codex/audit-app"
timeout 900 python3 "$audit_repo/tools/boss/docs/investigations/resync_traffic_audit.py" \
  overflow "$audit_repo/.codex/audit-app/Boss.app/Contents/MacOS/Boss" overflow
```

Run from the repository root. Outputs are in `.codex/audit-<label>`: `traffic.json`, `metrics-before.json`, `metrics-after.json`, engine/app logs, seed IDs, and `capture.png`. The metric snapshots include CLI and observer traffic; the proxy isolates app traffic. The capture's metrics requests occur after the measurement interval below.

## Forced overflow: observed before/after

The rate interval is exactly `[10, 70)` seconds after app launch, covering the mutation storm and its final refetch. All three runs received 16 resync markers in that interval. A “set” starts with the resync-driven `list_products`; startup is excluded.

| Measurement                                                        | Before #3256 | Merged #3256 | This follow-up |
| ------------------------------------------------------------------ | -----------: | -----------: | -------------: |
| `attentioncreated.degraded` counter delta                          |        2,591 |        2,597 |          2,637 |
| `attentioncreated.enqueued` counter delta                          |        2,209 |        2,203 |          2,163 |
| Actual resync markers/min                                          |           16 |           16 |             16 |
| Refetch sets/min                                                   |           16 |            6 |              6 |
| Resync-derived reads, total                                        |           74 |           48 |             66 |
| Mean reads/set                                                     |        4.625 |            8 |             11 |
| All app requests/min, including periodic polls and control replies |           91 |           60 |             78 |

Before #3256 each marker immediately sent three reads, while the product coalescer shared nine product refreshes across those sixteen bursts. One background refresh joined an already pending poll, yielding 74 observed resync-derived reads, rather than a fixed per-marker count. The merged and corrected runs each sent six complete sets. The corrected set adds CI-remediation, conflict-resolution, and Activity-history reads; it still used fewer resync-derived reads than the pre-change app (66 versus 74). The merged set's smaller count omitted state needed for correctness.

Per-verb app requests in the same 60-second interval:

| Verb                                             | Before | Merged | Follow-up |
| ------------------------------------------------ | -----: | -----: | --------: |
| `list_products`                                  |     16 |      6 |         6 |
| `list_worker_live_states`                        |     16 |      6 |         6 |
| `list_live_status_disabled_slots`                |     16 |      0 |         0 |
| `get_engine_health`                              |      0 |      6 |         6 |
| `git_hub_auth_status`                            |      0 |      6 |         6 |
| `get_engine_pool_config`                         |      0 |      6 |         6 |
| `get_work_tree`                                  |      9 |      6 |         6 |
| `list_deferred_scope_attentions`                 |      9 |      6 |         6 |
| `list_engine_attempts` (includes periodic polls) |     20 |     18 |        24 |
| `list_ci_remediations`                           |      0 |      0 |         6 |
| `list_conflict_resolutions`                      |      0 |      0 |         6 |
| `engine_response` (control replies)              |      5 |      0 |         0 |

The corrected app's set start times were 11.529, 16.030, 20.609, 24.936, 33.993, and 50.106 seconds. Its final marker arrived at 42.622 seconds, followed by the trailing set at 50.106 seconds. The first set's work-tree RPC took 290 ms; all replies for that set arrived within 377 ms of its first request. The slowest tree RPC, after all 4,800 questions existed, took 1.085 s; the complete set took 1.242 s including its product-coalescer delay. These are socket-observed response times, not UI-render completion times.

The registered captures “Before resync debounce: isolated engine counters after eight 600-attention overflow waves” and “Corrected resync recovery: isolated engine counters after eight 600-attention overflow waves” visibly show 4,800 mutations and the corresponding degraded/enqueued counts. Raw counter snapshots also match the app's per-verb proxy totals after excluding CLI and observer calls. Screenshots are local review evidence; no localhost attachment URL is needed to reproduce the run.

## Normal workload peaks and policy margins

Separate runs used the corrected app without throttling. Cold start is measured over `[0,10)` seconds; reconnect and manual Refresh over `[10,20)`; busy CLI over `[10,70)`. Peak windows slide over the recorded timestamps. “Inbound” includes replies and pushes. None of these four runs had a degraded enqueue, a slow-subscriber outcome, or a resync marker.

| Workload                                                              | Requests in interval | Peak requests/150 ms | Peak requests/1 s | Peak inbound/150 ms | Peak inbound/1 s | Slowest correlated reply |
| --------------------------------------------------------------------- | -------------------: | -------------------: | ----------------: | ------------------: | ---------------: | -----------------------: |
| Cold start                                                            |                   21 |                   19 |                20 |                  21 |               22 |                  13.5 ms |
| Reconnect                                                             |                   16 |                   15 |                15 |                  17 |               17 |                   4.5 ms |
| Full-board Refresh action                                             |                    4 |                    3 |                 3 |                   3 |                3 |                  11.7 ms |
| Busy CLI: one project mutation every 3 s, 12-project selected product |                   72 |                    3 |                 4 |                   3 |                5 |                  16.6 ms |

The Refresh action itself sent two reads (`list_products`, `get_work_tree`); two background polls also fell in its ten-second interval. Busy CLI caused exactly 20 tree reads, 20 deferred-scope reads, and 32 background snapshots: 20 mutation-triggered snapshots plus 12 periodic polls. There was no per-project request multiplication. All 20 CLI updates are independently present in `frontend_requests_total.updateworkitem`.

These observations support retaining the existing policy, with the following concrete margins and limits:

- **150 ms window:** the largest normal inbound window held 21 envelopes, versus a 256-entry queue, a 12.2-fold volume margin (235 entries if that entire observed window arrived while draining was stopped). The busy workload peaked at three envelopes in the window, an 85.3-fold volume margin. Seven of the eight corrected overflow marker pairs were separated by less than 150 ms; the eighth was 160 ms apart. The window is deliberately paired with backoff, not claimed to capture every possible overflow in one timer interval. It also shares the existing product-invalidation window, avoiding a second product burst.
- **1 s initial backoff:** the first complete overflow refills took 377 ms in the comparison and 448 ms in the additional ceiling/reset run below, leaving at least 552 ms before another set could fire. Normal replies peaked at 16.6 ms across the four workloads, leaving 983.4 ms. The first gap protects against immediately feeding the same queue while its first refill drains; subsequent gaps grow as the fixture's tree grows.
- **30 s ceiling:** the corrected set costs 11 reads, and the busy CLI's largest observed one-second inbound volume was five envelopes. The ceiling is a freshness bound for a sustained storm, not a claim that normal traffic needs a 30-second delay. In the eight-wave comparison, the final trailing set started 7.484 seconds after the last marker, within that bound. Do not substitute the theoretical ceiling rate for the observed six sets/minute in the comparison table.
- **10 s quiet reset:** the slowest measured complete storm refill was 1.242 s, leaving 8.758 s of quiet-reset margin (8.1 times the refill duration). Normal workloads' slowest reply was 16.6 ms. Thus an ordinary reply or even the large attention-tree refill is not itself long enough to look like a new isolated storm.

The envelope-volume margins describe the measured arrival windows, not a measured queue high-water mark or a guarantee against arbitrary unsolicited traffic. The forced workload intentionally exceeds them and verifies eviction recovery. These are single-host measurements with the specified fixture, not a production latency percentile or a universal throughput limit.

## Live ceiling and quiet-reset probe

Ran the committed driver with `overflow 6 reset`: six-second inter-wave pauses keep the storm active long enough to reach the ceiling, then a ninth wave follows 35 seconds after the eight-wave loop. The engine counted 5,400 attention mutations, 2,957 degraded enqueues, and 2,443 ordinary enqueues. The app received 18 resync markers and sent eight sets, each with 11 reads.

Set starts were 11.649, 19.240, 26.932, 34.374, 42.459, 59.106, 90.108, and 107.925 seconds. The 31.002-second gap from 59.106 to 90.108 demonstrates the capped backoff under real overflow. The configured ceiling is a scheduler delay, not a hard end-to-end deadline: the socket trace includes scheduling and observation delay. Exact gate timing is separately covered by clock-independent unit tests.

After the trailing fire at 90.108 seconds, the isolated ninth wave's markers arrived at 106.829 and 106.903 seconds. Its set started at 107.925 seconds, well before the old capped gate would have allowed it at approximately 120.108 seconds. This observes quiet-reset recovery after 16.721 seconds since the preceding fire; it does not mislabel the 1.096-second wire-observed marker-to-request delay as an exact 150 ms timer. The largest complete refill in this probe was 1.085 seconds, below the comparison run's 1.242-second maximum.

## Local regression validation

`bazel build //tools/boss/app-macos:Boss` and the four normal app test shards passed. No test timeout override was used. `ResyncRefetchTests` now belongs to shard 1; the gate tests retain explicit-clock timing coverage while the model tests use shorter waits. Regression cases cover recovery of lost CI clear/exhaustion pushes, both tree/list reply orders, conflict-success recovery, the complete refetch request set, and reconnect retaining the reads owed by a canceled resync. `checkleft run` checks the changed files without bypasses.
