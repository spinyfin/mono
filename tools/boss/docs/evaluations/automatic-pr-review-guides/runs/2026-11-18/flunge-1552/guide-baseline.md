**1. The problem being solved**

[PR #1552](https://github.com/brianduff/flunge/pull/1552) reduces the cost of running frontend tests. It changes test execution infrastructure; application behavior is unchanged.

Previously, the Bazel target ran the entire Jest suite in one process with `--runInBand`, using the default `ts-jest` preset. The PR’s profiling identified repeated TypeScript program construction as the dominant cost.

The solution removes type-checking work from Jest and distributes test files across four Bazel shards. The PR reports 58 suites and 449 tests, with the shipped configuration completing in **39.6–43.3 seconds**. These are the author’s measurements; I did not rerun the benchmarks.

**2. The core implementation**

Start with [frontend/jest.config.cjs](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L9). The old configuration’s `preset: "ts-jest"` becomes an explicit transform:

```js
"^.+\\.[tj]sx?$": [
  "ts-jest",
  {
    isolatedModules: true,
    tsconfig: {
      module: "CommonJS",
      moduleResolution: "Node",
      esModuleInterop: true,
    },
  },
],
```

The decisive setting is **`isolatedModules: true` on the ts-jest transformer**: Jest transpiles files individually without performing TypeScript type-checking. The module overrides align emitted code with Jest’s CommonJS runtime, while retaining application settings such as the JSX configuration.

The existing application tsconfig already requires isolated-module compatibility. Type-checking remains in the separate CI build:

- [run_frontend_tests.sh](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/run_frontend_tests.sh#L11) runs `npm run build` before `npm run test`.
- The build script starts with `tsc`, and [tsconfig.json](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/tsconfig.json#L24) includes `src`, including TypeScript tests.
- [The pipeline](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/upload_dynamic_pipeline.sh#L146) schedules both frontend CI steps under the same `need_frontend` condition.

This changes the testing contract: **a successful Jest run alone no longer establishes TypeScript correctness**. CI still checks it through the build. The transform change also applies to ordinary local Jest runs.

**3. Major logic and architecture changes, with a worked example**

The next review stop is [frontend/BUILD](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38). It adds `shard_count = 4` and includes the new sequencer in Bazel runfiles. It retains `--runInBand`, so Bazel owns parallel execution and each shard runs its assigned files sequentially.

Every shard receives the test-file list. The new [jest.shard-sequencer.cjs](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22) turns that list into a partition:

```js
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

The surrounding logic matters:

- **Ordering:** descending file size, then path via `localeCompare`. Failed file-stat calls count as size zero. File size is a cost estimate, not measured execution time.
- **Assignment:** zero-based position modulo shard count determines ownership. With valid Bazel indices, each file belongs to exactly one shard.
- **Protocol handshake:** loading the module writes an empty `TEST_SHARD_STATUS_FILE`, when supplied, to advertise sharding support to Bazel.
- **Activation:** the Jest config selects this sequencer only when `TEST_TOTAL_SHARDS` is truthy. Bare local runs retain Jest’s default sequencer.
- **Caching:** `cacheResults()` does nothing; the custom ordering does not use Jest’s historical timing data. Separately, Jest’s cache goes under `${TEST_TMPDIR}/jest-cache` when that variable is present, keeping Bazel-run cache files within its temporary directory.

For a concrete before/after, consider these **illustrative inputs**, assuming six existing test files with the indicated sizes and no prior cached transforms:

| File        |      Size | Position after sorting | New shard index |
| ----------- | --------: | ---------------------: | --------------: |
| `a.test.ts` | 600 bytes |                      0 |               0 |
| `b.test.ts` | 500 bytes |                      1 |               1 |
| `c.test.ts` | 400 bytes |                      2 |               2 |
| `d.test.ts` | 300 bytes |                      3 |               3 |
| `e.test.ts` | 200 bytes |                      4 |               0 |
| `f.test.ts` | 100 bytes |                      5 |               1 |

**Before:** the single Bazel test action ran all six files in-band, using the default `ts-jest` transformation path. There was no Bazel partition; the exact default ordering is immaterial to this example.

**After:** Bazel creates four shard actions. For the action with `TEST_TOTAL_SHARDS="4"` and `TEST_SHARD_INDEX="1"`:

1. Jest loads the custom sequencer, which acknowledges the Bazel sharding protocol.
2. Sorting produces the order shown above.
3. The filter retains positions `1` and `5`, because both satisfy `i % 4 === 1`.
4. This process runs only `b.test.ts` and `f.test.ts`, sequentially, with transpile-only transformation.

Across all four actions, every file still executes once. The observable change is concurrent execution of four disjoint groups, with less TypeScript work inside Jest.

Keeping `--runInBand` is deliberate. The PR reports that allowing four Jest workers **inside each of the four shards** increased runtime to **97.9–102.4 seconds**, consistent with oversubscribing the machine. Sharding is by file, so equal suite counts do not guarantee equal workloads: the reported real partition is 15/15/14/14 suites but 107/160/93/89 individual tests.

**4. Tests added and supporting infrastructure**

There is **one new regression test**, in [frontend/src/jest.shard-sequencer.test.js](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25). It protects a subtle distinction: initial test selection partitions the list, whereas failed-test handling must not partition an already-selected list again.

The test sets total shards to `"4"` and index to `"1"`, then supplies these nonexistent paths:

```js
[{ path: "/missing/c.test.js" }, { path: "/missing/a.test.js" }, { path: "/missing/b.test.js" }];
```

All stat calls fall back to size zero, so ordering becomes `a, b, c`. The assertions establish:

```text
sort(tests)           → [b]
allFailedTests(tests) → [a, b, c]
```

The corresponding implementation is intentionally just an ordering operation:

```js
allFailedTests(tests) {
  return [...tests].sort(bySizeThenPath);
}
```

Calling `sort()` here would incorrectly shrink this list to `[b]`. The test restores the original shard environment variables afterward. It also exercises the missing-file fallback and path tie-break, though it does not independently validate real-size balancing or the Bazel status-file handshake.

The infrastructure changes supporting this are the explicit transformer, conditional sequencer, four-shard Bazel target, sequencer runfile, and temporary cache location. Existing test-file globs already discover the new `.test.js` file. The existing TextEncoder/TextDecoder setup remains configured and available in Bazel runfiles, preserving React Router’s ability to load in jsdom.

The PR lists successful Bazel build, Bazel test, and `checkleft run` verification. This guide is based on the supplied base-to-head diff and source inspection, rather than a fresh execution of those checks.
