# Component deadlines included host input loading

Concurrent checkleft runs could report wall-clock timeouts for cheap checks as
well as `rust/giant-structs`. The host set the store's epoch deadline before
`lower_check_input`, which reads the base version of every modified file. In a
jj workspace those reads invoke `jj file show` separately for each file. VCS
and CPU contention could therefore exhaust the deadline before guest execution.

## Reproduction

On macOS, add a comment to 43 existing Rust files in a jj workspace and run
three instances of `checkleft run` concurrently with a Bazel build. Enable
`RUST_LOG=checkleft::external::runtime=debug` to capture the phase timings.
Restore the temporary file edits after measurement.

This intentionally oversized fixture must still fail the repository's 30-file
limit; success here means the components execute and report findings normally.
Validate the ordinary run and push gate after restoring the fixture edits.

The October 12 reproduction used checkleft `0.1.0-alpha.8` through the installed
repobin dispatcher, with `REPOBIN_BAZEL_FLAGS` unset and `compilation_mode=opt`.
The dispatcher SHA-256 was
`77f393eb14fa94b8f51636f4f46d75d70513deefb068c2e1e7dd7ff1ffa6ee50`.
The optimized bundled guest SHA-256 was
`7fb017d9ba7b4c3d1587ab1242fd53b28682b2388173fcb6063c3a4f7b31ba9e`.
The runtime reported AOT cache hits. One run measured:

| Check               | Host input loading |        Guest call | Existing limit |
| ------------------- | -----------------: | ----------------: | -------------: |
| file/size           |          43,027 ms | 0 ms, interrupted |       9,500 ms |
| rust/giant-structs  |          48,128 ms | 0 ms, interrupted |       9,300 ms |
| md/link-integrity   |          48,883 ms | 0 ms, interrupted |       9,500 ms |
| file/forbidden-path |          49,563 ms | 0 ms, interrupted |       9,500 ms |

These measurements reproduce the reported failure pattern with optimized guests;
pinning guest optimization does not fix this cause. The historical failing
changeset, dispatcher version, flags, and guest digest were not retained, so this
is a fresh reproduction of the mechanism, not an attribution to a particular
historical binary.

After the host fix, three concurrent runs on the same 43-file fixture reported
30 successful guest calls and no guest interruptions, using the identical
optimized guest digest and the unchanged AOT cache. In one run, base-input
loading still took 24,241–25,496 ms, while guest execution took 0–710 ms
(`rust/giant-structs`: 710 ms against its unchanged 9,300 ms limit).
The other checks' proportional budgets differ slightly because the revision
adds a regression test edit and this investigation note to the changeset.
The host ordering, rather than faster input reads or a different guest, makes
the difference: input loading alone still exceeds every affected guest budget.

## Fix and regression coverage

Prepare host input before setting the guest deadline for `run-check`,
`declare-required-files`, and `fix-check`. Instantiation and guest calls retain
the same deadline and memory limits. Compilation already occurred outside the
deadline. No detection rules, exclusions, caches, or timeout values change.

Debug tracing records artifact identity and load time, setup time, input-loading
time, instantiation time, and execution time, including failed guest calls.
The regression test advances the engine epoch beyond the entire host ceiling
during base-version reads, without sleeps. Checks and fixes must still execute,
giant-structs must still report a violation, and a zero guest budget must still
interrupt execution.
