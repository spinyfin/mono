# Faster frontend test feedback with transpile-only Jest and Bazel sharding

## Orientation

Developers waiting for frontend tests get faster feedback from this infrastructure change. The PR speeds up the frontend test target by reducing repeated compilation work and dividing the tests into four independently scheduled groups. It changes the test-running system rather than application behavior.

Jest is the tool that executes the frontend tests; Bazel is the build tool that schedules those executions. A **shard** is one assigned portion of the complete test suite. **Transpilation** converts TypeScript into executable JavaScript without checking whether its types are correct. This PR uses that faster conversion during tests while retaining type checking in the existing continuous integration (CI) build step.

A **sequencer** is the Jest component that orders and selects test files. The new sequencer translates Bazel assignments into file selections, while ordinary local runs retain the default sequencer. The complete change also preserves the existing browser-test setup and ensures that an already-selected failed-test list is not divided again.

## Change overview

The previous target ran all discovered test files through one Jest process using the standard `ts-jest` preset. The new target keeps one process per shard, but requests four shards and replaces the preset with an explicit transpile-only transform. The author attributes the original bottleneck to repeated construction of TypeScript programs.

The behavior divides into two independent changes:

- Compilation becomes file-by-file for all runs using this Jest configuration, including local runs.
- File partitioning is enabled only when `TEST_TOTAL_SHARDS` has a nonempty value. Without that environment setting, Jest keeps its default sequencing.

The [before configuration](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/jest.config.cjs#L1-L7), [before target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58), and [new configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L2-L34) show those boundaries. Adding local `npm test` sharding is explicitly outside the PR scope.

## Core implementation

The implementation connects three pieces:

1. [The Bazel target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64) requests four shards, retains `--runInBand`—execute tests within one Jest process—and packages the new sequencer as a runtime dependency.
2. [The Jest configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L33) enables transpile-only conversion, conditionally selects the sequencer, and places temporary Jest cache data under the Bazel-provided directory.
3. [The sequencer](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22-L60) acknowledges sharding support, orders files by size, and distributes that ordering across the requested shards.

Bazel supplies the shard count and index when each test action executes. Reading those values in the sequencer lets every action receive the same test-file arguments while executing only its assigned subset.

## Implementation walkthrough

### Remove repeated type-checking work from test execution

The [transform options](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L17-L31) contain the central compilation change:

```js
isolatedModules: true,
```

The same options override the application settings with `module: "CommonJS"`, `moduleResolution: "Node"`, and `esModuleInterop: true`. CommonJS is the module-loading format used by this Jest runtime. The application configuration instead targets a bundler and already enables `isolatedModules`, which constrains TypeScript code for independent file conversion; its other settings, including React syntax handling, remain inherited ([application configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/tsconfig.json#L3-L24)).

Type checking remains a separate build responsibility. The unchanged [CI script](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/run_frontend_tests.sh#L11-L13) runs `npm run build` before `npm run test`; [the build command](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/package.json#L6-L10) starts with `tsc`, the TypeScript compiler. Its configuration includes `src`, including TypeScript test files. The [pipeline](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/upload_dynamic_pipeline.sh#L146-L159) schedules both frontend steps under the same condition. Running Jest alone does not invoke that build check.

### Assign files at runtime, with one level of parallelism

At module load, the sequencer writes an empty file at `TEST_SHARD_STATUS_FILE` when supplied. This is the acknowledgement Bazel expects from a runner that supports sharding ([protocol and acknowledgement](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L5-L25)).

The [ordering helper](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L27-L37) sorts larger files first and breaks equal-size ties by path. A failed file-size lookup counts as zero. File size serves as an estimate of execution cost, rather than a measured duration.

The decisive [selection logic](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L44-L51) is:

```js
const total = Number(process.env.TEST_TOTAL_SHARDS);
const index = Number(process.env.TEST_SHARD_INDEX);
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

This copies the input before sorting and assigns every fourth position to each shard in the shipped configuration. It falls back to the full ordered list for the invalid values checked above. It assumes Bazel supplies an index in range: an integer index outside `0..total-1` selects no files.

**Worked example — illustrative file names and sizes.** Suppose eight discovered files, A through H, have descending sizes of 8 through 1 KB.

Before the change, the [single in-band target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58) passes all eight files to one execution. After the change, [four Bazel shards](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L59-L64) each receive that list. The helper orders it A–H, and the modulo filter assigns:

| Shard index | Sorted positions | Files executed |
| ----------- | ---------------- | -------------- |
| 0           | 0, 4             | A, E           |
| 1           | 1, 5             | B, F           |
| 2           | 2, 6             | C, G           |
| 3           | 3, 7             | D, H           |

Every file belongs to exactly one shard for these valid settings. The observable difference is four independently schedulable groups instead of one serial group; each group still executes in-band. The allocation equalizes file counts here, not necessarily execution times.

### Preserve failed-test selections and runtime setup

The [failed-test hook](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L60) deliberately only sorts its input:

```js
allFailedTests(tests) {
  return [...tests].sort(bySizeThenPath);
}
```

Applying the modulo filter again would discard entries from an already-sharded selection. This method returns every supplied entry; it does not independently determine which tests failed. The timing-persistence hook, `cacheResults()`, is empty.

Separately, the [Jest cache directory](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L33) becomes `${TEST_TMPDIR}/jest-cache` when that environment variable is set. Otherwise the configuration leaves the default location in place.

The configuration still loads [`jest.setup.cjs`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L8), and [Bazel still packages it](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L47-L54). This unchanged setup supplies missing text-encoding APIs in jsdom, the simulated browser environment used by the tests, so React Router can load ([setup implementation](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.setup.cjs#L1-L11)).

## Tests and test infrastructure

**Added:** one [sequencer regression test](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25-L39). With four shards and index 1, it supplies missing paths in the order c, a, b. Their failed size lookups produce equal zero sizes, so path ordering yields a, b, c. It asserts that `sort()` returns only b, while `allFailedTests()` preserves all three in sorted order. This directly exercises the distinction between partitioning a full list and ordering an already-selected list.

The [test setup](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L3-L23) saves the original environment values, sets the shard inputs before the test, and restores or deletes them afterward.

**Modified or removed tests:** none. Infrastructure changes are the transform, conditional sequencer, temporary cache location, four-shard target, and sequencer runtime dependency. The existing [test-file glob](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L6-L22) already includes the new JavaScript test.

## Evidence

The author ran `bazel build //frontend:jest_test`, `bazel test //frontend:jest_test --test_output=errors`, and `checkleft run` as the reported verification commands.

The author reported that profiling identified repeated per-file TypeScript program construction as the dominant cost. The rebased target ran **58 suites and 449 tests**, partitioned into **15/15/14/14 suites** and **107/160/93/89 tests**, totaling exactly 449 tests.

The author alternated the shipped in-band configuration with `--maxWorkers=4` after removing `--runInBand`; result: **39.6–43.3 seconds** for in-band Bazel shards versus **97.9–102.4 seconds** with four Jest workers inside every shard. The author attributed the slower configuration to oversubscribing the four concurrently scheduled Bazel shards and retained in-band execution.

The change added the regression test asserting that failed-test selection preserves an already-sharded list.

No screenshots are registered.
