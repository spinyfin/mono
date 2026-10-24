# Workspace-capable spoke

`boss-spoke` owns a private SQLite WAL journal and initiates one mTLS gRPC
stream. It samples capacity every five seconds even while disconnected, journals
before sending, and replays original encoded records from the hub's durable
receipt. Restart preserves its journal identity and sequence; reboot changes the
OS boot identity. It advertises `journal/1` and `workspace/1`; capacity still has
zero worker slots. Workspace commands execute through the shared host executor.
Other commands receive durable receipts and explicit rejected results.

This ships inert: neither installation nor production hub dispatch is wired.
An administrator with a compatible test hub and provisioned client credentials
can run the daemon explicitly:

```sh
bazel run //tools/boss/engine/spoke:boss-spoke -- \
  --state-dir /absolute/private/spoke-state --host-id host-name \
  --hub https://hub.example:7443 --tls-dir /absolute/private/credentials
```

The credential directory contains `ca.pem`, `cert.pem`, and `key.pem`. The hub
must authenticate the client certificate and authorize its Hello host identity.
There is no plaintext mode. The state directory must be private (0700). An
exclusive daemon lock prevents competing writers. Storage errors, unknown schema
versions, changed hub authority/command journals, and missing replay history stop
the process for reconciliation; network failures retry with shared exponential
backoff and jitter. No retry resets a cursor or discards unacknowledged records.

Commits use `synchronous=FULL` with `fullfsync` and `checkpoint_fullfsync` on, so
that on macOS (where plain `fsync` does not flush the drive cache) a receipted
record or command survives power loss; the pragmas are no-ops on Linux.

The spool defaults to 8 GiB, reserving 64 MiB for lifecycle evidence. Captures
stop at 75% and bulk records at 90% of ordinary capacity. Capacity admission
stops at the ordinary ceiling. Receipted host-only records are retained for seven
days. Run records remain pinned until a reconciliation API exists, and compaction
removes only a contiguous prefix, so one pinned run record (including the
rejection of a run-scoped command) pins every later record. An applied command is
reduced to a tombstone (id, sequence, digest) that no longer holds its payload or
counts against the spool. SQLite also has a page
ceiling and a bounded checkpoint cadence. Queue accounting includes metadata
overhead, but filesystem exhaustion can precede the configured ceiling and is
reported as a storage failure. No indefinite offline retention is promised.

The stream pipelines records within the negotiated byte credit and uses a
two-frame outgoing channel. Transport readiness is polled alongside inbound
traffic, with bounded control replies prioritized over replay. A full inbox
retains one raw command and retries admission after compaction, without requiring
a reconnect or resend. It validates downward-negotiated frame/credit limits,
deadlines, sequence continuity and connection generation. Receipts are never treated as command
success. Raw command bytes and their digest survive duplicate delivery; a
changed request under the same ID is rejected. The executor persists an attempting
phase before host effects and commits each result with its applied cursor. It
runs independently of the stream, retaining a completed result while the spool
is full. An interrupted mutation returns uncertainty instead of being repeated.

`--cube-bin` selects the host's cube executable (default `cube`). Commands cannot
select an executable or filesystem path. Workspace IDs resolve through cube's
registry and lease operations require the recorded assignment owner. Recovery
operations that do not need a working copy resolve the registered shared source,
so recovery remains available after release. Interrupted acquisition reconciles
the stable assignment identity in cube's task field; a recovered lease is reported
as uncertain because its setup may not have completed. The hub retains decisions
about release, retry, and GitHub PR state.

Run `bazel test //tools/boss/engine/spoke-journal/... //tools/boss/engine/spoke/...`
for storage and real daemon/tonic fault tests.
