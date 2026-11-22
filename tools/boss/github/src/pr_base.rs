//! Policy: managed work must land in the repository's default branch.

/// Checks that a PR targets the repository's default branch.
///
/// Returns `Ok(())` when `base` equals `default` (after trimming), and
/// `Err(reason)` otherwise, including when either name is missing: missing
/// metadata fails closed, because branch names cannot be inferred from
/// conventions.
pub fn require_default_branch(base: Option<&str>, default: Option<&str>) -> Result<(), String> {
    let base = base
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Cannot verify PR base: GitHub omitted the base branch")?;
    let default = default
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Cannot verify PR base: GitHub omitted the repository default branch")?;
    if base != default {
        return Err(format!(
            "PR targets `{base}`, but managed work must land in the repository default branch `{default}`. \
             Stacked changes are not supported. If this work needs another open PR, use the \
             `link-prerequisite-task --prerequisite-pr <PR URL>` proposal to link its owning work item and wait until it lands in `{default}`."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod pr_base_tests {
    use super::*;
    #[test]
    fn uses_actual_default_and_refuses_missing_metadata() {
        for default in ["main", "master", "trunk"] {
            assert!(require_default_branch(Some(default), Some(default)).is_ok());
            let error = require_default_branch(Some("feature/parent"), Some(default)).unwrap_err();
            assert!(error.contains("feature/parent"));
            assert!(error.contains(default));
            assert!(error.contains("link-prerequisite-task --prerequisite-pr"));
        }
        assert!(require_default_branch(Some(" trunk "), Some("trunk")).is_ok());
        for (base, default) in [
            (None, Some("main")),
            (Some("main"), None),
            (Some(""), Some("main")),
            (Some("  "), Some("main")),
            (Some("main"), Some(" ")),
        ] {
            assert!(require_default_branch(base, default).is_err());
        }
    }
}
