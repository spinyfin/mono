// Default (unstamped) provenance constants for non-Bazel (Cargo) builds.
// Bazel builds override BOSS_BUILD_PROVENANCE_RS via compile_data +
// rustc_env to point at installer:build_provenance_rs, which stamps
// these from the workspace-status script.
pub const GIT_SHA: &str = "unknown";
pub const GIT_DIRTY: bool = false;
pub const BUILD_TIME: &str = "unknown";
