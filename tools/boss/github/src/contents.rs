//! GitHub Contents API helper: fetch a file's raw bytes at a specific ref.
//!
//! Uses `gh api` rather than a direct HTTP call so that credentials are
//! handled by the `gh` CLI installation (same pattern as the rest of Boss).
//!
//! Conditional revalidation (`If-None-Match` / `304 Not Modified`) also
//! goes through `gh api`: `gh` will send an arbitrary `-H` request
//! header, and a 304 is distinguishable as a non-zero exit whose
//! stderr carries `HTTP 304` (parsed by
//! [`parse_http_status_from_stderr`]). `gh` does **not** treat 304 as
//! success — it is a 3xx, so the process exits 1 — which is why the
//! classifier has to look at the status rather than `status.success()`.
//! A 304 does not consume REST rate-limit quota; when `--include` is
//! used the `X-RateLimit-Remaining` header is surfaced so callers can
//! observe that.

use crate::gh_runner::{gh_output, parse_http_status_from_stderr};

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

/// Like [`raw_content_args`], plus `--include` so response headers
/// (ETag, rate-limit remaining) are on stdout, and an optional
/// `If-None-Match` so GitHub can answer 304.
pub(crate) fn raw_content_args_conditional(
    owner: &str,
    repo: &str,
    path: &str,
    git_ref: &str,
    etag: Option<&str>,
) -> (String, Vec<String>) {
    let (endpoint, mut args) = raw_content_args(owner, repo, path, git_ref);
    // `gh api --include <endpoint> …` — insert the flag immediately after
    // `api` so it cannot be mistaken for an endpoint path.
    args.insert(1, "--include".to_owned());
    if let Some(etag) = etag.filter(|e| !e.is_empty()) {
        args.push("-H".to_owned());
        args.push(format!("If-None-Match: {etag}"));
    }
    (endpoint, args)
}

/// Outcome of a Contents-API read that captured response headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentsFetch {
    /// HTTP 200: the file's raw text, plus the ETag to store for the
    /// next conditional request. `rate_limit_remaining` is whatever
    /// `X-RateLimit-Remaining` GitHub sent, when present.
    Body {
        text: String,
        etag: Option<String>,
        rate_limit_remaining: Option<u32>,
    },
    /// HTTP 304: the caller's cached copy is still current. A 304 does
    /// not consume REST rate-limit quota; `rate_limit_remaining` is
    /// reported when `--include` captured the header.
    NotModified { rate_limit_remaining: Option<u32> },
    /// HTTP 404: no file at that path/ref.
    NotFound,
}

/// Fetch the raw content of `path` from `owner/repo` at `ref_name` using
/// `gh api`.
///
/// Returns `Ok(Some(content))` on success, `Ok(None)` when the file does not
/// exist at that ref (HTTP 404 — the common "no file at this branch" case),
/// and `Err` only on a real transport or tool failure.
///
/// This is the unconditional path (no `If-None-Match`, no `--include`)
/// used by the populator and attentions detector. Document viewing uses
/// [`fetch_repo_file_conditional`] so it can store an ETag and revalidate.
pub async fn fetch_repo_file(owner: &str, repo: &str, path: &str, ref_name: &str) -> anyhow::Result<Option<String>> {
    let (endpoint, args) = raw_content_args(owner, repo, path, ref_name);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = gh_output(&arg_refs).await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_contents_response(output.status.success(), &output.stdout, &stderr)
        .map_err(|e| anyhow::anyhow!("`gh api {endpoint}` failed (exit {:?}): {}", output.status.code(), e))
}

/// Contents-API GET with optional `If-None-Match`, returning headers
/// (ETag / 304 / rate-limit remaining) in addition to the body.
///
/// Authentication is still the `gh` CLI installation — the same
/// credential path as [`fetch_repo_file`]. A 304 is classified from
/// either `--include`'s status line or `gh`'s stderr (`HTTP 304`),
/// because `gh` exits non-zero on 3xx.
pub async fn fetch_repo_file_conditional(
    owner: &str,
    repo: &str,
    path: &str,
    ref_name: &str,
    etag: Option<&str>,
) -> anyhow::Result<ContentsFetch> {
    let (endpoint, args) = raw_content_args_conditional(owner, repo, path, ref_name, etag);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = gh_output(&arg_refs).await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_contents_with_headers(output.status.success(), &output.stdout, &stderr)
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

/// Classify a `gh api --include` contents response into body / 304 / 404.
///
/// Pure (no I/O) so every branch is pinned by unit tests. `gh` exits
/// non-zero on 304 and 404, so `status_success` alone cannot distinguish
/// them — the HTTP status is read from `--include`'s status line when
/// present, otherwise from stderr via [`parse_http_status_from_stderr`].
pub(crate) fn classify_contents_with_headers(
    status_success: bool,
    stdout: &[u8],
    stderr: &str,
) -> anyhow::Result<ContentsFetch> {
    let parsed = parse_included_response(stdout);
    let status = parsed
        .as_ref()
        .map(|p| p.status)
        .or_else(|| parse_http_status_from_stderr(stderr));

    if status == Some(304) {
        return Ok(ContentsFetch::NotModified {
            rate_limit_remaining: parsed.as_ref().and_then(|p| p.rate_limit_remaining),
        });
    }
    if status == Some(404) || (!status_success && stderr.contains("Not Found")) {
        return Ok(ContentsFetch::NotFound);
    }
    if let Some(parsed) = parsed
        && (status_success || parsed.status == 200)
    {
        return Ok(ContentsFetch::Body {
            text: parsed.body,
            etag: parsed.etag,
            rate_limit_remaining: parsed.rate_limit_remaining,
        });
    }
    if status_success {
        // `--include` was absent or unparseable: stdout *is* the body,
        // matching the pre-conditional fetch_repo_file shape.
        return Ok(ContentsFetch::Body {
            text: String::from_utf8_lossy(stdout).into_owned(),
            etag: None,
            rate_limit_remaining: None,
        });
    }
    anyhow::bail!("{}", stderr.trim())
}

/// Parsed `gh api --include` stdout: status line, selected headers, body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncludedResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub rate_limit_remaining: Option<u32>,
    pub body: String,
}

