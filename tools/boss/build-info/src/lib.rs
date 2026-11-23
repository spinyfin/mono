//! Shared build-info stamping and `--version` handling for Boss binaries.
//!
//! **This crate is a leaf, and must stay one.** Its generated source
//! (`//tools/boss/installer:build_info_rs`) embeds the tag-derived
//! release version when `--stamp` / `--config=release` is set, so the
//! crate's compiled `.rlib` changes on every new `boss-v*` tag. Anything
//! that compiles against it recompiles too. That is why `engine_lib`,
//! `runtime`, `app`, `boss_lib`, and `bossctl_lib` never depend on this
//! crate: the `engine`, `boss`, and `bossctl` binaries are thin 1-file
//! crates that link it (and, for `engine`, `boss_build_provenance`), so
//! a new tag recompiles only this leaf crate and relinks those
//! binaries, leaving all library crates cached.
//!
//! Unstamped (`--nostamp`, the default) compiles the constant
//! `0.0.0-dev` placeholder so a new tag does not change this crate's
//! action key. Cargo builds fall back to `"unknown"` via `build.rs`.

mod stamp {
    include!(env!("BOSS_BUILD_INFO_RS"));
}

/// Stamped Boss release version: `STABLE_BOSS_BASE_VERSION` (e.g. `1.0.4`)
/// on a `--stamp` / `--config=release` build, `0.0.0-dev` when unstamped,
/// or `"unknown"` on a Cargo (non-Bazel) build.
pub fn version() -> &'static str {
    stamp::BOSS_VERSION
}

/// Canonical `--version` string: `<binary_name> <version>`
/// (e.g. `boss 1.0.4` on a stamped release, `boss 0.0.0-dev` otherwise).
pub fn version_string(binary_name: &str) -> String {
    format!("{binary_name} {}", version())
}

/// If the process was invoked with `--version` or `-V` as its first
/// argument, print `version` to stdout and return `true`; otherwise
/// return `false`.
///
/// Call this at the very top of `main()`, before argument parsing, and
/// return the caller's own success value when it returns `true`:
///
/// ```ignore
/// if boss_build_info::print_version_if_requested(&boss_build_info::version_string("boss")) {
///     return std::process::ExitCode::SUCCESS;
/// }
/// ```
///
/// The engine, whose `main` returns `anyhow::Result<()>`, returns
/// `Ok(())` instead.
pub fn print_version_if_requested(version: &str) -> bool {
    if is_version_flag(std::env::args().nth(1).as_deref()) {
        println!("{version}");
        true
    } else {
        false
    }
}

/// Whether `first_arg` (argv[1]) is a version-request flag. Split out
/// from [`print_version_if_requested`] so the argv-matching logic is
/// unit-testable without mutating the process's real arguments.
fn is_version_flag(first_arg: Option<&str>) -> bool {
    matches!(first_arg, Some("--version") | Some("-V"))
}

#[cfg(test)]
mod tests {
    use super::is_version_flag;
    use super::version;

    #[test]
    fn recognizes_version_flags() {
        assert!(is_version_flag(Some("--version")));
        assert!(is_version_flag(Some("-V")));
    }

    #[test]
    fn rejects_non_version_flags() {
        assert!(!is_version_flag(None));
        assert!(!is_version_flag(Some("")));
        assert!(!is_version_flag(Some("-v")));
        assert!(!is_version_flag(Some("version")));
        assert!(!is_version_flag(Some("--help")));
        assert!(!is_version_flag(Some("task")));
    }

    #[test]
    fn version_matches_expected_stamp() {
        // Under Bazel, each test target passes BOSS_EXPECTED_VERSION
        // ("0.0.0-dev" for unstamped build-info_test, "1.0.42" for
        // build-info-stamped_test). Under Cargo (unset), build.rs
        // provides "unknown".
        let expected = option_env!("BOSS_EXPECTED_VERSION").unwrap_or("unknown");
        assert_eq!(version(), expected);
    }
}
