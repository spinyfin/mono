// Default (unstamped) build-info constants for non-Bazel (Cargo) builds.
//
// Bazel builds override BOSS_BUILD_INFO_RS via compile_data + rustc_env
// to point at installer:build_info_rs, which writes BOSS_VERSION =
// "0.0.0-dev" unless the build is stamped (`--config=release`), in which
// case it embeds the tag-derived base version.
//
// BOSS_GIT_SHA / BOSS_BUILD_TIME are part of the stamped file's shape so
// the default and stamped files match, but this crate only surfaces
// BOSS_VERSION, so they carry #[allow(dead_code)] exactly as
// installer:build_info_rs emits them.
pub const BOSS_VERSION: &str = "unknown";
#[allow(dead_code)]
pub const BOSS_GIT_SHA: &str = "unknown";
#[allow(dead_code)]
pub const BOSS_BUILD_TIME: &str = "unknown";
