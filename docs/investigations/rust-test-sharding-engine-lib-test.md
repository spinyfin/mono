# Investigation: native `rust_test` sharding (rules_rust #3774 backport) for `engine_lib_test`

**Status:** investigation / writeup (experiment branch, not for merge as-is)
**Date:** 2026-07-17
**Scope:** `//tools/boss/engine/core:engine_lib_test` — whether patching mono's pinned rules_rust 0.70.0 with upstream [rules_rust#3774](https://github.com/bazelbuild/rules_rust/pull/3774) and using native `shard_count` beats the filter-partitioned multi-target layouts (main's 4-target layout; closed PR #2090's 11-target layout).

## TL;DR

**Adopt via patch.** The #3774 backport applies to `rules_rust` 0.70.0 with trivial effort, passes the full correctness gate (exact partition of all 3,226 tests, zero double-runs, zero misses, deterministic and machine-independent assignment, Bazel's sharding protocol satisfied), and turns every engine edit from **4 full-crate test-mode compiles into 1** — a measured ~7.8× reduction in compile action-time for the dominant inner-loop scenario (234 s → 30 s of rustc time per edit), ~2.2× wall-clock improvement in the paired edit measurement (149 s → 69 s), ~2.4× on the pure test-execution phase (94.7 s → 38–42 s, the most stable numbers in the matrix), and ~1.47× on a fully cold build (211 s → 138–145 s). It is strictly better than both filter layouts: it keeps PR #2090's balanced-execution win (slowest shard 39 s vs main's 94 s in the paired run) **without** the N-compiles cost that made #2090 ~4.2× worse on CI. Costs are small and understood: ~14 s/shard aggregate execution overhead (more processes, enumeration, startup), a wrapper weakness around empty test lists that we should harden, and carrying a ~430-line patch until upstreaming lands (which needs a rework iteration — reviewer wants a Rust-binary wrapper).

## Background

`tools/boss/engine/core`'s `engine_lib` crate is 235 files / ~209k LOC, and its unit
suite (3,226 tests at the experiment head, `2787a852`) runs via `rust_test(crate = ":engine_lib")`
targets. Rust's libtest does not honour Bazel's `TEST_SHARD_INDEX`/`TEST_TOTAL_SHARDS`
protocol, so plain `shard_count` would re-run the full suite once per shard — and Bazel 9's
`--incompatible_check_sharding_support` (on by default) fails the test outright because
nothing touches `TEST_SHARD_STATUS_FILE`. The repo has therefore been partitioning by hand:

- **main today:** 4 `rust_test` targets with disjoint positional-filter/`--skip` lists
  (`engine_lib_test_work` / `_a` / `_b` / `_rest`) under a `test_suite` umbrella.
