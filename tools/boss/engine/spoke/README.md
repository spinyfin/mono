# Journal-only spoke

`boss-spoke` owns a private SQLite WAL journal and initiates one mTLS gRPC
stream. It samples capacity every five seconds even while disconnected, journals
before sending, and replays original encoded records from the hub's durable
receipt. Restart preserves its journal identity and sequence; reboot changes the
OS boot identity. Only `journal/1` is advertised and capacity has zero worker
slots. Commands receive durable receipts and explicit rejected results.

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

The spool defaults to 8 GiB, reserving 64 MiB for lifecycle evidence. Captures
stop at 75% and bulk records at 90% of ordinary capacity. Capacity admission
stops at the ordinary ceiling. Receipted host-only records are retained for seven
days; run records and command tombstones remain pinned. SQLite also has a page
ceiling and a bounded checkpoint cadence. Queue accounting includes metadata
overhead, but filesystem exhaustion can precede the configured ceiling and is
reported as a storage failure. No indefinite offline retention is promised.

The stream allows one unreceipted record at a time and two bounded outgoing
frames. It validates downward-negotiated frame/credit limits, deadlines, sequence
continuity and connection generation. Receipts are never treated as command
success. Raw command bytes and their digest survive duplicate delivery; a
changed request under the same ID is rejected. Pending commands recover to one
durable rejection result in the same transaction as their applied cursor.

Run `bazel test //tools/boss/engine/spoke-journal/... //tools/boss/engine/spoke/...`
for storage and real daemon/tonic fault tests.
