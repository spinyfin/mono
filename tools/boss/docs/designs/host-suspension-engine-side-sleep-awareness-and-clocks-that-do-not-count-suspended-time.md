# Host suspension: the engine measures suspended time from two OS clocks, and every horizon subtracts it

- **Status:** proposed (design only; no code in this PR)
- **Date:** 2026-09-11
- **Provenance:** design run for the project "Host suspension: engine-side sleep awareness and clocks that do not count suspended time"; the measured evidence and prior-art table in the project description are the seed and are not repeated in full here.
- **Related:** `mono#803`, `mono#1824`, `mono#1836` (the app-side `kick()` path), `mono#1858`, `mono#2050`, `mono#2847` (`sleep_assertion.rs`); postmortem `incident-004` (its "zero-record hours are machine sleep" inference); `docs/worker-liveness-contract.md`; `docs/attention-lifecycle.md`.
- **Code this doc reasons about:** `engine/utils/src/epoch_time.rs`, `engine/core/src/{stale_worker_sweep,orphan_sweep,spawn_ack_sweep,run_done_backstop,build_wait_tracker,cube_lease_heartbeat}.rs`, `engine/core/src/coordinator/scheduler.rs`, `engine/core/src/completion/{stop,metadata_gate,execution_started,nudge}.rs`, `lib/rust/git_utils/src/gh_cli.rs`, `engine/transient-error/src/lib.rs`, `http_retry/src/lib.rs`, `engine/event-bus/src/event.rs`.

## Problem

This machine is suspended most of wall clock, and the launchd engine has no sleep awareness. The project seed measured ~80% of wall clock spent asleep; independently, the clock pair on this host on 2026-09-11 showed 1,041,209 s suspended of 1,612,908 s since boot (64.6%). Nothing in `tools/boss/` reads `CLOCK_MONOTONIC`, `CLOCK_UPTIME_RAW`, or an IOKit power notification (confirmed by pickaxe; see Findings). Every horizon that answers "how long has this worker been idle / waiting / spawned" currently subtracts two wall-clock stamps, so a lid-close sleep of a few hours is counted as lived-through working time.