- **PR [#2090](https://github.com/spinyfin/mono/pull/2090) (closed 2026-07-17, unmerged):**
  the same idea rebalanced into 11 module-grouped targets.

Every one of those targets independently compiles the entire crate in `cfg(test)` mode —
the filter only selects which already-compiled tests execute. 4 targets = 4 full test-mode
compiles per source edit; #2090's 11 targets = 11, which is what CI measured at ~4.2× worse
job wall-clock (Buildkite [7141](https://buildkite.com/flunge/mono/builds/7141) vs
[7135](https://buildkite.com/flunge/mono/builds/7135): `bazel build` 1278 s vs 182 s on the
same agent and disk cache; each of the ~11 concurrent full-crate compiles ballooned from
~175 s to ~1255 s). The hand-maintained filter lists also rot: main's BUILD comment still
claims "1145 tests" while the suite is at 3,226, and the 4-way split has drifted badly out
of balance (measured below: slowest target ~3× the fastest).

Upstream [rules_rust#3774](https://github.com/bazelbuild/rules_rust/pull/3774) ("Add test
sharding support for rust_test", open since 2025-12) adds real sharding: an opt-in
`experimental_enable_sharding` attribute wraps the test binary in a script that touches
`TEST_SHARD_STATUS_FILE`, enumerates tests via libtest's `--list --format terse`, partitions
them `index % TEST_TOTAL_SHARDS == TEST_SHARD_INDEX`, and runs the shard's subset with
`--exact`. One `rust_test` with `shard_count = 11` then means **one** test-mode compile and
11 parallel executions, and the filter lists disappear.

## The backport

mono pins `rules_rust 0.70.0` (BCR, official release tarball). #3774 is based on a newer
`rules_rust` HEAD (and is currently conflicting with upstream main — `mergeable_state:
dirty`), so it does not apply verbatim, but the conflicts are shallow: the PR bundles a
review-driven refactor of `rustc_compile_action` to return a **dict** of providers instead
of a list, and 0.70.0's surrounding code has drifted slightly. The backport
(`third_party/patches/rules_rust-0.70.0-test-sharding.patch`, 432 lines):

- `rust/private/test_sharding_wrapper.sh` / `.bat` — new files, **byte-identical** to the PR's.
- `rust/private/BUILD.bazel` — `exports_files` for the two wrappers, verbatim.
- `rust/private/rust.bzl` — the `_rust_test_impl` wrapper logic and the new attributes,
  applied onto 0.70.0's versions of those functions. Context-only adjustments (e.g. 0.70.0
  spells the attr-set constants `_COVERAGE_ATTRS` where the PR base has `_coverage_attrs`,
  and 0.70.0's `_rust_test_impl` has an extra `CC_CODE_COVERAGE_SCRIPT` env block).
- `rust/private/rustc.bzl` — the list→dict provider refactor with two 0.70.0-only
  additions folded in: the `AllocatorLibrariesImplInfo` provider path and the `LintsInfo`
  sidecar become dict entries.

Callers audit: within the `rules_rust` module as consumed from BCR, the only callers of
`rustc_compile_action` are the three in `rust/private/rust.bzl` (library-common, binary,
test) — all covered by the patch. The PR's remaining hunks touch `extensions/*` (separate
BCR modules mono doesn't consume: prost, protobuf, wasm_bindgen) and `test/unit/*` (never
loaded by consumers), so they are dropped. **The patch applies to 0.70.0 with trivial
effort and no functional divergence.**

Adoption mechanism (validated in this experiment — Bazel applies the patch on every
module fetch, so `bazel clean --expunge` is a non-event):

```starlark
single_version_override(
    module_name = "rules_rust",
    patch_strip = 1,
    patches = ["//third_party/patches:rules_rust-0.70.0-test-sharding.patch"],
    version = "0.70.0",
)
```

One mechanical note: Bazel's built-in patch parser rejects plain `diff -u` output for
new-file hunks ("old file name is not specified"); the checked-in patch must be in
`git diff` format (`diff --git` + `new file mode` headers). With that format the override
fetches, patches, and builds cleanly. The patch is **inert for every `rust_test` that does
not opt in**: `experimental_enable_sharding` defaults to `False`, the provider-dict
refactor does not change any rustc command line, and the 4-target layout produced
bit-identical action behaviour under the patched rules (all measurements below for the
main-4 layout were taken with the patch active).

## Experiment target

The 4 filter targets and their `test_suite` umbrella collapse into one target (the
`args` filter lists are deleted; the env is the union of the per-shard envs):

```starlark
rust_test(
    name = "engine_lib_test",
    size = "small",
    timeout = "moderate",
    crate = ":engine_lib",
    edition = "2024",
    env = {
        "BOSS_EVENT_BIN": "/opt/boss/bin/boss-event",
        "HOME": "/tmp",
    },
    experimental_enable_sharding = True,
    shard_count = 11,
    proc_macro_deps = all_crate_deps(proc_macro = True, proc_macro_dev = True),
    deps = all_crate_deps(normal = True, normal_dev = True),
)
```

`bazel test //tools/boss/engine/core:engine_lib_test` keeps working (it is now the test
itself rather than a suite).

## Correctness gate (passed)

All checks at head `2787a852`, where the binary's full `--list` enumeration is **3,226**
tests (the task brief's ~3,262 was PR #2090's head; tests have since moved/been removed).
Verification was name-level: each shard's `test.log` was parsed for executed test names
(single-threaded rerun for clean per-line output) and compared against an offline
recomputation of the wrapper's partition function.

| Check                                    | Result                                                                                                                                                                                        |
| ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Union of tests executed across 11 shards | **3,226 — identical set** to the full `--list` enumeration (no missing, no extra)                                                                                                             |
| Duplicates across shards                 | **0** (`sort \| uniq -d` over all executed names)                                                                                                                                             |
| Exact partition                          | Every shard's executed set matches the predicted `index % 11` assignment exactly (0 of 3,226 tests misplaced)                                                                                 |
| Shard balance                            | 294 × 3 shards + 293 × 8 — near-perfect; compare main-4's drifted split (measured test-phase times 31/48/63/94 s)                                                                             |
| Bazel sharding protocol                  | Passes with `--incompatible_check_sharding_support` (Bazel 9.1.0 default _and_ explicitly set) — the wrapper touches `TEST_SHARD_STATUS_FILE`                                                 |
| Enumeration determinism                  | `--list` output byte-identical across repeated invocations                                                                                                                                    |
| Machine-independence                     | libtest's `--list` output is **bytewise name-sorted** (verified), so the partition is a pure function of the test-name _set_ — identical on macOS and Linux, independent of compilation order |

Because assignment is `sorted-name-index % N`, adding or removing any test reshuffles
assignments globally — but that is irrelevant for caching: any test change rebuilds the
binary, which invalidates all shard results anyway (exactly as it invalidates all 4
filter-targets today).

## Measurements

**Machine:** Apple M2 Max, 12 cores, 64 GB RAM, macOS (Darwin 25.3.0), Bazel 9.1.0,
`--jobs=200` (repo default), rustc via rules_rust 0.70.0 BCR toolchain.

**Caveat — load:** this host runs many concurrent Boss workers; the 1-minute load average
during measurement swung between **20 and 111** (recorded per run below). Absolute wall
times are noisy. The comparison therefore leans on (a) a _paired_ profiled run per layout,
(b) Bazel-profile **action-time totals** (per-action process time, robust to when the run
happened), and (c) structural facts (compile counts), which are load-invariant.

**Layouts** (identical sources; the BUILD file is the only delta; rules_rust patch active
in both):

- **L1 "main-4":** today's 4 filter-partitioned targets.
- **L2 "pr2090-11":** _analytic only_, from the closed PR's CI measurements (per the task
  brief) — not rebuilt locally.
- **L3 "sharded":** the single `shard_count = 11` target above.

**Cache-state control:** _cold_ = `bazel clean` + a fresh empty `--disk_cache` directory
(nothing reusable, third-party deps included; repository cache untouched — "cold" means
build outputs, not downloads). _warm no-op_ = immediately repeated invocation. _edit_ =
append code to one source file, re-run (the crate has inline `#[cfg(test)]` tests, so
"test edit" = append a `#[tokio::test]` fn to `src/conflict_watch_tests.rs`, a test-only
file; "lib edit" = append an unused fn to `src/cube_commands.rs`, library code).

### Wall-clock, all runs

`bazel test //tools/boss/engine/core:engine_lib_test`, wall = Bazel "Elapsed time":

| Scenario                                                 | L1 main-4 (load₁)                             | L3 sharded (load₁)                           |
| -------------------------------------------------------- | --------------------------------------------- | -------------------------------------------- |
| cold (empty disk cache, everything compiles)             | 211.0 s (10), 207.5 s (25)                    | 137.8 s (22), 145.1 s (14)                   |
| warm no-op                                               | 1.6 s (94), 1.4 s (94)                        | 0.9 s (22), 0.5 s (21)                       |
| execution only (warm binaries, `--nocache_test_results`) | 94.6 s (19), 94.7 s (25)                      | 41.7 s (15), 38.7 s (38)                     |
| comment-only append to a src file                        | 79.6 s (57)                                   | 81.1 s (21)                                  |
| test-code edit (compile + re-run)                        | 362.2 s (94), 314.5 s (85), **149.1 s (72)†** | 188.0 s (23), 120.0 s (91), **69.2 s (42)†** |
| lib-code edit, dead code (compile only)                  | 115.0 s (65), 112.1 s (58)                    | 115.7 s (111), 173.1 s (45)                  |

† = the profiled pair, run back-to-back (closest to controlled conditions).

Two behaviours worth calling out:

- **Comment-only and dead-code edits do not re-run tests in either layout**: rustc emits a
  bit-identical binary (appended comments/dead fns change no spans before EOF), Bazel
  content-addresses the test action's inputs, and every shard/target stays cached. The
  cost is compile-only — and that is exactly where the layouts differ (L1 pays it ×4).
- Under heavy load, _wall_ for L1's 4 parallel compiles can approach L3's single compile
  (the box is oversubscribed either way; e.g. the dead-code rows above) — but the CPU
  story below shows L1 burning ~4× the compute to get there, which on a shared worker
  fleet is capacity taken from every other job.

### Action-time totals (paired profiled runs, test-code edit)

From Bazel JSON profiles (`action processing` events; per-action process time):

| Metric                                         | L1 main-4                     | L3 sharded                 | ratio    |
| ---------------------------------------------- | ----------------------------- | -------------------------- | -------- |
| full-crate test-mode compiles                  | 4 (55+55+55+69 = **234.2 s**) | 1 (**29.9 s**)             | **7.8×** |
| test-execution actions                         | 4 (31+48+63+94 = 235.0 s)     | 11 (32–39 s each, 387.0 s) | 0.61×    |
| slowest test action (test-phase critical path) | **94 s**                      | **39 s**                   | 2.4×     |
| total action time (compile + test)             | 469 s                         | 417 s                      | 1.13×    |
| invocation wall                                | **149.1 s**                   | **69.2 s**                 | **2.2×** |

Reading it: sharding eliminates ~204 s of redundant rustc work per edit but _adds_ ~152 s
of aggregate test-execution time (11 processes × binary startup + `--list` enumeration +
scheduling contention ≈ +14 s/shard vs the 4-process layout). Net total compute is
modestly better (1.13×); **wall-clock and critical path are much better** (2.2× and 2.4×)
because the single compile isn't competing with three clones of itself and the execution
phase is balanced (32–39 s vs 31–94 s). The aggregate-execution overhead is the tunable
cost of shard granularity — `shard_count = 8` would trade a little balance for less
startup overhead; 11 was chosen to mirror PR #2090's partition for comparability.

### Cold cache

Both colds ran at the quiet end of the load range (10–25) and are the most repeatable
wall numbers in the matrix (main-4: 211.0/207.5 s; sharded: 137.8/145.1 s → **~1.47×**).
Everything below the engine crate is identical work in both layouts (~380 third-party +
first-party dep compiles); the entire cold gap is the engine test-mode compiles and the
test phase. Profile action-time breakdown (run 1 of each):

| Metric                                    | L1 main-4                    | L3 sharded                   |
| ----------------------------------------- | ---------------------------- | ---------------------------- |
| wall                                      | 211.0 s                      | 137.8 s                      |
| Rust compile actions                      | 388, sum 1051.8 s            | 384, sum 847.2 s             |
| …of which full-crate test-mode compiles   | 4 × ~38 s = 153 s            | 1 × 27.6 s                   |
| test-execution actions                    | 4, sum 248.1 s (33/53/66/96) | 11, sum 382.3 s (~35 s each) |
| test-phase critical path (slowest action) | ~96 s                        | ~41 s                        |

Notable: on a _quiet_ box the full-crate test-mode compile is only ~28–39 s of process
time — the 80–190 s compiles seen in the incremental rows are pure load inflation. The
cold-wall gap ≈ (3 extra compiles racing the tail of the build) + (94 s vs 41 s test
phase), partially overlapped. On a CI agent with a cold _crate_ but warm deps (the common
push case — what Buildkite 7135 measured as 182 s build for L1), the sharded build phase
drops to a single ~30–175 s compile depending on agent, and the test phase to ~40–80 s.

### vs the 11-target layout (analytic, from CI)

PR #2090's own CI measurement is the cleanest apples-to-apples for L2, and it is damning:
same agent, same disk cache, `bazel build` went 182 s → **1278 s** (11 uncached full-crate
compiles, each ballooning from ~175 s to ~1255 s under mutual contention), for a ~4.2×
worse job wall. Its one win — test-phase balance (slowest target 115 s → 77 s) — is fully
retained by native sharding (slowest shard 39 s locally), which pays **one** compile
instead of eleven. L3 strictly dominates L2 on every axis: same execution balance, 1/11th
the compile work, no hand-maintained filter lists to rot. The filter-sharding approach
(in both its 4-target and 11-target forms) is obsolete the day the patch lands.

## Operational wrinkles

- **Flaky-test retries:** `--flaky_test_attempts` retries individual failed _shard runs_,
  not the whole suite — strictly finer-grained than today's per-target retries (a retry
  re-runs ~293 tests vs up to ~1,300 under L1).
- **Caching granularity:** per-shard results are cached independently, keyed on the test
  action (binary digest + shard index). Any crate change invalidates the single compile
  and therefore all 11 shard results — the same _re-execution_ scope as today (all 4
  filter-targets invalidate too), while the _rebuild_ scope shrinks 4× (or 11× vs L2).
  There is no scenario where sharding caches worse than filter-targets.
- **CI scheduling:** 11 shard executions schedule as 11 independent test actions, exactly
  like 11 targets would — no pipeline config changes; `bazel test //...` picks the target
  up as one test with `Stats over 11 runs` reporting. The wrapper is `#!/usr/bin/env
bash`; both CI platforms (linux-amd64, macos-arm64) are POSIX. The `.bat` path is
  untested here (irrelevant to mono CI).
- **Silent-zero-test hazard (wrapper weakness — harden at adoption):** the wrapper runs
  `$TEST_BINARY --list … 2>/dev/null | grep … || true` and **exits 0 if the list comes
  back empty**, so a binary that crashes during enumeration would pass all shards green
  having run nothing. Low likelihood (the binary just compiled; a startup crash also
  breaks today's layout — but _loudly_). Our patch should add: fail unless `--list`
  exits 0 and yields ≥ 1 test when `TEST_TOTAL_SHARDS` is set.
- **Ad-hoc filtering:** `bazel test --test_filter=…` has never worked for `rust_test`
  (`rules_rust` 0.70.0 has no `TESTBRIDGE_TEST_ONLY` handling — verified), so no
  regression. But `--test_arg` positional filters change meaning under sharding: the
  wrapper appends user args **after** its `--exact` name list
  (`binary "${shard_tests[@]}" --exact "$@"`), so a module prefix like `work::` no longer
  narrows anything (not an exact name → matches nothing extra), and an exact test name
  becomes an _additional_ filter in **every** shard — i.e. that test runs 11×. Single-test
  dev loops (verified live): plain `bazel run` on a sharded test is refused by Bazel
  ("'run' only works with tests with one shard"); use
  `bazel run --test_sharding_strategy=disabled //tools/boss/engine/core:engine_lib_test
-- --exact name::of::test`, which execs the binary directly (no sharding env) with args
  passed through — confirmed running exactly 1 of the suite's tests.
- **Debugger ergonomics (the upstream reviewer's concern):** under `bazel test` a script
  sits between Bazel and the binary, so `--run_under=lldb` debugs the wrapper. Under
  `bazel run` (no sharding env) the wrapper execs the real binary immediately, so the
  common local-debug path is unchanged.
- **`bazel clean --expunge` / fresh checkouts:** `single_version_override` patches are
  re-applied on every module fetch — nothing to remember. (During part of this
  experiment's ancestry the patch had been applied by editing the Bazel external
  directory in place; that approach dies on expunge and was replaced by the checked-in
  override, which is what all numbers in this doc were taken under.)

## Upstream status of #3774

Verified from the PR (2026-07-17): open since 2025-12; `mergeable_state: dirty` against
upstream main. Review state: **illicitonion** commented (requested the provider-dict
refactor — already incorporated); **UebelAndre: CHANGES_REQUESTED** — wants the wrapper
to be a Rust binary rather than `.sh`/`.bat` (cross-platform consistency; "once upon a
time we had a test wrapper and it ended up being problematic for debuggers"), and
suggests a build flag alongside/instead of the per-target attribute. So upstreaming
requires a rework iteration (rewrite wrapper as a Rust helper binary + rebase over the
provider refactor conflicts), not a rebase — patch-now and revive-upstream are
complementary tracks, not alternatives.

## Verdict

**Adopt via patch now; revive upstream in parallel.**

1. **Adopt-via-patch** is justified by the numbers: per engine edit, compile action-time
   drops 234 s → 30 s (7.8×) and paired wall 149 s → 69 s (2.2×) on a loaded 12-core box;
   on CI the compile phase stops paying 4 concurrent full-crate compiles per push. The
   patch is small (432 lines), inert for non-opted-in targets, self-reapplying via
   `single_version_override`, and validated by an exact name-level partition proof.
   Before landing: harden the wrapper's empty-list edge (fail loudly), and consider
   `shard_count = 8` vs 11 after a quick CI calibration.
2. **Revive-upstream-first** is not viable as a gate: the PR needs a rework iteration to
   satisfy review (Rust-binary wrapper), and mono shouldn't wait on that. But carrying
   the patch creates the obligation: rework and land #3774 (or successor), then drop the
   patch when mono's rules_rust pin reaches a release containing it.
3. **Not-worth-it** is ruled out by every measured axis.

### What this means for filter-sharding (PR #2090 and main's 4-target layout)

Dead end, correctly closed. Filter-sharding can only trade compile duplication for
execution balance; native sharding takes the balance without the duplication. On
adoption, the 4-target layout (and its `--skip` lists, drifted comments, and manual
rebalancing chores) is deleted outright — this experiment's BUILD diff _is_ that
deletion.

### What this means for the thin-integration-crate restructure track

The companion restructure analysis found 38% of `engine_lib`'s tests cleanly movable into
a thin integration crate plus 11% more with cheap exposure changes, for ~7.7× total
compile-work reduction but only **1.34× critical path** — because the remaining fat crate
still has to compile once, test-mode, on every edit. Native sharding lands almost the
same total-compile-work win (7.8× on the test targets) for a BUILD-file-sized diff
instead of a restructure. What sharding does _not_ touch: the **1× compile that remains**.
Every edit — even to a single test — still costs one full ~209k-LOC test-mode rustc run
(measured at ~30 s action-time unloaded-ish, 80–170 s under load, and it _is_ the whole
critical path of the inner loop now). That is precisely the restructure's remaining
unique value: inner-loop test-edit granularity (edit a moved test → compile the small
crate only), and it is why **crate-splitting stays the root fix for compile size** while
sharding is the cheap, immediate fix for compile _duplication_. The tracks compose;
adopting sharding first also de-risks the restructure (its extractions can then move
tests without re-partitioning any filter lists).

## Reproducibility

- Experiment branch: this doc's PR carries the patch, the `single_version_override`, and
  the converted BUILD target. The measurement source-probe edits (appended test/lib fns)
  are _not_ part of the PR.
- Raw data: per-run Bazel logs and JSON trace profiles were produced in the session
  scratchpad; the full per-run record is in the appendix below.
- The backport patch applies to the pristine `rules_rust-0.70.0` release tarball with
  `git apply` / `patch -p1`.

## Appendix — raw per-run record

`wall_s` = Bazel "Elapsed time"; `load1` = 1-min load average sampled immediately before
the run (12-core machine); `procs` = Bazel-reported process count for the invocation.
`edit_test_code` = append a `#[tokio::test]` fn; `edit_lib_code` = append an unused fn
(compile-only — output bit-identical, tests stay cached); `touch_test_file` = append a
comment (likewise compile-only); `exec_only` = `--nocache_test_results` with warm
binaries; cold runs used `bazel clean` + a fresh empty `--disk_cache`.

```csv
layout,phase,run,wall_s,critical_path_s,procs,load1
sharded,warm_noop,1,0.910,0.29,12,22.34
sharded,warm_noop,2,0.549,0.18,,20.54
sharded,touch_test_file,1,81.098,80.62,4,20.54
sharded,edit_test_code,1,187.987,187.44,16,23.14
sharded,edit_test_code,2,119.992,118.73,16,91.02
sharded,edit_test_code,3,69.205,68.86,16,42.11
sharded,edit_lib_code,1,115.710,115.02,4,111.18
sharded,edit_lib_code,2,173.061,172.37,4,45.10
sharded,exec_only,1,41.713,41.44,12,15.37
sharded,exec_only,2,38.722,38.30,12,37.87
sharded,cold,1,137.801,136.81,1366,21.97
sharded,cold,2,145.076,144.21,1366,13.95
main4,build_after_edits,1,384.006,383.29,18,44.98
main4,warm_noop,1,1.610,0.50,,93.74
main4,warm_noop,2,1.404,0.45,,93.74
main4,edit_test_code,1,362.198,361.34,15,93.74
main4,edit_test_code,2,314.504,313.64,15,84.74
main4,edit_test_code,3,149.127,148.67,15,72.15
main4,edit_lib_code,1,115.017,114.52,7,64.59
main4,edit_lib_code,2,112.071,111.12,7,57.60
main4,touch_test_file,1,79.582,79.04,7,57.27
main4,exec_only,1,94.612,94.22,5,18.78
main4,exec_only,2,94.702,94.31,5,24.75
main4,cold,1,211.040,210.06,1373,9.65
main4,cold,2,207.516,206.51,1373,24.51
```

(`edit_test_code` run 3 in each layout is the profiled pair quoted in the action-time
table. `build_after_edits` is the first 4-target build after several accumulated source
probes — equivalent in kind to `edit_test_code`.)
