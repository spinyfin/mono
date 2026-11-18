**Orientation.** PR #1552 changes the frontend test infrastructure across four files. It combines cheaper TypeScript transformation with four-way Bazel sharding. Application behavior is unchanged. This guide compares the supplied merge base with head `7db88ee`.

**1. The problem being solved**

The previous Bazel target ran the entire Jest suite in one process using `--runInBand`, with the standard `ts-jest` preset.

The PR’s profiling attributes the dominant cost to repeated TypeScript program construction. That work also overlapped with CI’s existing `tsc` build. The intended improvement is therefore twofold: reduce transformation overhead and distribute test execution across Bazel processes.

The description reports **39.6–43.3 seconds** with four in-band shards, versus **97.9–102.4 seconds** when each shard also used four Jest workers. Those measurements support retaining `--runInBand`; they are author-reported results, not measurements reproduced for this guide.

**2. The core implementation**

Start with [jest.config.cjs](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/frontend/jest.config.cjs:9). The preset is replaced by an explicit transform:

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

The decisive setting is `isolatedModules: true`: Jest transpiles files without performing TypeScript’s program-wide type-checking. The module overrides adapt the application’s ESM/bundler configuration to Jest’s CommonJS runtime; settings such as JSX handling remain inherited.

The safety argument depends on the **existing CI build**, not on a new type-check target:

- [The pipeline generator](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/.buildkite/scripts/upload_dynamic_pipeline.sh:146) schedules both frontend jobs under the same `need_frontend` condition.
- [Frontend Build and Test](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/.buildkite/scripts/run_frontend_tests.sh:11) runs `npm run build` before Jest; that build script begins with `tsc`.
- [tsconfig.json](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/frontend/tsconfig.json:13) already requires isolated-module compatibility and includes `src`, covering TypeScript test files.

Consequently, **Jest passing by itself no longer establishes type correctness**. This transform change applies to ordinary local Jest runs too; only the sharding configuration is conditional.

**3. Major logic and architecture changes**

[frontend/BUILD](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/frontend/BUILD:38) adds `shard_count = 4`, retains `--runInBand`, and includes the new sequencer in Bazel’s runtime files.

Each shard still receives the complete test-file argument list. The sequencer is what makes those four invocations execute different subsets. Merely adding `shard_count` would not implement that partition.

The config installs the sequencer when `TEST_TOTAL_SHARDS` is truthy:

```js
...(process.env.TEST_TOTAL_SHARDS
  ? { testSequencer: "<rootDir>/jest.shard-sequencer.cjs" }
  : {}),
```

In [jest.shard-sequencer.cjs](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/frontend/jest.shard-sequencer.cjs:22), the execution path is:

1. On module load, write an empty `TEST_SHARD_STATUS_FILE` if supplied. This acknowledges Bazel’s sharding protocol.
2. Sort suites by descending file size, breaking ties by path.
3. Assign sorted positions round-robin to the runtime shard index.

The decisive selection is:

```js
const total = Number(process.env.TEST_TOTAL_SHARDS);
const index = Number(process.env.TEST_SHARD_INDEX);
const ordered = [...tests].sort(bySizeThenPath);

if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

File size is a heuristic for execution cost, not a measured duration. A failed `statSync` contributes size zero. Invalid totals or noninteger indexes return the entire ordered list; the implementation otherwise relies on Bazel supplying a valid index within the shard range.

**Worked example — illustrative inputs, following the actual algorithm.**

Suppose six TypeScript suites arrive in this order:

```text
[c, a, f, b, e, d]
```

Their file sizes are respectively defined as `a=600`, `b=500`, `c=400`, `d=300`, `e=200`, and `f=100` bytes.

**Before:** one Bazel Jest invocation runs all six suites sequentially, using the default sequencer and the preset transformation. Its exact ordering can depend on Jest’s default sequencing, but there is no Bazel partition.

**After:** Bazel starts four invocations with `TEST_TOTAL_SHARDS=4` and indexes `0` through `3`. Every invocation computes the same size order:

```text
[a, b, c, d, e, f]
```

The modulo filter produces:

| Shard index | Sorted positions retained | Suites executed |
| ----------- | ------------------------- | --------------- |
| 0           | 0, 4                      | a, e            |
| 1           | 1, 5                      | b, f            |
| 2           | 2                         | c               |
| 3           | 3                         | d               |

For shard 1, the decisive checks are `1 % 4 === 1` and `5 % 4 === 1`, so it executes only `b` and `f`, sequentially, using transpile-only transformation.

The observable result is six suites executed exactly once across four independently schedulable processes. Parallelism occurs between shards. Neither equal durations nor a particular speedup follows from the partition alone.

Two supporting changes complete the design:

- **Cache lifecycle:** Jest’s cache moves to `${TEST_TMPDIR}/jest-cache` when that variable exists. Otherwise Jest keeps its default cache location. The custom sequencer’s `cacheResults()` is a no-op, so it does not persist timing-based ordering.
- **Compatibility:** the existing `jest.setup.cjs` TextEncoder/TextDecoder polyfill remains configured and included in Bazel’s runtime files. This preserves the setup needed by React Router in jsdom.

**4. Added tests and supporting infrastructure**

The only added test is in [jest.shard-sequencer.test.js](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1552/frontend/src/jest.shard-sequencer.test.js:25). It protects against partitioning an already-selected failed-test list a second time.

It sets four shards and index 1, then supplies:

```js
[{ path: "/missing/c.test.js" }, { path: "/missing/a.test.js" }, { path: "/missing/b.test.js" }];
```

Because these paths are missing, all sizes fall back to zero and path ordering yields `a, b, c`. The assertions deliberately distinguish two methods:

```js
sequencer.sort(tests); // retains only b
sequencer.allFailedTests(tests); // retains a, b, c, in order
```

These are separate calls with the same fixture. The second models a list that has already been selected for this shard: applying the modulo filter again would discard entries. Accordingly, `allFailedTests()` only sorts its input; it does not repartition it or consult failure-history data. The test restores the original shard environment afterward.

The existing BUILD glob automatically discovers this new `.test.js` file. No separate test target or CI job is added.

Coverage is narrow: the test exercises the missing-file fallback, path ordering, shard filtering, and failed-list preservation. It does not directly test real file-size balancing, status-file creation, configuration selection, or the transpile-only/type-checking separation.

I confirmed **58 matching test files**, which distribute as **15/15/14/14** under round-robin assignment. The PR reports **449 executed tests**, partitioned **107/160/93/89**, and lists Bazel build, Bazel test, and `checkleft run` verification. I inspected the source but did not rerun those checks in this read-only checkout.

https://github.com/brianduff/flunge/pull/1552
