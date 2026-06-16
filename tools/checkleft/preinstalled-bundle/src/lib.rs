//! The single multiplexed Component Model component that ships embedded in the
//! checkleft binary, exporting every PREINSTALLED wasm check.
//!
//! # Why a bundle
//!
//! A separate component per check would statically link its OWN copy of the wasm
//! `std`/`core`/`alloc` runtime, `checkleft-check-sdk`, `wit-bindgen` glue, and
//! `serde`/`serde_json`, growing the embedded binary size by a full shared baseline
//! with every new check (heavy deps like `syn` would be duplicated across checks).
//!
//! This crate is a single `cdylib` that depends on each check's source crate as
//! an rlib and calls [`export_checks!`] exactly once. Rust's LTO / dead-code
//! elimination deduplicates the shared baseline and shared deps across all checks
//! inside ONE component. The SDK supports multiple checks per component:
//! the generated `list-checks` / `run-check` exports dispatch by check name, and
//! the host (`tools/checkleft/src/external/runtime.rs`) drives them that way.
//! Check ids, messages, severities, and behavior are unaffected by the bundling.
//!
//! # Per-invocation isolation
//!
//! The host re-instantiates a fresh component instance for every `run-check`
//! call (cheap relative to the AOT compile, which is cached once per component).
//! Sharing one module across checks does not weaken that isolation: one check's
//! panic or memory growth cannot affect a sibling, because each call runs in its
//! own instance with its own linear memory and WASI sandbox.
//!
//! # Boundary: preinstalled only
//!
//! This consolidation is for the in-binary preinstalled set ONLY. It deliberately
//! does NOT change the path for externally-distributed checks, which are loaded
//! at runtime as their own separate components (see
//! `tools/checkleft/src/external/runtime.rs` and the standalone-component build
//! demonstrated by `tools/checkleft/sdk/examples/trivial-check`). Each preinstalled
//! check keeps its own authorship crate under `tools/checkleft/checks/<ns>/<name>/`;
//! only the component packaging is shared here.

use checkleft_check_sdk::export_checks;

export_checks!(
    checkleft_file_forbidden_path::forbidden_path_check,
    checkleft_file_size::file_size_check,
    checkleft_file_ifchange::file_ifchange_check,
    checkleft_md_link_integrity::md_link_integrity_check,
    checkleft_rust_giant_structs_use_builder::giant_structs_check,
    checkleft_rust_giant_structs_create::giant_structs_create_check,
);
