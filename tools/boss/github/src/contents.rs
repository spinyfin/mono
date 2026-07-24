//! GitHub Contents API helper: fetch a file's raw bytes at a specific ref.
//!
//! Uses `gh api` rather than a direct HTTP call so that credentials are
//! handled by the `gh` CLI installation (same pattern as the rest of Boss).

use crate::gh_runner::{gh_output, parse_http_status_from_stderr};

/// Fetch the raw content of `path` from `owner/repo` at `ref_name` using
/// `gh api`.
///
/// Returns `Ok(Some(content))` on success, `Ok(None)` when the file does not
/// exist at that ref (HTTP 404 — the common "no file at this branch" case),
/// and `Err` only on a real transport or tool failure.
///
/// `--method GET` is required so `-f ref=` lands in the query string (gh
/// otherwise switches to POST once a field is added), which also makes gh
/// URL-encode slashed branch / ref names like `boss/exec_*` correctly.
pub async fn fetch_repo_file(owner: &str, repo: &str, path: &str, ref_name: &str) -> anyhow::Result<Option<String>> {
    let endpoint = format!("repos/{owner}/{repo}/contents/{path}");
    let output = gh_output(&[
        "api",
        &endpoint,
        "--method",
        "GET",
        "-f",
        &format!("ref={ref_name}"),
        "-H",
        "Accept: application/vnd.github.raw",
    ])
    .await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_contents_response(output.status.success(), &output.stdout, &stderr)
        .map_err(|e| anyhow::anyhow!("`gh api {endpoint}` failed (exit {:?}): {}", output.status.code(), e))
}

/// Classify a `gh api` contents response into the three observable outcomes:
///
/// - `Ok(Some(body))` — the request succeeded; the decoded stdout is the file
///   content at that ref.
/// - `Ok(None)` — the file does not exist at that ref (HTTP 404). Detected via
///   the shared [`parse_http_status_from_stderr`] primitive (an `HTTP 404` in
///   gh's stderr), or gh's stderr containing the text `"Not Found"`.
/// - `Err(_)` — any other non-zero exit (transport failure, rate limit, auth
///   error, …). The error message is the trimmed stderr.
///
/// Kept as a pure helper (no I/O) so the classification branching can be
/// pinned by unit tests.
fn classify_contents_response(status_success: bool, stdout: &[u8], stderr: &str) -> anyhow::Result<Option<String>> {
    if status_success {
        return Ok(Some(String::from_utf8_lossy(stdout).into_owned()));
    }
    if parse_http_status_from_stderr(stderr) == Some(404) || stderr.contains("Not Found") {
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
