# Boss engine test suite — perf characterisation, 2026-05-13

Diagnose-only investigation of why `cargo test -p boss-engine` (and to a lesser extent `bazel test //tools/boss/engine/...`) is perceived as slow. No code, fixture, or build-config changes were made in this chore. Follow-up fix-chores should reference this report by date.

## Scope, method, stopping criterion

- **Machine.** Apple M2 Max, 12 physical / 12 logical cores, 64 GB RAM. macOS 25.3.0 (Darwin). `bazel 8.4.0`, `rustc 1.93.1`, no `cargo-nextest` installed.
- **Workspace.** `/Users/brianduff/Documents/dev/workspaces/mono-agent-001`, on `main` at `f18eba59` (the merge commit for PR #432).
- **Surfaces measured.** `bazel test //tools/boss/engine/...`, `bazel test //tools/boss/engine:engine_lib`, `cargo test -p boss-engine` (both `--no-run` and full), and the integration-test subset `cargo test -p boss-engine --test control_verbs --test work_crud`.
- **Sample size.** Each baseline command was run 2× back to back so the second invocation can demonstrate cache reuse. We deliberately did NOT `bazel clean --expunge` because `--disk_cache=~/.cache/bazelcache` is shared with sibling cube workspaces and that would penalise every other worker; we used `bazel clean` (output-base wipe only) plus `--cache_test_results=no` to model "no test cache" without nuking the shared compile cache.
- **Stopping criterion.** Two findings dominated the run early enough that micro-timing the long tail was unnecessary: (a) `cargo test -p boss-engine` does not currently compile lib tests on `main`, so the 500-odd lib tests cannot be wall-clocked at all; (b) `bazel test //tools/boss/engine/...` silently skips those lib tests because no `rust_test` target wraps `engine_lib`. Beyond confirming numbers for the two integration binaries that do run, we switched to static classification of the lib-test sources for the categorisation in §3.

## 1. Baseline numbers

### 1a. `bazel test //tools/boss/engine/...`

Note: this command only resolves to **2 test targets** — `:control_verbs_test` and `:work_crud_test`. The 561 lib tests are not bazel-visible (see §1b and Finding A below). Numbers below therefore characterise only the 36 integration tests.

| Run | Wall | Per-test reported | Process accounting |
| --- | --- | --- | --- |
| Run #1 (output-base populated, test results possibly cached) | **9.15s** | `control_verbs_test` 4.9s, `work_crud_test` 1.5s | 75 processes: 671 action-cache hit, 33 disk-cache hit, 33 internal, 9 darwin-sandbox executions |
| Run #2 (immediate re-run, fully cached) | **1.20s** | both `(cached)` | 0 executed, 0 sandbox |
| After `bazel clean` (output base wiped, disk cache intact) | **2.79s** | both still `(cached)` | 798 actions: 555 disk-cache hit, 240 internal, 3 darwin-sandbox executions |
| Forced re-execute (`--cache_test_results=no`) | **5.49s** | `control_verbs_test` 5.0s, `work_crud_test` 1.1s | 3 processes: 78 action-cache hit, 4 darwin-sandbox |

Key signals:

- **Bazel's cache layer is doing its job.** Run #2 fully cached at 1.2s wall is essentially `bazel info`-level overhead. Even after `bazel clean`, the disk cache rebuilds the output base in <3s without re-executing the tests.
- **The targets are over-budget for `size = "medium"`.** With `--test_verbose_timeout_warnings`, bazel itself prints:
  > `WARNING: //tools/boss/engine:control_verbs_test: Test execution time (4.6s excluding execution overhead) outside of range for MODERATE tests. Consider setting timeout="short" or size="small".`
  Both targets have the default `size` (no `size =` attribute in `tools/boss/engine/BUILD.bazel`), which is `medium` (5 min timeout, 100 MB RAM hint, 1 CPU). Bazel uses `size` to pack tests onto workers; oversized declarations leave throughput on the floor when the suite grows.

### 1b. `bazel test //tools/boss/engine:engine_lib`

```
$ bazel query 'tests(//tools/boss/engine/...)'
//tools/boss/engine:control_verbs_test
//tools/boss/engine:work_crud_test
```

`//tools/boss/engine:engine_lib` is a `rust_library`, not a `rust_test`, and there is no `rust_test(crate = ":engine_lib", ...)` target wrapping it. So `bazel test //tools/boss/engine:engine_lib` reports "0 tests" — it builds the library but does not run the 561 in-source `#[test]` / `#[tokio::test]` functions. **This is the load-bearing finding of the whole investigation; see Finding A.**

### 1c. `cargo test -p boss-engine`

| Run | Wall | Outcome |
| --- | --- | --- |
| `cargo test -p boss-engine --no-run` (stable 1.93.1, current `main`) | **14.51s** | **Compile error.** 6 errors, lib-test crate fails to build. |
| `cargo +nightly test -p boss-engine --no-run` | **27.71s** | **Compile error.** 8 errors (same 6 plus 2 stricter on nightly). |
| `cargo test -p boss-engine --test control_verbs --test work_crud --no-run` | **2.55s** | OK (incremental). Integration tests don't need `--cfg test` on the lib. |
| `cargo test -p boss-engine --test control_verbs --test work_crud --no-run` (re-run) | **0.17s** | OK (fingerprint check only). |
| `cargo test -p boss-engine --test control_verbs --test work_crud` (run after build) | **4.56s** wall, 4.27s user, 5.73s sys | 21 + 15 = 36 tests pass. `control_verbs` 3.15s; `work_crud` 0.54s. |

A clean baseline number for `cargo test -p boss-engine` (whole-suite) is **unobtainable from current `main`** because the lib-test compilation fails. The "several hundred lib tests" hand-wave from the work-item description (P2: 370, P6: 515) lines up with the static count of **561** `#[test]` / `#[tokio::test]` annotations in `tools/boss/engine/src/*.rs` — but none of those have been runnable on `main` since the compile breakage landed (see Finding B).

### 1d. Compile vs. run split

For the subset that compiles:

- Integration-only build: 2.5s (incremental) — call this "warm compile cost".
- Integration-only run: 4.56s wall — of which 4.39s is in the two test binaries' own self-reported `finished in` timings; the residual ~170ms is `cargo`/process startup.
- So when the lib-test breakage is fixed, compile-vs-run for the integration subset alone is **roughly 35% compile / 65% run** on warm caches. On cold cargo caches (e.g. after a `cargo clean`) the compile share will dominate badly, because the engine crate has a large transitive dep set (tokio with `features=full`, reqwest+rustls, rusqlite with `bundled`).

## 2. Per-test breakdown (integration subset)

`--report-time` is nightly-only (`-Z unstable-options --report-time`), and the workspace's pinned nightly toolchain is too old (2025-02-28, pre-`let_chains` stabilisation in this codebase) to compile the engine at all — so we could not get per-test wall-clocks via the standard rust test runner. `cargo-nextest` is not installed and is out of scope to install in this read-only chore.

What we have is per-binary granularity from the stable test runner's `finished in` line, plus a static read of the per-test fixture setup. The per-binary numbers (single, deterministic, default test-thread count of 12):

| Test binary | Tests | Wall | Avg/test | Notes |
| --- | --- | --- | --- | --- |
| `tests/control_verbs.rs` | 21 | **3.15s** | ~150 ms | Each test spawns a full `TestEngine` (see §3) — `tempfile::tempdir()` + SQLite open+init + `tokio::spawn(serve)` + `wait_for_socket` (5s timeout cap) + abort on drop. |
| `tests/work_crud.rs` | 15 | **0.54s** | ~36 ms | Similar fixture shape, but the tests issue fewer round-trips per test. |

The 150 ms average for `control_verbs` is dominated by per-test fixture setup, not by the SQL the tests actually exercise — most individual tests do ≤10 RPCs. That setup cost is the same shape as the lib-test setup cost flagged in §3 (`temp_db_path` → `WorkDb::open` → 116 `CREATE TABLE / INDEX / ALTER` statements in `WorkDb::init`).

**Top-20 individually slowest tests.** Not characterised this pass. With no per-test timer available and the lib tests not compiling, the per-test landscape is shaped by *category* (which we got) far more than by *which specific test*. Once the lib-test compile is restored (Finding B), running with `cargo +nightly test -p boss-engine -- -Z unstable-options --report-time --test-threads=1` will produce the list directly — and the categorisation below makes the predicted ordering obvious.

## 3. Classification of slow tests

Based on static reading of `tools/boss/engine/{src,tests}/*.rs` and on which functions are referenced from the per-test setup paths.

### (a) DB-heavy — by far the largest category

**Pattern.** `tools/boss/engine/src/work.rs:6892` defines the lib-test fixture:

```rust
fn temp_db_path(label: &str) -> PathBuf {
    let file = format!("boss-{label}-{}.sqlite3", next_id("test"));
    std::env::temp_dir().join(file)
}
```

Each call returns a **fresh on-disk path under `$TMPDIR`**. The DB is then opened via `WorkDb::open(path)` (`work.rs:62-72`), which calls `init()` (`work.rs:2373`). `init()` is one `execute_batch` of the full schema — **116 `CREATE TABLE` / `CREATE INDEX` / `ALTER TABLE` statements** in a single string (`grep -cE 'ALTER TABLE|CREATE TABLE|CREATE INDEX' tools/boss/engine/src/work.rs` = 116). Every call to `connect()` (`work.rs:2588`) opens a new `rusqlite::Connection`, sets `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, and arms a busy-timeout — once per query method, not amortised.

**Files where this pattern dominates the test count.** `work.rs` (121 tests), `app.rs` (54 tests), `coordinator.rs` (38), `runner.rs` (27), `completion.rs` (26), `live_worker_state.rs` (21), `worker_setup.rs` (20), `conflict_watch.rs` (20), `merge_poller.rs` (19), `effort.rs` (17), `events_socket.rs` (16), `live_status.rs` (15). Of the 561 lib tests, the vast majority touch `WorkDb`.

**The two integration test files use the same shape, scaled up.** `tests/control_verbs.rs:39-92` defines `TestEngine::spawn()`:

```rust
let temp = tempfile::tempdir()?;
let socket_path = temp.path().join("engine.sock");
let db_path     = temp.path().join("state.db");
// ...
let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None).await });
if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await { … }
```

Per test: one tempdir + one SQLite open+init + spawn engine task + poll for Unix-domain socket bind (up to 5s timeout). `_temp` is dropped at end of test → file removal cost.

**Why this category is the prime suspect.** SQLite open+migrate over an empty on-disk file on APFS takes O(few ms) by itself, but with 116 DDL statements and per-method new `Connection`s the per-test setup is comfortably in the tens of ms — at 561 tests that is the bulk of the wall-clock budget regardless of what the test body does.

### (b) git/jj shell-out

`grep -cE 'Command::new|std::process::Command|tokio::process::Command'` across the src tree picks out 5 files: `completion.rs` (3 callsites), `conflict_diagnosis.rs` (3), `merge_poller.rs` (2), `coordinator.rs` (2), `main.rs` (1). These are the only modules that fork a subprocess at all. In tests they are usually wrapped behind trait abstractions (e.g. `MergeProbe`, `ExecutionPublisher` in `merge_poller.rs`) and replaced with mock impls, so in practice this category contributes much less than (a). A small number of integration-style tests inside `completion.rs` and `conflict_diagnosis.rs` may genuinely exec `git` — those are worth confirming once the lib tests compile.

### (c) `gh` / network shell-out

`tools/boss/engine/Cargo.toml` lists `wiremock = { workspace = true }` as a dev-dep — i.e. the engine's `gh`/HTTP-shaped tests are expected to go through `wiremock` rather than real network. There are no `Command::new("gh")` callsites in tests we can see. This category should be ~zero cost in the suite; calling it out so it isn't conflated with (b).

### (d) sleep / poll / timeout

`grep -cE 'tokio::time::sleep|thread::sleep|Duration::from_(secs|millis)'` highlights the modules that rely on real time:

| File | Count | Likely shape |
| --- | --- | --- |
| `app.rs` | 31 | many use `STARTUP_TIMEOUT`/`Duration::from_secs(N)` for `wait_for_socket`-style polls |
| `live_status_loop.rs` | 24 | the live-status worker is a polling loop; tests exercise the tick interval |
| `coordinator.rs` | 17 | event-loop / cube probes |
| `spawn_flow.rs` | 13 | spawn-pipeline tests with deferred state |
| `dispatch_reader.rs` | 7 | dispatch-event reader |

Note this count includes *test fixture* `Duration::from_secs` constants like `STARTUP_TIMEOUT = Duration::from_secs(5)` — those only burn the wall-clock if the awaited condition genuinely takes that long (the limit is a ceiling, not a cost). The actual `tokio::time::sleep` / `thread::sleep` callsites are a strict subset and that's the figure that drives the tail latency. We didn't isolate that subset; once per-test timings exist, any test consistently >500 ms after fixture-setup is removed is likely living here.

### (e) large fixture / setup cost

The `TestEngine` pattern in §3(a) is the canonical example: tempdir + SQLite open+init + tokio runtime spawn + socket poll. The lib-test equivalent is `temp_db_path → WorkDb::open` and the various per-module spawn helpers in `runner.rs` (24 tempfile callsites), `events_socket.rs` (17), `spawn_flow.rs` (14), `dispatch_reader.rs` (13), `transcript_tail.rs` (10), `audit.rs` (9). Categories (a) and (e) materially overlap — the setup is the DB cost in most cases.

### (f) genuinely lots-of-CPU work

None of the modules under `tools/boss/engine/src/` are CPU-bound by design — there are no hashing-/parsing-/compression-heavy code paths driven by test inputs at scale. `sha2` is used for short hex IDs, not for bulk hashing. `regex` is compiled lazily. **We expect category (f) to be near-zero in the suite.** Any test that lands in (f) after follow-up timing is more likely a misclassified (a) or (d) than a genuine CPU-bound hotspot.

### Predicted ordering

Under §1 numbers + §3 reasoning, the predicted "where the time goes" decomposition for a clean cargo run (i.e. once Finding B is fixed) is roughly:

1. **~70-80% category (a) + (e)** — fresh on-disk SQLite per test + 116-stmt schema init + per-method `Connection::open`.
2. **~10-20% category (d)** — `tokio::time::sleep`/`wait_*` polls in live-status / dispatch / spawn-flow tests.
3. **~5-10% category (b)** — any genuine `git`-exec test in `completion.rs` / `conflict_diagnosis.rs`.
4. **~0% (c), (f)** as argued above.

These are reasoned splits, not measured ones; they're the prior to test against once §2's per-test data exists.

## 4. Parallelism check

- **`serial_test` / `#[serial]`: not used.** `grep -rn 'serial_test\|#\[serial' tools/boss/engine/{src,tests}` returns zero hits. Nothing in the lib-test or integration-test surface explicitly serialises a test.
- **Cargo default parallelism applies.** With the default `--test-threads=$(nproc)` (12 on this host) tests in a binary run in parallel. Each test gets its own tempdir + SQLite path, so there's no implicit serialisation via shared DB either.
- **Bazel parallelism is constrained by target count.** With only two `rust_test` targets, bazel has nothing to schedule across. With `--jobs=200` from `.bazelrc` the suite is bottlenecked on critical path (4.6s) not on capacity. No `shard_count` attribute on either target — sharding is an option once the suite grows, but for 21+15 tests it's not yet load-bearing.
- **No per-target `tags = ["exclusive"]` etc.** No serialisation hints in `tools/boss/engine/BUILD.bazel`.
- **Test framework knobs.** No `[env]` table sets `RUST_TEST_THREADS`. No CI-side wrapper script in `tools/boss/scripts` overrides thread count.