/// Split `gh api --include` stdout into headers + body and pull the
/// fields revalidation cares about.
///
/// Returns `None` when stdout does not start with an HTTP status line
/// (the `--include` flag was absent, or gh wrote a bare body).
pub(crate) fn parse_included_response(stdout: &[u8]) -> Option<IncludedResponse> {
    let (headers, body) = split_headers_and_body(stdout)?;
    let status = parse_status_line(headers)?;
    Some(IncludedResponse {
        status,
        etag: header_value(headers, "etag"),
        rate_limit_remaining: header_value(headers, "x-ratelimit-remaining").and_then(|v| v.parse().ok()),
        body: String::from_utf8_lossy(body).into_owned(),
    })
}

fn split_headers_and_body(stdout: &[u8]) -> Option<(&str, &[u8])> {
    // Prefer the HTTP-standard `\r\n\r\n` separator; fall back to `\n\n`
    // for gh builds that emit Unix newlines.
    let sep = stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| stdout.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)))?;
    let headers = std::str::from_utf8(&stdout[..sep.0]).ok()?;
    if !headers.starts_with("HTTP/") {
        return None;
    }
    Some((headers, &stdout[sep.0 + sep.1..]))
}

fn parse_status_line(headers: &str) -> Option<u16> {
    let first = headers.lines().next()?;
    let mut parts = first.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    for line in headers.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // Status line has no colon; skip it rather than aborting
            // the whole scan.
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return None;
            }
            return Some(trimmed.to_owned());
        }
    }
    None
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

    fn included_200() -> Vec<u8> {
        b"HTTP/2.0 200 OK\r\nETag: W/\"abc123\"\r\nX-RateLimit-Remaining: 4999\r\n\r\n# hello\n".to_vec()
    }

    fn included_304() -> Vec<u8> {
        b"HTTP/2.0 304 Not Modified\r\nETag: W/\"abc123\"\r\nX-RateLimit-Remaining: 4999\r\n\r\n".to_vec()
    }

    #[test]
    fn include_parser_extracts_etag_body_and_remaining() {
        let parsed = parse_included_response(&included_200()).expect("parse");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.etag.as_deref(), Some("W/\"abc123\""));
        assert_eq!(parsed.rate_limit_remaining, Some(4999));
        assert_eq!(parsed.body, "# hello\n");
    }

    #[test]
    fn include_parser_accepts_unix_newlines() {
        let stdout = b"HTTP/2 200 OK\nEtag: \"plain\"\n\nbody";
        let parsed = parse_included_response(stdout).expect("parse");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.etag.as_deref(), Some("\"plain\""));
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn include_parser_rejects_a_bare_body() {
        assert!(parse_included_response(b"# just markdown\n").is_none());
    }

    #[test]
    fn conditional_200_returns_body_and_etag() {
        let result = classify_contents_with_headers(true, &included_200(), "").unwrap();
        assert_eq!(
            result,
            ContentsFetch::Body {
                text: "# hello\n".into(),
                etag: Some("W/\"abc123\"".into()),
                rate_limit_remaining: Some(4999),
            }
        );
    }

    #[test]
    fn http_304_on_include_status_line_is_not_modified() {
        // gh exits non-zero on 304 (it is a 3xx), so status_success is false.
        let result = classify_contents_with_headers(false, &included_304(), "gh: Not Modified (HTTP 304)").unwrap();
        assert_eq!(
            result,
            ContentsFetch::NotModified {
                rate_limit_remaining: Some(4999),
            }
        );
    }

    #[test]
    fn http_304_from_stderr_alone_is_not_modified() {
        // Some gh versions print the status on stderr and leave stdout empty
        // even with --include.
        let result = classify_contents_with_headers(false, b"", "gh: Not Modified (HTTP 304)").unwrap();
        assert_eq!(
            result,
            ContentsFetch::NotModified {
                rate_limit_remaining: None
            }
        );
    }

    #[test]
    fn conditional_404_is_not_found() {
        let stdout = b"HTTP/2.0 404 Not Found\r\n\r\n{\"message\":\"Not Found\"}";
        let result = classify_contents_with_headers(false, stdout, "gh: Not Found (HTTP 404)").unwrap();
        assert_eq!(result, ContentsFetch::NotFound);
    }

    #[test]
    fn tls_handshake_timeout_is_an_error_not_a_404() {
        let err = classify_contents_with_headers(
            false,
            b"",
            "Get \"https://api.github.com/repos/o/r/contents/d.md?ref=main\": net/http: TLS handshake timeout",
        )
        .unwrap_err();
        assert!(err.to_string().contains("TLS handshake timeout"));
    }

    #[test]
    fn conditional_args_send_if_none_match() {
        let (_endpoint, args) = raw_content_args_conditional("o", "r", "d.md", "main", Some("W/\"abc\""));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-H" && w[1] == "If-None-Match: W/\"abc\"")
        );
        assert!(args.iter().any(|a| a == "--include"));
    }

    #[test]
    fn conditional_args_omit_if_none_match_when_etag_absent() {
        let (_endpoint, args) = raw_content_args_conditional("o", "r", "d.md", "main", None);
        assert!(!args.iter().any(|a| a.contains("If-None-Match")));
    }
}
