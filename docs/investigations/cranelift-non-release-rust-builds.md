# Cranelift for non-release Rust builds in mono

- Date: 2026-11-23
- Scope: an opt-in Cranelift codegen backend for edit → `bazel test` loops, added through local patches to rules_rust 0.70.0
- Repository baseline: `44adb25d`; stable Rust 1.99.0, edition 2024; rules_rust and rules_rust_prost 0.70.0 with mono's patches; Bazel 9.3.3
- Experiment toolchain: `nightly-2026-08-15` (`rustc 1.99.0-nightly d453bdd8f`), the last nightly of the 1.99 cycle, with the `rustc-codegen-cranelift-preview` component of the same date
- Hosts: one Apple-silicon Mac (12 cores, 64 GB) and the macOS remote-execution workers. Nothing was run on Linux.
- Related investigation: [Rust test sharding](rust-test-sharding-engine-lib-test.md)

An earlier revision of this note stopped at one question: does mono's rules_rust supply Cranelift? It does not, and no release through 0.75.0 does. This revision adds the missing support as a local patch, wires an opt-in config, and runs the experiment.

## Verdict

**Do not adopt.** The patch is small and works. Every crate in the engine, CLI and checkleft graphs compiles with Cranelift, on this Mac and on the remote workers, and no crate had to be excluded. The experiment still comes back negative on both things that matter:

