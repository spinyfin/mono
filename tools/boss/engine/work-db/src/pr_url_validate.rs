//! Product-repo gate for worker-submitted PR URLs.
use git_utils::repo_slug::parse_github_owner_repo;

static PLACEHOLDER_SLUGS: &[&str] = &["foo/bar", "octocat/hello-world", "someuser/somerepo", "example/example"];

fn parse_product_slug(repo_remote_url: &str) -> Option<String> {
    let (owner, repo) = parse_github_owner_repo(repo_remote_url).ok()?;
    Some(format!("{}/{}", owner.to_lowercase(), repo.to_lowercase()))
}

/// Validate that `pr_url` belongs to the product identified by
/// `product_repo_remote_url`.
pub fn validate_pr_url(pr_url: &str, product_repo_remote_url: &str) -> Result<(), String> {
    let pr_slug = boss_github::pr_url::repo_from_pr_url_lenient(pr_url)
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
