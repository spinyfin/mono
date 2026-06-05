# buildifier-wasm (PROTOTYPE)

A throwaway spike: the built-in checkleft buildifier check, reimplemented as a
`sandbox-v1` wasm external check running under wasmtime. It exists to ground the
checkleft external-check capability-model design — **not** to replace the
built-in `BuildifierCheck`, which is untouched and still runs.

**Read [PROTOTYPE-NOTES.md](./PROTOTYPE-NOTES.md) first** — it is the real
deliverable (the discovered wasm guest ABI, the fenced policy relaxations, what
worked/didn't, and what production needs).

## Layout

| File | What |
|---|---|
| `src/lib.rs` | wasm guest: ABI glue (`checkleft_run`, `checkleft_alloc`), the `run_command` host import, and the per-file buildifier orchestration. |
| `src/parser.rs` | buildifier JSON → `Finding` transform, copied from the built-in check; host-testable with `cargo test`. |
| `buildifier-wasm.toml` | the `sandbox-v1` external check manifest. |
| `build.sh` | builds `buildifier_wasm.wasm` (cargo, wasm32 target) and prints its sha256. |
| `run-parity.sh` | runs the built-in vs wasm check over the fixtures and diffs findings. |

## Why cargo, not bazel

There is no `wasm32-unknown-unknown` rust toolchain registered in `MODULE.bazel`,
so this crate is built out-of-band with cargo and is **detached** from the repo
Cargo workspace (empty `[workspace]` in its `Cargo.toml`) so it can't perturb the
monorepo lockfile / crate_universe / bazel build.

## What runs in CI

The host-side primitive (the genuinely new infrastructure) and the transform are
covered hermetically:

- `bazel test //tools/checkleft:checkleft_lib_test` — the `run_command` host
  primitive, the policy fencing, and a WAT guest exercising the import.
- `cargo test` in this directory — the parser's parity with the built-in's golden
  inputs.

The full buildifier-through-wasm run needs `buildifier` + the wasm32 target and is
documented via `build.sh` / `run-parity.sh`.
