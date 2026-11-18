# Faster frontend tests through transpilation and four-way Bazel sharding

## Orientation

Developers waiting for frontend test results should get feedback sooner. This infrastructure change speeds up the frontend test runner by reducing repeated TypeScript compilation work and distributing test files across four concurrent runs. It changes how the tests execute, rather than changing the application interface.

Jest is the tool that executes the frontend tests; Bazel is the build system that launches them. A shard is one partition of the test files. Each shard runs its files sequentially in one Jest process, while Bazel manages concurrency between shards. The TypeScript adapter, ts-jest, now translates source files into executable JavaScript without checking their types during test execution. Type checking remains in the separate frontend build performed by continuous integration (CI), the automated build-and-test service.

A sequencer is the hook that selects and orders test files. The new sequencer activates when the shard-count environment variable is set; ordinary local runs retain the default Jest sequencer. The shared translation change also applies to local runs.

## Change overview

The previous target passed all discovered test paths to a single Jest process with `--runInBand`, meaning execution within that process was sequential. Its configuration used the standard `ts-jest` preset. The new target retains sequential execution within each process but requests four Bazel shards, and the shared Jest configuration explicitly enables file-by-file translation.

The two changes address different costs: translation avoids repeated TypeScript program construction identified by the author’s profiling, while sharding spreads test execution across processes. The PR also moves Jest cache data—saved work reused during execution—into the Bazel test temporary directory when that directory is supplied.

Sources: [previous target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58), [previous configuration](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/jest.config.cjs#L1-L7), [new target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64).

## Core implementation

The implementation divides responsibility among three files:

- [`frontend/BUILD`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64) requests four shards, retains `--runInBand`, and supplies the sequencer as a runtime file.
- [`jest.config.cjs`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L1-L34) configures translation, conditionally selects the sequencer, and conditionally relocates the cache.
- [`jest.shard-sequencer.cjs`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22-L63) acknowledges Bazel sharding, orders files by size, and selects the files belonging to the current shard.

The existing test discovery patterns still collect JavaScript and TypeScript test files, including the newly added sequencer test. Each Bazel invocation receives the discovered paths; the sequencer performs the actual selection. See [discovery patterns](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L6-L22) and [path arguments](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L40-L46).

## Implementation walkthrough

### 1. Translate files without repeating type checking

The explicit transform replaces the preset. Its central options are:

```js
isolatedModules: true,
tsconfig: {
  module: "CommonJS",
  moduleResolution: "Node",
  esModuleInterop: true,
},
```

[Transform source](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L17-L31).

CommonJS is the module-loading format used by this Jest runtime. The overrides adapt the application configuration, which otherwise targets a bundler, while retaining settings such as JSX—the syntax used to describe React elements. The existing [TypeScript configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/tsconfig.json#L3-L25) already requires compatibility with file-by-file translation and includes `src`.

Type checking is a separate build responsibility: the [frontend build command](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/package.json#L6-L10) starts with `tsc`, the TypeScript compiler, and the [CI script](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/run_frontend_tests.sh#L11-L13) runs that build before tests. The [pipeline emits both frontend steps under the same condition](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/upload_dynamic_pipeline.sh#L146-L159). Running Jest alone does not perform this separate check.

### 2. Turn the runtime shard assignment into a file selection

The [configuration condition](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L8) selects the custom sequencer when `TEST_TOTAL_SHARDS` is nonempty. On loading, the sequencer [writes an empty shard-status file](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22-L25) if its path is supplied; this is the acknowledgement that the runner supports Bazel sharding.

The [comparison helper](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L27-L37) sorts larger files first and breaks size ties by path. A failed file-size lookup counts as zero bytes. File size is a workload estimate, not a measurement of test duration.

The decisive selection is:

```js
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

[Selection source](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L44-L52).

Thus each shard takes every fourth entry from the same ordering. The guard falls back to the entire ordered list for an invalid total or noninteger index. It does not check that an integer index is within range; normal assignment relies on the Bazel environment.

### 3. Worked example: one complete run, four partitions

Consider an **illustrative** suite containing five files with distinct sizes: A = 500 bytes, B = 400, C = 300, D = 200, and E = 100.

Before this change, the [single in-band target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L46) supplied all five paths to one Jest process; all five ran there, with ordering delegated to the default sequencer.

After this change, [Bazel requests four shards](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L59-L63). Each receives the same paths and `TEST_TOTAL_SHARDS=4`, but a different zero-based `TEST_SHARD_INDEX`. The [size comparator and modulo selection](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L27-L51) produce:

| Shard index | Positions in size order | Files executed |
| ----------- | ----------------------- | -------------- |
| 0           | 0, 4                    | A, E           |
| 1           | 1                       | B              |
| 2           | 2                       | C              |
| 3           | 3                       | D              |

Every file belongs to exactly one partition under these inputs. A and E execute sequentially within their process; the other shard processes can execute alongside it. The observable change is concurrent execution of portions of the suite without running the entire suite four times.

Failed-test selection has a different contract: [`allFailedTests`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L60) only sorts its supplied list. It neither applies the modulo filter again nor consults saved failure history. This preserves all entries in an already-selected list.

### 4. Preserve setup and contain temporary data

The [cache override](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L33) places Jest data under `TEST_TMPDIR/jest-cache` when `TEST_TMPDIR` is nonempty; otherwise Jest keeps its default cache location. Separately, the custom sequencer has an empty `cacheResults()` method, so it does not persist test timing history.

The existing browser simulation, jsdom, and its setup file remain configured. That [unchanged setup](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.setup.cjs#L1-L11) supplies missing text-encoding APIs needed when React Router loads. The setup file remains among the [Bazel runtime files](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L47-L54).

## Tests and test infrastructure

**Added:** one test in [`jest.shard-sequencer.test.js`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25-L39), asserting that an already-sharded failed-test list is not partitioned again.

The test uses nonexistent paths in the order c, a, b. Their failed size lookups all become zero, so ordering resolves to a, b, c. With four shards and index 1, it asserts that `sort()` returns only b, while `allFailedTests()` returns all three paths in sorted order. Its [environment setup and cleanup](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L1-L23) set the shard variables and restore their original values afterward.

**Modified infrastructure:** the Bazel target gains the sequencer runtime file and four-way sharding; Jest gains explicit translation settings and conditional sequencer/cache configuration. Existing discovery includes the new JavaScript test without a separate target or fixture file.

**Modified or removed tests:** none.

## Evidence

The author reported the following verification commands:

- The author ran `bazel build //frontend:jest_test`.
- The author ran `bazel test //frontend:jest_test --test_output=errors`; the reported rebased target ran 58 suites and 449 tests, partitioned into 15/15/14/14 suites and 107/160/93/89 tests across the four shards, for an exact 449-test partition.
- The author ran `checkleft run`.

The author reported profiling that identified repeated per-file TypeScript program construction as the dominant cost. For the worker comparison, the author alternated the shipped `--runInBand` configuration with `--maxWorkers=4` after removing `--runInBand`. On that machine, in-band Bazel shards completed in **39.6–43.3 seconds**, while four Jest workers inside each shard took **97.9–102.4 seconds**. The author attributed the slower configuration to too many workers competing across four concurrently scheduled Bazel shards and retained in-band execution.

No screenshots are registered.
