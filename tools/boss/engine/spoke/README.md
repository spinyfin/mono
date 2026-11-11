# Worker observation spoke

`boss-spoke` owns a private SQLite WAL journal and initiates one mTLS gRPC
stream. It samples capacity every five seconds even while disconnected, journals
before sending, and replays original encoded records from the hub's durable
receipt. Restart preserves its journal identity and sequence; reboot changes the
OS boot identity. It advertises `journal/1`, `workspace/1`, `execution/1`,
`worker-event/1` and `transcript-delta/1`; capacity still has zero worker slots.
Workspace and worker commands execute through the shared host executor.

This ships inert: production hub dispatch is not wired. `boss-spoke-package`
([README](../../spoke-package/README.md)) installs the daemon as a launchd or
systemd service on a worker host. An administrator with a compatible test hub and
provisioned client credentials can also run the daemon explicitly:

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
PR positioning saves its pointer snapshot in the command journal before goto;
restart restores those pointers before returning the interrupted result.
Pre-effect authorization refusals and failed reads return rejected results.

Journal schema 3 added durable pointer snapshots and the last negotiated result
bound. Result admission measures the encoded record with envelope overhead and
replaces an oversized payload with a bounded diagnostic. Reads are rejected;
operations that may have changed the host report uncertainty. A later negotiation
too small for retained records or pending command identities is retried without
discarding evidence. Schema 4 added execution ownership; schema 5 adds producer
receipts and transcript checkpoints. Older readers refuse newer schemas.

The daemon binds an owner-only `events.sock` inside its state directory. It
authenticates hook peers by OS user, process ancestry, boot identity and the
registered worker's process start time. Payload run IDs only check consistency.
`--event-bin` selects the installed `boss-event` forwarder (default `boss-event`).
Claude settings receive the forwarder hooks; Codex uses its correlated rollout
file as the authoritative progress feed. Drivers requiring driver-owned policy
installation are rejected until that policy is provisioned.

Each run has a private pending directory beside its worker home. The shim saves
producer identity, sequence and payload before connecting and removes a payload
only after an explicit durable receipt. The daemon independently drains these
directories on startup and while offline; no later hook is needed. Duplicate
delivery is accepted only when its original payload digest matches. Each pending
directory is bounded at 64 MiB; exhaustion retains earlier events and fails the
new hook explicitly.

Transcript discovery and normalizers are shared with local execution. Raw chunks
are at most 64 KiB and retain generation, byte offset, checksum and EOF. Their
source cursor and file normalizer state commit atomically with journal records.
Rotation or truncation starts a new generation and records an explicit gap for
any unavailable suffix. Bulk admission and outstanding byte credit pause reads
without advancing the cursor; lifecycle hooks retain their reserved allowance.
Stop carries the observed transcript watermark even while bulk admission is
paused. Worker homes cannot be reclaimed until pending events and transcript
bytes are durable. Hub projection remains a separate journal consumer.

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
