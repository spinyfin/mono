//! Static validation for scoping glob patterns declared via the `applies_to`
//! (include) and `exclude` framework keys.
//!
//! A glob pattern can be **structurally empty** — incapable of matching any
//! path in any changeset, in any repo — decidable from the pattern text
//! alone, with no repo access and no false positives. That is a distinct,
//! narrower claim than "matches nothing in this repo" (not statically
//! decidable; a `--all` warning at most, never an error) or "matches nothing
//! in this changeset" (the ordinary, silent, correct outcome of a diff run).
//! Only the structurally-empty case is validated here.
//!
//! Three shapes are decidable with zero false positives:
//! - a leading `./`: changeset paths never carry a `./` prefix, so a pattern
//!   anchored on one can never match.
//! - a trailing path separator: a changeset holds files only, never
//!   directories, so nothing can end in `/`.
//! - a `!` prefix: this glob dialect (`globset`) treats `!` as a literal
//!   character, not a gitignore-style negation, so a pattern written with
//!   negation in mind matches nothing rather than negating anything.
//!
//! A bare name with no path separator and no glob metacharacter (`src`) is
//! deliberately NOT covered here: it is indistinguishable at this level from
//! a legitimate top-level file pattern (`Makefile`), so treating it as
//! structurally empty would produce false positives on real configs.

/// Returns `Some(reason)` when `pattern` can never match any changeset path,
/// decidable from the pattern text alone. `is_exclude` only changes wording:
/// a `!`-prefixed include-side (`applies_to`) pattern is pointed at the
/// `exclude` key, since that's very likely what the author meant; a `!`-
/// prefixed `exclude` pattern itself has nowhere further to point.
pub fn structurally_empty_reason(pattern: &str, is_exclude: bool) -> Option<&'static str> {
    if pattern.starts_with('!') {
        return Some(if is_exclude {
            "starts with `!`; this glob dialect treats `!` as a literal character, not a \
             negation, so it can never match a real path to exclude"
        } else {
            "starts with `!`; this glob dialect treats `!` as a literal character, not a \
             negation, so it can never match a real path — use the `exclude` key to exclude \
             paths instead of negating an `applies_to` pattern"
        });
    }
    if pattern.starts_with("./") {
        return Some("has a leading `./`; changeset paths never carry a `./` prefix, so this can never match");
    }
    if pattern.ends_with('/') || pattern.ends_with('\\') {
        return Some(
            "has a trailing path separator; a changeset contains files only, never directories, \
             so this can never match",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_leading_dot_slash() {
        assert!(structurally_empty_reason("./src/*.rs", false).is_some());
    }

    #[test]
    fn flags_negation_prefix_include_points_at_exclude_key() {
        let reason = structurally_empty_reason("!src/**", false).expect("must be flagged");
        assert!(reason.contains("exclude"), "reason: {reason}");
    }

    #[test]
    fn flags_negation_prefix_exclude_does_not_point_at_exclude_key() {
        let reason = structurally_empty_reason("!src/**", true).expect("must be flagged");
        assert!(!reason.contains("`exclude` key"), "reason: {reason}");
    }

    #[test]
    fn flags_trailing_separator() {
        assert!(structurally_empty_reason("src/", false).is_some());
        assert!(structurally_empty_reason("src\\", false).is_some());
    }

    #[test]
    fn accepts_matchable_patterns() {
        assert!(structurally_empty_reason("src/*.rs", false).is_none());
        assert!(structurally_empty_reason("**/*.kt", false).is_none());
        assert!(structurally_empty_reason("Makefile", false).is_none());
        assert!(structurally_empty_reason("srcc/**", false).is_none());
        assert!(structurally_empty_reason("SRC/**", false).is_none());
    }
}
