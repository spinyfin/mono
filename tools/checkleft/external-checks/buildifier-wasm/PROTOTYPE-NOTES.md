# PROTOTYPE-NOTES — buildifier as a sandbox-v1 wasm external check

**Status: sanctioned throwaway spike.** Goal: prove the built-in buildifier check
can run as a `sandbox-v1` (wasm/wasmtime) external check, to ground the broader
checkleft external-check capability-model design. This is *not* a production
migration; the built-in `BuildifierCheck` is untouched and still runs alongside.

This document is the primary deliverable. It records (a) the wasm guest ABI as
reverse-engineered, (b) exactly what was relaxed in `command_policy.rs` and why,
(c) what worked / what didn't, and (d) what a production version needs.

---

## (a) The sandbox-v1 wasm guest ABI

Reverse-engineered from `tools/checkleft/src/external/runtime.rs`. The runtime
tries a **core module** first, then falls back to a **component**. A Rust
`cdylib` compiled to `wasm32-unknown-unknown` is a core module, so the guest and
this writeup target the **core-module** contract.

### What already existed

Core-module exports the runtime reads:

- `(memory "memory")` — the guest's linear memory.
- `(func "checkleft_run" (param i32 i32) (result i64))` — the entry point.
  - params `(input_ptr, input_len)` point at a UTF-8 JSON `ExternalCheckRuntimeInput`
    the host writes at offset 0: `{ changeset, config, capabilities }`.
    - `changeset` is the full `ChangeSet` (note `changed_files[].path` only — **no
      file contents**).
    - `config` is the check's TOML config, serialized as JSON.
    - `capabilities` is `{ commands, command_timeout_ms, max_stdout_bytes,
      max_stderr_bytes }` — the *resolved* allow-list and limits.
  - result is an `i64` packing `(output_ptr << 32) | output_len`, pointing at a
    UTF-8 JSON `{ "findings": [Finding, ...] }` the guest wrote into its memory.

`Finding` serializes as
`{ severity, message, location: {path,line,column}|null, remediations: [..],
suggested_fix: ..|null }`, `severity` lowercased (`"warning"` etc.).

### THE KEY GAP (the spike's central finding)

`command_policy.rs` defined `ExternalCommandCapabilities` (`validate_invocation`,
timeout, stdout/stderr caps), and `runtime.rs` *serialized* those capabilities
into the guest input — **but there was no host function a guest could call to
actually run a command.** Core modules were instantiated with **no imports**
(`Instance::new(store, module, &[])`) and components with an **empty `Linker`**.
So `validate_invocation` was dead code: nothing in the runtime ever called it,
and a wasm guest had no way to shell out. The capability plumbing was a façade in
front of a primitive that didn't exist yet.

### What this spike added (the host-mediated command primitive)

A new host import, wired into the **core** path only:

- import `("checkleft", "run_command")`, `(param i32 i32) (result i64)`:
  - params `(request_ptr, request_len)` point at a UTF-8 JSON `HostCommandRequest`
    `{ "program": "...", "args": ["..."] }` the guest wrote into memory.
  - the host **validates** `program`/`args` against `ExternalCommandCapabilities`
    and enforces the **timeout** + **stdout/stderr caps**, runs the process with
    cwd = repo root, then writes a UTF-8 JSON `HostCommandResponse`
    `{ exit_code, stdout, stderr, timed_out, error }` back into guest memory.
  - result is an `i64` packing `(response_ptr << 32) | response_len`.
- to get a landing buffer for the response, the host calls a new guest export
  `(func "checkleft_alloc" (param i32) (result i32))` (returns a pointer to N
  writable bytes). This mirrors the wasm-bindgen-style allocator pattern.

Policy/command failures are reported **in-band** (`error` field), not as wasm
traps, so the guest can turn them into findings (mirroring the built-in's
`error_finding` path). Genuine ABI faults (bad pointer, missing `checkleft_alloc`)
do trap.

