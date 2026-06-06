# PROTOTYPE-NOTES — giant-struct check as a sandbox-v1 wasm external check

**Status: sanctioned spike.** Goal: prove the custom **programmatic**
external-check path works end-to-end in checkleft by reimplementing the built-in
`rust-giant-structs-use-builder` check as a `sandbox-v1` (wasm/wasmtime) external
check, loaded through the bundled check-def provider + a CHECKS source directive,
running with **parity** to the built-in.

This check is the right vehicle precisely because it is a poor fit for the
declarative/zero-code model the buildifier spikes used: it parses Rust with `syn`,
counts named struct fields (>5 ⇒ require a builder), and exempts clap-derive
`Args`/`Parser`/`Subcommand` structs. None of that is expressible as
config/pattern-matching, so it exercises the programmatic wasm path for real.

The built-in `rust-giant-structs-use-builder` is **untouched** and stays as the
parity reference.

This builds on three landed foundations — T1371 (wasm checks restricted to Rust),
T1398 (declarative zero-code external check), T1407 (bundled check-def provider +
CHECKS source directive) — and on the **still-in-review** sandbox-v1 wasm runtime
(T1397 / PR 1376), which this branch is stacked on. It does **not** re-implement
that runtime; it only authors a guest against the ABI PR 1376 added.

---

## (a) How it runs end-to-end

1. A CHECKS file references the check via the **source directive**
   `bundled:giant-struct-wasm`. The bundled check-def provider
   (`src/external/bundled.rs`) resolves it to the embedded TOML manifest
   `checks/giant-struct-wasm/check.toml` (`mode = "wasm"`, `runtime = "sandbox-v1"`,
   `capabilities.commands = ["cat"]`, `artifact_path` + `artifact_sha256`).
2. The runtime (`DefaultExternalCheckExecutor`) reads the committed
   `giant_struct_wasm.wasm`, validates its sha256, instantiates it as a wasm core
   module, writes the `ExternalCheckRuntimeInput` JSON (`{changeset, config,
   capabilities}`), and calls `checkleft_run(ptr, len)`.
3. **The guest** (`src/lib.rs` + `src/analyzer.rs`, compiled to
   `wasm32-unknown-unknown`): for each changed `.rs` file (skipping deleted), it
   reads the file **contents** by calling the host import
   `("checkleft","run_command")` with `cat <path>` (cwd = repo root), parses the
   bytes with `syn`, runs the giant-struct analysis, and turns each violation into
   a checkleft `Finding` with the **same** message, location, severity, and
   remediations as the built-in. It returns `{ "findings": [...] }`.
4. The host deserializes the findings unchanged.

Parity is proven two ways:

- **Host-side analysis parity** (`cargo test` in this crate): `src/analyzer.rs` is
  copied verbatim from the built-in's `rust_giant_struct_common.rs`; its tests feed
  the same golden sources the built-in's own tests use. `src/lib.rs`'s tests assert
  the finding shape (message/remediation/location/severity/`suggested_fix: null`).
- **Real wasm-execution parity** (`bazel test
  //tools/checkleft:giant_struct_wasm_e2e_test`): loads the committed `.wasm`, runs
  it through the real executor over a temp source tree, and asserts the findings are
  **byte-for-byte identical** to the built-in check run via the public check
  registry — including the flagged 6-field struct AND the exempt clap `Args` struct.

---

## (b) The discovered gaps (the spike's central findings)

### Gap 1 — the sandbox-v1 ABI has no file-read primitive

The changeset the host hands the guest carries only `changed_files[].path`, **not
contents** (confirmed in PR 1376's notes). A check that needs the bytes — this one
does, to feed `syn` — must obtain them itself. The only host primitive in the ABI
is `("checkleft","run_command")`, so the guest shells out to **`cat <path>`**.

That works, and needs **no command-policy relaxation**: `cat` is already in
checkleft's production global command ceiling (`["cat","grep","sed","wc"]`). But it
couples file access to two unrelated knobs:

- **stdout cap → silent truncation of large files.** `run_command` truncates the
  command's stdout at `max_stdout_bytes` (production: 64 KiB; PR 1376's prototype:
  4 MiB). A `.rs` file larger than the active cap is delivered truncated, `syn`
  fails to parse the partial source, and the guest **silently skips the file** —
  missing violations the built-in (which reads the whole file from the source tree)
  would flag. This is a real parity divergence for files over the cap.
- **one subprocess spawn per changed file**, versus the built-in's in-memory
  `tree.read_file`.

A production design should give the guest a **host file-read primitive** (or feed
file contents in the changeset) so byte-needing checks don't depend on `cat`, the
command allow-list, or the stdout cap.

### Gap 2 — wasm fuel is far too low for a syn guest, and is coupled to the command flag

The runtime's production fuel ceiling is **10M** instructions; PR 1376 raised it to
**5,000M** only under `CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1` (the same flag that
widens the command ceiling). A `syn` + `serde_json` guest needs far more than 10M.

