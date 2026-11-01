//! Parsing for canonical GitHub PR URLs.
//!
//! Every PR URL the engine handles has the canonical shape
//! `https://github.com/<owner>/<repo>/pull/<N>`. Several modules used to
//! carry their own near-duplicate parsers — some naive (a bare
//! last-segment `parse()`), some robust (validating the `https://github.com/`
//! prefix and the `pull` path segment). This module is the single home for
//! that logic so every call site gets the same validation.
//!
//! Two grades of consumer share the same strict structural validation (host,
//! owner, repo, and the `pull` segment) but differ on how the trailing number
//! is read:
//!
//! - [`parse_pr_url_parts`] / [`repo_from_pr_url`] are strict end to end: the
//!   `pull/` tail must be *exactly* a number with nothing after it. Callers
//!   that validate user-supplied "give me the canonical PR URL" input rely on
//!   this (e.g. the CLI's `validate_github_pr_url`).
//! - [`pr_number_from_url`] is lenient about the tail only: GitHub decorates
//!   real PR URLs with a further path segment (`/pull/<N>/files`), a query
//!   (`?tab=…`), or a fragment (`#issuecomment-…`), and any of these is
//!   tolerated — the leading run of digits after `pull/` is returned and the
//!   decoration ignored. This is the single home for the lenient PR-number
//!   parser that `merge_poller::parse_pr_number` used to duplicate. A tail
//!   that does not *start* with digits (e.g. `pull/abc` or a bare `pull/`) is
//!   still rejected as non-numeric.

