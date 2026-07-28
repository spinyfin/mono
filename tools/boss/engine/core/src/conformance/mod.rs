//! Reference-driver conformance harness (design §Migration shape).
//!
//! Golden + cross-transport tests that gate acceptance of every agent-driver
//! extraction. Four surfaces:
//!
//! 1. **Claude byte-for-byte goldens** — spawn line, settings.json, CLAUDE.md,
//!    and deny rules produced *through the driver interface* (and the
//!    `worker_setup` renderers that take `&dyn AgentDriver`).
//! 2. **Ingress equivalence** — stdout-JSONL and hook ingress produce the
//!    same [`WorkerEvent`] sequence for an equivalent session.
//! 3. **Boundary equivalence** — (a) both transports produce equal `TurnEnd`
//!    via `turn_boundary`; (b) the decoded `WorkerEvent` sequence drives the
//!    live-worker activity machine to Idle. See `boundary_equivalence`.
//! 4. **Version pinning** — fixtures and the installed agent CLI must match
//!    the pinned Codex version; the `--json` stream has no schema version of
//!    its own, so this harness is the only defence against silent drift.
//!    Live `codex --version` is soft-skip without the binary; set
//!    `BOSS_REQUIRE_CODEX_CLI=1` to require it.
//! 5. **Native-dialect transcript normalize** — every registry slug has a
//!    fixture in that driver's on-disk dialect, and normalizing it surfaces a
//!    `[blocked]` marker. Fails closed when a driver is registered without a
//!    fixture (the all-drivers completion test only exercises the post-normalize
//!    canonical shape).
//!
//! Tolerance policy (Codex stream): tolerate additive fields and unknown enum
//! variants (forward-compatible). Fail loudly on removals and on semantic
//! changes to existing fields (item-id base, `error` meaning, required flags).

#![cfg(test)]

mod boundary_equivalence;
mod claude_goldens;
mod fixtures;
mod ingress_equivalence;
mod native_transcript;
mod version_pin;
