# giant-struct-wasm

A **prototype** checkleft `sandbox-v1` external check: the built-in
`rust-giant-structs-use-builder` check reimplemented as a wasm guest, to prove the
custom **programmatic** external-check path end-to-end (parsing Rust with `syn`,
counting named struct fields, exempting clap-derive structs).

The built-in stays in place as the parity reference. **Read `PROTOTYPE-NOTES.md`** —
it is the primary deliverable (the discovered ABI gaps, the fuel measurements, and
what a production version needs).

## Layout

- `src/analyzer.rs` — the syn-based giant-struct analysis, copied verbatim from the
  built-in's `rust_giant_struct_common.rs` (host-testable).
- `src/lib.rs` — the wasm ABI (`checkleft_run` / `checkleft_alloc` / the
  `run_command` import) and control flow: read each changed `.rs` file via
  `cat <path>` through the host command primitive, parse with `syn`, emit findings
  identical to the built-in.
- `giant_struct_wasm.wasm` — the committed, prebuilt artifact (loaded by the
  hermetic e2e test and the bundled manifest). Rebuild with `./build.sh`.
- `../../checks/giant-struct-wasm/check.toml` — the bundled check-def manifest a
  CHECKS file references via the source directive `bundled:giant-struct-wasm`.

## Build

```sh
rustup target add wasm32-unknown-unknown
./build.sh   # rebuilds the .wasm, prints the sha256 to paste into check.toml
cargo test   # host-side analyzer + finding-shape parity tests
```

There is no `wasm32-unknown-unknown` rust toolchain in `MODULE.bazel`, so the guest
is built out-of-band with cargo (not bazel) and the artifact is committed.