Two incident shapes cluster in those sleep windows (the seed's contrast is p = 2.65e-15; see Risks for the detection-timing caveat):

1. **Horizon false-positives.** A healthy `Working` slot with a recent checkpoint is auto-reaped after 7200 s of wall clock; the just-in-time recheck reads the same wall clock and confirms the false verdict. The same inflation probes idle workers via the run-done backstop, expires build-wait and background-children nudge suppression, and emits `scheduler heartbeat: kick/drain handoff may have dropped a wakeup` for every ready row on wake. Tokio timers freeze during sleep (they are `Instant`-based), so the sweeps do not catch up — they resume and then judge the wall-clock gap.
2. **SHA-delta wedge.** Acceptance-side `gh` fetches of the PR head fail with `error connecting to api.github.com` while the host is asleep (the network is gone). The failure is not classified transient, is folded into `ShaDeltaGateOutcome::Inapplicable`, and the Stop arm parks the execution in `AwaitingInput` waiting for a next Stop that an idle worker will never produce.

Five prior attempts were app-side (`kick()` on wake, App Nap opt-out, idle-sleep assertion, spawn-failure NACK, `ConnectionRefused` transient marker). The launchd engine gets none of those signals when the app is not running. This design puts detection in the engine, measures horizons in awake time, and retries the three acceptance-side SHA-delta fetches.

The contested bet, stated up front: **the engine does not need a power-management framework to know it was suspended.** macOS exposes two monotonic clocks, one that stops during sleep (`CLOCK_UPTIME_RAW`, which is what Rust's `Instant` and therefore every tokio timer already reads) and one that does not (`CLOCK_MONOTONIC`); their difference is the total time the host has been suspended since boot, and sampling that difference is a complete, testable, app-independent suspend/resume detector. Everything else in this design follows from treating "time elapsed" as "awake time elapsed", computed by subtracting recorded suspensions from wall-clock stamps rather than by rewriting the stamps.

## TL;DR

Three ordered pieces. (1) A new `boss-host-suspension` crate samples the clock pair, keeps a persisted ledger of suspension intervals, exposes `HostClock` and `awake_elapsed_secs(since, now)`, and invokes an injected resume callback after each persisted interval, with no app involvement. `engine/core` maps that callback onto `EventKind::HostResumed` on the engine event bus; the crate never imports the bus. (2) The seven horizons split cleanly: the three in-memory trackers (`BuildWaitTracker` for the run-done backstop, build-wait nudge suppression, and background-children nudge suppression) consume `HostClock::awake_secs()` from the crate and do not use the ledger or the resume event; the four persisted-stamp horizons (stale-worker, orphan, spawn-ack, scheduler-heartbeat) subtract ledger time and depend on the ledger but not on the resume event. (3) The SHA-delta gate gets a bounded retry using the existing `boss-http-retry` policy math and a `gh`-stderr transient classifier, on the three acceptance-side `CommandBranchVerifier` fetches. Nothing pins the machine awake, nothing raises a threshold, and no check is silenced.

## Goals

- The launchd engine learns, on its own, that the host suspended and resumed, and by how much, with no app running. Detection must cover lid-close clamshell sleep and the DarkWake slices inside it, because that is where the measured losses cluster.
- Every horizon in the table below measures elapsed working time. A healthy worker must never cross a reap, probe, or spawn-timeout horizon because the lid was closed.
- The spurious `scheduler heartbeat: kick/drain handoff may have dropped a wakeup` warnings stop by becoming true measurements, not by being muted.
- Acceptance-side SHA-delta fetches retry transient network failures with bounded backoff. The stranded `AwaitingInput` defect is owned by [mono#2931](https://github.com/spinyfin/mono/pull/2931), which deletes that Stop branch and supplies a universal idle backstop.
- The engine-side resume signal drives the existing `ExecutionCoordinator::kick()` so `mono#1836`'s recovery path works without the app, closing the question `mono#2847` left open: reaping and redispatch do need explicit wake-path handling.
- The suspend/resume behaviour is assertable without a physical lid: every consumer takes an injectable clock, and a manual real-sleep procedure is written down and executed once before the project is called done.

## Non-goals

- **Preventing sleep.** No `caffeinate -d`, no display assertion, no change to `sleep_assertion.rs`'s `-i` idle-only assertion, no change to the App Nap token. The operator closes the lid on purpose.
- **Redefining the completion contract.** [mono#2931](https://github.com/spinyfin/mono/pull/2931) owns terminal-at-submit declarations and deletion of the old completion branches. This design adds no retry that re-enters completion; bounded fetch retries remain acceptance-side checks, outside the terminal call stack.
- **Retuning any horizon constant.** The 20/40 min, 1800 s, 7200 s, 90 s, 60/300 s, 15 s and 45 min values are untouched. This design changes what the numbers are measured against, not the numbers.
- **Re-doing the five landed pieces.** The app-side wake observers, the spawn-failure NACK, the `ConnectionRefused` transient marker, App Nap opt-out and the idle-sleep assertion all stay as they are.
- **Sleep awareness on remote dispatch hosts.** The ledger is per engine host. Remote workers reached over ssh are out of scope; their clocks are a different problem.
- **A pre-sleep hook.** Nothing in this design needs to know sleep is _about_ to happen; see the IOKit alternative below for why that capability is not bought.
- **Making cube's 24 h lease TTL sleep-aware.** Cube's TTL sweep is wall-clock and lives in a different tool. This design adds one engine-side mitigation (a heartbeat pass on resume) and records the residual risk; changing cube is not proposed.

## Findings the design rests on

These are checked facts, not assumptions. Where a decision was never made, that is said.

### The two clocks, verified on this host

Rust's standard library on Apple targets implements `Instant` with `clock_gettime(CLOCK_UPTIME_RAW)` (`library/std/src/sys/pal/unix/time.rs`, `CLOCK_ID` under `target_vendor = "apple"`, checked against the 1.93 toolchain source; the Bazel toolchain is 1.95 and the choice has been stable since Rust 1.71). Apple's own comment there, and the `clock_gettime(3)` man page, say `CLOCK_UPTIME_RAW` does not increment while the system is asleep and `CLOCK_MONOTONIC` does. Tokio's timers are built on `std::time::Instant`. So every `tokio::time::sleep`-driven sweep loop in the engine is frozen for the duration of a suspension and resumes with no catch-up burst, exactly as observed (the stale-worker sweep ran 9 times across 4.59 h of sleep, the orphan sweep went 2 h 43 m without a pass).

Measured on this machine on 2026-09-11 05:11 CDT, booted 2026-08-23 13:09:42 CDT:

| clock              | seconds   | meaning                                                   |
| ------------------ | --------- | --------------------------------------------------------- |
| `CLOCK_MONOTONIC`  | 1,612,908 | matches wall time since boot (18 d 16 h)                  |
| `CLOCK_UPTIME_RAW` | 571,699   | awake time since boot (6.6 d)                             |
| difference         | 1,041,209 | suspended time since boot (12.05 d, 64.6 % of wall clock) |

`pmset -g log` on the same host holds 755 `Entering Sleep` records since boot. The difference of the two clocks is therefore a free, always-available, unprivileged "total time suspended" counter. Nothing in `tools/boss/` reads it today; the pickaxe in the project description (zero commits ever mentioning `CLOCK_MONOTONIC`, `mach_continuous_time`, `IORegisterForSystemPower`) is confirmed. The only place the engine reasons about sleep at all is `automation_scheduler.rs`'s catch-up window for missed cron occurrences, which is a policy for _what to do after_ a gap, not a way to know a gap was sleep.

### Which horizons read which clock

| Mechanism                                             | file                                             | Horizon                        | Loop clock               | Predicate stamp                                                      | Stamp lives in |
| ----------------------------------------------------- | ------------------------------------------------ | ------------------------------ | ------------------------ | -------------------------------------------------------------------- | -------------- |
| run_done backstop                                     | `run_done_backstop.rs`, via `BuildWaitTracker`   | 20 min / 40 min                | Stop events (not a loop) | `first_seen_epoch` = wall `now_epoch_secs()` at a Stop               | memory         |
| build-wait nudge suppression                          | `completion/nudge.rs`, via `BuildWaitTracker`    | 45 min                         | Stop events              | same tracker, wall                                                   | memory         |
| background-children nudge suppression                 | `completion/nudge.rs`, second `BuildWaitTracker` | configured                     | Stop events              | same tracker, wall                                                   | memory         |
| stale-worker sweep                                    | `stale_worker_sweep.rs`                          | 1800 s stale, 7200 s auto-reap | tokio (frozen)           | `progress_at` / `started_at`, ISO-8601 wall, stamped at hook arrival | DB + registry  |
| orphan sweep                                          | `orphan_sweep.rs`                                | 90 s min age                   | tokio (frozen)           | `tasks.updated_at`, wall                                             | DB             |
| spawn-ack sweep                                       | `spawn_ack_sweep.rs`                             | 60 s / 300 s                   | tokio (frozen)           | `started_at`, wall                                                   | DB             |
| scheduler heartbeat                                   | `coordinator/scheduler.rs`                       | 15 s                           | tokio (frozen)           | `work_runs.created_at`, wall                                         | DB             |
| nudge breaker (already Instant; out of scope)         | `nudge_breaker.rs`                               | debounce                       | Instant (sleep-immune)   | Instant                                                              | memory         |
| merge-poller schedule (already Instant; out of scope) | `merge_poller/schedule.rs`                       | adaptive poll                  | Instant (sleep-immune)   | Instant                                                              | memory         |

The row this design is really about is the stale-worker auto-reap: a `Working` slot with an idle tool condition and a checkpoint more than 7200 s of wall clock old is re-verified (fresh tmux inspect, fresh checkpoint read) and then destroyed. Both re-verifications also read wall clock, so after a two-hour-plus sleep they _confirm_ the false verdict rather than catching it. The comment on `DEFAULT_AUTO_REAP_THRESHOLD_SECS` says two hours "exists for the case nobody [notices]"; it never considered that the two hours might not have been lived through. That is not a lost rationale, it is a decision that was never made, and this design makes it.

The two Instant-based rows are listed to show the codebase has both conventions with no rule choosing between them; they are out of scope because they are already sleep-immune. `epoch_time.rs` documents itself as "the single source of truth for reading the current wall-clock time" and is correct about that: the defect is not in the helper but in horizons that use a wall-clock reading to answer an awake-time question.

### The SHA-delta gate path, as it actually runs

- `lib/rust/git_utils/src/gh_cli.rs::run_gh` is a single `gh` spawn with no retry and no timeout of its own. `fetch_pr_head_oid` is called from three completion sites: the Stop-boundary gate (`metadata_gate.rs`), the dispatch-time `pr_head_before` snapshot (`execution_started.rs`), and the revision `revision_stop_contributed_head` stamp (`stop.rs`). All three log a warning and return on error.
- `metadata_gate.rs` folds "the fetch failed" and "there was never a baseline" into one `ShaDeltaGateOutcome::Inapplicable`. `stop.rs`'s `Inapplicable` arm then runs the satisfied-deliverable gate (PR health), and if that is not satisfied returns `StopOutcome::AwaitingInput` with the log line "will retry on the next Stop". For an idle worker there is no next Stop. That is the wedge.
- The `TRANSIENT_MARKERS` list that `mono#1858` extended lives in `engine/transient-error` and classifies **Claude API error text extracted from worker transcripts** for `transient_recovery`. It is never consulted on the `gh` path. Separately, the text seen in both incidents, `error connecting to api.github.com`, matches none of its markers anyway (`connection error` and `unable to connect` are present; `error connecting` is not). So the answer to the project's question is: not classified transient today, and the classification is not reached on this path either.
- `boss_github::gh_runner` already has the stderr-parsing primitive (`parse_http_status_from_stderr`) and two per-module `classify_failure` functions (`trees.rs`, `contents.rs`) that bucket `gh` failures into rate-limited / not-authorized / not-found / unreachable. There is no shared "is this `gh` failure transient" function, and `git_utils::gh_cli::run_gh` and `boss_github::gh_runner::run_gh` are byte-for-byte the same helper in two crates. This design reuses the `boss_github` primitives rather than adding a third classifier, and does not propose the dedup as a task because the change does not need it.
- `boss-http-retry` provides `RetryPolicy` and `backoff_delay`/`jitter` as pure policy math with no transport coupling; `app/design_docs.rs` already uses `RetryPolicy::new(3, 2 s, 32 s)` for a non-HTTP caller. That is the retry primitive to reuse.

### One adjacent wall-clock horizon outside the seven

`cube_lease_heartbeat.rs` refreshes each live worker's cube lease every 300 s against cube's 86,400 s TTL, and after three consecutive heartbeat failures auto-reaps the execution. Cube's expiry is `lease_expires_at_epoch_s`, wall clock. The heartbeat loop is a tokio timer, so it is frozen during sleep, and the longest observed suspension in the seed data is 25.05 h, longer than the TTL. A lease can therefore expire in cube while its worker is frozen but alive, which is precisely the "phantom-free workspace" failure that module was written to prevent. This design adds one cheap mitigation (an immediate heartbeat pass on resume) and lists the residual as an open question; it does not change cube.

## Alternatives considered

### A. `IORegisterForSystemPower` / `IOPMConnection` from the daemon

Register with IOKit from a dedicated thread running a `CFRunLoop`, receive `kIOMessageSystemWillSleep`, `kIOMessageSystemWillPowerOn`, `kIOMessageSystemHasPoweredOn`, and acknowledge each with `IOAllowPowerChange`. This is the canonical daemon-side mechanism and gives a _pre-sleep_ edge as well as the resume edge.

Not chosen for v1. It is checkable why: (1) the pre-sleep edge buys nothing this design needs, because every consumer is written to survive suspension rather than prepare for it; (2) it requires linking the IOKit framework and hand-declared FFI (the only precedent in the repo is `keychain`'s `Security` framework link, so this is allowed but not free), a run-loop thread outside tokio, and correct acknowledgement semantics, none of which is testable in the hermetic sandbox; (3) it still does not give "time suspended" directly, so the clock pair is needed anyway; (4) a callback API needs a trait seam to fake in tests, while a clock pair _is_ the seam. The rejection does not disqualify `keychain`'s framework link, which exists because there is no non-framework way to reach the keychain; here there is. If a later design needs the pre-sleep edge (for example to defer a `gh` call it knows will fail), IOKit is the upgrade path and the `HostResumed` event and ledger are unchanged by it.

### B. Parse `pmset -g log` periodically

Shell out to `pmset -g log`, parse the `Entering Sleep` / `Wake` / `DarkWake` records, and derive intervals. Rejected as the primary mechanism: it is text-parsing a diagnostic tool's output, has multi-second latency, and gives no in-process signal. It is the right tool for one thing the clock pair cannot do, which is learning about sleeps that happened while the engine itself was down; that is carried as a deferred entry.

### C. Reset horizons on resume instead of measuring awake time

On a resume signal, `forget()` every in-memory tracker and re-stamp `progress_at`, `started_at` and `updated_at` to now. Rejected: (1) it rewrites observations. `progress_at` is driver-originated evidence, and the liveness contract's first rule is never to make an irreversible decision from derived bookkeeping; forging the evidence is worse than deriving from it. (2) It does not survive DarkWake fragmentation: 51 slices in one night means 51 resets, and a genuinely wedged worker is never reaped while the lid is closed, which is a silent escape hatch. (3) It requires touching every store and every stamp rather than one comparison per site. (4) It couples piece 2 to the resume _event_, when piece 2 only needs the _ledger_.

### D. Store a second "suspended-total-at-write" column beside every stamp

Awake age of a stamp = (wall now − stamp) − (suspended-total now − suspended-total at write). Works with no ledger and no event. Rejected because it needs a schema change per stamped column (`progress_at`, `started_at`, `created_at`, `updated_at`, and the in-memory registries), and because `CLOCK_MONOTONIC − CLOCK_UPTIME_RAW` resets at boot, so every such column would also need a boot identifier. The ledger is one table and touches no existing column.

### E. Widen horizons or add a post-resume grace window

Raise 7200 s, or skip the stale-worker verdict for N minutes after a resume. Both are the escape hatches the project forbids, and both are wrong on their own terms: a widened horizon is still crossed by a long enough sleep, and a grace window is a second wall-clock horizon with the same defect.

## Chosen approach

### Piece 1: `boss-host-suspension` crate, the ledger, and `Event::HostResumed`

A new leaf crate at `tools/boss/engine/host-suspension` (per the crates-over-modules convention; it has its own vocabulary, its own tests, and three consumers: the in-memory `BuildWaitTracker` callers, the persisted-stamp sweep sites, and the resume-consumer subscriber in `app/server.rs`). It depends on `libc` (the clock pair) and `tokio` (the sampler loop). It never depends on the engine or on `engine/event-bus`. `engine/core` depends on it.

**`HostClock` trait.** Three readings: `wall_epoch_secs()`, `awake_secs()` (`CLOCK_UPTIME_RAW` on Apple, `CLOCK_MONOTONIC` on Linux), and `since_boot_secs()` (`CLOCK_MONOTONIC` on Apple, `CLOCK_BOOTTIME` on Linux). `suspended_total_secs() = since_boot_secs() − awake_secs()`. The production impl is a few `libc::clock_gettime` calls. `FakeHostClock` lets a test advance awake time and asleep time independently, which is the whole point: a test can say "five hours passed, none of it awake".

**`SuspensionLedger`.** An ordered list of closed intervals `(slept_at_epoch, woke_at_epoch)` on the engine host, in memory, with a `LedgerStore` trait for persistence. `engine/core` implements the store as a new `host_suspensions` table (migration in `work/`), loaded at boot. The single query every consumer uses:

`awake_elapsed_secs(since_epoch, now_epoch) = (now − since) − Σ overlap(interval, [since, now])`

It is a pure function over the ledger and is the load-bearing invariant of piece 2: **a horizon compares awake-elapsed time, never wall-elapsed time, against its threshold.** That is stated at the level of the comparison, not the clock, because a site could switch clocks and still compute the wrong quantity (for instance by using `Instant` for a stamp that must survive an engine restart).

**Sampler.** A tokio loop with a 5 s cadence reads `suspended_total_secs()` and compares with the previous reading. Growth `Δ ≥ 1 s` means a suspension of length `Δ` ended within the last tick: append `(wall_now − Δ, wall_now)` to the ledger, persist it, log one `info` line with the interval and the running total, then invoke an injected `on_resumed` callback with a crate-local `HostResumed { suspended_secs: Δ, resumed_at_epoch: wall_now }` payload. The crate does not import `engine/event-bus`; `engine/core` supplies the callback that publishes `EventKind::HostResumed` on the bus. The cadence is `Instant`-based, so it ticks during DarkWake slices and records them individually as they happen; several slices between two ticks collapse into one interval whose _length_ is exact and whose _placement_ is off by at most one tick, which is harmless because horizons only consume totals over a range. At boot the sampler seeds `previous = suspended_total_secs()` and records nothing, because sleep before the engine started is not the engine's to measure (see the deferred backfill entry).

**Ordering invariant.** The ledger is appended and persisted _before_ the injected callback runs. Any consumer that runs on resume therefore already sees the suspension when it computes awake-elapsed time. Without this a resume-triggered sweep could reap on the stale wall-clock age one last time.

**Event.** `EventKind::HostResumed` / topic `host_resumed` is added to `engine/event-bus` alongside the existing kinds, following the event-bus design's "add a variant, subscribe by kind" pattern. Naming is deliberately `Host`+`Resumed` rather than reusing `HostDisabled`'s "host" sense (a dispatch host record); the doc comment on the variant says so. The variant lives in `engine/event-bus`; the crate's `HostResumed` payload is a separate, libc-and-tokio-only type that `engine/core` translates at the callback.

**Observability.** The `info` line per interval, a `warn` when the ledger store fails to persist, and the `host_suspensions` table itself are the forensic surfaces. `bossctl doctor` gains nothing in v1.

### Piece 2: horizons measure awake time

The answer to "does piece 2 depend on piece 1" is: the in-memory half depends on `HostClock` from the crate (not the ledger or the event), and the persisted-stamp half depends on the ledger but not on the event. `HostClock` stays in `boss-host-suspension`; it is not extracted to a second crate. The in-memory work therefore cannot merge before the crate lands `HostClock`, but it does not wait on the ledger, the sampler, or the resume-consumer subscriber.

**In-memory trackers (depend on `HostClock`, not the ledger or the event).** `BuildWaitTracker` is in-memory and resets on restart, so an engine-uptime clock is exactly right for it. Its `record()`/`decide()` API keeps `now: i64` seconds so existing tests stay meaningful, but production callers pass `awake_secs()` from a `HostClock` handle instead of `now_epoch_secs()`. That single change fixes the run_done backstop (20/40 min), the build-wait suppression (45 min), and the background-children suppression, because all three go through that tracker. The `waited_secs` they log becomes awake seconds, which is what a human reading the log wanted anyway.

**Persisted-stamp horizons (depend on the ledger).** Each site replaces `now_epoch_secs − stamp` with `awake_elapsed_secs(stamp, now_epoch_secs)`:

- `classify_semantic_staleness` takes the ledger (or a `&dyn AwakeElapsed`) and compares `awake_elapsed_secs(effective_at, now)` against `stale_threshold_secs`. Both the first classification and `attempt_auto_reap`'s just-in-time recheck go through it, so the recheck can no longer confirm a sleep-induced verdict. The "no checkpoint at all" branch (`started_at` fallback) gets the same treatment.
- `orphan_sweep`'s `list_orphan_active_candidates(min_age_secs)` currently pushes the age filter into SQL. It becomes a candidate list with `updated_at` returned, filtered in Rust by awake age; the churn-guard window stays wall clock on purpose, because "three terminal executions in an hour" is a rate statement about the human's hour, not the worker's.
- `spawn_ack_sweep`'s `grace_cutoff` and `driver_start_grace_secs` cutoffs become awake-age comparisons per slot. This also stops `SpawnHealthTracker` from being fed a sleep-induced failure, which matters because that breaker is cross-item.
- `stranded_ready_executions` computes `age_ms` from awake-elapsed seconds. The `warn!` text is unchanged and now fires only when a ready row has genuinely waited longer than the heartbeat interval while the engine was awake. The unconditional `note_dispatch_ready()` re-kick stays, so nothing that used to be recovered by the heartbeat is lost.

The loops themselves stay on tokio timers. Running less often while suspended is correct once the predicates are honest: nothing needs to happen to a frozen worker.

**Tests that pin the old premise are swept in the same PRs.** Every existing sweep test that constructs "a stamp N seconds in the past" and expects a verdict is stating a premise about wall clock. Each such test is re-expressed as "N awake seconds" with an empty ledger (same verdict), and a sibling test adds a five-hour suspension between stamp and now and asserts the verdict does not flip. Leaving the old tests untouched would turn the superseded premise into a defended invariant.

### Piece 3: SHA-delta gate bounded retry

**Bounded retry (no dependency on piece 1).** `CommandBranchVerifier` in `completion.rs` wraps `fetch_pr_head_oid`, `fetch_pr_head_oid_fresh` and `fetch_pr_head_ref` in a retry driven by `boss_http_retry::RetryPolicy::new(3, 2 s, 4 s)` with `jitter`, with 2 s / 4 s bounded backoff between the three attempts. A retry is attempted only when a new `boss_github::gh_runner::classify_gh_failure(stderr) -> GhFailureClass { Transient, RateLimited, Permanent }` says the failure is transient: connect failures (`error connecting to`, `dial tcp`, `connection refused`, `TLS handshake timeout`, `i/o timeout`, `no such host`), HTTP 5xx, and empty stderr with non-zero exit. Rate-limited is _not_ retried here (the merge poller's budget owns that), and 4xx is permanent. The two existing per-module `classify_failure` functions in `boss_github` are pointed at the shared classifier's transport bucket so there is one list to maintain. The backoff sleep is a tokio sleep, so suspended time does not advance it. These fetches survive as acceptance-side checks under mono#2931, which explicitly excludes repairing the SHA-delta probe retry policy from its scope.

The same wrapper covers the dispatch-time `pr_head_before` snapshot in `execution_started.rs`, which closes cause (b) of the `Inapplicable` arm (no baseline, ever) for the transient case.

**Enum ownership.** This task does not split `ShaDeltaGateOutcome::Inapplicable` into fetch-failure and no-baseline reasons. The enum and its consumers live in the `completion/metadata_gate.rs` code mono#2931 deletes; that project owns any distinction still needed after deletion.

**Former Piece 3b removed: completion owns the stranded execution.** The real defect is an execution stranded in `AwaitingInput` waiting for a Stop that no actor is obliged to cause. [mono#2931](https://github.com/spinyfin/mono/pull/2931) owns that defect: it deletes the Stop branch and gives every live state an exit edge through a universal idle backstop. It deletes the downstream completion arms as well; there is no re-enterable evaluation path to retry. Its transport trap requires zero network-capable invocations on the terminal call stack. No resume-triggered or timer-triggered completion retry is scheduled here.

### Resume consumers (depend on piece 1)

One subscriber in `app/server.rs` boot wiring handles `HostResumed`:

1. `execution_coordinator.kick()`. This is the engine-side twin of `handle_spawn_capability_restored` and is what makes `mono#1836`'s recovery work with no app connected. Kicks are idempotent latches, so 51 DarkWake resumes in a night cost nothing.
2. One immediate `cube_lease_heartbeat::run_one_pass`, because the longest observed suspension exceeded cube's TTL. Whether this actually beats cube's own TTL sweep is an open question below; it is cheap and cannot make things worse.

Deliberately **not** on resume: extra passes of the stale-worker, orphan, dead-pid or spawn-ack sweeps. With honest predicates their verdicts are correct whenever they next run on their own cadence, and adding resume-driven passes would be the first step back toward "act on wake" logic that DarkWake fragmentation would multiply.

### The seven horizons across a five-hour suspension, before and after

Scenario: lid closed for 5 h with no network in DarkWake; a worker was one minute past its last hook event, `Working`, tool condition idle, when the lid closed.

| Horizon                                | Before this design                                                                                                                                                                                                                                                                                                      | After this design                                                                                                                                |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| run_done backstop 20/40 min            | First Stop after wake sees `waited = 5 h` ⇒ `Ask`; a worker merely between turns is probed, the breaker is charged; repeated sleep-inflated probes lead to abandonment and worker teardown, with its slot and lease released.                                                                                           | `waited` counts awake seconds ⇒ `HoldingWithinHorizon`; the probe fires only after 20/40 awake minutes of silence.                               |
| stale-worker 1800 s / 7200 s           | First pass after wake (or a DarkWake slice past the two-hour mark) classifies `Stale`, the just-in-time recheck confirms it from the same wall clock, and `execute_auto_reap` destroys a healthy worker: backup, `orphaned`, process tree killed, slot and lease released. If a tool was in flight the worker survives. | Awake age is one minute ⇒ `Healthy`. A worker that is _genuinely_ wedged still crosses 1800 s and then 7200 s of awake time and is still reaped. |
| orphan sweep 90 s                      | An item whose worker had not yet committed `run_started` is ≥ 90 s old on wake; a redispatch is attempted and only the durable-pid guard stands between it and a duplicate worker.                                                                                                                                      | Awake age is seconds ⇒ not a candidate until 90 awake seconds elapse. A genuinely dead worker's item is still redispatched.                      |
| spawn-ack 60 s / 300 s                 | A spawn 10 s old at sleep entry is 5 h old on wake with no pid and no hook ⇒ reaped as a spawn-ack timeout and counted as a failure by the cross-item `SpawnHealthTracker`.                                                                                                                                             | Awake age is 10 s ⇒ left alone; the grace window keeps accruing only while awake.                                                                |
| scheduler heartbeat 15 s               | Every `ready` row is > 15 s old on wake ⇒ one `warn` per row per beat until dispatched (53 observed), each misattributed to a dropped wakeup.                                                                                                                                                                           | Warns only for a row that waited > 15 awake seconds. The unconditional re-kick still runs every beat.                                            |
| build-wait nudge 45 min                | First post-wake Stop narrating a build wait sees `waited = 5 h` ⇒ `Expired` ⇒ the normal nudge/park flow interrupts a legitimately waiting worker whose bazel build was also frozen.                                                                                                                                    | 45 minutes of awake waiting before suppression ends.                                                                                             |
| background-children nudge (configured) | Same tracker type and same wall-clock stamp as build-wait, second `BuildWaitTracker` instance in `completion/nudge.rs`: first post-wake Stop sees `waited = 5 h` ⇒ suppression expired ⇒ the nudge/park flow interrupts a worker whose background children were also frozen.                                            | Configured awake waiting before suppression ends — identical clock swap to the build-wait row, distinct production caller.                       |

The backstop consequence follows the live chain traced by mono#2931: `completion/metadata_gate.rs:529-620` → `nudge_or_park` → `completion/nudge.rs:650-675` → abandonment and teardown. The parking comment at `run_done_backstop.rs:64` is stale and marked for deletion there. Awake-time horizons protect against losing a worker and releasing its resources, strengthening the case beyond avoiding an unnecessary probe.

Not one of the seven but changed by the same table: the cube lease heartbeat still cannot beat a 25 h sleep against a 24 h TTL on its own; see risks.

## Verification

The rule this section follows: a hand-built reproduction can only find the bugs its author already believes in, so the automated layer asserts the mechanism and the manual layer runs the genuine end-to-end path once, and both are required.

**Unit, hermetic, in `bazel test`.**

- `boss-host-suspension`: `FakeHostClock` drives the sampler; assertions cover one interval, several intervals between ticks (length exact, placement within a tick), a sub-threshold jitter that must record nothing, persistence round-trip through a fake `LedgerStore`, and that the store write completes before the injected `on_resumed` callback runs (the fake store and fake callback record ordering). `engine/core`'s wiring test asserts that callback publishes `EventKind::HostResumed`.
- `awake_elapsed_secs`: intervals fully inside, straddling either end, fully outside, and adjacent; the property `awake_elapsed_secs ≤ wall elapsed` is asserted over random ledgers.
- Each horizon site: the existing "stamp N seconds ago ⇒ verdict" tests re-expressed against an empty ledger, plus one test per site with a five-hour interval between stamp and now asserting the verdict does not flip, plus one with five awake hours asserting it does. For the stale-worker sweep the auto-reap recheck is tested with the interval inserted _between_ the first classification and the recheck, which is the DarkWake-slice shape.
- Gate retry: exercise the wrapper on all three `CommandBranchVerifier` fetches with transient failures followed by success; assert attempt counts, exhaustion at three attempts, no retry on permanent or rate-limited errors, and classifier table coverage including the exact incident stderr.

**Automated integration proxy.** One engine-level test wires the real sampler with a `FakeHostClock`, the real ledger and DB migration, and the real stale-worker and scheduler-heartbeat passes over a fixture DB, then advances the fake clock through a five-hour suspension and asserts no reap, no stranded warning, and a `HostResumed` on the bus. This is the genuine engine path with only the OS clock faked.

**Real sleep is not possible in CI.** Buildkite mac agents cannot be suspended and the hermetic test wrapper forbids it anyway. This is stated plainly so nobody reads the proxy as the end-to-end test.

**Manual procedure, executed once and recorded (its own task).** On a dev machine, with an isolated engine (`--socket-path /tmp/boss-sleep-<uuid>.sock`, `BOSS_EVENTS_SOCKET` unset), start a worker execution that reaches a Stop and idles, note its `progress_at`, then `sudo pmset relative wake 240 && sudo pmset sleepnow` (the `-i` idle-only assertion from `sleep_assertion.rs` does not block forced sleep, by its own documentation, so this is a faithful clamshell stand-in). After the scheduled wake, assert from the engine log and DB: one `host_suspensions` row of about 240 s; one `HostResumed`; the next stale-worker pass logs `Healthy` for the slot; zero `dropped a wakeup` warnings for rows created before the sleep; one scheduler kick and one cube-lease heartbeat pass from the resume consumer. Repeat once with a real lid close of at least two hours to cover DarkWake slices, and record `pmset -g log` alongside the ledger rows to show they agree. The evidence goes into `tools/boss/docs/investigations/host-suspension-validation-<date>.md`.

## Risks / open questions

- **Cube lease TTL versus the longest sleep.** The resume heartbeat pass helps only if cube's TTL sweep has not already reclaimed the lease by the time the heartbeat runs; whether cube sweeps lazily on its next command or on a schedule of its own decides that, and this design has not verified it. If cube reclaims first, the existing three-strikes auto-reap will fire for a live worker after a > 24 h sleep, which is a correct-by-its-own-rules outcome this design does not fix. A human should decide whether that is acceptable or whether a cube-side change is wanted.
- **Sleep while the engine is down** leaves a ledger hole, so a horizon spanning that gap counts it as awake time, which is exactly today's behaviour. The deferred `pmset -g log` backfill closes it.
- **Ledger placement error** is bounded by the 5 s sampler cadence. A stamp written inside a DarkWake slice between two ticks can be mis-attributed by up to one tick. No horizon here is finer than 15 s, so the error is below any threshold, but a future sub-5-second horizon must not use the ledger without shortening the cadence.
- **The orphan sweep's SQL age filter moving into Rust** changes the shape of a query that other callers (`bossctl`, the DB-fallback sweeps) may share. The implementer must check for other users of `list_orphan_active_candidates` and keep the SQL variant for them if any exist.
- **Statistical caveat carried forward.** The p = 2.65e-15 contrast in the seed is partly a detection-timing effect (orphan/fail timestamps record when the engine noticed). The design does not depend on the exact magnitude, only on the mechanism the incidents show directly.

## Proposed implementation task breakdown

Breakdown size: 8 entries (7 in-scope, 1 deferred) — the change has one new measurement seam (a crate with a sampler, a ledger and an event), two horizon families (three in-memory trackers behind one `HostClock` swap, and four persisted-stamp sites of which the destructive stale-worker sweep carries enough review weight to stand alone), a bounded acceptance-side fetch retry, one thin resume-consumer wiring PR, and a manual validation campaign the project explicitly demands; the deferred entry is the one ledger gap the design names and does not close in v1.

Parallelism: two depth-0 entries (`boss-host-suspension` crate, gate bounded retry) touch disjoint files and may run concurrently. The in-memory-trackers entry depends on the crate for `HostClock` only and starts after that crate lands; it does not wait on the ledger's persisted-stamp consumers or the resume-consumer subscriber. After the crate lands, the in-memory entry, the stale-worker entry, the remaining persisted-stamp entry and the resume-consumer entry may run concurrently; the two persisted-stamp entries share no files with each other, but both add the ledger parameter to sweep-construction call sites in `app/server.rs`, so the second to land forward-ports the first's wiring preservingly.

**Cross-project dependency edge: these rows → mono#2931's universal idle backstop.** That project's hard requirement is that suspended time must not count as evidence of observed idleness; it does not implement host power signals. The prerequisite rows here are **Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event** (Piece 1), **In-memory trackers measure awake time**, **Stale-worker sweep measures awake time, including the auto-reap recheck**, and **Orphan, spawn-ack and scheduler-heartbeat horizons measure awake time** (Piece 2). Schedule mono#2931's sleep-aware idle-backstop integration and validation after these rows, using their awake clock and ledger for idle horizons. This is an edge to existing work, not an additional implementation task here.

### Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event

Create `tools/boss/engine/host-suspension` with the `HostClock` trait (production impl over `libc::clock_gettime` using `CLOCK_UPTIME_RAW`/`CLOCK_MONOTONIC` on Apple and `CLOCK_MONOTONIC`/`CLOCK_BOOTTIME` on Linux) and `FakeHostClock`; the `SuspensionLedger` with `awake_elapsed_secs(since, now)` and a `LedgerStore` trait; the 5 s sampler that appends and persists an interval before invoking an injected `on_resumed` callback with a crate-local `HostResumed { suspended_secs, resumed_at_epoch }` payload. In `engine/core`: the `host_suspensions` migration and `LedgerStore` impl, `EventKind::HostResumed` in `engine/event-bus`, boot wiring in `app/server.rs` that loads the ledger, starts the sampler, and supplies the callback that publishes `EventKind::HostResumed` on the bus, and the per-interval `info` line. Minimal Bazel visibility (`engine/core` only). Unit tests as listed under Verification, including the store-before-callback ordering test.

- **Effort:** large
- **Dependencies:** none
- Scope: in-scope

### In-memory trackers measure awake time

Give `BuildWaitTracker`'s three production callers (`run_done_backstop::decide`, and the build-wait and background-children paths in `completion/nudge.rs`) an awake-seconds reading from a `HostClock` handle on the completion handler instead of `now_epoch_secs()`. The tracker API keeps `i64` seconds; existing tests are re-read as awake seconds and one test per caller adds a five-hour suspension (fake clock advanced asleep only) asserting the decision does not flip. This entry does not need the ledger or the event: the trackers are in-memory and reset on restart, so engine-uptime is the correct clock. It does need `HostClock` from the crate, which is why it is not depth-0.

- **Effort:** small
- **Dependencies:** Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event (`HostClock` only; this entry does not use the ledger or the event)
- Scope: in-scope

### SHA-delta gate bounded retry and `gh` failure classification

Add `classify_gh_failure` to `boss_github::gh_runner` next to `parse_http_status_from_stderr`, covering the connect-failure texts observed in both incidents, and point `trees.rs`/`contents.rs`'s transport bucket at it. Wrap `CommandBranchVerifier`'s three head/ref fetches in a `boss_http_retry::RetryPolicy::new(3, 2 s, 4 s)` loop with jitter that retries only on `Transient`. The fetches remain acceptance-side checks under mono#2931. Leave `ShaDeltaGateOutcome::Inapplicable` and its consumers to that project, which deletes their completion code and owns any distinction needed afterwards. Tests: attempt counting against a failing fake verifier, no retry on permanent errors, classifier table tests including the exact incident stderr.

- **Effort:** medium
- **Dependencies:** none
- Scope: in-scope

### Stale-worker sweep measures awake time, including the auto-reap recheck

Thread the ledger into `classify_semantic_staleness` and both of its callers (the per-slot classification and `attempt_auto_reap`'s just-in-time recheck), replacing the `iso8601_utc(now − threshold)` cutoff comparison with `awake_elapsed_secs(effective_at, now) ≥ threshold`. Re-express the existing staleness and auto-reap tests against an empty ledger and add the five-hour-suspension counterparts, including one with the interval inserted between classification and recheck. No constant changes.

- **Effort:** medium
- **Dependencies:** Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event
- Scope: in-scope

### Orphan, spawn-ack and scheduler-heartbeat horizons measure awake time

Move `orphan_sweep`'s min-age filter from the `list_orphan_active_candidates` SQL to a Rust filter over returned `updated_at` (checking first for other callers of the SQL variant and keeping it if any exist), convert `spawn_ack_sweep`'s two grace cutoffs to per-slot awake-age comparisons, and compute `stranded_ready_executions`'s `age_ms` from awake-elapsed seconds while leaving the `warn!` text and the unconditional re-kick untouched. Same test discipline as the stale-worker entry, plus an assertion that a ready row created before a five-hour suspension produces no stranded warning on the first beat after it.

- **Effort:** medium
- **Dependencies:** Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event (may run in parallel with the stale-worker entry; whichever lands second forward-ports the other's `app/server.rs` wiring)
- Scope: in-scope

### Engine-side resume consumers: scheduler kick and cube-lease heartbeat pass

Subscribe to `HostResumed` in `app/server.rs` boot wiring and, per event, call `execution_coordinator.kick()` and run one `cube_lease_heartbeat::run_one_pass`. Document in the subscriber that resume-driven sweep passes are intentionally absent and why. Test with a fake bus that the kick latch and the heartbeat pass each run once per event and that repeated events are idempotent.

- **Effort:** small
- **Dependencies:** Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event
- Scope: in-scope

### Manual suspend/resume validation against a live isolated engine

Execute the manual procedure in the Verification section on a dev machine: the `pmset relative wake` forced-sleep run and one real lid-close run of at least two hours. Record the ledger rows, `pmset -g log` excerpts, sweep verdicts, heartbeat warning counts and resume-consumer kick/lease-heartbeat evidence in `tools/boss/docs/investigations/host-suspension-validation-<date>.md`, and add the procedure to `tools/boss/docs/runbooks/` so it is re-runnable. This is a validation of the chosen approach, not a comparison; a failure is filed as a defect against the entry it implicates.

- **Effort:** medium
- **Dependencies:** Stale-worker sweep measures awake time, including the auto-reap recheck; Orphan, spawn-ack and scheduler-heartbeat horizons measure awake time; Engine-side resume consumers: scheduler kick and cube-lease heartbeat pass; In-memory trackers measure awake time
- Scope: in-scope

### Backfill the suspension ledger from `pmset -g log` at engine boot

At boot, parse `pmset -g log` records newer than the ledger's last interval and append any sleep that happened while the engine was down, so a horizon spanning an engine outage that overlapped a sleep is still measured honestly. Rejected as the primary detector for the reasons in Alternatives, kept here because it is the one ledger hole the design names.

- **Effort:** small
- **Dependencies:** Host-suspension crate: clock pair, ledger, sampler, `HostResumed` event
- Scope: deferred (future / not a v1 blocker) — the hole reproduces today's behaviour exactly, and no incident in the seed involved an engine outage during sleep
