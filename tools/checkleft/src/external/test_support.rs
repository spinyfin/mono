//! Test-only helpers shared by checkleft's component-runtime tests.
//!
//! The headline helper, [`executor_with_precompiled_cache`], points a
//! [`DefaultExternalCheckExecutor`] at the build-time precompiled `.cwasm`
//! fixture directory (`//tools/checkleft:precompiled_cwasm`) so the heavy
//! `rust/giant-structs` tests *deserialize* the AOT artifact (~20 ms) instead of
//! JIT-compiling the component at runtime (~10 s in a `fastbuild`/debug test
//! binary, stacking up under test parallelism). That runtime compile is what
//! pushed `checkleft_lib_test` toward its 60 s `size = "small"` timeout — see
//! `docs/investigations/checkleft-lib-test-wasm-compile-timeout.md`.

use std::path::{Path, PathBuf};

use super::DefaultExternalCheckExecutor;

/// Resolve the build-time precompiled `.cwasm` fixture directory from the test's
/// runfiles, or `None` when it was not staged (e.g. running under plain
/// `cargo test`, where there are no runfiles).
///
/// Under `bazel test`, `checkleft_lib_test` sets `CHECKLEFT_PRECOMPILED_CWASM_DIR`
/// to the runfiles path of the `:precompiled_cwasm` directory via
/// `$(rlocationpath ...)`, so the precompiled path is always taken in CI. As with
/// the buildifier wiring in `declarative::parity_e2e`, we *assert* we are
/// genuinely outside Bazel (no `TEST_SRCDIR`) when the env var is absent, so a
/// broken `data`/`env` wiring can never silently fall back to JIT-compiling the
/// component and re-open the timeout.
pub(crate) fn precompiled_cwasm_dir() -> Option<PathBuf> {
    match std::env::var("CHECKLEFT_PRECOMPILED_CWASM_DIR") {
        Ok(rlocationpath) => {
            let runfiles = runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let dir = runfiles
                .rlocation(&rlocationpath)
                .expect("precompiled .cwasm dir rlocation must resolve");
            assert!(
                dir.is_dir(),
                "staged precompiled .cwasm dir must exist at {}",
                dir.display()
            );
            Some(dir)
        }
        Err(_) => {
            assert!(
                std::env::var_os("TEST_SRCDIR").is_none(),
                "running under `bazel test` but CHECKLEFT_PRECOMPILED_CWASM_DIR is unset — the \
                 precompiled-.cwasm `data`/`env` wiring on checkleft_lib_test is broken; refusing \
                 to silently JIT-compile the giant-structs component (which re-opens the 60 s timeout)"
            );
            None
        }
    }
}

/// Build a [`DefaultExternalCheckExecutor`] rooted at `root`, pointed at the
/// build-time precompiled `.cwasm` cache when available (under `bazel test`) so
/// component checks deserialize the AOT fixture instead of compiling at runtime.
///
/// Outside Bazel (`cargo test`) there is no fixture, so it falls back to the
/// platform-cache-backed `::new` constructor — correct, just slower on a cold
/// cache.
pub(crate) fn executor_with_precompiled_cache(root: &Path) -> DefaultExternalCheckExecutor {
    match precompiled_cwasm_dir() {
        Some(dir) => {
            DefaultExternalCheckExecutor::new_with_cache(root, dir).expect("executor with precompiled .cwasm cache")
        }
        None => DefaultExternalCheckExecutor::new(root).expect("executor"),
    }
}
