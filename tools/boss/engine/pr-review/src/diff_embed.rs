//! Shared bounded review-diff embedding, with an opt-in lockfile filter.

/// Return the complete diff only when it fits the configured line cap.
/// Filtering happens before counting; a zero cap disables embedding.
pub fn bounded_review_diff(diff: &str, max_lines: u64, exclude_lockfiles: bool) -> Option<String> {
    if max_lines == 0 {
        return None;
    }
    let filtered;
    let diff = if exclude_lockfiles {
        filtered = diff
            .split_inclusive("\n")
            .fold((String::new(), false), |(mut output, mut skip), line| {
                if let Some(header) = line.strip_prefix("diff --git ") {
                    // Git quotes unusual paths; ordinary paths may contain spaces.
                    // Match the destination basename, not whitespace-delimited fragments.
                    skip = header
                        .rsplit_once(" \"b/")
                        .or_else(|| header.rsplit_once(" b/"))
                        .is_some_and(|(_, path)| is_lockfile(path.trim_end_matches(['\r', '\n'])));
                }
                if !skip {
                    output.push_str(line);
                }
                (output, skip)
            })
            .0;
        filtered.as_str()
    } else {
        diff
    };
    ((!exclude_lockfiles || !diff.is_empty()) && diff.lines().count() as u64 <= max_lines).then(|| diff.to_owned())
}

fn is_lockfile(path: &str) -> bool {
    matches!(
        path.trim_matches('"').rsplit('/').next().unwrap_or(path),
        "Cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "poetry.lock"
            | "uv.lock"
            | "Pipfile.lock"
            | "Gemfile.lock"
            | "composer.lock"
            | "go.sum"
            | "Podfile.lock"
            | "Package.resolved"
            | "MODULE.bazel.lock"
            | "flake.lock"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_is_inclusive_and_zero_disables_embedding() {
        let diff = "diff --git a/main.rs b/main.rs\n+new\n";
        assert_eq!(bounded_review_diff(diff, 2, false).as_deref(), Some(diff));
        assert!(bounded_review_diff(diff, 1, false).is_none());
        assert!(bounded_review_diff(diff, 0, false).is_none());
    }

    #[test]
    fn lockfiles_are_removed_before_counting_only_when_requested() {
        let code = "diff --git a/src/main.rs b/src/main.rs\n+new\n";
        let lock = "diff --git a/nested/Cargo.lock b/nested/Cargo.lock\n-old\n+new\n";
        let diff = format!("{lock}{code}{lock}");
        assert_eq!(bounded_review_diff(&diff, 2, true).as_deref(), Some(code));
        assert!(bounded_review_diff(&diff, 1, true).is_none());
        assert!(bounded_review_diff(&diff, 2, false).is_none());
        assert_eq!(bounded_review_diff(&diff, 8, false).as_deref(), Some(diff.as_str()));
        assert!(bounded_review_diff(lock, 100, true).is_none());
        assert!(bounded_review_diff(&diff, 0, true).is_none());
    }

    #[test]
    fn spaces_and_renames_do_not_hide_source_files() {
        for header in [
            "diff --git a/Cargo.lock snapshots/source.rs b/Cargo.lock snapshots/source.rs",
            "diff --git a/Cargo.lock b/source.rs",
        ] {
            let diff = format!("{header}\n+source\n");
            assert_eq!(bounded_review_diff(&diff, 10, true).as_deref(), Some(diff.as_str()));
        }
        let quoted = "diff --git \"a/lock snapshots/Cargo.lock\" \"b/lock snapshots/Cargo.lock\"\n+lock\n";
        assert!(bounded_review_diff(quoted, 10, true).is_none());
        let renamed = "diff --git a/old-name b/Cargo.lock\n+lock\n";
        assert!(bounded_review_diff(renamed, 10, true).is_none());
    }

    #[test]
    fn lockfile_basenames_do_not_filter_source_files_or_similar_names() {
        for name in [
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "Package.resolved",
            "MODULE.bazel.lock",
        ] {
            let diff = format!("diff --git a/sub/{name} b/sub/{name}\n+locked\n");
            assert!(bounded_review_diff(&diff, 10, true).is_none(), "{name}");
        }
        for name in ["Cargo.lock.rs", "lockfile.rs", "Cargo.lock/main.rs"] {
            let diff = format!("diff --git a/{name} b/{name}\n+source\n");
            assert_eq!(bounded_review_diff(&diff, 10, true).as_deref(), Some(diff.as_str()));
        }
    }
}
