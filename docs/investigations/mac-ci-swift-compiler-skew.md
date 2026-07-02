# Recurring Mac CI Swift compiler-version skew (UpdateCore .swiftmodule)

## Symptom

`mac-app-build` intermittently fails while compiling
`//tools/boss/app-macos:boss_mac_app_lib`:

```
tools/boss/app-macos/Sources/BossMacApp.swift:4:8: error: compiled module was
created by a different version of the compiler '6.3.2.1.108'; rebuild
'UpdateCore' and try again:
    .../bin/tools/boss/app-macos/Sources/UpdateCore/UpdateCore.swiftmodule
```

It is "random": the same source passes on developer laptops and on some CI
runs, and fails on others. The failure correlates with cache/agent state, not
with the change under test.

## Root cause

A `.swiftmodule` is compiler-version-specific. Swift stamps every `.swiftmodule`
with the **swiftlang build id** of the `swiftc` that produced it and refuses to
import a module produced by any other build. That id is exactly the token in the
error — `swiftc --version` prints it verbatim:

```
Apple Swift version 6.3.3 (swiftlang-6.3.3.1.3 clang-2100.1.1.101)
                          ^^^^^^^^^^^^^^^^^^^^^
```

So `6.3.2.1.108` in the error is `swiftlang-6.3.2.1.108` — a build produced by a
different `swiftc` than the one that later tried to import the module.

Two `swiftc` builds end up interacting through Bazel's persistent
`--disk_cache`:

- **Per-agent Xcode upgrade.** An agent builds `UpdateCore` under `swiftc` X and
  writes `UpdateCore.swiftmodule` into the disk cache. Xcode is later upgraded
  to `swiftc` Y. A subsequent build gets a **cache hit** on X's module and feeds
  it to a `swiftc`-Y compile of `boss_mac_app_lib`, which rejects it.
- **Heterogeneous / shared agents.** Multiple `macos-arm64` agents on differing
  Xcodes populate (or share) one disk cache; a module built by one agent's
  `swiftc` is served to a build on another's.

### Why the existing safeguard did not catch it

`.buildkite/steps/ci-env.sh` already expunges on an Xcode version-string change,
and the `bazel()` wrapper retries once on `Xcode version ... is not available`.
Neither helps here:

- `bazel clean --expunge` clears only the **output base** (on Darwin CI the
  output base is on the internal disk — see `.ci.darwin.startup.bazelrc`). It
  does **not** touch `--disk_cache`, which lives on `/Volumes/ssd` and persists
  across upgrades. The poisoned `.swiftmodule` survives the expunge.
- The retry triggers on a different error string (`xcode-locator` / `Xcode
version ... is not available`), not on the `.swiftmodule` compiler-version
  mismatch.

The mixed config hashes in the log (`darwin_arm64-fastbuild` versus
`darwin_arm64-fastbuild-macos-arm64-min15.0-ST-*`) are a red herring for the
**compiler** skew: `UpdateCore` is built in both the default config and the
`rules_apple` platform-transitioned config, so there are two
`UpdateCore.swiftmodule` outputs — but each is only invalid when the disk cache
serves one built by a **different** `swiftc`. Same-compiler modules import fine
regardless of config.

## Fix

Partition the Darwin `--disk_cache` by the exact swiftlang build id. Each
`swiftc` gets its own cache directory
(`/Volumes/ssd/bazel/disk_cache/swiftlang-6.3.3.1.3`, and so on), so a module
built by compiler X can never be a cache hit for a build on compiler Y — the two
never share a directory. This is the "cache key must fully capture the compiler
version" fix at cache-partition granularity, and it is correct in **every** cache
topology (per-agent upgrade or a cache shared across heterogeneous agents),
independent of how finely Bazel's own action keys capture the toolchain.

Implementation:

- `.buildkite/steps/ci-env.sh` derives `SWIFT_BUILD_ID` from `swiftc --version`
  (falling back to the Xcode build id if parsing ever fails, so the path is
  never empty), exports `BAZEL_DARWIN_DISK_CACHE` pointed at the partitioned
  path, injects it into the `bazel()` wrapper (both the primary and retry
  invocations), and appends it to `REPOBIN_BAZEL_FLAGS` so repobin's own bazel
  calls use the same partitioned cache.
- `.ci.bazelrc` no longer sets a static `build:ci-darwin --disk_cache`; the
  dynamic value from `ci-env.sh` is the single source of truth (an explicit
  command-line `--disk_cache` overrides the base one from `.bazelrc`).

The existing expunge-on-Xcode-change and retry-once behavior are retained: they
address a **different** failure (stale `apple_cc_configure` paths in the output
base), which the disk-cache partitioning does not.

### What this deliberately does not do

- No blanket `bazel clean --expunge` on every run (slow, masks the cause).
- No CI auto-retry loop that hides the skew.
- No brittle `--xcode_version` pin. Pinning every agent to one Xcode via
  `xcodes` is the complementary **operational** half and is the right long-term
  guarantee, but it requires agent provisioning (not a repo change) and would
  red-CI on the next legitimate Xcode upgrade until every agent and the repo pin
  are bumped in lockstep. The cache partitioning above makes the build correct
  regardless of per-agent Xcode drift; the agent-side pin is tracked separately.

## Operational note (disk usage)

Each distinct `swiftc` build gets its own disk-cache subdirectory. Xcode
upgrades are infrequent, so stale sibling directories accumulate slowly; the
existing `--experimental_disk_cache_gc_*` settings GC within the active
directory. If `/Volumes/ssd` pressure ever becomes a concern, periodically prune
`/Volumes/ssd/bazel/disk_cache/swiftlang-*` directories not accessed recently.
