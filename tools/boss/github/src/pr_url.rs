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

/// `owner/repo#n` from a canonical or decorated github.com pull-request URL.
///
/// Uses the same host/owner/repo/`pull` validation as [`repo_from_pr_url_lenient`]
/// and [`pr_number_from_url`], so a trailing `/files`, query, or fragment is
/// ignored. `None` when the URL is not a github.com pull request.
pub fn compact_pr_label(pr_url: &str) -> Option<String> {
    let repo = repo_from_pr_url_lenient(pr_url)?;
    let number = pr_number_from_url(pr_url)?;
    Some(format!("{repo}#{number}"))
}

/// Normalize a decorated GitHub PR URL to a case-folded identity.
pub fn canonical_pr_url(pr_url: &str) -> Option<String> {
    let slug = repo_from_pr_url_lenient(pr_url)?;
    let number = pr_number_from_url(pr_url)?;
    Some(format!("https://github.com/{}/pull/{number}", slug.to_lowercase()))
}

/// Compare PR identities, ignoring repository case and URL decorations.
pub fn same_pr(left: &str, right: &str) -> bool {
    left == right || canonical_pr_url(left).is_some_and(|url| Some(url) == canonical_pr_url(right))
}

/// Parse a positive PR number from `#N` or `N`, ignoring surrounding whitespace.
pub fn parse_pr_number_only(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if digits.is_empty() || digits == "0" || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Parse `owner/repo#N` into `("owner/repo", N)`.
pub fn parse_slug_pr(raw: &str) -> Option<(&str, u64)> {
    let (slug, rest) = raw.split_once('#')?;
    let digits = rest.trim();
    if digits.is_empty() || digits == "0" || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    let slug = slug.trim();
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((slug, number))
}

/// Canonicalize `owner/repo` + `number` into `https://github.com/owner/repo/pull/N`.
pub fn canonical_slug_pr(slug: &str, number: u64) -> Option<String> {
    if number == 0 {
        return None;
    }
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    canonical_pr_url(&format!("https://github.com/{owner}/{repo}/pull/{number}"))
}

/// Scan `text` for GitHub PR URLs and `owner/repo#N` references, appending
/// canonicalized URLs to `out`. Unqualified `#N` references are ignored.
pub fn scan_pr_refs(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("https://github.com/") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ',' | '"' | '\'' | ']'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ';', ':']);
        if let Some(canonical) = canonical_pr_url(url) {
            out.push(canonical);
        }
        rest = &candidate[end.max(1)..];
    }
    let mut rest = text;
    while let Some(hash) = rest.find('#') {
        if let Some((slug, number)) = slug_pr_at(rest, hash)
            && let Some(canonical) = canonical_slug_pr(slug, number)
        {
            out.push(canonical);
        }
        rest = &rest[hash + 1..];
    }
}

fn slug_pr_at(text: &str, hash: usize) -> Option<(&str, u64)> {
    let after = text.get(hash + 1..)?;
    let digits_end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    if digits_end == 0 {
        return None;
    }
    let digits = &after[..digits_end];
    if digits == "0" {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    let before = text.get(..hash)?;
    // One space between the slug and '#' ("owner/repo #12") is a common
    // written form; more whitespace still fails so "#12" on its own is not
    // pulled into an earlier word.
    let before = before.strip_suffix(' ').unwrap_or(before);
    let slug_start = before
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let slug = before.get(slug_start..)?.trim();
    if slug.contains('/') && !slug.starts_with('/') && !slug.ends_with('/') {
        Some((slug, number))
    } else {
        None
    }
}

/// Shape check for a PR reference: a GitHub PR URL, `owner/repo#N`, `#N`, or a
/// bare non-zero PR number, with no whitespace.
pub fn is_pr_reference(text: &str) -> bool {
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return false;
    }
    parse_pr_url_parts(text).is_some() || parse_slug_pr(text).is_some() || parse_pr_number_only(text).is_some()
}

