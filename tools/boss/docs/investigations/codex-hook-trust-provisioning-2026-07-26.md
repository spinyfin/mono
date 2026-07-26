# Codex hook-trust provisioning

- **Date:** 2026-07-26
- **Question:** What is `[hooks] trusted_hash` computed over, can Boss stamp it deterministically per run, and is there an observable signal that a configured hook is armed — without `--dangerously-bypass-hook-trust`?
- **Related:** Codex-as-first-class-agent-driver design (OQ-1 / hook-trust gate); [codex-progress-channel-decision](./codex-progress-channel-decision-2026-07-24.md) (hooks fail open silently — progress uses stdout; interception still rides hooks).
- **Implementation:** `tools/boss/engine/codex-hook-trust` (`boss_engine_codex_hook_trust`).

## TL;DR

| Question                             | Answer                                                                                                                                                                              |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| What is `trusted_hash` over?         | A **normalized hook identity** (event label + matcher + command handler fields), hashed as canonical JSON SHA-256 → `sha256:{hex}`. **Not** the executable bytes.                   |
| Can Boss stamp it deterministically? | **Yes.** Write `[hooks.state."<key>"].trusted_hash` in the per-run user `config.toml` with the same identity Codex will compute.                                                    |
| Observable arming signal?            | **Yes.** `codex app-server` `hooks/list` returns `trustStatus` (`trusted` / `untrusted` / `modified` / `managed`) and `currentHash` per hook. Empty/missing responses are failures. |
| Bypass flag?                         | **Do not use.** It also trusts project-local `.codex/` hooks.                                                                                                                       |

**Gate rule:** missing, stale, or unobservable attestation → refuse the worker. Silence is not success.

## Trusted-hash inputs (codex-cli 0.145.0)

Source of truth: `openai/codex` `codex-rs/hooks/src/engine/discovery.rs` (`command_hook_hash`) and `codex-rs/config/src/fingerprint.rs` (`version_for_toml`).

```text
NormalizedHookIdentity {
  event_name: "pre_tool_use" | "session_start" | …,   // snake_case
  matcher: Option<String>,                            // omitted when None
  hooks: [
    {
      type: "command",
      command: "<exact config command string>",
      timeout: <u64>,     // default 600; SessionEnd default 1
      async: <bool>,
      // statusMessage / additionalContextLimit omitted when None
      // commandWindows forced to None before hashing
    }
  ]
}
→ TOML value → JSON → sort object keys recursively → SHA-256 → "sha256:{hex}"
```

State key:

```text
{absolute_config_path}:{event_label}:{group_index}:{handler_index}
```

Example: `/private/var/.../home/config.toml:pre_tool_use:0:0`

**Path realpath matters.** On macOS, `/var/folders/...` and `/private/var/folders/...` are different strings; Codex keys use the resolved form. Boss must stamp with `canonicalize`d paths that match the command strings written into config.

**Executable content is not in Codex's hash.** Changing the guard script bytes without changing the command path leaves `trusted_hash` valid. Boss therefore binds a separate `guard_content_sha256` into the attestation.

## State location

```toml
[hooks.state."/abs/path/config.toml:pre_tool_use:0:0"]
trusted_hash = "sha256:…"
```

Only **user** and **session-flags** layers contribute state (`hook_states_from_stack`). Boss owns the per-run `CODEX_HOME` user config, so this is the right place to stamp.

## Observation (anti-silence)

`codex app-server --listen stdio://` with `CODEX_HOME` set, then JSON-RPC:

1. `initialize`
2. `notifications/initialized`
3. `hooks/list` → each hook has `key`, `currentHash`, `trustStatus`, `enabled`

Verified live 2026-07-26: after stamping matching hashes, `trustStatus` becomes `trusted` and `codex exec` (without `--dangerously-bypass-hook-trust`) fires `SessionStart`.

Failure modes that **must refuse** dispatch:

| Symptom                                        | Gate outcome        |
| ---------------------------------------------- | ------------------- |
| `hooks/list` fails / empty `data` / no entries | `ObservationFailed` |
| Required key absent from list                  | `HookNotListed`     |
| `trustStatus != trusted`                       | `HookNotTrusted`    |
| `currentHash != stamped hash`                  | `HashMismatch`      |
| Guard path missing / not executable            | `GuardExecutable*`  |
| Prior attestation but guard bytes changed      | `AttestationStale`  |

## Bypass flag blast radius

`--dangerously-bypass-hook-trust` runs **all** enabled hooks without a trust record — including project-local `.codex/` hooks from the repo under work. That content is worker- and attacker-controllable in Boss's model. The gate never passes this flag.

## Crate API (for CodexDriver spawn)

```text
arm_and_attest(ArmRequest) -> Result<HookTrustAttestation, TrustGateError>
verify_attestation(&attestation, &hooks) -> Result<(), TrustGateError>
write_attestation_file(path, &attestation)
command_hook_trusted_hash(...)   // pure
stamp_hook_trust(config_path, hooks)  // write-only; prefer arm_and_attest
```

Call sequence at spawn:

1. Write per-run `CODEX_HOME/config.toml` with `[[hooks.*]]` definitions (guard absolute paths).
2. Materialize guard executables (mode `0755`).
3. `arm_and_attest` — stamps trust, observes via `hooks/list`, returns attestation or refuse.
4. Persist attestation JSON next to the run for audit.
5. Spawn `codex exec` **without** `--dangerously-bypass-hook-trust`.

## What this does not prove

Arming proves Codex **will invoke** the configured handlers (trust record matches identity). It does not prove a hook ran during a later worker turn — that remains a runtime concern (e.g. SessionStart marker, deny telemetry). The gate's job is the silent-trust failure mode, not end-to-end runtime liveness of every tool call.
