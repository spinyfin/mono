# Docs-only PR re-executes 30 engine tests in CI (2026-10-21)

## Question

A PR that changed exactly one file,
`tools/boss/docs/investigations/hub-and-spoke-distributed-engine-2026-10-21.md`,
made the `bazel-build-test` job (Buildkite build 21819) recompile
`engine_lib` and execute 30 of 144 test targets, including the 11-shard
`//tools/boss/engine/core:engine_lib_test`. Bazel caching should have made
that build a near no-op. Why didn't it?

## Answer in one paragraph

Nothing pulls the markdown file into the build graph. The misses come from
`stable-status.txt`: `tools/boss/installer/workspace-status.sh` writes the
full commit sha into it, and two independent paths turned "the sha changed"
into "recompile and re-test":

1. `//tools/boss/installer:build_provenance_rs` reads the sha out of
   `stable-status.txt` into a generated Rust source file, that file is the
   body of the `boss_build_provenance` crate, and `engine_lib` linked that
   crate. A Rust crate's compiled output changes whenever its source
   changes, so every commit — docs-only or not — changed an input of
   `engine_lib`'s compile, of every `rust_test(crate = ":engine_lib")`
   compile, and of every binary linking `engine_lib`. The 30 executed
   tests are exactly the Linux-runnable subset of
   `tests(rdeps(//..., //tools/boss/build-provenance))`.
2. `.bazelrc` set `build --stamp` globally. rules_rust's `rust_binary`
   defaults to `stamp = -1` (follow `--stamp`), and a stamped `Rustc`
   action takes `stable-status.txt` itself as an input
   (`rules_rust/rust/private/rustc.bzl`, `stamp_info = [ctx.version_file,
ctx.info_file] if stamp else []`). So every `rust_binary` in the repo
   (`boss`, `bossctl`, `cube`, `checkleft`, `repobin`, `hood`, `release`,
   `changelog`, …) recompiled on every commit. Those recompiles happened to
   reproduce byte-identical binaries (nothing reads the substitutions), so
   downstream tests stayed cached, but the compile time was paid on every
   build and the test cache was one non-deterministic byte away from
   missing too.

The candidate explanations that were ruled out: a glob/`data`/`compile_data`
covering `tools/boss/docs/**` (the file is not in any target), a per-agent
cache (same-commit reruns on the same host executed 0 tests), a stale
merge-base (the executed set matches the stamped-crate dependents, and a
Swift-only `main` commit built on `main` shows the same 31 executions).

## Evidence

### The markdown file is not an input to anything

```
$ bazel query 'rdeps(//..., "//tools/boss/docs:investigations/hub-and-spoke-distributed-engine-2026-10-21.md")'
ERROR: no such target '//tools/boss/docs:investigations/hub-and-spoke-distributed-engine-2026-10-21.md':
target 'investigations/hub-and-spoke-distributed-engine-2026-10-21.md' not declared in package 'tools/boss/docs'
```

`//tools/boss/docs/BUILD.bazel` exports two individual design docs that
`engine_lib` `include_str!`s; there is no glob over `docs/`.

### Buildkite: what executed, on which host, against which cache

`bazel-build-test` runs on the mixed `bazel-any` queue; each host has its
own disk cache (`/mnt/ssd/bazel/disk_cache`). `main` is rebuilt every 15
minutes by schedule, so the same commit is built repeatedly on different
hosts, which is a natural experiment:

| Build | Branch / commit                                               | Host       | Executed tests | Note                                   |
| ----- | ------------------------------------------------------------- | ---------- | -------------- | -------------------------------------- |
| 21798 | main `929afbe3f8` (Swift-only change, excluded from this job) | zoologist  | 31 / 144       | first build of that commit on the host |
| 21800 | main `929afbe3f8`                                             | zoologist  | 0 / 144        | same commit, same host: fully cached   |
| 21802 | main `929afbe3f8`                                             | empiricist | 65 / 144       | host was several `main` commits behind |
| 21810 | main `929afbe3f8`                                             | anaplian   | 0 / 144        | repeat on a warm host                  |
| 21819 | PR 3232 `412ab1ab`, docs-only, based on `721f4d34`            | empiricist | 30 / 144       | the build in question                  |

Build 21819's build phase (before any test ran) already shows the shape:
`Compiling Rust rlib engine_lib (686 files)` and `Compiling Rust bin
engine_lib_test (686 files)` with 55 sandboxed actions and a 120 s critical
path, for a commit whose only Rust-relevant difference from the host's
cached `main` was the commit sha.

The 30 tests build 21819 executed (from the job log, minus `(cached)`
lines) are a strict subset of

```
bazel query 'tests(rdeps(//..., //tools/boss/build-provenance))'
```

(45 targets; minus 5 `manual` live/integration tests and the 2 macOS-only
ones that this job excludes). The 8 dependents that still reported
`(cached)` (`bossctl_test`, `review_start_test`, `cli:boss_test`,
`engine_retry_test`, `shake_test`, `uninstall_test`,
`editorial_pretooluse_test`, `planner_e2e_fixtures_test`) relinked but came
out byte-identical because nothing in them keeps the stamped strings alive;
`engine_lib_test` does (`build_info` has tests that call `git_sha()`), so its
binary digest — an input of the test action — changed, and all 11 shards
re-ran. Build 21798 executed the same 30 plus `tmux_only_surface_test`.

### The action chain, from Bazel's own graph

Actions in `deps(engine_lib_test)` that read `stable-status.txt`:

