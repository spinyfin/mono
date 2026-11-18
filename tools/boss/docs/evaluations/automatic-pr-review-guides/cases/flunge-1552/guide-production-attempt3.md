# PR #1552: Faster Jest execution through transpilation and Bazel sharding

## 1. Problem being solved

This guide covers the complete comparison from `54e4e293` to `7db88ee2`. Links below pin source to those revisions.

The frontend’s Bazel test target previously ran the entire Jest suite in one process using the default `ts-jest` preset. The PR reports that repeated TypeScript program construction dominated runtime. It addresses that compilation overhead and adds parallel execution across test files.

The reported performance measurements are author results, not independently reproduced here: four in-band Bazel shards completed in **39.6–43.3 seconds**, compared with **97.9–102.4 seconds** when each shard also used four Jest workers. That comparison supports retaining one Jest process per shard; it does not independently establish the title’s overall **2.2×** improvement against the base revision.

## 2. Core fix / implementation

The change separates three responsibilities:

- **Jest transpiles and executes tests.** An explicit `ts-jest` transform replaces the preset and enables transpile-only operation.
- **Bazel schedules parallel work.** The target requests four shards while retaining `--runInBand`.
- **The existing frontend build checks TypeScript.** CI schedules that build alongside the Bazel tests.

The new sequencer connects Bazel’s runtime shard environment to Jest’s test selection. Each shard receives the full candidate file list, sorts it consistently, and keeps its own round-robin subset. Adding `shard_count` alone would not implement that selection.

The shared Jest configuration also places its cache under `TEST_TMPDIR` when that variable is set. Without the relevant environment variables, local Jest keeps its default sequencer and cache location—but still uses the new transpile-only transform. See [Jest configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L2-L34) and [Bazel target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L37-L64).

## 3. Implementation walkthrough and worked example

### Remove type-checking work from Jest

The decisive transform options are:

```js
isolatedModules: true,
tsconfig: {
  module: "CommonJS",
  moduleResolution: "Node",
  esModuleInterop: true,
},
```

These options select file-by-file transpilation and adapt the app’s ESM/bundler configuration to Jest’s CommonJS runtime. Other settings—including the Emotion JSX configuration—remain inherited from the application tsconfig. The transform pattern now matches JS/JSX as well as TS/TSX. See [the transform](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L9-L32) and [inherited compiler options](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/tsconfig.json#L2-L25).

The replacement type-checking path is verifiable: the dynamic pipeline emits both frontend steps under the same `need_frontend` condition; the npm step runs `npm run build`; that script starts with `tsc`; and tsconfig includes `src` without excluding TypeScript tests. This supports the comment’s claim for TS/TSX tests in `src`. It does **not** make a standalone Bazel Jest invocation a type check, nor enable JavaScript checking for the newly added `.js` test. See [CI scheduling](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/upload_dynamic_pipeline.sh#L146-L159), [CI commands](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/run_frontend_tests.sh#L9-L13), and [build script](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/package.json#L6-L11).

### Partition files when Bazel launches each shard

The [Bazel target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L37-L64) retains explicit test paths and `--runInBand`, adds the sequencer to runfiles, and sets `shard_count = 4`. Bazel can schedule four actions; Jest does not create another worker pool within each action.

The configuration installs the custom sequencer when `TEST_TOTAL_SHARDS` is truthy. On module load, the sequencer writes an empty `TEST_SHARD_STATUS_FILE`, if supplied, to acknowledge sharding support. Its core selection is:

```js
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

The comparator orders files by descending byte size, breaking ties with `path.localeCompare`. Failed file stats count as size zero. This distributes file counts evenly for valid shard indices, assuming every shard sees the same files and ordering; file size is only a runtime-cost heuristic. See [ordering and selection](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22-L52).

The guard is limited: it checks integer values and a positive total, but does not enforce `0 <= index < total`. Correct index bounds come from the expected Bazel environment.

### Worked example: distributing one candidate list

**Illustrative input**, applying the implemented algorithm: five test files have sizes `a=500`, `b=400`, `c=300`, `d=200`, and `e=100` bytes. Discovery returns them in any order.

At the [base revision](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L37-L58), one in-band Jest invocation runs all five files. Its execution order may depend on Jest’s default sequencer, but no Bazel partition removes any of them.

At the head revision, each action receives `TEST_TOTAL_SHARDS="4"` and its own zero-based `TEST_SHARD_INDEX`:

1. Sorting produces `[a, b, c, d, e]`.
2. Their positions modulo four are `[0, 1, 2, 3, 0]`.
3. The actions select:

| Shard index | Files executed |
| ----------- | -------------- |
| 0           | `a`, `e`       |
| 1           | `b`            |
| 2           | `c`            |
| 3           | `d`            |

The observable result is one execution of every file across four independently schedulable actions. Each action executes its selected files serially. This follows directly from the [modulo filter](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L44-L51).

A related boundary explains the regression test: `allFailedTests()` must not apply that filter again. With total `4`, index `1`, and the test’s nonexistent paths `[c, a, b]`, all sizes become zero. `sort()` returns `[b]`, while `allFailedTests()` returns `[a, b, c]`. Repartitioning an already selected list could silently drop files. The implementation therefore only orders that list:

```js
allFailedTests(tests) {
  return [...tests].sort(bySizeThenPath);
}
```

This method does **not** itself identify historical failures; it preserves every supplied entry. `cacheResults()` is also a no-op, so the custom sequencer does not maintain Jest’s normal timing/failure cache. See [these methods](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L60).

## 4. Tests and supporting infrastructure

**Added:** one test in [jest.shard-sequencer.test.js](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L1-L40). It sets total `4` and index `1`, restores the original environment afterward, and asserts both outcomes described above: ordinary sorting selects only `b`, while failed-test handling retains all three paths in order. Nonexistent paths exercise the stat-failure fallback without filesystem fixtures.

**Modified or removed tests:** none in this comparison.

**Infrastructure changes:** the shared transform becomes transpile-only, the Bazel target gains four shards and the sequencer runfile, and Jest’s cache moves under `TEST_TMPDIR` when provided. The existing `jest.setup.cjs` registration and runfile remain intact; its TextEncoder/TextDecoder polyfill is preserved, not newly introduced.

**Coverage limits:** the added test directly checks the two sequencer methods, not Jest’s complete failed-test execution path. It does not verify real-size balancing, the status-file handshake, or complete/disjoint selection across all four shards.

**Validation:** the author reports successful `bazel build //frontend:jest_test`, `bazel test //frontend:jest_test --test_output=errors`, and `checkleft run`, plus a 58-suite/449-test partition across four shards. I inspected the complete pinned comparison, supporting CI configuration, and matching installed Jest/ts-jest implementation; I did not run tests or reproduce timings.
