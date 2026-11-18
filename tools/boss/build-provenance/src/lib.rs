//! Engine build provenance: the stamped release version, commit sha,
//! working-tree dirty flag, and build wall-clock time, stamped at Bazel
//! build time.
//!
//! **This crate is a leaf, and must stay one.** Its generated source
//! (`//tools/boss/installer:build_provenance_rs`, read from Bazel's
//! stable-status.txt) embeds the full commit sha, so the crate's source —
//! and therefore its compiled `.rlib` — changes on every commit. Anything
//! that links it recompiles on every commit too, and any test binary that
//! links it re-executes on every commit (the binary's digest, an input of
//! the test action, changes). That is why only the `engine` rust_binary
//! targets in `//tools/boss/engine/core` depend on it: they hand the four
//! values to `boss_engine::build_info::install_provenance` at startup, and
//! `engine_lib` (which `boss`, `bossctl` and every
//! `rust_test(crate = ":engine_lib")` share) never sees this crate.
//! `//tools/boss/engine/core:stamped_provenance_isolation_test` guards
//! that edge; the history is in
//! `tools/boss/docs/investigations/docs-only-pr-reexecutes-engine-tests-2026-10-21.md`.
//!
//! `build_info_rs`'s own doc comment (`tools/boss/installer/pkg.bzl`)
//! explains why `BOSS_GIT_SHA` / `BOSS_BUILD_TIME` are deliberately left as
//! the literal `"unknown"` in the shared, `include!`d-into-every-binary
//! stamp that `engine_lib` does compile in.
//!
//! Exposed as plain functions (not `pub const`s) as a style preference
//! matching the rest of the Boss crates' API.

mod stamp {
    include!(env!("BOSS_BUILD_PROVENANCE_RS"));
}

/// Stamped Boss version: `1.0.N` on a release tag, `1.0.N-dev-<sha>`
/// otherwise, or `"unknown"` on a Cargo (non-Bazel) build. Never the
/// Cargo-crate placeholder `0.0.0`.
pub fn version() -> &'static str {
    stamp::VERSION
}

/// Full git commit sha the running engine was built from, or
/// `"unknown"` on a Cargo (non-Bazel) build. Long enough to feed
/// directly to `git merge-base --is-ancestor <sha> main`.
pub fn git_sha() -> &'static str {
    stamp::GIT_SHA
}

/// `true` if the working tree had uncommitted changes at build time.
/// Always `false` on a Cargo (non-Bazel) build.
pub fn git_dirty() -> bool {
    stamp::GIT_DIRTY
}

/// ISO-8601 UTC timestamp of the build that last produced this commit's
/// provenance stamp, or `"unknown"` on a Cargo (non-Bazel) build. Bazel
/// does not re-run the stamping action just because wall-clock time
/// passed (the value comes from the volatile-status file, which is
/// excluded from the action cache key), so this may lag a later rebuild
/// of the same commit — use [`git_sha`] plus a binary content fingerprint
/// (see `engine::build_info::binary_fingerprint`) to tell two builds of
/// the same commit apart.
pub fn build_time() -> &'static str {
    stamp::BUILD_TIME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_values_are_non_empty() {
        assert!(!version().is_empty());
        assert_ne!(version(), "0.0.0");
        assert!(!git_sha().is_empty());
        assert!(!build_time().is_empty());
        // git_dirty() is a plain bool; just confirm the call compiles
        // and returns without panicking under the Cargo default stamp.
        let _ = git_dirty();
    }
}
