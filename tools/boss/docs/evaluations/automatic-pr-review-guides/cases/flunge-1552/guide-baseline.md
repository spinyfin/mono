1. **The problem: too much work inside the Jest runner.**

   This PR changes test execution infrastructure, rather than application behavior. Previously, Bazel launched one Jest process with `--runInBand`, using the standard `ts-jest` preset. The PR description identifies repeated TypeScript program construction as the dominant cost. See the original [Jest configuration](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/jest.config.cjs#L1-L8) and [Bazel target](https://github.com/brianduff/flunge/blob/54e4e293dfe4e142bf8a63761f4f0348af205a0e/frontend/BUILD#L38-L58).

   The solution removes type checking from Jest’s transform and distributes test files across four Bazel processes. The PR reports 58 suites and 449 tests partitioned across those shards. Its worker experiment measured **39.6–43.3 seconds with in-band shards**, versus **97.9–102.4 seconds with four Jest workers inside each shard**. Those measurements compare worker configurations; they do not independently establish the title’s 2.2× improvement over the base revision.

2. **The core implementation: transpile each file, then let Bazel own parallelism.**

   The preset becomes an explicit transform whose important settings are:

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

   `isolatedModules: true` makes this a transpile-only path. The module overrides adapt the application’s TypeScript configuration to Jest’s CommonJS runtime; other settings remain inherited. The changed contract is that **Jest no longer performs type checking**. The configuration comments and PR description say CI separately checks all of `src`, including tests. That CI configuration and the underlying TypeScript configuration were not supplied, so this review cannot independently verify that safeguard. [Transform and rationale](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L9-L32)

   Bazel adds `shard_count = 4` while retaining `--runInBand`. Each shard still receives the discovered test paths; the new sequencer selects its share at runtime. Thus, parallel execution happens between Bazel shards, with serial Jest execution inside each one. [Bazel wiring](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L38-L64)

3. **The main logic to review is the translation from Bazel’s shard environment to a test-file partition.**

   The sequencer sorts files by descending size, breaking ties by path. It then assigns sorted positions round-robin:

   ```js
   const total = Number(process.env.TEST_TOTAL_SHARDS);
   const index = Number(process.env.TEST_SHARD_INDEX);
   const ordered = [...tests].sort(bySizeThenPath);
   if (!Number.isInteger(total) || total < 1 || !Number.isInteger(index)) {
     return ordered;
   }
   return ordered.filter((_, i) => i % total === index);
   ```

   File size is a cost heuristic, not a measured runtime. Files that cannot be statted receive size zero. The partition logic is the decisive step that makes four processes divide the work instead of repeat it. [Ordering and partitioning](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L27-L52)

   **Worked example — illustrative inputs:** suppose eight discovered files, `a.test.ts` through `h.test.ts`, have descending sizes of 800, 700, …, 100 bytes.

   - **Before:** the unsharded Bazel target passes all eight files to one in-band Jest process. All eight execute there; their exact order depends on Jest’s default sequencer.
   - **After:** Bazel launches four shards with `TEST_TOTAL_SHARDS="4"` and indices `"0"` through `"3"`. Each sequencer sorts the files into `a` through `h`, then applies the modulo filter:

     | Shard index | Sorted positions retained | Files executed |
     | ----------- | ------------------------- | -------------- |
     | 0           | 0, 4                      | a, e           |
     | 1           | 1, 5                      | b, f           |
     | 2           | 2, 6                      | c, g           |
     | 3           | 3, 7                      | d, h           |

   The observable result is eight file executions spread across four processes, with each file selected exactly once. This balances file counts and spreads larger files across shards; it does not guarantee equal execution times.

   Three supporting details complete the integration:

   - **Bazel handshake:** loading the sequencer writes an empty `TEST_SHARD_STATUS_FILE`, when provided, to advertise sharding support. The comments explain why the runtime environment is used instead of a fixed Jest `--shard` argument. [Protocol handling](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L5-L25)
   - **Local behavior:** the custom sequencer is installed only when `TEST_TOTAL_SHARDS` is truthy. Ordinary local runs retain Jest’s default sequencer, although they also receive the new transpile-only transform. [Conditional configuration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L3-L9)
   - **Caching:** Bazel’s Jest cache moves to `${TEST_TMPDIR}/jest-cache`; without that variable, the default location remains. The custom sequencer’s `cacheResults()` does nothing, intentionally foregoing timing persistence in Bazel sandboxes. [Cache directory](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L33-L34), [sequencer cache hook](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L58-L60)

4. **The added test protects against partitioning failed tests twice.**

   The single new test sets four shards and index one, then supplies three nonexistent paths in order `c, a, b`. Because stat failures produce zero sizes, path ordering gives `a, b, c`. It verifies two distinct contracts:

   ```js
   sequencer.sort(tests); // selects b
   sequencer.allFailedTests(tests); // retains a, b, c
   ```

   This is the useful boundary case: a failed-test list already belonging to a shard must retain every supplied entry. Reusing `sort()` there would retain only `b`, dropping `a` and `c`. Accordingly, `allFailedTests()` only orders its input; it does not apply the modulo filter. [Regression test](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/src/jest.shard-sequencer.test.js#L25-L39), [implementation](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.shard-sequencer.cjs#L54-L56)

   Infrastructure changes comprise the transform, conditional sequencer, four-way Bazel sharding, cache placement, and explicit inclusion of the `.cjs` sequencer in Bazel runfiles. The existing `jest.setup.cjs` registration remains in both Jest and Bazel. [Runfiles](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/BUILD#L47-L55), [setup registration](https://github.com/brianduff/flunge/blob/7db88ee23b84dcc1a4d997bf4f29c19b5a8d66b1/frontend/jest.config.cjs#L8)

   The added unit test does **not** cover real file-size ordering, the complete cross-shard partition, or the status-file handshake. The PR reports successful Bazel build/test and `checkleft run`; I have reviewed the supplied pinned source, not rerun those checks.
