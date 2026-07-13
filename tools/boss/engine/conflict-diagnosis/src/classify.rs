//! Per-event conflict classification (Layer 0 telemetry,
//! `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`
//! T1). Coarse, repo-agnostic pattern matching over conflicted file
//! paths — the engine core stays ignorant of any particular repo's
//! tooling; this is a heuristic for telemetry grouping, not the
//! rung-0 deterministic-resolver framework (a separate, later task).

use std::collections::BTreeSet;

/// Classify a conflict event from the set of conflicted file paths.
/// `"unknown"` when `paths` is empty, `"mixed"` when the paths span
/// more than one class, otherwise the single class every path shares.
pub fn classify_conflict_class(paths: &[String]) -> &'static str {
    if paths.is_empty() {
        return "unknown";
    }
    let classes: BTreeSet<&'static str> = paths.iter().map(|p| classify_path(p)).collect();
    if classes.len() == 1 {
        classes.into_iter().next().unwrap_or("unknown")
    } else {
        "mixed"
    }
}

fn classify_path(path: &str) -> &'static str {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if file_name.ends_with(".lock") {
        "lockfile"
    } else if file_name == "BUILD.bazel" || file_name == "BUILD" || file_name.ends_with(".bzl") {
        "build_file"
    } else if file_name == "mod.rs" || file_name == "lib.rs" || file_name == "__init__.py" {
        "registry"
    } else if path.contains("/migrations") || file_name.contains("migration") {
        "migration"
    } else if path.contains("/tests/") || file_name.contains("_test.") || file_name.ends_with("_tests.rs") {
        "test"
    } else {
        "semantic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paths_are_unknown() {
        assert_eq!(classify_conflict_class(&[]), "unknown");
    }

    #[test]
    fn single_lockfile_is_lockfile() {
        let paths = vec!["Cargo.lock".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "lockfile");
    }

    #[test]
    fn nested_lockfile_is_lockfile() {
        let paths = vec!["MODULE.bazel.lock".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "lockfile");
    }

    #[test]
    fn build_bazel_is_build_file() {
        let paths = vec!["tools/boss/cli/BUILD.bazel".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "build_file");
    }

    #[test]
    fn bzl_file_is_build_file() {
        let paths = vec!["defs.bzl".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "build_file");
    }

    #[test]
    fn mod_rs_is_registry() {
        let paths = vec!["tools/boss/engine/core/src/mod.rs".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "registry");
    }

    #[test]
    fn migrations_path_is_migration() {
        let paths = vec!["tools/boss/engine/core/src/work/migrations_b.rs".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "migration");
    }

    #[test]
    fn tests_dir_is_test() {
        let paths = vec!["tools/boss/engine/core/tests/control_verbs.rs".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "test");
    }

    #[test]
    fn arbitrary_source_is_semantic() {
        let paths = vec!["tools/boss/engine/core/src/completion.rs".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "semantic");
    }

    #[test]
    fn mixed_classes_report_mixed() {
        let paths = vec!["Cargo.lock".to_owned(), "src/completion.rs".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "mixed");
    }

    #[test]
    fn same_class_multiple_files_is_not_mixed() {
        let paths = vec!["Cargo.lock".to_owned(), "MODULE.bazel.lock".to_owned()];
        assert_eq!(classify_conflict_class(&paths), "lockfile");
    }
}
