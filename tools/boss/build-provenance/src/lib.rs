//! Engine build provenance: the actual commit sha, working-tree dirty
//! flag, and build wall-clock time, stamped at Bazel build time.
//!
//! Kept as its own crate — rather than a module inside `engine/core`,
//! where `boss_engine::build_info` lives — for a normal crate-boundary
//! reason: it isolates the per-commit/per-build stamped values (and the
//! `include!(env!(...))` machinery that reads them) from `engine/core`'s
//! own source, so a rebuild of this crate never touches `engine/core`'s
//! compilation unit directly. `build_info_rs`'s own doc comment
//! (`tools/boss/installer/pkg.bzl`) explains why `BOSS_GIT_SHA` /
//! `BOSS_BUILD_TIME` are deliberately left as the literal `"unknown"`
//! in that shared, `include!`d-into-every-binary stamp.
//!
//! **This crate boundary does NOT protect `engine_lib`'s Bazel action
//! cache.** The functions below are trivial getters over `&'static str`
//! / `bool` constants, so rustc's `.rmeta` for this crate embeds those
//! constant values (they're cross-crate inlining candidates) — with
//! `pipelined_compilation` on or off, a changed `GIT_SHA` changes this
//! crate's `.rmeta`, and `engine_lib` (which depends on that `.rmeta`/
//! `.rlib`) still recompiles on every commit — verified empirically by
//! building twice with only the commit sha changed and observing
//! `engine_lib` recompile both times, with `pipelined_compilation`
//! enabled. The crate split buys logical separation and a much smaller,
//! near-instant *compile* of the changed value, but not cache isolation
//! for `engine_lib` itself — that would require reading these values
//! from a runtime resource instead of a compiled-in Rust constant —
//! not done here.
//!
//! Exposed as plain functions (not `pub const`s) as a style preference
//! matching the rest of this crate's API — it does not change the cache
//! behavior described above.

mod stamp {
    include!(env!("BOSS_BUILD_PROVENANCE_RS"));
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
        assert!(!git_sha().is_empty());
        assert!(!build_time().is_empty());
        // git_dirty() is a plain bool; just confirm the call compiles
        // and returns without panicking under the Cargo default stamp.
        let _ = git_dirty();
    }
}