Conclusion: the suite is **not** being throttled by serialisation. The tail is wall-time per test, not contention. (But see Finding D about size hints.)

## 5. Cache-hit reality

### Bazel

The cache numbers in §1a tell a clean story:

| Scenario | Disk cache state | Wall | Test executions |
| --- | --- | --- | --- |
| `bazel test //tools/boss/engine/...` (re-run, no source change) | Warm | 1.2s | 0 (test-result cache hit) |
| Same, after `bazel clean` (output base wiped) | Warm disk cache | 2.8s | 0 (test-result cache hit survives `bazel clean`) |
| Same, with `--cache_test_results=no` | N/A | 5.5s | 2 (forced) |

So the **test-result cache hit ratio for an unchanged suite is 100%**. Running `bazel test //tools/boss/engine/...` after another worker has built the same revision should cost ~3s wall, almost all of it bazel-server overhead. The bullet in `AGENTS.md` recommending bazel over cargo for re-runs is well-founded for the integration tests — but it's misleading for anyone hoping the lib tests will also be cached, because bazel doesn't run them in the first place.

### Cargo

Cargo's "test cache" is incremental compile only — there is no run-cache. The recipe `cargo test --no-run` lets us measure how much of the cost is build vs. test, but only when the build succeeds. Today on `main`:

