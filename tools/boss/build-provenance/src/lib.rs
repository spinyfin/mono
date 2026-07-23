//! Engine build provenance: the actual commit sha, working-tree dirty
//! flag, and build wall-clock time, stamped at Bazel build time.
//!
//! Kept as its own crate — rather than a module inside `engine/core`,
//! where `boss_engine::build_info` lives — so that stamping a value
//! which changes on every commit (the sha) and every build (the build
//! time) invalidates only this handful of lines, not a full recompile
//! of the largest crate in the tree. `build_info_rs`'s own doc comment
//! (`tools/boss/installer/pkg.bzl`) explains why `BOSS_GIT_SHA` /
//! `BOSS_BUILD_TIME` are deliberately left as the literal `"unknown"`
//! in that shared, `include!`d-into-every-binary stamp: those values
//! live here instead, behind a normal crate boundary, so `engine/core`
//! depends on this crate's compiled interface rather than splicing its
//! generated source directly into `engine/core`'s own compilation unit.
//!
//! Exposed as plain functions (not `pub const`s) so a changed value
//! only requires relinking `engine/core`'s dependents, not re-checking
//! their use of a cross-crate constant.

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

/// ISO-8601 UTC timestamp of when this binary was built, or
/// `"unknown"` on a Cargo (non-Bazel) build.
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
