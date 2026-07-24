//! Canonical GitHub PR-URL scanning, shared by every channel that recovers a
//! PR URL from free text: the `PostToolUse` Bash-response capture, and the
//! driver's final-message fallback producer for
//! [`StructuredOutputKind::PrUrl`](crate::StructuredOutputKind::PrUrl).
//!
//! Scanning only *finds* a syntactically canonical URL. Deciding whether it
//! belongs to this execution's product and branch is engine policy and stays
//! in the engine (`pr_url_capture::validate_pr_url` and the staged-URL branch
//! verification).

use std::sync::LazyLock;

use regex::Regex;

/// Canonical PR URL pattern: `https://github.com/<owner>/<repo>/pull/<N>`.
/// Owner / repo accept `[A-Za-z0-9._-]+` (GitHub's actual character set).
///
/// Trailing path components (`/files`, `/commits`, `#issuecomment-…`, query
/// strings) are *not* matched into the canonical form — the regex stops at the
/// digit run, so `https://github.com/owner/repo/pull/123/files` yields
/// `https://github.com/owner/repo/pull/123`.
static PR_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://github\.com/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/pull/\d+").expect("PR URL regex compiles")
});

/// First canonical PR URL in `text`, or `None`.
///
/// Used where the earliest URL is the authoritative one — `gh pr create`
/// prints the URL it just made before any subsequent chatter.
pub fn find_first_pr_url(text: &str) -> Option<String> {
    PR_URL_RE.find(text).map(|m| m.as_str().to_owned())
}

/// Last canonical PR URL in `text`, or `None`.
///
/// Used for final-message recovery, where the convention is "print the PR URL
/// on its own line as the last thing in your final response": a message that
/// quotes an earlier/unrelated PR in its prose still ends with the real one.
pub fn find_last_pr_url(text: &str) -> Option<String> {
    PR_URL_RE.find_iter(text).last().map(|m| m.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_canonical_url_and_stops_at_the_number() {
        assert_eq!(
            find_first_pr_url("see https://github.com/owner/repo/pull/123/files for detail"),
            Some("https://github.com/owner/repo/pull/123".to_owned()),
        );
    }

    #[test]
    fn first_and_last_differ_when_several_urls_appear() {
        let text = "context: https://github.com/o/r/pull/1\n\nhttps://github.com/o/r/pull/42";
        assert_eq!(
            find_first_pr_url(text),
            Some("https://github.com/o/r/pull/1".to_owned())
        );
        assert_eq!(
            find_last_pr_url(text),
            Some("https://github.com/o/r/pull/42".to_owned())
        );
    }

    #[test]
    fn non_pr_urls_and_prose_are_ignored() {
        assert_eq!(find_last_pr_url("PR #458 is ready; see the repo"), None);
        assert_eq!(find_last_pr_url("https://github.com/owner/repo/issues/12"), None);
    }
}