Measured on this guest (production 10M vs. prototype 5,000M fuel), one `.rs` file of
N six-field structs:

| structs (N) | file size | production (10M) | prototype (5,000M) |
|---|---|---|---|
| 10  | 0.8 KiB | ✅ 10 findings | ✅ 10 |
| 100 | 8.2 KiB | ❌ `all fuel consumed` trap | ✅ 100 |
| 500 | 41 KiB  | ❌ trap | ✅ 500 |
| 1000| 83 KiB  | ❌ trap | ✅ 1000 |

So 10M only covers a *tiny* file (~10 giant structs); any realistic Rust file
exhausts it. The check is therefore usable only under the prototype fuel today.

Two distinct problems: the production ceiling is mis-sized for a real Rust guest,
**and** fuel is bundled with the command-policy escape hatch — there is no way to
ask for "more fuel, production command policy." A production design needs a
fuel budget that is policy-derived (or per-check) and **decoupled** from the
command allow-list. (Note: unlike buildifier, this check needs *no* command
relaxation — only fuel — which is exactly why the coupling is the wrong shape.)

### Gap 3 — config + exclusion fidelity not fully ported

The guest honors `max_fields`, `builder`, and **simple-name** `exclude_structs`.
It does **not** port the built-in's `config_dir`-scoped / qualified (`path::Struct`)
exclusions, `exclude_files` globs, or the stale-exclusion auditing — those need the
CHECKS file directory scope and a source-tree walk the guest does not have, and the
auditing hooks (`declared_exclusions` / `evaluate_exclusion`) live on the built-in
`ConfiguredCheck` trait, which has no analogue in the wasm finding-only ABI. Parity
holds for the core rule and the ported knobs; the grandfathering machinery is a
larger, separate design.

---

## (c) What worked / what didn't

**Worked (verified):**

- `syn` compiles to `wasm32-unknown-unknown` and parses Rust inside the sandbox
  (artifact ~566 KiB with `opt-level="s"` + `lto` + `strip`).
- Real wasm-execution parity with the built-in, through the actual runtime + the
  `cat` host primitive, including remediation rendering and the clap `Args`
  exemption (`giant_struct_wasm_e2e_test`).
- Bundled registration + CHECKS source-directive resolution: `bundled:giant-struct-wasm`
  resolves through the provider to a package whose declared sha256 matches the
  committed artifact.
- Host-side analysis parity (`cargo test`): the copied analyzer + finding shapes.

**Didn't / deferred:**

- **No bazel wasm artifact.** There is no `wasm32-unknown-unknown` rust toolchain in
  `MODULE.bazel`, so the guest is a cargo-built, workspace-detached crate and the
  `.wasm` is **committed** (loaded via `include_bytes!`), as buildifier-wasm
  documented. `build.sh` rebuilds it and prints the sha to paste into the manifest.
- **Analyzer is copied, not shared** (same reason as buildifier-wasm: the guest
  can't depend on the `checkleft` crate, which pulls wasmtime/tokio). Parity is by
  test, not by construction.
- **Not enabled in mono's dogfooding CHECKS.** Turning it on at the repo root would
  flag real un-grandfathered giant structs across mono and break CI; the directive
  path is proven by test instead.
- **Component path unchanged** (PR 1376 wired host commands into the core path only).

---

## (d) What a production version would need

1. **A host file-read primitive** (or file contents in the changeset), so
   byte-needing checks don't shell out to `cat` and aren't bound by the stdout cap.
2. **A right-sized, decoupled fuel budget** — policy-derived/per-check, not tied to
   the command-policy escape hatch; 10M is far too low for a real `syn` guest.
3. **A shared, dependency-light protocol crate** holding `Finding`/`Location`/
   `Severity`, the `ChangeSet` view, and the check logic, so the built-in and the
   wasm guest share one implementation and parity is guaranteed by construction.
4. **A capability/exclusion model rich enough** to carry `config_dir` scope and the
   stale-exclusion auditing hooks to a wasm guest (or keep auditing host-side).
5. **A bazel rust→wasm rule + toolchain**, so the artifact is built, hashed, and
   provenance-stamped by bazel and the parity test is fully hermetic.

---

## How to reproduce

```sh
rustup target add wasm32-unknown-unknown
cd tools/checkleft/external-checks/giant-struct-wasm
./build.sh            # rebuilds giant_struct_wasm.wasm, prints its sha256
# paste the sha into ../../checks/giant-struct-wasm/check.toml's artifact_sha256
cargo test            # host-side analyzer + finding-shape parity
```

Hermetic tests that run in CI:

```sh
bazel test //tools/checkleft:giant_struct_wasm_e2e_test  # real wasm + parity + bundled directive
bazel test //tools/checkleft:checkleft_lib_test          # bundled-def parse guard, runtime, policy
```