Implementation: `register_host_command` / `host_run_command` / `execute_host_command`
in `runtime.rs`. The core path now instantiates through a
`wasmtime::Linker<HostCommandState>` (state = repo root + resolved capabilities)
instead of `Instance::new(.., &[])`. Modules that don't import `run_command`
(e.g. the simple WAT test fixtures) still instantiate — an unused linker
definition is harmless — so all pre-existing runtime tests pass unchanged.

### Guest control flow (this crate)

`checkleft_run` → parse input → for each changed Starlark file (skip deleted /
non-Starlark): build `run_command` requests for the **format** pass
(`buildifier --mode=check --format=json <path>`) and the **lint** pass
(`buildifier --mode=check --lint=warn --format=json <path>`) → call the import →
parse buildifier's JSON into `Finding`s (`src/parser.rs`, copied from the built-in)
→ return `{ "findings": [...] }`.

Note the one behavioral difference from the built-in: the built-in pipes file
**contents** to buildifier via stdin with `-path=<p>` (because it has the bytes
from the source tree); the guest only gets the **path** (the changeset carries no
contents), so it runs buildifier on the path directly and lets buildifier read
the file from disk. The parsed findings are equivalent because the parser keys the
`Location` off the checkleft-side path, not buildifier's echoed filename.

---

## (b) What was relaxed in `command_policy.rs`, and why

All relaxations are **fenced** behind a separate constructor and an env flag; the
production `from_manifest` path is byte-for-byte unchanged (a unit test,
`production_ceiling_still_rejects_buildifier`, locks that down).

| Constant (PROTOTYPE-ONLY) | Production | Prototype | Why |
|---|---|---|---|
| ceiling | `["cat","grep","sed","wc"]` | adds `["buildifier","bazel"]` | the check needs `buildifier`; `bazel` is admitted so the `buildifier_target` resolution route is *available* to test (the guest defaults to a direct path) |
| timeout | 2s | 120s | `bazel build` of the buildifier target can exceed 2s on a cold cache |
| max stdout | 64 KiB | 4 MiB | buildifier `--format=json` over a large BUILD/.bzl file (or bazel chatter) can exceed 64 KiB |
| max stderr | 64 KiB | 1 MiB | same |
| wasm fuel | 10M | 5,000M | a Rust+serde_json guest needs far more compute fuel than the tiny hand-written WAT checks the limit was sized for |

Mechanics:

- `ExternalCommandCapabilities::from_manifest_prototype()` uses the union of the
  global + prototype ceilings and the relaxed limits. Shell binaries
  (`sh`/`bash`/`zsh`) and inline `python -c` / `node -e` remain blocked.
- The runtime selects it **only** when `CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1`
  (`prototype_commands_enabled()`), and only for the artifact (wasm) path. Unset
  ⇒ production policy + 10M fuel.
- Nothing widens the default policy for all checks. The trust-tiered model is
  explicitly out of scope (see (d)).

---

## (c) What worked / what didn't

**Worked (verified hermetically under `bazel test //tools/checkleft/...`):**

- The new host command primitive end-to-end through wasmtime: a WAT guest writes
  a `run_command` request, the host validates + runs the process + writes the
  response back via `checkleft_alloc`, the guest returns findings, no trap
  (`wasm_guest_invokes_host_command_import`).
- The host executor itself: runs an allow-listed command and captures stdout
  (`host_command_runs_allowed_command_and_captures_stdout`), refuses a command
  outside the allow-list before spawning
  (`host_command_rejects_command_outside_allow_list`), and truncates stdout at
  the cap (`host_command_truncates_stdout_at_capability_cap`).
- Policy fencing: prototype ceiling admits `buildifier`/`bazel` but not arbitrary
  binaries and still blocks shells; production ceiling still rejects `buildifier`.
- Transform parity: the guest's copied parser produces the same findings as the
  built-in check's own golden parser inputs (`cargo test` in this crate — 6
  tests mirroring `checks/buildifier.rs`'s parser tests).