1. **There is no build-time gain at mono's settings.** A cold local build uses 1% to 3% less compiler CPU with Cranelift than with LLVM on the same nightly, where repeats of one configuration differ by up to 2.3%, and it takes no less time: 692 s against 689 s built side by side. Edit loops are no faster on the clock: rebuilding the engine's test binary after a shared-crate edit takes 138 s with either backend. mono's `fastbuild` already compiles with `opt-level=0` and `debuginfo=0`, and most of each large compile is the compiler's front end, which a backend does not change. See [Build time](#build-time).
2. **Tests that depend on unwinding fail.** The backend that rustup distributes is built without unwinding support. A panic does not run destructors and is not caught by `catch_unwind`. Ten of the 149 engine, CLI and checkleft test targets fail on every run (18 distinct tests), and one suite outside those three hangs until its timeout. Engine code relies on the same behavior at runtime, so a Cranelift-built engine does not behave like the shipped one when something panics.

The opt-in in this change is safe to keep in the tree: ordinary, `-c opt` and release builds resolve exactly the actions they did before, and two tests pin that. Keeping it costs a third rules_rust patch to carry across upgrades and a nightly pin to move with each stable upgrade. It buys a one-flag way to measure again. If that is not wanted, drop the patch and the wiring and keep this note.

## What was added

### The rules_rust patch

[`rules_rust-0.70.0-codegen-backends.patch`](../../third_party/patches/rules_rust-0.70.0-codegen-backends.patch) adds one attribute, `codegen_backends`, to the toolchain repository rule and to everything that feeds it: the `rust.toolchain` and `rust.repository_set` tags, `rust_register_toolchains`, `rust_repository_set` and `rust_toolchain_repository`. For each named backend the repository rule downloads the `rustc-codegen-<name>-preview` component for the compiler's host triple into the same repository as `rustc`.

Nothing else was needed. rules_rust 0.70.0 already globs `lib/rustlib/<host>/codegen-backends/*` into the compiler's library files and copies those into the sysroot it generates, so once the file is downloaded it is an input of every compile action, locally and on a remote worker. The backend is then selected with the existing `extra_rustc_flags` attribute.

| File                                | Added lines | What changes                                                                                                                   |
| ----------------------------------- | ----------: | ------------------------------------------------------------------------------------------------------------------------------ |
| `rust/private/repository_utils.bzl` |          28 | `load_rustc_codegen_backend`, a thin wrapper over the existing `load_arbitrary_tool`                                           |
| `rust/repositories.bzl`             |          52 | The attribute, the download loop, and the plumbing through the three macros; the request fails early without a nightly version |
| `rust/extensions.bzl`               |           9 | The attribute on both module-extension tags                                                                                    |

The patch is 232 lines as a file: 89 added, none removed, no existing line changed. It follows the path the existing `dev_components` attribute takes, and it does not touch a compile action. It is independent of the test-wrapper patch: the two share no file.

Two limits are deliberate. The component is distributed only on the nightly channel (the 1.99.0 stable manifest lists it with no available target), so the rule refuses a stable or beta version, and a repository set that mixes channels applies the backend to its nightly toolchain only. The patch also does not add a build setting that chooses the backend. A flag on the toolchain is enough, and a global flag would be wrong here: it would also reach the wasm guests, which Cranelift cannot target.

### Wiring in mono

- **[`MODULE.bazel`](../../MODULE.bazel)** applies the patch and registers one extra toolchain with `rust.repository_set`: the pinned nightly for `aarch64-apple-darwin`, with the Cranelift component and `extra_rustc_flags = ["-Zcodegen-backend=cranelift"]`. Seven archive checksums are pinned. The toolchain carries `target_settings` for the nightly channel explicitly. That line is load-bearing: the toolchain hub declares a repository-set toolchain with only the settings the root module gives it, and it registers toolchains in name order, so without the line this nightly toolchain was selected for every build. That was observed during the work, not inferred.
- **[`.bazelrc`](../../.bazelrc)** gains one line, `build:cranelift --@rules_rust//rust/toolchain/channel=nightly`. Nothing selects the config.
- **Proc macros and build scripts** are compiled by the same nightly, with LLVM. `extra_rustc_flags` does not apply to the exec configuration, and the channel flag does carry into it on Bazel 9.3.3, so a proc macro and the crate that loads it always come from one compiler.
- **The musl and wasm transitions** under `tools/checkleft` now pin the channel back to stable. The static checkleft binary and the wasm guest components are release artifacts, and their toolchains exist only for the stable channel. With the pin they are built by the stable compiler under `--config=cranelift` too, and the component digest check still passes there.
- **Two tests** guard the arrangement. `//third_party/patches:rust_toolchain_channel_test` runs the compiler the build resolved: on the stable channel it must not be a nightly, and under `--config=cranelift` it must compile a probe with the Cranelift backend. `//tools/test-sandbox:cranelift_config_source_test` fails if an rc file selects the config, sets the channel from another config, or passes a backend flag.

Only macOS on Apple silicon has a nightly toolchain. `--config=cranelift` on another host stops at analysis with no matching toolchain.

### Ordinary, `-c opt` and release builds are unchanged

`bazel aquery --output=text` over `deps()` of `//tools/boss/engine/core/...`, `//tools/boss/cli/...` and `//tools/checkleft/...` was captured before and after the change in three configurations.

| Configuration             | Actions | Rustc actions | Differences in command lines, inputs, outputs or environment | Action keys that differ |
| ------------------------- | ------: | ------------: | -----------------------------------------------------------: | ----------------------: |
| default (`fastbuild`)     |   6,553 |         1,487 |                                                            0 |                     344 |
| `-c opt`                  |   6,553 |         1,487 |                                                            0 |                     344 |
| `--config=release -c opt` |   6,554 |         1,487 |                                                            0 |                     344 |

The 344 keys that differ all belong to `RepoMappingManifest` actions, which hash the set of repositories the module graph defines. Their output did not change: 136 manifests saved before the change are byte-identical to the current ones, and 117 of those were regenerated after it. The first stable-channel run after the change reused 90 of 149 test results from the cache.

## Correctness

`bazel test --keep_going //tools/boss/engine/... //tools/boss/cli/... //tools/checkleft/...` ran on the remote workers in three configurations. The two nightly runs used `--test_timeout=240` so that a hung test could not hold a shared worker for long; the slowest single test attempt under stable LLVM took 75 s.

| Configuration                 | Targets | Passed | Failed | Build actions failed |
| ----------------------------- | ------: | -----: | -----: | -------------------: |
| Stable 1.99.0, LLVM (default) |     149 |    149 |      0 |                    0 |
| Nightly, LLVM (control)       |     150 |    149 |      1 |                    0 |
| Nightly, Cranelift            |     149 |    138 |     11 |                    0 |

The control is the same nightly toolchain without the backend flag. It separates what the nightly compiler changes from what Cranelift changes. Its 150th target is the toolchain channel test, which passes under Cranelift as well.

### Everything compiles

No compile action failed. The Cranelift run executed 1,652 compiles on the workers, among them `ring`, `rustls`, `sha2` with `sha2-asm`, `curve25519-dalek`, `tokio`, `rusqlite`, `tree-sitter` and the `wasmtime` host that checkleft embeds. The build log has no Cranelift diagnostic: no unsupported intrinsic and no inline-assembly error. No crate exclusion list is needed.

### What fails, and why

Ten targets fail deterministically. They failed in the full run and again when rerun.

| Target                                             | Failing tests                                                                                                                                                                                                                                                                                                                                                     |
| -------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `engine/core:engine_lib_test` and its jj 0.46 twin | `a_panicking_job_does_not_wedge_its_pr_forever`, `hook_progress_snapshot_lock_recovers_from_poison`, `spawn_event_subscriber_redispatches_on_execution_terminal`, `a_project_unpark_reconciles_each_child_with_progress_events`, `the_jobs_github_calls_are_attributed_to_the_unpark_caller`, `legacy_recovery_respects_live_ownership_but_not_completed_history` |
| `engine/completion:completion_test`                | `a_panicking_teardown_still_clears_its_mark`                                                                                                                                                                                                                                                                                                                      |
| `engine/driver:driver_test`                        | `refuse_one_turn_per_process_panics_on_a_one_shot_declaration`                                                                                                                                                                                                                                                                                                    |
| `engine/event-bus:event-bus_test`                  | `restarts_after_a_panic`, `reconciles_on_every_attempt_including_restarts`                                                                                                                                                                                                                                                                                        |
| `engine/metrics-registry:metrics_registry_test`    | `name_validation_rejects_empty_and_edge_dots`, `name_validation_rejects_uppercase`                                                                                                                                                                                                                                                                                |
| `engine/single-flight:single-flight_test`          | `panicking_loader_releases_waiters_and_allows_retry`                                                                                                                                                                                                                                                                                                              |
| `engine/task-scope:task-scope_test`                | `error_return_drains_tasks_and_finished_handles_keep_tokio_semantics`, `a_panicking_destructor_does_not_strand_shutdown`                                                                                                                                                                                                                                          |
| `engine/test-process:test-process_test`            | `unwinding_through_panic_reaps_the_process_group`                                                                                                                                                                                                                                                                                                                 |
| `engine/work-db:work_db_test`                      | `unwinding_removes_ownership_without_hiding_mutex_poisoning`, `statement_counter_counts_cached_and_nested_queries_and_clears_its_callback`                                                                                                                                                                                                                        |

Every one of the 18 tests exercises a panic that the code is expected to survive: a `catch_unwind` in the test or in the code under test, a destructor that must run while unwinding, or a tokio task whose panic the runtime is expected to contain. Under Cranelift the panic passes straight through the frames Cranelift compiled. No destructor runs, and the first frame that catches it is one LLVM compiled, in the standard library or in libtest. A standalone probe compiled with this nightly shows the same thing: with LLVM a `catch_unwind` around a panicking closure drops its guard, returns `Err` and leaves the mutex poisoned and released; with Cranelift the process prints the panic message and exits with status 101.

The consequences go beyond a failed assertion. `//tools/boss/log-files:log-files_test`, outside the three suites, hangs: one test panics while it holds a mutex, the guard is never dropped, and the other tests wait on the lock until Bazel's timeout (2 of 2 runs; the captured thread stacks show them parked in `Mutex::lock`). Tests that only use `#[should_panic]` pass, because libtest itself catches the panic.

This is a property of the distributed backend and not of the patch or of macOS. The backend's `unwinding` Cargo feature is off by default with the comment "Not yet included in unstable-features for performance reasons", at the pinned commit and at the nightly of 2026-11-23, and Rust's own build of the component does not enable it. The upstream [tracking issue](https://github.com/rust-lang/rustc_codegen_cranelift/issues/1567) says a backend built from source with `--panic-unwind-support` should work on arm64 macOS. Supplying one means building the backend inside Bazel against `rustc-dev`, which is a custom-toolchain project and not something a component download can provide. It was not attempted.

The same code paths exist outside tests. `single-flight`, `write-batch` and the event-bus supervisor call `catch_unwind` in production code, and the engine relies on tokio containing a panicking task. An engine binary built with this backend would turn those contained failures into leaked locks and dead threads.

### Failures that were not Cranelift

- `engine/spoke-hub:integration_test` failed once under Cranelift, on a test that polls with sleeps. It passed in the next nine Cranelift runs of the binary and in nine stable runs. It ran while the workers were busy with the full build.
- The control's one failure was `an_answer_that_launches_work_is_not_queued_behind_a_slow_cube_lease`, which asserts a 300 ms budget and measured 371 ms. It passed three of three reruns.

Both nightly configurations print one extra warning on every linked binary, 176 in this build: the nightly-only `linker_messages` lint repeats a line of `clang` output about the deployment target. It is noise. `-Alinker-messages` on the nightly toolchain would silence it.

## Build time

### Method

Three configurations were compared throughout:

- **Stable LLVM**: the default build.
- **Nightly LLVM**: `--config=cranelift` in a copy of the workspace whose nightly toolchain has no backend flag. It isolates the backend from everything else the config changes.
- **Cranelift**: `--config=cranelift`.

Local builds used `--config=dev-local` with an output base and a disk cache directory that exist only for this experiment, one pair per configuration. The shared disk cache and the remote cache were never cleared or disabled. A cold build is `bazel clean` in the experiment output base plus an empty experiment cache directory, measured from the start of the execution phase. Remote builds used this machine's default, which executes actions and tests on the macOS workers.

An edit loop appends a function with a value never used before to one source file and rebuilds the affected test target, so the changed compiles cannot come from any cache.

**The machine was not quiet, and that limits what wall-clock time can show.** Other agents' builds ran on this Mac throughout, with a one-minute load average between 11 and 150 on 12 cores, and the laptop slept for an hour part-way (runs that overlapped the sleep were discarded). The same cold Cranelift build took between 288 s and 642 s. Two measures are robust to that and carry the conclusions:

- **Compiler CPU time**: user plus system time of the `Rustc` actions and their child processes, from Bazel's build metrics. It moved by at most 2.3% between separate repeats of one configuration, and rose by several percent when three builds shared the machine.
- **Simultaneous runs**: the edit loops, and one cold build, ran the three configurations at the same moment in separate output bases, so each saw the same background load. Their absolute times are inflated by the sharing. Their differences are meaningful.

### Cold build, local

Targets: `//tools/boss/engine/core:engine_lib_test`, `//tools/boss/engine/core:engine`, `//tools/boss/cli:boss` and `//tools/checkleft:checkleft`.

| Configuration | Compile actions | Compiler CPU per build (s) | Wall per build (s), load average at start in brackets |
| ------------- | --------------: | -------------------------- | ----------------------------------------------------- |
| Stable LLVM   |             812 | 1,233 · 1,232              | 382 (48) · 591 (44)                                   |
| Nightly LLVM  |             928 | 1,418 · 1,450              | 437 (31) · 422 (44)                                   |
| Cranelift     |             928 | 1,405 · 1,427 · 1,438      | 288 (32) · 452 (79) · 642 (151)                       |

One more build of each configuration ran with all three at once on an otherwise quiet machine (load average 10 at the start). Stable LLVM, nightly LLVM and Cranelift took 687 s, 689 s and 692 s of wall time and 1,281 s, 1,535 s and 1,482 s of compiler CPU.

Cranelift against nightly LLVM is the backend's effect. In the separate builds it is 1,423 s against 1,434 s of compiler CPU on average, 0.8% less. In the simultaneous builds it is 3.4% less CPU and no less wall time. Both nightly configurations cost at least 16% more CPU than the stable build. That is the second set of host tools for the wasm guests, described under [Other observations](#other-observations), and it applies because `checkleft` is among the targets. The wall-clock column of the table shows the noise and nothing else.

### Edit loops, local

Each value is one rebuild after one edit, done in all three configurations at once. Times are the duration of the named compile action from Bazel's profile, or of the whole command where the row says end to end. Brackets hold the compiler CPU of the rebuild. In the second and third repeats of the test-binary row a few small dependents rebuilt alongside it; the times are the test binary's own action.

| Edit, and what is rebuilt                                                            | Compiles | Stable LLVM (s)           | Nightly LLVM (s)          | Cranelift (s)             |
| ------------------------------------------------------------------------------------ | -------: | ------------------------- | ------------------------- | ------------------------- |
| Engine runtime crate (368 files); its library, `core:runtime`                        |        1 | 21.9 · 19.8 (22.6 · 21.5) | 22.0 · 19.7 (22.9 · 22.0) | 25.1 · 23.0 (21.3 · 20.4) |
| Same edit; the test binary, `core:engine_lib_test`                                   |        1 | 74.8 · 52.1 · 67.5 (69.3) | 74.6 · 51.9 · 70.9 (69.5) | 64.3 · 51.9 · 70.4 (56.0) |
| `boss-protocol`, a crate most of the engine depends on; up to `core:engine_lib_test` |       47 | 138.1 end to end (174.9)  | 138.8 end to end (175.8)  | 138.0 end to end (153.3)  |
| An integration test source; `bazel test core:work_crud_test`, including the test run |        2 | 12.4 end to end           | 12.2 end to end           | 13.0 end to end           |

Cranelift uses less CPU where a large first-party crate is rebuilt: 19% less for the test binary and 13% less across the 47 compiles. None of it reaches the clock. The shared-crate rebuild takes 138 s in every configuration, the test binary is 10 s faster in one repeat and within 3 s either way in the other two, and the library is 3 s slower both times.

### Where the time goes

The reason is visible in the compiler's own phase timings (`-Ztime-passes`), taken for the two largest compiles on a quiet machine (load average 11), both backends at once. They exist only for the nightly compiler.

| Phase                                | Library, LLVM (s) | Library, Cranelift (s) | Test binary, LLVM (s) | Test binary, Cranelift (s) |
| ------------------------------------ | ----------------: | ---------------------: | --------------------: | -------------------------: |
| Macro expansion                      |               1.7 |                    1.7 |                   5.5 |                        5.5 |
| Type checking                        |               5.1 |                    5.2 |                  12.6 |                       12.4 |
| Borrow checking                      |               2.8 |                    2.8 |                  12.1 |                       12.1 |
| Monomorphization collection          |               2.4 |                    2.4 |                   5.0 |                        5.0 |
| Crate metadata                       |               3.2 |                    3.2 |                     — |                          — |
| Code generation (`codegen_crate`)    |               2.1 |                    5.3 |                  10.2 |                       10.1 |
| Backend passes (`LLVM_passes` event) |               2.4 |                    4.2 |                   5.0 |                        4.2 |
| Linking                              |                 — |                      — |                   1.0 |                        1.1 |
| **Whole compile**                    |          **16.6** |               **19.4** |              **44.5** |                   **43.7** |

Some phases run concurrently, so the rows do not add up to the last one.

The front end, the first four rows, is 12 s of the library's compile and 35 s of the test binary's. It is the same code in both configurations. Code generation is the only part a backend can change, and at `opt-level=0` with no debug information LLVM is already quick at it: Cranelift is level on the test binary and slower on the library. Linking is one second.

### Edit loops, remote

The same edits, with the three configurations on the workers at once. Times are the execution time the worker reported for the action, two repeats each.

| Rebuilt action                                   | Stable LLVM (s) | Nightly LLVM (s) | Cranelift (s) |
| ------------------------------------------------ | --------------- | ---------------- | ------------- |
| `core:runtime` library                           | 14.3 · 14.7     | 11.5 · 15.5      | 13.7 · 40.6   |
| `core:engine_lib_test` binary                    | 39.3 · 52.0     | 36.3 · 56.3      | 33.3 · 39.6   |
| 47 compiles after a `boss-protocol` edit, summed | 417 · 323       | 366 · 361        | 440 · 350     |
| Same, end to end on the client                   | 218 · 118       | 149 · 127        | 217 · 161     |
| `core:work_crud_test` binary                     | 10.2 · 11.7     | 3.7 · 14.0       | 6.3 · 15.8    |

The workers are shared, and one action's time varies up to fourfold between repeats in the same configuration. No configuration is consistently ahead. The first full build of each nightly configuration does not settle it either: over the 648 compiles that executed in both, the workers spent 4,374 s with Cranelift and 5,121 s with LLVM, but the two builds ran at different times under different load, and the largest actions differed by factors from 0.35 to 2.0 in both directions.

## Other observations

- **Debug information.** For a probe compiled with `-Cdebuginfo=2`, LLVM describes 4 parameters, 6 local variables and 4 struct or enum types. Cranelift describes none of them. It emits function entries and a line table (32 rows against 31), so breakpoints and stepping work and inspecting a variable does not. mono's `fastbuild` uses `debuginfo=0`, so this matters only for `-c dbg`.
- **Binary size.** Unstripped `fastbuild` binaries are 10% to 17% larger with Cranelift: the engine 299 MB against 273 MB, `engine_lib_test` 233 MB against 210 MB, `boss` 75 MB against 64 MB, `checkleft` 164 MB against 140 MB. Stable and nightly LLVM agree to within 0.3%.
- **A second set of host tools.** Because the wasm guests stay on the stable compiler, a build that includes them compiles their proc macros and build scripts twice, once per compiler: 928 compile actions against 812 for the four-target build measured above. Engine-only builds do not pay this.
- **Cache separation is automatic.** The toolchain files and the flag are part of every action key, so Cranelift outputs never mix with LLVM outputs in the disk or remote cache. Switching the flag in one output base discards the analysis cache and rebuilds in place, so the config suits a checkout that stays on it.
- **A cosmetic mislabel.** rules_rust 0.70.0 maps nightly dates to versions with a table that ends in April 2026, so it labels this toolchain 1.97.0. The compiler is 1.99.0-nightly, and nothing in mono reads the label.

## Upstreaming

The patch is a reasonable upstream change as it stands. It is additive, it mirrors `dev_components`, and it is what [rules_rust #2555](https://github.com/bazelbuild/rules_rust/issues/2555) asks for without committing rules_rust to a backend-selection design. Upstream main (`5d99ff2e`, 2026-11-20) still has no equivalent. A pull request would need what this local patch leaves out: the generated documentation for the new attribute, a test in rules_rust's own suite, and checksums for the component in `known_shas.bzl` so that a pinned nightly is reproducible without the caller listing them.

One thing found on the way belongs in a separate upstream report. In the bzlmod toolchain hub, a `repository_set` toolchain loses the channel `target_settings` that the same toolchain gets when it is registered directly, in 0.70.0 and on main. mono works around it by naming the setting itself.

## Not measured

- **Linux.** No nightly toolchain is registered for Linux hosts and no Linux worker ran a Cranelift action. The component exists for both Linux architectures, and the unwinding limit applies there too because it comes from how the component is built.
- **Remote cold builds under equal conditions.** A cold remote build cannot be repeated without defeating the remote cache. The remote numbers above come from edits that are unique by construction and from the one natural first build of each nightly configuration.
- **`-c dbg` build times.** Cranelift usually gains most where LLVM has to emit debug information, and mono's inner loop does not.
- **A backend with unwinding enabled**, for the reason given above.
- **Runtime speed of the generated code.** Test execution times on the workers varied more between runs than between backends: 981 s against 946 s in total over the 137 targets that passed in both nightly configurations, with per-target ratios from 0.13 to 2.07.

## When to revisit

Revisit when the distributed backend is built with unwinding, which upstream ties to fixing the compile-time cost of that feature. Then move the nightly pin and run the correctness suite again. Without a gain in the edit loop, a passing suite alone would still not justify a nightly toolchain, so the second trigger is a change in what mono's inner loop costs, for example a decision to build with debug information by default.
