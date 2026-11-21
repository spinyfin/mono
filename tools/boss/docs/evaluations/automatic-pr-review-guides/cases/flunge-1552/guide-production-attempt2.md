# PR #1552: Faster Jest transforms and Bazel-owned sharding

This guide covers base `54e4e293dfe4e142bf8a63761f4f0348af205a0e` → head `7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1`. Links below use pinned source locations because exact diff anchors were not supplied.

## 1. Problem being solved

The frontend’s Bazel Jest target previously ran every test file in one process using the default `ts-jest` preset. The author reports that profiling identified repeated TypeScript program construction as the dominant cost. Meanwhile, CI already ran a separate TypeScript build, so Jest’s semantic checking duplicated work.

The PR addresses both transformation cost and execution parallelism. Its title reports a **2.2× speedup**; that is an author-reported measurement, not a result independently reproduced for this guide.

## 2. Core fix / implementation

The solution separates three responsibilities:

- **Jest transpiles and executes tests.** An explicit `ts-jest` transform enables file-by-file compilation without semantic type-checking.
- **The existing frontend build checks TypeScript.** CI schedules this build under the same condition as the Bazel frontend tests.
- **Bazel partitions execution into four shards.** A new Jest sequencer translates Bazel’s runtime environment into disjoint file selections. Each shard retains `--runInBand`, avoiding another layer of Jest workers.

The transform change applies to local Jest runs too. The custom sequencer applies only when `TEST_TOTAL_SHARDS` is truthy. Running Jest or the Bazel test target alone therefore no longer provides the semantic checking supplied by the separate frontend build. See the [Jest configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L33) and [Bazel target](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64).

## 3. Walkthrough of the implementation

### First, remove type-checking from the transform

The former `preset: "ts-jest"` becomes an explicit transform. Its decisive options are:

```js
isolatedModules: true,
tsconfig: {
  module: "CommonJS",
  moduleResolution: "Node",
  esModuleInterop: true,
},
```

The outer `isolatedModules` option selects ts-jest’s transpile-only path. That is distinct from the pre-existing TypeScript `isolatedModules` compiler option, which constrains source compatibility with file-by-file compilation. The module overrides adapt the application’s ESM/bundler configuration to Jest’s CommonJS execution; JSX, Emotion’s JSX import source, and other compiler settings remain inherited. See [transform options](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L17-L30) and [TypeScript configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/tsconfig.json#L2-L25).

The replacement type-checking path is concrete: [CI schedules both frontend steps together](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/upload_dynamic_pipeline.sh#L146-L159); the [npm step runs `npm run build`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/.buildkite/scripts/run_frontend_tests.sh#L9-L13); and that [build starts with `tsc`](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/package.json#L6-L10). The tsconfig includes `src`, including its TypeScript tests. This does not mean the newly added JavaScript test receives TypeScript checking: `allowJs`/`checkJs` are not enabled.

### Next, make each Bazel invocation select its own files

The [BUILD change](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64) adds `shard_count = 4` and includes the `.cjs` sequencer in runfiles. The existing test-file glob and `--runTestsByPath` arguments still supply the full candidate list to each invocation.

The sequencer then:

1. Writes an empty `TEST_SHARD_STATUS_FILE`, when supplied, to acknowledge sharding support.
2. Sorts a copy of the candidates by descending file size, breaking ties with `path.localeCompare`.
3. Selects positions belonging to the current zero-based shard.

The [selection logic](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L44-L51) is:

```js
const total = Number(process.env.TEST_TOTAL_SHARDS);
const index = Number(process.env.TEST_SHARD_INDEX);
const ordered = [...tests].sort(bySizeThenPath);
if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
  return ordered;
}
return ordered.filter((_, i) => i % total === index);
```

File size is a cost heuristic, not a guarantee of equal runtime. A failed file-stat operation contributes size zero. The guard also assumes Bazel supplies a valid index range: integer indices outside `0..total-1` yield no selected files. See the [status handshake and comparator](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L21-L36).

### Worked example: one candidate list, four executions

**Illustrative inputs:** eight valid TypeScript test files, `a.test.ts` through `h.test.ts`, with strictly descending sizes of 8 KB through 1 KB. Each shard sees the same files and metadata; Bazel supplies `TEST_TOTAL_SHARDS="4"` and indices `"0"` through `"3"`.

**Before:** the [base target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58) has no shard count. One in-band Jest invocation runs all eight files through the default ts-jest configuration.

**After:** every invocation computes the order `a,b,c,d,e,f,g,h`, then applies the modulo selection above:

| Shard index | Selected positions | Files executed |
| ----------- | ------------------ | -------------- |
| 0           | 0, 4               | a, e           |
| 1           | 1, 5               | b, f           |
| 2           | 2, 6               | c, g           |
| 3           | 3, 7               | d, h           |

For shard 1, `b` survives because `1 % 4 === 1`; `f` survives because `5 % 4 === 1`. The other six candidates are excluded from that invocation.

**Observable result:** the four invocations collectively execute every test file once, two files per process, with transpile-only transforms. They can run concurrently as Bazel resources permit. Shared imported modules may still be transformed in multiple shards; the change does not promise globally unique compilation or equal shard durations.

### Finally, avoid partitioning again and control cache lifetime

The sequencer deliberately gives failed-test selection a separate implementation:

```js
allFailedTests(tests) {
  return [...tests].sort(bySizeThenPath);
}
```

[Jest 29.7 calls `sort` before its `onlyFailures` handling](https://raw.githubusercontent.com/jestjs/jest/v29.7.0/packages/jest-core/src/runJest.ts). Calling the partitioning `sort` again could discard files already assigned to this shard. The [new hook](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L60) preserves all incoming candidates and only orders them. Despite its name, it does **not** determine which candidates previously failed; `cacheResults()` is also a no-op.

Separately, [Jest’s cache directory](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L33) becomes `${TEST_TMPDIR}/jest-cache` when that variable is truthy. Without these environment overrides, local runs retain Jest’s default sequencer and cache location.

## 4. Tests and supporting infrastructure

**Added:** one [regression test](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25-L39), with two complementary assertions. It sets total shards to four and index to one, then supplies nonexistent paths in `c,a,b` order:

```js
expect(sequencer.sort(tests).map((test) => test.path)).toEqual(["/missing/b.test.js"]);
```

All stat calls fall back to zero, so the path tie-break produces `a,b,c`; partitioning keeps only `b`. Calling `allFailedTests` on the same three candidates must instead return all three in `a,b,c` order. The fixture is entirely synthetic—no files are created—and teardown restores or removes the original environment variables.

**Modified or removed tests:** none. The complete comparison changes two configuration/build files and adds the sequencer plus this test.

**Supporting infrastructure:** the existing BUILD glob automatically discovers the new `.test.js`; the new `.cjs` helper is explicitly added to runfiles. The pre-existing jsdom setup and its [TextEncoder/TextDecoder polyfill](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.setup.cjs#L1-L11) remain wired into both Jest and Bazel.

**Coverage limits:** the unit test verifies partition-versus-order behavior directly. It does not exercise real size-based balancing, the status-file handshake, the full Jest failed-test lifecycle, or complete four-shard coverage.

**Validation:** the author reports successful `bazel build //frontend:jest_test`, `bazel test //frontend:jest_test --test_output=errors`, and `checkleft run`, with 58 suites and 449 tests partitioned across four shards. Reported in-band timings were 39.6–43.3 seconds versus 97.9–102.4 seconds with four Jest workers inside each shard. For this guide, the pinned comparison, CI/type-checking path, and Jest caller behavior were inspected; tests and benchmarks were not rerun.