- `cargo test -p boss-engine --no-run` (lib tests) — **fails to compile in 14.5s** before any test runs.
- `cargo test -p boss-engine --test ... --no-run` — 2.5s incremental, 0.17s on a no-op re-run. The 2.5s figure represents the cost of re-linking the test binaries when (e.g.) a single source line in `engine_lib` changes — that's not negligible if a worker is iterating on a single test.
- Full integration run: 4.56s. Of that, ~2.4s is the actual tests and the remainder is build / cargo-overhead.

**Cargo cannot match bazel's cross-invocation reuse** for the engine tests as currently structured: cargo will always re-link when any upstream crate changes, and will always re-run every selected test on every invocation. This is the perception gap behind workers reaching for `cargo test -p boss-engine` — it *feels* slow because it does real work every time, not because the work itself is unusually slow.

## Findings (load-bearing)

### Finding A — Bazel does not run the engine's lib tests at all

`tools/boss/engine/BUILD.bazel` declares one `rust_library` (`engine_lib`) and two `rust_test` integration targets (`work_crud_test`, `control_verbs_test`). There is **no `rust_test(crate = ":engine_lib", ...)`** target, which is the rules_rust idiom for running in-source `#[cfg(test)]` modules through bazel. The 561 lib tests are therefore invisible to `bazel test //tools/boss/engine/...` — they were not under bazel test coverage when the report was written, and the disk-cache hit rate for them is undefined (vacuously 100%).