```
$ bazel aquery 'inputs(".*stable-status.txt", deps(//tools/boss/engine/core:engine_lib_test))'
  Mnemonic: BuildInfoRs          Target: //tools/boss/installer:build_info_rs
  Mnemonic: BuildProvenanceRs    Target: //tools/boss/installer:build_provenance_rs
  Mnemonic: Rustc                Target: //tools/repobin:repobin        <- path 2 (--stamp)
  Mnemonic: TranslateBuildInfo   Target: @bazel_tools//tools/build_defs/build_info:cc_build_info
```

(`build_info_rs` is byte-stable by design — it reads only the base version —
so it does not propagate. `repobin` is in `engine_lib_test`'s `data`.)

Actions consuming the stamped crate's outputs:

```
$ bazel aquery 'inputs(".*(build_provenance_generated\.rs|libboss_build_provenance).*", deps(//tools/boss/engine/core:engine_lib_test))'
  Rustc  //tools/boss/build-provenance:build-provenance
  Rustc  //tools/boss/engine/core:engine_lib
  Rustc  //tools/boss/engine/core:engine_lib_test
```

Across the whole Linux CI target set, 18 actions read `stable-status.txt`:
the two generators above, `TranslateBuildInfo`, and 15 `Rustc` actions, one
per `rust_binary` (`boss`, `bossctl`, `engine`, `engine_fixture_test`,
`boss-event`, `changelog`, `checkleft`, `checkleft_test_support_bin`,
`precompile_cwasm`, `checkleft_package`, `cube`, `hood`, `release`,
`repobin`, rules_rust's `rustfmt` wrapper).

### Local reproduction: change only the commit sha

Build `engine_lib_test` once, then `jj describe` (which rewrites the
working-copy commit id and nothing else) and build again with
`--explain --verbose_explanations`:

```
Executing action 'BazelWorkspaceStatusAction stable-status.txt': unconditional execution is requested.
Executing action 'Generating build_provenance_generated.rs': action changed since cached execution.
Executing action 'Generating build_info_generated.rs': action changed since cached execution.
Executing action 'Compiling Rust bin repobin (15 files)': action changed since cached execution.
Executing action 'Compiling Rust rlib build-provenance (1 file)': action changed since cached execution.
Executing action 'Compiling Rust bin engine_lib_test (689 files)': action changed since cached execution.
Executing action 'Compiling Rust rlib engine_lib (689 files)': action changed since cached execution.
```

Digests before/after the sha bump: `stable-status.txt`,
`build_provenance_generated.rs` and `libboss_build_provenance-*.rlib`
changed; `libboss_protocol-*.rlib` (control) did not. The changed rlib is
listed verbatim in `engine_lib`'s and `engine_lib_test`'s `Rustc` inputs
(`--extern=boss_build_provenance=.../libboss_build_provenance-….rlib`).

`volatile-status.txt` (build time) is not the problem: Bazel excludes it
from action keys, which is why `BUILD_TIME` was already routed there.

## Fix

1. **The stamped crate became a leaf that only the `engine` binaries
   link.** `engine_lib` no longer depends on `//tools/boss/build-provenance`;
   `build_info` holds a write-once `Provenance` cell, and `main.rs` (shared
   by `engine` and `engine_fixture_test`) installs the stamped values via
   `build_info::install_provenance` as its first statement. Library code,
   `boss`, `bossctl` and every test read `"unknown"`/`false` — the same
   placeholders a Cargo build already produced — unless a binary installed
   the real thing. The engine binary still embeds and reports its real
   sha/version/dirty/build-time exactly as before.
2. **`build --stamp` is no longer set for ordinary builds.** The
   workspace-status script still runs on every build and the custom rules
   (`build_info_rs`, `build_provenance_rs`, `boss_short_version_plist`,
   `boss_pkg_unsigned`) read `ctx.info_file` unconditionally, so dev builds
   still show `1.0.N-dev-<sha>` in About and the engine still gets stamped.
   Only rules_rust's per-binary stamping — which nothing used — goes away.
   `--config=release` keeps `--stamp`.

## What a docs-only commit executes now

Same experiment with the fix applied (`jj describe` to change only the
commit id, then `bazel build --explain` of `engine_lib_test`, `engine`,
`boss`, `bossctl`, `repobin`, `cube`, `checkleft`):

```
Executing action 'BazelWorkspaceStatusAction stable-status.txt': unconditional execution is requested.
Executing action 'Generating build_info_generated.rs': action changed since cached execution.
Executing action 'Generating build_provenance_generated.rs': action changed since cached execution.
Executing action 'Compiling Rust rlib build-provenance (1 file)': action changed since cached execution.
Executing action 'Compiling Rust bin engine (1 file)': action changed since cached execution.
```

(`build_info_generated.rs` is regenerated but byte-identical, so nothing
downstream of it moves.) `engine_lib`, `engine_lib_test`, `boss`, `bossctl`,
`repobin` and `cube` are action-cache hits; the new sha appears in the
`engine` binary and in neither `engine_lib_test` nor `boss`. A subsequent
`bazel test` reports `engine_lib_test` as `(cached) PASSED`.

So a commit that changes only the sha now re-runs: `BuildProvenanceRs`,
the one-file `boss_build_provenance` compile, the `engine` /
`engine_fixture_test` link, and one test target,
`//tools/boss/build-provenance:build-provenance_test` (it genuinely
depends on the stamp; it takes under a second). On macOS the Swift
`EngineReconnectIntegrationTests` also re-runs, because it has
`engine_fixture_test` in its data. Everything else is a cache hit.

## Guard

`//tools/boss/engine/core:stamped_provenance_isolation_test` is a `sh_test`
over two `genquery` outputs evaluated against the real dependency graph at
build time: `deps(engine_lib_test)` filtered to the stamped targets must be
empty, and — as a positive control so a rotted regex cannot pass silently —
`deps(engine)` filtered the same way must be non-empty. Reintroducing the
edge fails that test on the offending PR.