/// Resolve a PR reference (`URL | owner/repo#N | #N | N`) to a canonical PR URL
/// in `repo_remote_url`, rejecting references from other repos.
pub fn resolve_pr_reference(reference: &str, repo_remote_url: &str) -> Result<String, String> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return Err("PR reference is empty".to_owned());
    }
    if let Some(url) = canonical_pr_url(trimmed) {
        validate_pr_url(&url, repo_remote_url)?;
        return Ok(url);
    }
    if let Some((slug, number)) = parse_slug_pr(trimmed) {
        let product = parse_product_slug(repo_remote_url)
            .ok_or_else(|| format!("could not parse product repo slug from `{repo_remote_url}`"))?;
        if slug.to_ascii_lowercase() != product {
            return Err(format!("PR repo `{slug}` does not match product repo `{product}`"));
        }
        return Ok(format!("https://github.com/{product}/pull/{number}"));
    }
    if let Some(number) = parse_pr_number_only(trimmed) {
        let product = parse_product_slug(repo_remote_url)
            .ok_or_else(|| format!("could not parse product repo slug from `{repo_remote_url}`"))?;
        return Ok(format!("https://github.com/{product}/pull/{number}"));
    }
    Err(format!(
        "`{trimmed}` is not a PR reference; expected a GitHub PR URL, owner/repo#N, #N, or PR number"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_identity_ignores_case_and_decoration() {
        let canonical = "https://github.com/acme/widget/pull/9";
        for url in [
            canonical,
            "https://github.com/Acme/Widget/pull/09/files?tab=diff#comment",
        ] {
            assert_eq!(canonical_pr_url(url).as_deref(), Some(canonical));
            assert!(same_pr(url, canonical));
        }
        assert!(!same_pr(canonical, "https://github.com/other/repo/pull/9"));
        assert!(!same_pr(canonical, "https://github.com/acme/widget/pull/10"));
        assert_eq!(canonical_pr_url("https://example.com/acme/widget/pull/9"), None);
    }

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

    #[test]
    fn compact_pr_label_formats_owner_repo_number() {
        assert_eq!(
            compact_pr_label("https://github.com/acme/widget/pull/9").as_deref(),
            Some("acme/widget#9"),
        );
        assert_eq!(
            compact_pr_label("https://github.com/acme/widget/pull/9/files").as_deref(),
            Some("acme/widget#9"),
        );
        assert_eq!(
            compact_pr_label("https://github.com/acme/widget/pull/9?tab=files").as_deref(),
            Some("acme/widget#9"),
        );
        assert_eq!(compact_pr_label("https://example.com/acme/widget/pull/9"), None);
        assert_eq!(compact_pr_label("not-a-url"), None);
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

    #[test]
    fn scan_pr_refs_preserves_repo_and_number_boundaries() {
        let mut refs = Vec::new();
        scan_pr_refs(
            "See https://github.com/spinyfin/mono/pull/29270, spinyfin/mono#29270, and other/repo#2927.",
            &mut refs,
        );
        assert_eq!(
            refs,
            vec![
                "https://github.com/spinyfin/mono/pull/29270",
                "https://github.com/spinyfin/mono/pull/29270",
                "https://github.com/other/repo/pull/2927",
            ]
        );
        assert!(
            !refs
                .iter()
                .any(|r| same_pr(r, "https://github.com/spinyfin/mono/pull/2927"))
        );
    }

    #[test]
    fn resolve_pr_reference_accepts_short_forms_and_rejects_foreign_repos() {
        let remote = "git@github.com:spinyfin/mono.git";
        assert_eq!(
            resolve_pr_reference("#2977", remote).as_deref(),
            Ok("https://github.com/spinyfin/mono/pull/2977")
        );
        assert_eq!(
            resolve_pr_reference("2977", remote).as_deref(),
            Ok("https://github.com/spinyfin/mono/pull/2977")
        );
        assert_eq!(
            resolve_pr_reference("spinyfin/mono#2977", remote).as_deref(),
            Ok("https://github.com/spinyfin/mono/pull/2977")
        );
        assert!(resolve_pr_reference("other/repo#2977", remote).is_err());
        assert!(resolve_pr_reference("https://github.com/other/repo/pull/2977", remote).is_err());
        assert!(resolve_pr_reference("0", remote).is_err());
        assert!(resolve_pr_reference("##2977", remote).is_err());
    }

    #[test]
    fn is_pr_reference_validates_all_supported_shapes() {
        assert!(is_pr_reference("https://github.com/spinyfin/mono/pull/1"));
        assert!(is_pr_reference("spinyfin/mono#1"));
        assert!(is_pr_reference("#1"));
        assert!(is_pr_reference("1"));
        assert!(!is_pr_reference(""));
        assert!(!is_pr_reference("0"));
        assert!(!is_pr_reference("#0"));
        assert!(!is_pr_reference("##1"));
        assert!(!is_pr_reference("spinyfin/mono #1"));
        assert!(!is_pr_reference("not a pr"));
    }
}