**Didn't / deferred (environment limits, not design blockers):**

- **No live full end-to-end run in this worker.** The CI worker has no
  `rustup`/`wasm32-unknown-unknown` target and no `buildifier` on PATH, so the
  `.wasm` could not be built and a real buildifier-through-wasm run could not be
  captured here. The build + the full parity comparison are scripted instead
  (`build.sh`, `run-parity.sh`) to run where those tools exist. The mechanism is
  nonetheless proven hermetically by the WAT test above (a real subprocess run
  through the wasm host import) plus the transform-parity tests.
- **No bazel wasm artifact.** There is no `wasm32-unknown-unknown` rust toolchain
  registered in `MODULE.bazel`, so the guest is a cargo-built, workspace-detached
  crate rather than a bazel target. (The task explicitly permitted documenting
  the build step when no rust→wasm bazel rule exists.)
- **Parser is copied, not shared.** The guest can't depend on the `checkleft`
  crate (it pulls wasmtime/tokio, which don't target wasm), so `src/parser.rs` is
  a copy. Parity is currently by golden-test, not by construction.
- **Component path unchanged.** Host commands are wired into the core path only.
- **bazel-target resolution not exercised.** The guest defaults to a direct
  `buildifier_path`; resolving `buildifier_target` would mean the guest shelling
  `bazel build`/`bazel cquery` through `run_command` and parsing paths — fiddly,
  and a host primitive is the better answer (see (d)).

---

## (d) What a production version would need

1. **A capability/trust-tiered policy model.** Replace the single global ceiling
   + prototype escape hatch with declared, reviewed capability tiers (e.g.
   "first-party check may run `buildifier`"; "third-party check: read-only
   coreutils only"). Per-command timeouts/caps should be policy-derived, not a
   single global relaxation. This spike's `from_manifest_prototype` is the
   anti-pattern to replace.
2. **bazel-label resolution as a host primitive**, not a guest-driven `bazel`
   shell-out. The host should expose something like
   `resolve_label(label) -> path` (build + cquery, cached, locked) so guests get
   a resolved executable path without `bazel` in their command allow-list and
   without parsing cquery output in wasm.
3. **A shared protocol crate.** Factor `Finding`/`Location`/`Severity`, the
   `ChangeSet` view, and the buildifier JSON types + parser into a lean,
   dependency-light crate that BOTH the built-in check and wasm guests depend on,
   so parity is guaranteed by construction. Define the host↔guest ABI
   (`run_command`, `checkleft_alloc`, the request/response JSON) there as the
   stable contract, ideally with a small guest SDK so check authors don't
   hand-roll the memory/encoding glue.
4. **A bazel rust→wasm rule + toolchain.** Register the
   `wasm32-unknown-unknown` triple and a `rust_shared_library`/platform-transition
   rule so artifacts are built, hashed, and provenance-stamped by bazel (the
   manifest already has a `provenance` slot), and the parity test can be
   hermetic.
5. **Optionally feed file contents to the guest** (or give it a host read
   primitive) so checks that need bytes don't depend on a binary re-reading from
   disk; this also removes the stdin-vs-path discrepancy noted in (a).
6. **Component-model support** for the host command import, so guests built as
   components (not just core modules) get the same primitive.

---

## How to reproduce

```sh
rustup target add wasm32-unknown-unknown
cd tools/checkleft/external-checks/buildifier-wasm
./build.sh            # builds buildifier_wasm.wasm, prints its sha256
# paste the sha into buildifier-wasm.toml's artifact_sha256, then:
./run-parity.sh       # needs buildifier + a built `checkleft` on PATH
```

Hermetic tests that DO run in CI:

```sh
bazel test //tools/checkleft:checkleft_lib_test   # host primitive + policy + WAT wiring
( cd tools/checkleft/external-checks/buildifier-wasm && cargo test )  # transform parity
```
