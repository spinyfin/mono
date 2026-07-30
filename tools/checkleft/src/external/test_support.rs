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

/// Whether a staged runfiles path is expected to be a regular file (a binary)
/// or a directory (a fixture tree). Distinguishes the `exists()` vs `is_dir()`
/// existence check in [`staged_path_from_runfiles`].
pub(crate) enum StagedPathKind {
    File,
    Dir,
}

/// Resolve a Bazel-staged `data` dependency (binary or directory) from the
/// test's runfiles via `env_var`, or `None` when it was not staged (e.g.
/// running under plain `cargo test`, where there are no runfiles).
///
/// Shared by every hermetic e2e test that stages a real external tool
/// (buildifier, rustfmt) or fixture directory (the precompiled `.cwasm`
/// cache) as Bazel `data` + `env`: under `bazel test` the env var is always
/// set to the dependency's runfiles path, so the staged path is always taken
/// in CI. When the env var is absent we *assert* we are genuinely outside
/// Bazel (no `TEST_SRCDIR`), so a broken `data`/`env` wiring can never
/// silently skip the test it backs instead of failing loudly. `what` names
/// the dependency in the resulting panic messages (e.g. `"buildifier"`,
/// `"rustfmt wrapper"`, `"precompiled .cwasm dir"`).
pub(crate) fn staged_path_from_runfiles(env_var: &str, what: &str, kind: StagedPathKind) -> Option<PathBuf> {
    match std::env::var(env_var) {
        Ok(rlocationpath) => {
            let runfiles = runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let path = runfiles
                .rlocation(&rlocationpath)
                .unwrap_or_else(|| panic!("{what} rlocation must resolve"));
            let staged = match kind {
                StagedPathKind::File => path.exists(),
                StagedPathKind::Dir => path.is_dir(),
            };
            assert!(staged, "staged {what} must exist at {}", path.display());
            Some(path)
        }
        Err(_) => {
            assert!(
                std::env::var_os("TEST_SRCDIR").is_none(),
                "running under `bazel test` but {env_var} is unset — the {what} `data`/`env` \
                 wiring on checkleft_lib_test is broken; refusing to silently skip the test it backs"
            );
            None
        }
    }
}

/// Resolve the build-time precompiled `.cwasm` fixture directory from the test's
/// runfiles, or `None` when it was not staged (e.g. running under plain
/// `cargo test`, where there are no runfiles).
///
/// Under `bazel test`, `checkleft_lib_test` sets `CHECKLEFT_PRECOMPILED_CWASM_DIR`
/// to the runfiles path of the `:precompiled_cwasm` directory via
/// `$(rlocationpath ...)`, so the precompiled path is always taken in CI. A
/// broken `data`/`env` wiring can never silently fall back to JIT-compiling the
/// component and re-open the timeout (see [`staged_path_from_runfiles`]).
pub(crate) fn precompiled_cwasm_dir() -> Option<PathBuf> {
    staged_path_from_runfiles(
        "CHECKLEFT_PRECOMPILED_CWASM_DIR",
        "precompiled .cwasm dir",
        StagedPathKind::Dir,
    )
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
