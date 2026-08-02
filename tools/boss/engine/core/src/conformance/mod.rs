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
//! 4. **Version pinning** — fixtures and the installed Codex CLI must match
//!    the pinned Codex version; Codex's `--json` stream has no schema version
//!    of its own, so this harness is the only defence against silent drift.
//!    Live `codex --version` is soft-skip without the binary (or on any
//!    live-check failure); set `BOSS_REQUIRE_CODEX_CLI=1` to require it
//!    instead. Grok carries no version pin — the CLI auto-updates itself and
//!    a hard pin turned every automatic bump into a fail-closed provisioning
//!    outage (the observed `grokVersion` is now only logged on drift, never
//!    gated; see `grok::home::assert_inspect_json_posture`). The harness
//!    still pins two version-independent live-CLI surfaces for Grok: the
//!    hidden `--trust` flag still parses, and `grok models` still matches the
//!    pinned descriptor's single-SKU menu; set `BOSS_REQUIRE_GROK_CLI=1` to
//!    require the binary instead of soft-skipping — see `version_pin`.
//! 5. **Native-dialect transcript normalize** — every registry slug has a
//!    fixture in that driver's on-disk dialect, and normalizing it surfaces a
//!    `[blocked]` marker. Fails closed when a driver is registered without a
//!    fixture (the all-drivers completion test only exercises the post-normalize
//!    canonical shape).
//! 6. **Codex `PreToolUse` guard conformance** — for every model Codex can
//!    dispatch, a fixed live probe exercises the tool-surface routes known to
//!    matter (a plain shell command, a push attempt, `apply_patch`, the
//!    `exec_command`/`write_stdin` bypass, an MCP app tool) against the exact
//!    production guard set, and asserts the observed `(tool_name,
//!    tool_input` key set`, decision)` shape against a checked-in fixture. A
//!    mismatch fails the build — it means Codex's tool surface (or Boss's
//!    guard wiring) drifted from what the fixture recorded. Soft-skip without
//!    a live credential; see `guard_conformance`.
//!
//! Tolerance policy (Codex stream): tolerate additive fields and unknown enum
//! variants (forward-compatible). Fail loudly on removals and on semantic
//! changes to existing fields (item-id base, `error` meaning, required flags).

#![cfg(test)]

mod boundary_equivalence;
mod claude_goldens;
mod fixtures;
mod grok_goldens;
mod guard_conformance;
mod ingress_equivalence;
mod native_transcript;
mod version_pin;

/// Truthy-env-var parsing shared by every driver's "require the live CLI"
/// gate (`BOSS_REQUIRE_CODEX_CLI`, `BOSS_REQUIRE_GROK_CLI`, …), so a future
/// third driver's pin does not add yet another copy of the same parsing.
fn truthy_env(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

fn require_codex_cli() -> bool {
    truthy_env("BOSS_REQUIRE_CODEX_CLI")
}

fn require_grok_cli() -> bool {
    truthy_env("BOSS_REQUIRE_GROK_CLI")
}

/// Resolve `bin` on `PATH` via `which`, same lookup every live-CLI pin needs
/// before it can spawn the binary directly (rather than trusting ambient
/// `PATH` resolution at spawn time). `None` when `which` fails or the binary
/// is absent — never an error, since "not installed" is the caller's normal
/// soft-skip case.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("which").arg(bin).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}