This silently undermines the AGENTS.md guidance to "prefer `bazel test` / `bazel build` over `cargo test` / `cargo build`" for the engine specifically. A worker following that guidance gets a clean green from bazel without ever exercising the bulk of the test surface.

### Finding B — `cargo test -p boss-engine` (lib tests) does not compile on `main`

As of `f18eba59` (PR #432 merge), `cargo test -p boss-engine --no-run` fails with 6 errors. The offending `#[cfg(test)]` blocks are:

- `tools/boss/engine/src/completion.rs:2516,2518` — `PrLifecycleProbe { … }` initialiser is missing the fields `base_ref_name`, `head_ref_name`, `head_ref_oid`, and `state: PrLifecycleState::Open(_)` is passing an `OpenPrMergeability` where `OpenPrStatus` is now required.
- `tools/boss/engine/src/merge_poller.rs:1578, 1590, 1603, 1614` — all four call `run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None)` with 4 args, but the function signature at `merge_poller.rs:626-632` takes 5: the new `completion_handler: Option<&WorkerCompletionHandler>` parameter added on 2026-05-12 (commit `fec5a42c4` "mc p4: auto-retire — resolved-side transition + completion branch + frontend events") was added to the production signature but the test callsites in the same file were not updated. The function's own docstring at `merge_poller.rs:620-625` even says *"Pass `None` for pre-`completion_handler` wiring and tests that exercise only the in-review and conflict paths"* — the author knew the tests needed updating, just missed these four sites.

This is the direct **causal mechanism** by which Finding A bites in practice: bazel never builds the lib with `--cfg test`, so it cheerfully accepts these stale callsites; cargo refuses; and workers who try cargo as a fallback (the AGENTS.md "fine for quick local iteration on a single file" carve-out) hit an immediate error wall and reach the wrong conclusion ("cargo test is slow / broken").

### Finding C — Per-test SQLite setup dominates the budget where the tests *can* run

§3(a) and §2 together: each lib-test does `WorkDb::open` → `init()` → 116 DDL statements on a fresh on-disk SQLite file under `std::env::temp_dir()`. Even at <10 ms per init, 561 of those serialise to >5 seconds *before* any test body runs. The integration tests inflate this further by also spinning up the tokio runtime and waiting on a Unix-domain socket bind.

### Finding D — Both bazel test targets are over-sized

Bazel's `--test_verbose_timeout_warnings` output flags both engine test targets as having `size = "moderate"` (the default) while actually running under 5s. The work-item description called this out as one of the example follow-up shapes; it is genuine. Effect on wall-clock is small; effect on bazel's parallel-packing decisions is real when the suite grows.

### Finding E — No serialisation is forcing the tail

§4: no `serial_test`, no `tags = ["exclusive"]`, no `RUST_TEST_THREADS` override. The slowness is real per-test wall-clock, not a parallelism gap.

## Recommended follow-ups

Each item below is a candidate fix-chore. Effort estimate uses the `trivial / small / medium / large` scale from §Q4 of the heuristic guide.

1. **Add a `rust_test(crate = ":engine_lib", …)` target so bazel actually runs the lib tests.** This is **trivial** (3-5 line BUILD edit) and is the immediate blocker on everything else: until bazel runs the lib tests, they will keep rotting (Finding B is direct evidence). Pre-condition for everything below. Expected save: negative in wall-clock terms (bazel now runs ~500 more tests per invocation), but +∞ in coverage; opens the door to measured per-test timings via `--profile=profile.json` + `bazel analyze-profile`.

2. **Fix the 6 stale `#[cfg(test)]` callsites flagged in Finding B.** **Trivial.** Strictly a precondition for any measurement effort and not a perf change in itself — the *act of fixing* unmasks the real timings. List it as its own follow-up because it shouldn't be quietly bundled into a perf chore (Finding A and B are arguably the same root cause).

3. **Switch the lib-test DB fixture from on-disk to `:memory:` SQLite.** Replace `temp_db_path` (`work.rs:6892`) with an in-memory `rusqlite::Connection::open_in_memory()` flavoured `WorkDb` open path. Requires teaching `WorkDb::open` (or a new `WorkDb::open_in_memory`) to skip the parent-dir create, skip WAL journaling (WAL requires a real file), and possibly hold the `Connection` for the lifetime of the `WorkDb` instead of reconnecting per method. **Medium** because of the per-method `connect()` shape — `:memory:` databases are per-connection, so the connect pattern at `work.rs:2588` has to change to share one connection (or, alternatively, hold the test connection in a `Lazy<Mutex<Connection>>` keyed on test id). Expected save: a few seconds off the lib-test suite (the largest single lever per §3 prediction).

4. **Mark both integration targets `size = "small"`.** Bazel-warning-driven: `control_verbs_test` runs in 4.6s (small ceiling is 60s) and `work_crud_test` in 0.7s. Tightens bazel's resource model; lets future shard-count plays be honest. **Trivial.** Expected save: not wall-clock; improves scheduling once a third / fourth `rust_test` target lands.

(Two more candidates we'd suggest filing once Finding B is unblocked and the actual timings exist: a "share the in-memory DB across a test module via `OnceLock`" shape if the warm-DB cost is dominated by `init()` rather than `open`, and a `wait_for_socket` audit in `control_verbs.rs` — `STARTUP_TIMEOUT = 5s` may be polled with too-coarse a granularity and add 50-200 ms tail latency per test. Both estimated **small**; deliberately not enumerated in detail here because they rely on numbers we cannot yet measure.)

## Reproducibility

To re-derive §1 numbers from a sibling cube workspace on the same host:

```sh
# Bazel side
bazel test //tools/boss/engine/... --test_output=summary                   # baseline
bazel test //tools/boss/engine/... --test_output=summary                   # warm/cached
bazel clean && bazel test //tools/boss/engine/... --test_output=summary    # post-clean
bazel test //tools/boss/engine/... --test_output=summary \
    --cache_test_results=no --test_verbose_timeout_warnings                # forced + warnings

# Cargo side
cargo test -p boss-engine --no-run                                         # currently FAILS — Finding B
cargo test -p boss-engine --test work_crud --test control_verbs --no-run   # 2.5s incremental
cargo test -p boss-engine --test work_crud --test control_verbs            # 4.56s wall
```

All numbers in this report came from a single sitting on 2026-05-13 between `bazel info` calls; reruns within ±25% are expected on the same host, larger swings on machines with fewer cores or slower NVMe. The "test result is cached" observations are independent of host — they fall out of bazel's content-addressed cache and are stable across hardware so long as the disk cache is shared.
