# Worker slot identities

`boss_protocol::worker_identity::WorkerSlotIdentity` is a pool plus a positive
`u32` index. Local pools reuse `DispatchPool`; `WorkerSlotPool::Remote` identifies
virtual slots without treating remote hosting as a dispatch policy. Indexes 32,
64, 256 and larger remain distinct across pools. Zero is rejected at construction
and deserialization. This identity is neither a session name nor a persona lease.

## Legacy compatibility

The codec freezes the existing numeric layout independently of future capacity:

| Pool           | Legacy slots | Legacy worker IDs                   |
| -------------- | ------------ | ----------------------------------- |
| Main           | 1–16         | worker-1 through worker-16          |
| Automation     | 17–24        | auto-worker-1 through auto-worker-8 |
| Review         | 25–40        | review-1 through review-16          |
| Progression    | 41–44        | progression-1 through progression-4 |
| Remote virtual | 200–255      | worker-200 through worker-255       |

Remote canonical indexes are 1–56. Zero and slots 45–199 are invalid.
Out-of-range pool ordinals are rejected, not reinterpreted: `worker-25` cannot
mean main index 25 or review index 1, and `auto-worker-9` cannot mean review
index 1. Decimal IDs must have no sign, whitespace or leading zeros.
Canonical identities outside these ranges cannot convert to legacy numbers.

Execution-contracts delegates its conversion helpers to this codec. The existing
infallible formatter retains its signature and panics for invalid legacy slots;
callers handling untrusted data should use the checked protocol methods. Remote
slots are no longer incorrectly formatted as progression workers. Live-worker
session names, numeric wire fields, concurrency limits and durable persona leases
are unchanged.

## Staged migration

**Dynamic capacity is not activated by this groundwork.** No configuration or
operator action can activate it through this change.

Consumers should first carry the canonical identity internally and use the
checked codec at existing numeric boundaries. Next, migrate storage, wire
messages, registry keys, session addressing and UI consumers together with
explicit compatibility/version handling. Only after those boundaries no longer
require a legacy byte can allocation limits be raised in a separate change.
Keep persona leases independent throughout; do not derive names from indexes.
