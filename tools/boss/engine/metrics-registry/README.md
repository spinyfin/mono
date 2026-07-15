# boss-engine-metrics-registry

The in-memory counter / gauge store behind the engine's metrics framework, plus the `register_counter!` / `register_gauge!` macros that declare handles against it. It lives in its own crate so that declaring or touching a metric doesn't rebuild `boss-engine`, which is the largest Rust target in the repo.

## Architecture

A `Registry` owns two name-keyed maps of atomic entries — monotonic `u64` counters and signed `i64` gauges. Registration takes a write lock; the hot path (`inc`, `set`) takes a read lock and then an atomic update, so concurrent producers never serialise against each other. Steady state is on the order of ~50 entries, so the map plus lock cost is irrelevant next to the work being measured.

The registry is plumbed explicitly as an `Arc<Registry>` rather than parked in a global. Every call site takes a `&Registry`, which is what lets a unit test build a local registry and assert on it without leaking counts into other tests.

Handles are static descriptors: `register_counter!` expands to a `static` holding a name and a description, and the running value lives in whichever `Registry` the handle is resolved against. Names are validated at registration (lowercase ASCII, digits, `.`, `_`), and duplicate registration panics, so both classes of mistake surface at engine startup rather than at the first increment. For names only known at runtime — a counter keyed by product id, say — `counter_inc_by_dynamic` registers on first use and is idempotent thereafter.

Rows rehydrated from `state.db` whose name no longer matches any handle in the current binary are kept and flagged `stale`, so historical values stay queryable across a rename. If a handle later registers under that name, it adopts the persisted value and clears the flag.

## Scope

This crate is storage and declaration only. Two neighbouring pieces deliberately stay in `boss-engine`:

- **Persistence** (`metrics::persistence`) depends on `WorkDb` for the `state.db` flush and rehydrate paths.
- **`metrics::init_all`** reaches into every metric-declaring engine module to force registration at boot.

Both sit on the consumer side of the edge, which keeps the dependency one-way: `boss-engine` -> `boss-engine-metrics-registry`. Moving persistence down would mean introducing a sink trait at the boundary.

See `tools/boss/docs/designs/engine-counter-metrics-framework.md` for the framework design and its open questions.
