# Worker slot identities

`boss_protocol::worker_identity::WorkerSlotIdentity` is a pool plus a positive
`u32` index. Local pools reuse `DispatchPool`; `WorkerSlotPool::Remote` identifies
virtual slots without treating remote hosting as a dispatch policy. Indexes 33,
64, 256 and larger remain distinct across pools. Zero is rejected at construction
and deserialization. This identity is neither a session name nor a persona lease.

## Legacy compatibility

The codec freezes the existing numeric layout independently of future capacity:

| Pool           | Legacy slots | Legacy worker IDs                   |
| -------------- | ------------ | ----------------------------------- |
| Main (lower)   | 1–16         | worker-1 through worker-16          |
| Automation     | 17–24        | auto-worker-1 through auto-worker-8 |
| Review         | 25–40        | review-1 through review-16          |
| Progression    | 41–44        | progression-1 through progression-4 |
| Main (upper)   | 45–60        | worker-17 through worker-32         |
| Remote virtual | 200–255      | worker-200 through worker-255       |

The main pool ceiling is 32. Main indexes 17–32 were added after the other
ranges were allocated, so they take the first free legacy slots (45–60) rather
than shifting automation, review and progression. No existing slot number
moved, which keeps live tmux session names (`boss-{slot}-{short_exec}`), stored
worker ids and post-restart re-adoption valid without a drain or remap. A main
`worker-N` id therefore decodes to slot N for N ≤ 16 and slot N + 28 for
N in 17–32.

Remote canonical indexes are 1–56. Zero and slots 61–199 are invalid.
Out-of-range pool ordinals are rejected, not reinterpreted: `worker-33` cannot
mean main index 33 or review index 1, and `auto-worker-9` cannot mean review
index 1. Decimal IDs must have no sign, whitespace or leading zeros.
Canonical identities outside these ranges cannot convert to legacy numbers.

Execution-contracts delegates its conversion helpers to this codec. The existing
infallible formatter retains its signature and panics for invalid legacy slots;
callers handling untrusted data should use the checked protocol methods. Remote
slots are no longer incorrectly formatted as progression workers. Live-worker
session names, numeric wire fields and durable persona leases are unchanged;
the concurrency ceiling is the main pool's 32.

## Staged migration

**Dynamic capacity is not activated by this groundwork.** No configuration or
operator action can activate it through this change.

The 32-slot main pool is an intentional exception to the ordering below:
splitting main indexes 17–32 into the unallocated legacy slots 45–60 fits
within the existing legacy slot byte, moves no existing slot number, and
requires no wire, registry, or session-addressing migration. The full
migration described next remains a separate, optional refactor and is not a
prerequisite for the 32-slot ceiling; any further raise beyond the legacy
slot byte still follows the staged ordering below.

Consumers should first carry the canonical identity internally and use the
checked codec at existing numeric boundaries. Next, migrate storage, wire
messages, registry keys, session addressing and UI consumers together with
explicit compatibility/version handling. Only after those boundaries no longer
require a legacy byte can allocation limits be raised in a separate change.
Keep persona leases independent throughout; do not derive names from indexes.
