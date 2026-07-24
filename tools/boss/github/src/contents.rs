//! GitHub Contents API helper: fetch a file's raw bytes at a specific ref.
//!
//! Uses `gh api` rather than a direct HTTP call so that credentials are
//! handled by the `gh` CLI installation (same pattern as the rest of Boss).

use crate::gh_runner::gh_output;

/// Build the endpoint and full `gh api` argv for a raw-content GET against
/// the Contents API: `repos/{owner}/{repo}/contents/{path}` with `ref=` in
/// the query string and the raw media type.
///
/// Shared by [`fetch_repo_file`] and [`crate::trees::fetch_blob_text`] so
/// the endpoint shape and argv (in particular `--method GET`, required so
/// `-f ref=` lands in the query string instead of gh switching to POST once
/// a field is added — which also makes gh URL-encode slashed branch/ref
/// names like `boss/exec_*` correctly) live in exactly one place; each
/// caller applies its own error classification to the result.
pub(crate) fn raw_content_args(owner: &str, repo: &str, path: &str, git_ref: &str) -> (String, Vec<String>) {
    let endpoint = format!("repos/{owner}/{repo}/contents/{path}");
    let args = vec![
        "api".to_owned(),
        endpoint.clone(),
        "--method".to_owned(),
        "GET".to_owned(),
        "-f".to_owned(),
        format!("ref={git_ref}"),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw".to_owned(),
    ];
    (endpoint, args)
}

/// Fetch the raw content of `path` from `owner/repo` at `ref_name` using
/// `gh api`.
///
/// Returns `Ok(Some(content))` on success, `Ok(None)` when the file does not
/// exist at that ref (HTTP 404 — the common "no file at this branch" case),
/// and `Err` only on a real transport or tool failure.
pub async fn fetch_repo_file(owner: &str, repo: &str, path: &str, ref_name: &str) -> anyhow::Result<Option<String>> {
    let (endpoint, args) = raw_content_args(owner, repo, path, ref_name);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = gh_output(&arg_refs).await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_contents_response(output.status.success(), &output.stdout, &stderr)
        .map_err(|e| anyhow::anyhow!("`gh api {endpoint}` failed (exit {:?}): {}", output.status.code(), e))
}

/// Classify a `gh api` contents response into the three observable outcomes:
///
/// - `Ok(Some(body))` — the request succeeded; the decoded stdout is the file
///   content at that ref.
/// - `Ok(None)` — the file does not exist at that ref (HTTP 404). Detected via
///   gh's stderr containing `"Not Found"` or `"404"`.
/// - `Err(_)` — any other non-zero exit (transport failure, rate limit, auth
///   error, …). The error message is the trimmed stderr.
///
/// Kept as a pure helper (no I/O) so the classification branching can be
/// pinned by unit tests.
fn classify_contents_response(status_success: bool, stdout: &[u8], stderr: &str) -> anyhow::Result<Option<String>> {
    if status_success {
        return Ok(Some(String::from_utf8_lossy(stdout).into_owned()));
    }
    if stderr.contains("Not Found") || stderr.contains("404") {
        return Ok(None);
    }
    anyhow::bail!("{}", stderr.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_returns_decoded_body() {
        let body = b"fn main() {}\n";
        let result = classify_contents_response(true, body, "").unwrap();
        assert_eq!(result, Some("fn main() {}\n".to_string()));
    }

    #[test]
    fn not_found_stderr_returns_none() {
        let stderr = "gh: Not Found (HTTP 404)";
        let result = classify_contents_response(false, b"", stderr).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn status_404_stderr_returns_none() {
        // Some gh error shapes surface the numeric code without "Not Found".
        let stderr = "HTTP 404: the resource could not be located";
        let result = classify_contents_response(false, b"", stderr).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn unrelated_failure_returns_err() {
        let stderr = "error connecting to api.github.com: dial tcp: lookup failed";
        let err = classify_contents_response(false, b"", stderr).unwrap_err();
        assert!(err.to_string().contains("dial tcp"));
    }

    #[test]
    fn rate_limit_failure_returns_err() {
        let stderr = "gh: API rate limit exceeded (HTTP 403)";
        assert!(classify_contents_response(false, b"", stderr).is_err());
    }
}
