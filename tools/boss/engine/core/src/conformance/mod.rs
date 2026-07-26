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
//! 3. **Boundary equivalence** — a turn boundary drives completion identically
//!    whether it arrived as a hook-fired `Stop` or a native `turn.completed`.
//! 4. **Version pinning** — fixtures and the installed agent CLI must match
//!    the pinned Codex version; the `--json` stream has no schema version of
//!    its own, so this harness is the only defence against silent drift.
//!
//! Tolerance policy (Codex stream): tolerate additive fields and unknown enum
//! variants (forward-compatible). Fail loudly on removals and on semantic
//! changes to existing fields (item-id base, `error` meaning, required flags).

#![cfg(test)]

mod boundary_equivalence;
mod claude_goldens;
mod fixtures;
mod ingress_equivalence;
mod version_pin;
