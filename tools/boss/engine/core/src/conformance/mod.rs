//! Reference-driver conformance harness (design §Migration shape).
//!
//! Golden + cross-transport tests that gate acceptance of every agent-driver
//! extraction. Five surfaces:
//!
//! 1. **Byte-for-byte goldens** — Claude's spawn line, settings.json,
//!    CLAUDE.md, and deny rules (`claude_goldens`), and Grok's pane spawn
//!    command (`grok_goldens`), all produced *through the driver interface*
//!    (or a pure builder the real driver method delegates to) — never by
//!    reaching past the trait into driver-only helpers.
//! 2. **Ingress equivalence** — stdout-JSONL (Codex) and hook ingress
//!    (Claude, Grok) produce the same [`WorkerEvent`] sequence shape for an
//!    equivalent session.
//! 3. **Boundary equivalence** — (a) every transport produces equal `TurnEnd`
//!    via `turn_boundary`; (b) the decoded `WorkerEvent` sequence drives the
//!    live-worker activity machine to Idle. See `boundary_equivalence`.
//! 4. **Version pinning** — fixtures and the installed agent CLI must match
//!    the pinned Codex / Grok versions; Codex's `--json` stream has no schema
//!    version of its own, so this harness is the only defence against silent
//!    drift. Live `codex --version` / `grok inspect --json` are soft-skip
//!    without the binary (or on any live-check failure, e.g. not logged in);
//!    set `BOSS_REQUIRE_CODEX_CLI=1` / `BOSS_REQUIRE_GROK_CLI=1` to require
//!    them instead. The Grok pin additionally asserts the hidden `--trust`
//!    flag still parses and that `grok models` still matches the pinned
//!    descriptor's single-SKU menu — see `version_pin`.
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
mod grok_goldens;
mod ingress_equivalence;
mod native_transcript;
mod version_pin;