/// Validate the strict structural prefix `https://github.com/<owner>/<repo>/pull/`
/// and return `(owner, repo, tail)` where `tail` is everything after `pull/`
/// (still undecoded — it may carry a trailing `/segment`, `?query`, or
/// `#fragment`). Returns `None` for a wrong host, an empty owner or repo, or a
/// non-`pull` third segment.
fn split_pr_url(pr_url: &str) -> Option<(&str, &str, &str)> {
    let path = pr_url.strip_prefix("https://github.com/")?;
    let mut parts = path.splitn(4, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    let tail = parts.next()?;
    Some((owner, repo, tail))
}

/// Parse `(owner, repo, number)` from a canonical GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
///
/// Strict: returns `None` for any URL that doesn't match the canonical shape,
/// including one with a trailing path/query/fragment after the number. Use
/// [`pr_number_from_url`] when the number must be extracted from a decorated
/// URL.
pub fn parse_pr_url_parts(pr_url: &str) -> Option<(&str, &str, u64)> {
    let (owner, repo, tail) = split_pr_url(pr_url)?;
    let number: u64 = tail.parse().ok()?;
    Some((owner, repo, number))
}

/// Extract `"owner/repo"` from a canonical GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
pub fn repo_from_pr_url(pr_url: &str) -> Option<&str> {
    let (owner, repo, _) = parse_pr_url_parts(pr_url)?;
    let path = pr_url.strip_prefix("https://github.com/")?;
    let end = owner.len() + 1 + repo.len();
    Some(&path[..end])
}

/// Extract `"owner/repo"` from a GitHub PR URL, tolerating a decorated tail
/// the same way [`pr_number_from_url`] does: a further path segment
/// (`/files`), a query (`?tab=…`), or a fragment (`#issuecomment-…`) after
/// the PR number is ignored. The host, owner, repo, and `pull` segments are
/// still validated strictly. Use this over [`repo_from_pr_url`] when the URL
/// may come from prose or an artifact rather than a canonical API response.
pub fn repo_from_pr_url_lenient(pr_url: &str) -> Option<&str> {
    let (owner, repo, _) = split_pr_url(pr_url)?;
    let path = pr_url.strip_prefix("https://github.com/")?;
    let end = owner.len() + 1 + repo.len();
    Some(&path[..end])
}

/// Extract the PR number from a GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<N>`.
///
/// Lenient about the tail: a further path segment (`/files`), a query
/// (`?tab=…`), or a fragment (`#issuecomment-…`) after the number is ignored
/// and the leading PR number is returned. The host, owner, repo, and `pull`
/// segments are still validated strictly.
pub fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    let (_, _, tail) = split_pr_url(pr_url)?;
    tail.split(['/', '?', '#']).next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_url_parts_extracts_all_fields() {
        assert_eq!(
            parse_pr_url_parts("https://github.com/spinyfin/mono/pull/568"),
            Some(("spinyfin", "mono", 568)),
        );
        assert_eq!(
            parse_pr_url_parts("https://github.com/owner/my-repo/pull/1"),
            Some(("owner", "my-repo", 1)),
        );
    }

    #[test]
    fn parse_pr_url_parts_rejects_non_canonical() {
        // Wrong host.
        assert_eq!(parse_pr_url_parts("https://example.com/owner/repo/pull/1"), None);
        // Not a URL at all.
        assert_eq!(parse_pr_url_parts("not-a-url"), None);
        // Missing the `pull` segment.
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/issues/1"), None);
        // Trailing segment isn't a number.
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/pull/abc"), None);
        // Empty owner.
        assert_eq!(parse_pr_url_parts("https://github.com//repo/pull/1"), None);
        // Bare domain with nothing after it.
        assert_eq!(parse_pr_url_parts("https://github.com/"), None);
        // Strict: a decorated tail (trailing path / query / fragment) is NOT
        // accepted here — that leniency lives only in `pr_number_from_url`.
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/pull/1/files"), None);
        assert_eq!(parse_pr_url_parts("https://github.com/owner/repo/pull/1?tab=x"), None);
    }

    // The following cases were migrated from `merge_poller::parse_pr_number`'s
    // unit tests when that lenient parser was consolidated onto this helper.
    // They pin the tolerated URL decorations (trailing path, query, fragment)
    // and the strict rejections.

    #[test]
    fn pr_number_from_url_strips_query_and_fragment() {
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/123?foo=bar"), Some(123));
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/123#issuecomment-1"),
            Some(123)
        );
        // Query and fragment together.
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/123?foo=bar#frag"),
            Some(123)
        );
    }

    #[test]
    fn pr_number_from_url_stops_at_trailing_path() {
        // A further path segment (`/files`, `/commits`, …) after the number is
        // ignored; a bare trailing slash likewise leaves the number intact.
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/123/files"), Some(123));
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/123/"), Some(123));
    }

    #[test]
    fn pr_number_from_url_rejects_missing_pull_segment() {
        assert_eq!(pr_number_from_url("https://github.com/o/r/issues/123"), None);
        assert_eq!(pr_number_from_url("https://github.com/o/r"), None);
    }

    #[test]
    fn pr_number_from_url_rejects_non_numeric_tail() {
        // No leading digits after `pull/` → nothing parses.
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/abc"), None);
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/"), None);
    }

    #[test]
    fn pr_number_from_url_rejects_empty_or_garbage() {
        assert_eq!(pr_number_from_url(""), None);
        assert_eq!(pr_number_from_url("not a url at all"), None);
    }

    #[test]
    fn repo_from_pr_url_extracts_owner_repo() {
        assert_eq!(
            repo_from_pr_url("https://github.com/spinyfin/mono/pull/568"),
            Some("spinyfin/mono"),
        );
        assert_eq!(
            repo_from_pr_url("https://github.com/owner/my-repo/pull/1"),
            Some("owner/my-repo"),
        );
        assert_eq!(repo_from_pr_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(repo_from_pr_url("not-a-url"), None);
    }

    #[test]
    fn repo_from_pr_url_lenient_tolerates_decorated_tail() {
        assert_eq!(
            repo_from_pr_url_lenient("https://github.com/spinyfin/mono/pull/568/files"),
            Some("spinyfin/mono"),
        );
        assert_eq!(
            repo_from_pr_url_lenient("https://github.com/spinyfin/mono/pull/568?tab=diff"),
            Some("spinyfin/mono"),
        );
        assert_eq!(
            repo_from_pr_url_lenient("https://github.com/spinyfin/mono/pull/568#issuecomment-1"),
            Some("spinyfin/mono"),
        );
    }

    #[test]
    fn repo_from_pr_url_lenient_matches_strict_on_canonical_urls() {
        assert_eq!(
            repo_from_pr_url_lenient("https://github.com/spinyfin/mono/pull/568"),
            Some("spinyfin/mono"),
        );
        assert_eq!(repo_from_pr_url_lenient("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(repo_from_pr_url_lenient("not-a-url"), None);
    }

    #[test]
    fn pr_number_from_url_extracts_number() {
        assert_eq!(
            pr_number_from_url("https://github.com/spinyfin/mono/pull/568"),
            Some(568),
        );
        assert_eq!(pr_number_from_url("https://github.com/owner/my-repo/pull/1"), Some(1),);
        assert_eq!(pr_number_from_url("https://example.com/owner/repo/pull/1"), None);
        assert_eq!(pr_number_from_url("not-a-url"), None);
    }
}

/// Well-known placeholder owner/repo slugs used in tests and documentation
/// (compared case-insensitively). These are rejected as a belt-and-suspenders
/// check even before the product-repo gate runs.
static PLACEHOLDER_SLUGS: &[&str] = &["foo/bar", "octocat/hello-world", "someuser/somerepo", "example/example"];

/// Parse `product_repo_remote_url` (SSH `git@github.com:owner/repo.git` or
/// HTTPS `https://github.com/owner/repo`) into a lowercase `owner/repo` slug.
/// Returns `None` if the URL is not a recognisable github.com remote.
pub fn parse_product_slug(repo_remote_url: &str) -> Option<String> {
    let (owner, repo) = git_utils::repo_slug::parse_github_owner_repo(repo_remote_url).ok()?;
    Some(format!("{}/{}", owner.to_lowercase(), repo.to_lowercase()))
}

/// Validate that `pr_url` belongs to the product identified by
/// `product_repo_remote_url`. Returns `Ok(())` when the URL is a
/// legitimate product PR, or `Err(reason)` explaining why it was
/// rejected.
///
/// Two gates run in order:
/// 1. **Placeholder reject** — slugs from `PLACEHOLDER_SLUGS` are
///    dropped immediately with an informative reason. These are test
///    fixtures that should never appear in real worker output.
/// 2. **Repo-remote-url gate** — the URL's `owner/repo` must
///    case-insensitively match the parsed slug of
///    `product_repo_remote_url`. A worker operating on the product's
///    cube workspace can only legitimately emit a PR URL for that repo.
pub fn validate_pr_url(pr_url: &str, product_repo_remote_url: &str) -> Result<(), String> {
    let pr_slug = repo_from_pr_url_lenient(pr_url)
        .map(|slug| slug.to_lowercase())
        .ok_or_else(|| format!("URL does not contain a recognisable owner/repo slug: {pr_url}"))?;

    if PLACEHOLDER_SLUGS.iter().any(|p| pr_slug == *p) {
        return Err(format!("owner/repo `{pr_slug}` is a well-known test placeholder"));
    }

    let product_slug = parse_product_slug(product_repo_remote_url)
        .ok_or_else(|| format!("could not parse product repo slug from `{product_repo_remote_url}`"))?;

    if pr_slug != product_slug {
        return Err(format!(
            "URL repo `{pr_slug}` does not match product repo `{product_slug}`"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod product_gate_tests {
    use super::*;

    #[test]
    fn rejects_every_placeholder_before_the_product_gate() {
        for slug in PLACEHOLDER_SLUGS {
            let url = format!("https://github.com/{slug}/pull/42");
            let remote = format!("git@github.com:{slug}.git");
            assert!(validate_pr_url(&url, &remote).unwrap_err().contains("placeholder"));
        }
    }

    #[test]
    fn matches_product_case_insensitively_and_rejects_other_repos() {
        assert_eq!(
            validate_pr_url(
                "https://github.com/SpinYFin/Mono/pull/99",
                "git@github.com:spinyfin/mono.git"
            ),
            Ok(())
        );
        assert!(
            validate_pr_url(
                "https://github.com/another/repo/pull/1",
                "git@github.com:spinyfin/mono.git"
            )
            .unwrap_err()
            .contains("does not match")
        );
        assert!(
            validate_pr_url("not-a-url", "git@github.com:spinyfin/mono.git")
                .unwrap_err()
                .contains("recognisable")
        );
        assert!(
            validate_pr_url("https://github.com/spinyfin/mono/pull/1", "not-a-remote")
                .unwrap_err()
                .contains("could not parse")
        );
    }

    #[test]
    fn parses_github_remotes() {
        for remote in [
            "git@github.com:SpinYFin/Mono.git",
            "https://github.com/spinyfin/mono.git",
            "https://github.com/spinyfin/mono",
        ] {
            assert_eq!(parse_product_slug(remote).as_deref(), Some("spinyfin/mono"));
        }
        assert_eq!(parse_product_slug("https://gitlab.com/foo/bar"), None);
    }

    #[test]
    fn validate_rejects_url_for_wrong_repo() {
        // A worker running tests that mention another GitHub repo's PR URL.
        let result = validate_pr_url(
            "https://github.com/some-org/other-repo/pull/10",
            "git@github.com:spinyfin/mono.git",
        );
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(reason.contains("does not match"), "got: {reason}");
    }

    #[test]
    fn parse_product_slug_handles_ssh_and_https() {
        assert_eq!(
            parse_product_slug("git@github.com:spinyfin/mono.git"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(
            parse_product_slug("https://github.com/spinyfin/mono.git"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(
            parse_product_slug("https://github.com/spinyfin/mono"),
            Some("spinyfin/mono".to_owned()),
        );
        assert_eq!(parse_product_slug("https://gitlab.com/foo/bar"), None);
    }

    #[test]
    fn validate_is_case_insensitive_for_slug_matching() {
        // GitHub names are case-insensitive; SpinYFin/Mono must match
        // spinyfin/mono from the product's repo_remote_url.
        let result = validate_pr_url(
            "https://github.com/SpinYFin/Mono/pull/99",
            "git@github.com:spinyfin/mono.git",
        );
        assert_eq!(result, Ok(()));
    }
}
