# PR #1552: Faster Jest transforms and four Bazel-owned shards

## 1. Problem being solved

The frontend Jest target previously ran its full test list in one process using the `ts-jest` preset. The PR addresses two sources of elapsed time: TypeScript transformation overhead and serial execution of the suite. The author reports that profiling identified repeated TypeScript program construction as the dominant cost; the supplied source establishes the old configuration, but does not include those profiles. See the [base Jest configuration](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/jest.config.cjs#L1-L7) and [base Bazel target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58).

This guide covers the complete comparison from `54e4e293` to `7db88ee2`. Links below pin the supplied source revisions; exact diff anchors were not provided.

## 2. Core fix / implementation

The change combines two independent optimizations:

- **Transpile files without Jest-side type checking.** An explicit `ts-jest` transform replaces the preset and enables `isolatedModules`.
- **Partition test files across four Bazel shards.** A custom sequencer reads Bazel’s runtime shard environment and selects each shard’s files. Each Jest process retains `--runInBand`, so Bazel controls parallelism.

The [transform configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L9-L31) contains the essential compiler settings:

```js
"^.+\\.[tj]sx?$": [
  "ts-jest",
  {
    isolatedModules: true,
    // ...
    tsconfig: {
      module: "CommonJS",
      moduleResolution: "Node",
      esModuleInterop: true,
    },
  },
],
```

The CommonJS overrides adapt the transform to Jest’s runtime. The surrounding comment says other compiler settings are inherited from the application configuration.

**Type checking is now a separate responsibility.** The PR description and configuration comment say CI runs `tsc` over all of `src`, including tests, under the same frontend-change condition. Neither that CI configuration nor the TypeScript configuration is supplied, so this retained coverage cannot be independently verified here. The Jest configuration itself does not enforce that a separate type-check runs.

## 3. Walkthrough of the implementation

### Start with the Bazel execution boundary

The [Bazel target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64) still passes the globbed test paths with `--runTestsByPath` and retains `--runInBand`. It adds:

```python
"jest.shard-sequencer.cjs",  # in data
```

and:

```python
shard_count = 4,
```

Thus each shard receives the target’s test list; the sequencer performs the actual partition. Adding the `.cjs` file explicitly to runfiles matters because the existing JavaScript/TypeScript source glob does not include that extension.

### Activate the sequencer from runtime state

[Jest configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L8) installs the custom sequencer conditionally:

```js
...(process.env.TEST_TOTAL_SHARDS
  ? { testSequencer: "<rootDir>/jest.shard-sequencer.cjs" }
  : {}),
```

The condition is a truthy environment value, not independent detection of Bazel. With that variable absent, bare Jest retains its default sequencer; the new transform still applies.

When the sequencer module loads, it [writes the shard-status file](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L22-L25), if supplied:

```js
const statusFile = process.env.TEST_SHARD_STATUS_FILE;
if (statusFile) {
  fs.writeFileSync(statusFile, "");
}
```

This acknowledges sharding support. It happens before selection and is not validation of the shard parameters.

### Order once, then distribute by position

The [ordering helper](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L27-L37) sorts larger files first, breaking size ties by path. Failed filesystem lookups count as size zero.

The [core partition](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L44-L52) is:

```js
const total = Number(process.env.TEST_TOTAL_SHARDS);
const index = Number(process.env.TEST_SHARD_INDEX);
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

For four shards, shard zero receives positions `0, 4, 8…`; shard one receives `1, 5, 9…`. With identical ordering and valid indices, this partitions the list without overlap. File size is a cost heuristic, so equal runtime is not guaranteed.

Validation is limited: invalid totals or noninteger indices return the full ordered list, but integer indices outside `0…total-1` produce an empty selection.

### Worked example: selection and failed-test preservation

Use the [added test’s synthetic fixture](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L7-L10), with its [inputs and assertions](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25-L38):

```text
TEST_TOTAL_SHARDS = "4"
TEST_SHARD_INDEX  = "1"
input paths: /missing/c.test.js, /missing/a.test.js, /missing/b.test.js
```

These are missing-file test objects, not executable suites.

1. **Before:** the Bazel target had no sharding, and Jest had no custom partition. There was no shard-selection step reducing the target’s test list; its default ordering is not established by the supplied code.
2. **After:** each size lookup fails and returns zero. Path ordering produces `[a, b, c]`.
3. Positions are `[0, 1, 2]`. Only position `1` satisfies `i % 4 === 1`, so `sort(tests)` returns **`[b]`**, exactly as asserted.
4. For the failed-test hook, the contract differs: an already-selected list must retain its membership. [`allFailedTests(tests)`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L56) returns **`[a, b, c]`**. Reusing `sort()` there would incorrectly discard `a` and `c`.

The test directly proves those two method results. Jest’s caller is not supplied, so the statement that the failed-test hook receives an already-sharded list comes from the intended integration contract, rather than a traced caller. The method itself only orders its input; it does not identify failures.

### Keep temporary state scoped to Bazel

The [cache setting](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L32-L34) uses `${TEST_TMPDIR}/jest-cache` when that variable is truthy; otherwise Jest chooses its default location. Separately, the custom sequencer’s [`cacheResults()` is empty](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L58-L60), so it does not persist sequencing timings.

## 4. Tests and supporting infrastructure

**Added:** one regression test asserts that `sort()` partitions the fixture while `allFailedTests()` preserves all three entries in sorted order. It sets shard variables before the test and restores their original values afterward. Missing paths exercise the zero-size fallback without creating fixture files. See the [complete test](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L1-L40).

**Modified or removed tests:** none in the supplied comparison.

**Infrastructure changes:** the existing Bazel glob automatically includes the new `.test.js` file; runfiles gain the sequencer; the target gains four shards; Jest gains the explicit transform, conditional sequencer, and temporary-directory cache. The existing `jest.setup.cjs` configuration and runfile entry remain present. Its claimed TextEncoder/TextDecoder polyfill is not supplied for inspection.

The regression does **not** directly test real-file size balancing, complete cross-shard coverage, status-file acknowledgement, or the separate CI type-check. Those remain distinct from the behavior its assertions establish.

The author reports successful `bazel build`, `bazel test`, and `checkleft run`, plus 58 suites and 449 tests partitioned across four shards. Reported in-band timings were 39.6–43.3 seconds versus 97.9–102.4 seconds with four Jest workers inside each shard, supporting the retained `--runInBand` choice. That comparison does not independently establish the title’s 2.2× improvement over the base revision. No tests or benchmarks were run for this guide.
