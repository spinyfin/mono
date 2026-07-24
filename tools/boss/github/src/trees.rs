//! GitHub Git-Trees API helper: enumerate a repo's file tree at a ref.
//!
//! Uses `gh api` (same credential path as [`crate::contents`]) so the
//! `gh` CLI installation owns authentication — no second token surface.
//!
//! Three primitives, deliberately kept separate so a caller that already
//! knows the repo's default branch or HEAD sha can skip the calls it does
//! not need:
//!
//! - [`fetch_default_branch`] — one call, `repos/{owner}/{repo}`.
//! - [`fetch_head_sha`] — one *cheap* call. The
//!   `application/vnd.github.sha` media type makes the commits endpoint
//!   return the bare 40-char commit sha instead of the full commit
//!   object, which is what makes cache validation affordable enough to
//!   run on every list.
//! - [`fetch_tree`] — one call with `recursive=1`, the whole tree in a
//!   single round trip (the Contents API would need one call per
//!   directory).
//!
//! Failures are classified into [`TreeApiErrorKind`] rather than
//! collapsed into a single opaque error, because the four cases a UI
//! must distinguish (missing auth, rate limit, missing repo, offline)
//! need four different remedies.

use crate::gh_runner::gh_output;

/// One blob in a repo's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeBlob {
    /// Repo-relative path, e.g. `docs/design-docs/foo.md`.
    pub path: String,
    /// Blob size in bytes as GitHub reports it. `None` when the field
    /// is absent (GitHub omits it for some entry types).
    pub size: Option<u64>,
}

/// A repo's tree at one commit, already filtered to the blobs the
/// caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTree {
    /// The commit sha the tree was read at.
    pub sha: String,
    pub blobs: Vec<TreeBlob>,
    /// GitHub's own `truncated` flag: the repo has more entries than a
    /// single recursive response can carry (>100k entries or >7 MB).
    /// Surfaced rather than silently ignored so the UI can say the
    /// listing is partial instead of quietly showing a subset.
    pub truncated: bool,
}

/// Why a GitHub tree/blob read failed, in the terms a UI needs to pick
/// a remedy. Anything that is not recognisably one of the first three
/// is [`Self::Unreachable`] — the catch-all for offline, DNS failure, a
/// missing `gh` binary, or a 5xx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeApiErrorKind {
    /// 403/429 with rate-limit wording. Retrying immediately will fail
    /// the same way; the remedy is to wait or authenticate.
    RateLimited,
    /// 401/403 without rate-limit wording: the token is missing,
    /// expired, or lacks access to a private repo.
    NotAuthorized,
    /// 404: the repo (or ref) does not exist, or the token cannot see
    /// it. GitHub deliberately returns 404 rather than 403 for private
    /// repos an unauthorized token asks about, so this and
    /// `NotAuthorized` are genuinely ambiguous — the message says so.
    NotFound,
    /// Everything else: network failure, `gh` not installed, 5xx.
    Unreachable,
}

/// A classified failure from a `gh api` call against the trees/blobs
/// endpoints. Carries `gh`'s own message so the UI can show what
/// actually went wrong under the category headline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeApiError {
    pub kind: TreeApiErrorKind,
    pub message: String,
}

impl std::fmt::Display for TreeApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TreeApiError {}

type TreeResult<T> = std::result::Result<T, TreeApiError>;

/// Classify a failed `gh api` invocation from its stderr.
///
/// Order matters: the rate-limit check runs before the generic
/// 401/403 check because a rate-limit response *is* an HTTP 403, and
/// telling the user to re-authenticate when they are simply throttled
/// sends them down the wrong path entirely.
///
/// Pure (no I/O) so every branch is pinned by unit tests.
fn classify_failure(stderr: &str) -> TreeApiError {
    let message = stderr.trim().to_owned();
    let lower = message.to_lowercase();
    let kind = if lower.contains("rate limit") || lower.contains("secondary rate") || lower.contains("http 429") {
        TreeApiErrorKind::RateLimited
    } else if lower.contains("http 401") || lower.contains("http 403") || lower.contains("bad credentials") {
        TreeApiErrorKind::NotAuthorized
    } else if lower.contains("http 404") || lower.contains("not found") {
        TreeApiErrorKind::NotFound
    } else {
        TreeApiErrorKind::Unreachable
    };
    TreeApiError {
        kind,
        // An empty stderr would render as a blank error in the UI. Fall
        // back to the category name so there is always something to show.
        message: if message.is_empty() {
            "GitHub request failed with no diagnostic output".to_owned()
        } else {
            message
        },
    }
}

