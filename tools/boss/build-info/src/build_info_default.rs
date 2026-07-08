// Default (unstamped) build-info constants for non-Bazel (Cargo) builds.
//
// Shared by every Boss binary: each crate's build.rs points
// BOSS_BUILD_INFO_RS at this one file so `include!(env!(...))` compiles
// without Bazel. Bazel builds override BOSS_BUILD_INFO_RS via
// compile_data + rustc_env to point at installer:build_info_rs, which
// stamps BOSS_VERSION with the real tag-derived value.
//
// BOSS_GIT_SHA / BOSS_BUILD_TIME are part of the stamped file's shape so
// the default and stamped files match, but not every consumer reads them
// (the CLIs only surface BOSS_VERSION), so they carry #[allow(dead_code)]
// exactly as installer:build_info_rs emits them.
pub const BOSS_VERSION: &str = "unknown";
#[allow(dead_code)]
pub const BOSS_GIT_SHA: &str = "unknown";
#[allow(dead_code)]
pub const BOSS_BUILD_TIME: &str = "unknown";
