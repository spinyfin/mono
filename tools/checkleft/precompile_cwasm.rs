//! Build-time tool: precompile bundled wasm components to AOT `.cwasm` fixtures.
//!
//! Invoked by the `precompiled_cwasm_dir` Bazel rule
//! (`//tools/checkleft/wasm:defs.bzl`). For each input `.wasm` component it runs
//! `checkleft::external::precompile_into_cache_dir`, which uses the *same*
//! wasmtime engine configuration the runtime builds and writes the result under
//! the canonical cache-key filename. The output directory is therefore a
//! ready-to-use `.cwasm` cache that `checkleft_lib_test` points at (via
//! `DefaultExternalCheckExecutor::new_with_cache`), so the heavy `giant-structs`
//! tests deserialize the precompiled artifact instead of JIT-compiling it at
//! runtime — see
//! `docs/investigations/checkleft-lib-test-wasm-compile-timeout.md`.
//!
//! Usage: `precompile_cwasm <out_dir> <component.wasm>...`
//!
//! The produced `.cwasm` is wasmtime-version + engine-config + host-target
//! specific (all folded into the cache key), so it is a host-build artifact,
//! never a portable committed blob.

use std::path::Path;

use anyhow::{Context, Result, bail};

use checkleft::external::precompile_into_cache_dir;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let out_dir = args
        .next()
        .context("usage: precompile_cwasm <out_dir> <component.wasm>...")?;
    let out_dir = Path::new(&out_dir);

    let mut count = 0usize;
    for wasm_arg in args {
        let wasm_path = Path::new(&wasm_arg);
        let bytes = std::fs::read(wasm_path)
            .with_context(|| format!("failed to read wasm component {}", wasm_path.display()))?;
        let dest = precompile_into_cache_dir(out_dir, &bytes)
            .with_context(|| format!("failed to precompile {}", wasm_path.display()))?;
        eprintln!("precompiled {} -> {}", wasm_path.display(), dest.display());
        count += 1;
    }

    if count == 0 {
        bail!("no .wasm component inputs were given");
    }
    Ok(())
}