/// Run `gh <args>` and return stdout, classifying any failure.
///
/// A spawn failure (no `gh` on PATH) is [`TreeApiErrorKind::Unreachable`]
/// — from the operator's point of view it is the same "can't reach
/// GitHub from here" condition as being offline.
async fn gh_api(args: &[&str]) -> TreeResult<Vec<u8>> {
    let output = gh_output(args).await.map_err(|e| TreeApiError {
        kind: TreeApiErrorKind::Unreachable,
        message: format!("could not run `gh`: {e}"),
    })?;
    if !output.status.success() {
        return Err(classify_failure(&String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

/// The repo's default branch name (`main`, `master`, …).
pub async fn fetch_default_branch(owner: &str, repo: &str) -> TreeResult<String> {
    let endpoint = format!("repos/{owner}/{repo}");
    let stdout = gh_api(&["api", &endpoint, "--jq", ".default_branch"]).await?;
    let branch = String::from_utf8_lossy(&stdout).trim().to_owned();
    if branch.is_empty() {
        return Err(TreeApiError {
            kind: TreeApiErrorKind::Unreachable,
            message: format!("`gh api {endpoint}` returned no default_branch"),
        });
    }
    Ok(branch)
}

/// The commit sha at the tip of `git_ref`.
///
/// `Accept: application/vnd.github.sha` makes GitHub return the bare
/// sha as the response body rather than the full commit object — a few
/// dozen bytes instead of a few KB. That is what makes it cheap enough
/// to call on every listing as a cache validator.
pub async fn fetch_head_sha(owner: &str, repo: &str, git_ref: &str) -> TreeResult<String> {
    let endpoint = format!("repos/{owner}/{repo}/commits/{git_ref}");
    let stdout = gh_api(&["api", &endpoint, "-H", "Accept: application/vnd.github.sha"]).await?;
    let sha = String::from_utf8_lossy(&stdout).trim().to_owned();
    if sha.is_empty() {
        return Err(TreeApiError {
            kind: TreeApiErrorKind::Unreachable,
            message: format!("`gh api {endpoint}` returned an empty sha"),
        });
    }
    Ok(sha)
}

/// The full recursive tree at `sha`, keeping only blobs whose path
/// satisfies `keep_path`.
///
/// Filtering happens here rather than in the caller so the (large)
/// unfiltered entry list never escapes this function.
pub async fn fetch_tree<F>(owner: &str, repo: &str, sha: &str, keep_path: F) -> TreeResult<RepoTree>
where
    F: Fn(&str) -> bool,
{
    let endpoint = format!("repos/{owner}/{repo}/git/trees/{sha}?recursive=1");
    let stdout = gh_api(&["api", &endpoint]).await?;
    let body: serde_json::Value = serde_json::from_slice(&stdout).map_err(|e| TreeApiError {
        kind: TreeApiErrorKind::Unreachable,
        message: format!("could not parse the tree response from `gh api {endpoint}`: {e}"),
    })?;
    Ok(parse_tree(sha, &body, keep_path))
}

/// Map a `git/trees` response body into a [`RepoTree`], keeping only
/// `blob` entries whose path passes `keep_path`.
///
/// Non-blob entries (`tree`, `commit` — the latter is a submodule
/// pointer) are dropped: the caller reconstructs directory structure
/// from the surviving blob paths, and a submodule is not a file we can
/// fetch. Kept as a pure `Value -> RepoTree` function so the filtering
/// is testable without a network call.
fn parse_tree<F>(sha: &str, body: &serde_json::Value, keep_path: F) -> RepoTree
where
    F: Fn(&str) -> bool,
{
    let blobs = body["tree"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry["type"].as_str() == Some("blob"))
                .filter_map(|entry| {
                    let path = entry["path"].as_str()?;
                    if !keep_path(path) {
                        return None;
                    }
                    Some(TreeBlob {
                        path: path.to_owned(),
                        size: entry["size"].as_u64(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    RepoTree {
        sha: sha.to_owned(),
        blobs,
        truncated: body["truncated"].as_bool().unwrap_or(false),
    }
}

/// Fetch one blob's raw text at `git_ref`.
///
/// Routed through the Contents API with the `raw` media type rather
/// than the blobs API so GitHub does the base64 decoding; the response
/// body is the file's bytes verbatim. Argv construction is shared with
/// `contents::fetch_repo_file` via [`crate::contents::raw_content_args`]
/// so there is exactly one place that knows the Contents-API invocation
/// shape; this function keeps its own `TreeApiError` classification.
pub async fn fetch_blob_text(owner: &str, repo: &str, path: &str, git_ref: &str) -> TreeResult<String> {
    let (_endpoint, args) = crate::contents::raw_content_args(owner, repo, path, git_ref);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = gh_api(&arg_refs).await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Whether `path` names a markdown file, by extension. Case-insensitive
/// so `README.MD` is included.
pub fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_classifies_ahead_of_the_403_it_arrives_as() {
        // A rate-limit response *is* an HTTP 403. Classifying it as
        // NotAuthorized would tell the user to re-authenticate when
        // waiting is the actual remedy — so the ordering in
        // `classify_failure` is load-bearing, not incidental.
        let err = classify_failure("gh: API rate limit exceeded (HTTP 403)");
        assert_eq!(err.kind, TreeApiErrorKind::RateLimited);
    }

    #[test]
    fn secondary_rate_limit_classifies_as_rate_limited() {
        let err = classify_failure("You have exceeded a secondary rate limit (HTTP 403)");
        assert_eq!(err.kind, TreeApiErrorKind::RateLimited);
    }

    #[test]
    fn http_429_classifies_as_rate_limited() {
        let err = classify_failure("gh: Too Many Requests (HTTP 429)");
        assert_eq!(err.kind, TreeApiErrorKind::RateLimited);
    }

    #[test]
    fn plain_403_classifies_as_not_authorized() {
        let err = classify_failure("gh: Resource not accessible by integration (HTTP 403)");
        assert_eq!(err.kind, TreeApiErrorKind::NotAuthorized);
    }

    #[test]
    fn bad_credentials_classifies_as_not_authorized() {
        let err = classify_failure("gh: Bad credentials (HTTP 401)");
        assert_eq!(err.kind, TreeApiErrorKind::NotAuthorized);
    }

    #[test]
    fn not_found_classifies_as_not_found() {
        let err = classify_failure("gh: Not Found (HTTP 404)");
        assert_eq!(err.kind, TreeApiErrorKind::NotFound);
    }

    #[test]
    fn network_failure_classifies_as_unreachable() {
        let err = classify_failure("error connecting to api.github.com: dial tcp: lookup failed");
        assert_eq!(err.kind, TreeApiErrorKind::Unreachable);
        assert!(err.message.contains("dial tcp"), "gh's own message is preserved");
    }

    #[test]
    fn empty_stderr_still_yields_a_showable_message() {
        // A blank message would render as an empty error row in the UI.
        let err = classify_failure("   \n ");
        assert_eq!(err.kind, TreeApiErrorKind::Unreachable);
        assert!(!err.message.is_empty());
    }

    #[test]
    fn markdown_extensions_match_case_insensitively() {
        assert!(is_markdown_path("docs/design-docs/foo.md"));
        assert!(is_markdown_path("README.MD"));
        assert!(is_markdown_path("notes.markdown"));

        assert!(!is_markdown_path("src/main.rs"));
        assert!(!is_markdown_path("docs/mdbook.toml"));
        // A directory *named* `.md` has no extension of its own; only
        // trailing-extension matches count.
        assert!(!is_markdown_path("md/readme.txt"));
    }

    fn sample_tree_body() -> serde_json::Value {
        serde_json::json!({
            "sha": "deadbeef",
            "truncated": false,
            "tree": [
                { "path": "docs", "type": "tree" },
                { "path": "docs/design.md", "type": "blob", "size": 1234 },
                { "path": "README.md", "type": "blob", "size": 42 },
                { "path": "src/main.rs", "type": "blob", "size": 99 },
                { "path": "vendor/lib", "type": "commit" },
                { "path": "notes.markdown", "type": "blob" },
            ]
        })
    }

    #[test]
    fn parse_tree_keeps_only_matching_blobs() {
        let tree = parse_tree("abc123", &sample_tree_body(), is_markdown_path);
        assert_eq!(tree.sha, "abc123");
        assert!(!tree.truncated);
        assert_eq!(
            tree.blobs,
            vec![
                TreeBlob {
                    path: "docs/design.md".to_owned(),
                    size: Some(1234)
                },
                TreeBlob {
                    path: "README.md".to_owned(),
                    size: Some(42)
                },
                // Size absent in the response => None, entry still kept.
                TreeBlob {
                    path: "notes.markdown".to_owned(),
                    size: None
                },
            ]
        );
    }

    #[test]
    fn parse_tree_drops_tree_and_submodule_entries() {
        // `docs` (a tree) and `vendor/lib` (a submodule pointer) must not
        // appear even under a filter that would accept any path — the
        // caller reconstructs directories from blob paths, and a
        // submodule is not a fetchable file.
        let tree = parse_tree("abc123", &sample_tree_body(), |_| true);
        let paths: Vec<&str> = tree.blobs.iter().map(|b| b.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["docs/design.md", "README.md", "src/main.rs", "notes.markdown"]
        );
    }

    #[test]
    fn parse_tree_surfaces_githubs_truncated_flag() {
        let body = serde_json::json!({ "truncated": true, "tree": [] });
        assert!(parse_tree("abc123", &body, is_markdown_path).truncated);
    }

    #[test]
    fn parse_tree_tolerates_a_missing_tree_array() {
        // Degrade to an empty listing rather than panicking on a
        // response shape we didn't expect.
        let body = serde_json::json!({});
        let tree = parse_tree("abc123", &body, is_markdown_path);
        assert!(tree.blobs.is_empty());
        assert!(!tree.truncated);
    }
}
