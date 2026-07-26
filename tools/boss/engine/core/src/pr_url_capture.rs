//! Primary-path PR URL capture from worker progress events.
//!
//! Background: the engine receives the URL of every PR a worker
//! opens in real time, embedded in the tool-output surface of the
//! worker's `gh pr create` / `cube pr create` call. For Claude that
//! surface is `tool_response.stdout` on a `PostToolUse` hook event;
//! for a stdout-JSONL driver (Codex) it is
//! `command_execution.aggregated_output` on the same normalised
//! `WorkerEvent::PostToolUse`. Historically the engine ignored the
//! stream and reconstructed the URL later by shelling out to `jj log`
//! against the worker's cube workspace and querying the GitHub API
//! for each candidate commit sha. That reconstruction path is fragile
//! (it failed once when the worker did `jj new main` after pushing; it
//! failed again when a date-format mismatch broke the bookmark-tip
//! revset expansion) and unnecessary — the URL is literally already
//! in the event stream.
//!
//! The driver owns the *shape* of the tool observation
//! ([`boss_engine_driver::AgentDriver::pr_url_capture_feed`]); this
//! module owns the *algorithm* and the staging cache:
//!
//! - [`extract_pr_url_from_text`] / [`extract_pr_url_from_bash_response`]
//!   — pure regex scans (the shared
//!   [`boss_engine_structured_output::pr_url::find_first_pr_url`]) over
//!   free text or a Claude-shaped `tool_response` object. There is
//!   exactly one extraction algorithm; drivers only change what feeds
//!   it.
//! - [`is_gh_pr_command`] / [`is_gh_pr_command_str`] — Layer-1 gate so
//!   arbitrary Bash/shell output that happens to mention a PR URL does
//!   not stage the wrong PR.
//! - [`StagedPrUrlCache`] — a thread-safe `HashMap<execution_id,
//!   pr_url>` that callers populate from progress events and the
//!   `on_stop` handler reads on Stop. First-writer-wins semantics
//!   so a worker that re-runs `gh pr view` after `gh pr create`
//!   can't overwrite the legitimate first URL.
//!
//! The reconciliation path (`completion::detect_pr` →
//! `jj_candidate_commit_shas` → GitHub commits/{sha}/pulls) is
//! preserved as the engine-restart recovery fallback. If the engine
//! restarts after a worker pushed but before Stop fired, the staged
//! URL is lost from this cache (it lives in memory only) and the
//! fallback path runs on the next sweep. The staging cache is the
//! hot path; the reconstruction path is the cold path.
//!
//! **Not** a GitHub branch→PR poll: that is a different mechanism with
//! different failure modes and must not mask a broken extraction path.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use boss_engine_gh_invocation::{GhNoun, classify};
use boss_engine_structured_output::pr_url::find_first_pr_url;

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

/// Scan free text for the first canonical GitHub PR URL.
///
/// Thin wrapper over the shared
/// [`boss_engine_structured_output::pr_url::find_first_pr_url`] so every
/// primary-path caller (Claude hook feed, Codex stream feed, tests)
/// goes through one name in this module. Drivers must not reimplement
/// this — they supply the text via
/// [`boss_engine_driver::AgentDriver::pr_url_capture_feed`].
pub fn extract_pr_url_from_text(text: &str) -> Option<String> {
    find_first_pr_url(text)
}

/// Scan a Claude-shaped Bash `tool_response` JSON value for a GitHub PR URL.
///
/// Reads the `stdout` and `stderr` fields (both are strings in the
/// claude-code Bash tool response shape) and returns the first
/// canonical pull URL it finds, or `None` if neither field carries
/// one. `stdout` is checked first because `gh pr create` and
/// `gh pr view` both print the URL there; `stderr` is the fallback
/// for shell configurations / wrapper scripts that redirect.
///
/// The live capture path no longer calls this directly — it asks the
/// driver for a [`boss_engine_driver::PrUrlCaptureFeed`] and runs
/// [`extract_pr_url_from_text`] on the feed's text. This helper remains
/// as the Claude-shape unit-test surface and as documentation of the
/// historical object layout.
///
/// The regex is anchored to `https://github.com/` — heuristic
/// strings the worker might emit ("see the PR at …", "PR #458 is
/// ready") that don't carry the full URL are ignored. We want a
/// captured URL we can write verbatim to `tasks.pr_url`, not a
/// pattern that might bind us to the wrong repo.
pub fn extract_pr_url_from_bash_response(tool_response: &serde_json::Value) -> Option<String> {
    let scan = |field: &str| -> Option<String> {
        let text = tool_response.get(field)?.as_str()?;
        extract_pr_url_from_text(text)
    };
    scan("stdout").or_else(|| scan("stderr"))
}

/// In-memory `execution_id` set tracking revision workers that ran a push
/// command (`cube pr update`, or a legacy direct `jj git push`) since the
/// last Stop event. Populated by the `PostToolUse` hook dispatcher; consumed
/// (and cleared) by `WorkerCompletionHandler::on_stop_inner`'s SHA-delta
/// gate.
///
/// A revision worker that pushed is far more likely to be the source of a
/// SHA delta than the concurrently-active parent worker. `take` returns
/// `true` and clears the flag so the evidence is consumed at the Stop
/// boundary that acted on it.
#[derive(Debug, Default)]
pub struct StagedRevisionPushCache {
    inner: Mutex<HashSet<String>>,
}

impl StagedRevisionPushCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the revision execution `execution_id` ran a push command.
    /// Idempotent — calling twice for the same id is a no-op.
    pub fn record(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("StagedRevisionPushCache mutex poisoned")
            .insert(execution_id.to_owned());
    }

    /// Check whether a push was staged for `execution_id` and clear it.
    /// Returns `true` if a push was recorded; `false` otherwise.
    pub fn take(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("StagedRevisionPushCache mutex poisoned")
            .remove(execution_id)
    }
}

/// Check whether a shell command string is a push invocation that would
/// advance the parent PR's branch on the remote. Used to populate the
/// [`StagedRevisionPushCache`] for revision workers.
///
/// Returns `true` for:
/// - `cube pr update …` (and the deprecated `cube pr ensure` alias) — the
///   sanctioned way every worker pushes to an existing PR today. The
///   worker-facing contract (`worker_setup.rs`) forbids bare `jj git push`
///   via a `PreToolUse` hook, so this is the command a compliant revision
///   worker's Bash tool call actually shows; the push itself happens inside
///   the `cube` subprocess and is invisible to the hook stream.
/// - `jj git push …` (excluding `--dry-run`) — kept for defence-in-depth /
///   older worker prompts that still push directly. Plain `git push` is
///   intentionally excluded — the worker fleet uses `jj` exclusively.
pub fn is_revision_push_command_str(command: &str) -> bool {
    if command.contains("cube pr update") || command.contains("cube pr ensure") {
        return true;
    }
    command.contains("jj git push") && !command.contains("--dry-run")
}

/// Claude-shaped wrapper: read `tool_input.command` and delegate to
/// [`is_revision_push_command_str`]. Prefer the string form when the
/// command already came from a [`boss_engine_driver::PrUrlCaptureFeed`].
pub fn is_revision_push_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    is_revision_push_command_str(command)
}

/// If `command` is a single shell `-c` / `-lc` wrapper whose remainder is
/// one quoted script argument, return the script payload. Otherwise `None`.
///
/// Codex-shaped normalisers commonly emit `/bin/zsh -lc '…'` (or
/// `bash -c "…"`). The shared [`classify`] matcher strips quoted string
/// contents before matching so a `gh pr create` phrase inside a
/// commit-message argument does not false-positive — but that same strip
/// empties a payload that lives entirely inside the `-lc` argument. Peeling
/// the envelope first lets Layer-1 gates classify the inner command.
///
/// Matched shapes (whole command, optional leading whitespace):
/// - `/bin/zsh -lc 'gh pr create …'`
/// - `/usr/bin/bash -c "cube pr update …"`
/// - `zsh -lc '…'` / `bash -c '…'` / `sh -c '…'`
///
/// Flags must contain `c` (`-c`, `-lc`, `-cl`, …). Anything after the
/// closing quote (other than trailing whitespace) rejects the peel so we
/// do not mis-handle compound commands.
fn peel_shell_c_payload(command: &str) -> Option<&str> {
    let s = command.trim();
    // Optional absolute path prefix for the shell binary.
    let after_path = s
        .strip_prefix("/usr/bin/")
        .or_else(|| s.strip_prefix("/bin/"))
        .unwrap_or(s);
    // Shell name as a whole token (try longer names before `sh` so
    // `shadow` / `bashful` do not false-match).
    let mut after_shell = None;
    for name in ["zsh", "bash", "sh"] {
        if let Some(rest) = after_path.strip_prefix(name)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            after_shell = Some(rest);
            break;
        }
    }
    let after_shell = after_shell?.trim_start();
    // Flag group starting with `-` that includes the `-c` option letter.
    if !after_shell.starts_with('-') {
        return None;
    }
    let flags_end = after_shell[1..].find(|c: char| c.is_whitespace()).map(|i| i + 1)?;
    let flags = &after_shell[..flags_end];
    // `flags` is like `-lc` / `-c` / `-cl`; require a `c` option letter.
    if !flags.as_bytes().get(1..).is_some_and(|b| b.contains(&b'c')) {
        return None;
    }
    let after_flags = after_shell[flags_end..].trim_start();
    if after_flags.len() < 2 {
        return None;
    }
    let quote = after_flags.as_bytes()[0];
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let inner = &after_flags[1..];
    let close = if quote == b'\'' {
        // POSIX single quotes: no escapes; first `'` closes.
        inner.find('\'')?
    } else {
        // Double quotes: honour `\"` so a title with quotes still peels.
        let mut chars = inner.char_indices();
        loop {
            let (i, ch) = chars.next()?;
            if ch == '\\' {
                chars.next(); // skip escaped char
            } else if ch == '"' {
                break i;
            }
        }
    };
    let after_close = inner[close + 1..].trim();
    if !after_close.is_empty() {
        return None;
    }
    Some(&inner[..close])
}

/// Check whether a shell command string is a deliberate `gh pr` /
/// `cube pr` invocation (create, view, list, or edit).
///
/// Returns `true` only when the command is a `gh pr <subcommand>`
/// invocation whose subcommand can legitimately surface a PR URL for
/// the worker's own PR, or a `cube pr create|update|ensure` wrapper.
/// Handles environment-variable prefixes such as
/// `GIT_DIR=.jj/repo/store/git gh pr create ...` via the shared
/// [`classify`] matcher, and Codex-style shell wrappers
/// (`/bin/zsh -lc 'gh pr …'`) via [`peel_shell_c_payload`].
///
/// Use this as the Layer-1 gate in the progress-event capture path:
/// arbitrary shell commands whose output happens to contain a PR URL
/// (file reads, test runs, chore descriptions echoed via shell) must
/// not stage a wrong PR against the running execution.
pub fn is_gh_pr_command_str(command: &str) -> bool {
    // `cube pr create` / `cube pr update` (and the deprecated `cube pr
    // ensure` alias) are the jj-aware wrappers that output a PR URL as their
    // only stdout line — treat them the same as `gh pr create` for capture
    // purposes. They are not `gh` invocations, so the shared classifier
    // doesn't see them; check them directly. `.contains` also covers the
    // Codex `/bin/zsh -lc 'cube pr …'` envelope without peeling.
    if command.contains("cube pr create") || command.contains("cube pr update") || command.contains("cube pr ensure") {
        return true;
    }
    // Peel shell `-c`/`-lc` wrappers before classify: quote-stripping inside
    // classify would otherwise empty a bare `gh pr …` that lives entirely
    // inside the `-lc` argument (see `peel_shell_c_payload`).
    let command = peel_shell_c_payload(command).unwrap_or(command);
    matches!(
        classify(command),
        Some(inv)
            if inv.noun == GhNoun::Pr
                && matches!(inv.subcommand.as_str(), "create" | "view" | "list" | "edit")
    )
}

/// Claude-shaped wrapper: read `tool_input.command` and delegate to
/// [`is_gh_pr_command_str`]. Prefer the string form when the command
/// already came from a [`boss_engine_driver::PrUrlCaptureFeed`].
pub fn is_gh_pr_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    is_gh_pr_command_str(command)
}

/// Outcome of [`StagedPrUrlCache::record_if_unset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagePrUrlOutcome {
    /// The URL was new for this execution and is now staged.
    Staged,
    /// An earlier event already staged a URL for this execution; the
    /// new value was ignored (first-writer-wins).
    AlreadyStaged,
}

/// In-memory `execution_id → pr_url` staging cache. Populated by the
/// `PostToolUse` hook dispatcher when a Bash event surfaces a PR
/// URL; consumed by `WorkerCompletionHandler::on_stop` on the
/// matching Stop hook.
///
/// First-writer-wins. A worker that pushes, opens a PR (URL latched),
/// then later runs `gh pr view <other-PR>` while editing — the later
/// `view` doesn't clobber the legitimate first `create`.
#[derive(Debug, Default)]
pub struct StagedPrUrlCache {
    inner: Mutex<HashMap<String, String>>,
}

impl StagedPrUrlCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage `pr_url` against `execution_id` if no URL is currently
    /// staged. Returns whether the staging happened or was skipped.
    pub fn record_if_unset(&self, execution_id: &str, pr_url: &str) -> StagePrUrlOutcome {
        let mut guard = self.inner.lock().expect("StagedPrUrlCache mutex poisoned");
        if guard.contains_key(execution_id) {
            StagePrUrlOutcome::AlreadyStaged
        } else {
            guard.insert(execution_id.to_owned(), pr_url.to_owned());
            StagePrUrlOutcome::Staged
        }
    }

    /// Read the staged URL for `execution_id`, if any. Does not
    /// remove the entry — callers that want to clear should call
    /// [`Self::forget`].
    pub fn get(&self, execution_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned")
            .get(execution_id)
            .cloned()
    }

    /// Drop any staged URL for `execution_id`. Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_from_gh_pr_create_stdout() {
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_from_text_is_the_same_regex_as_bash_response() {
        // One algorithm: free-text feed (Codex aggregated_output) and the
        // Claude tool_response helper must agree on the URL they find.
        let text = "https://github.com/spinyfin/mono/pull/458\n";
        assert_eq!(
            extract_pr_url_from_text(text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
        assert_eq!(
            extract_pr_url_from_text(text),
            extract_pr_url_from_bash_response(&json!({ "stdout": text, "stderr": "" })),
        );
    }

    #[test]
    fn driver_feed_plus_shared_regex_captures_codex_aggregated_output() {
        // End-to-end of the non-Claude primary path without a second
        // extraction algorithm: driver feed → extract_pr_url_from_text
        // → is_gh_pr_command_str. No GitHub poll.
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!("/bin/zsh -lc 'cube pr create --branch boss/exec_x --title t'"),
            &json!("Opening https://github.com/spinyfin/mono/pull/99\n"),
        )
        .expect("codex-shaped feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert_eq!(url, "https://github.com/spinyfin/mono/pull/99");
        assert!(is_gh_pr_command_str(&feed.command));
    }

    #[test]
    fn driver_feed_plus_shared_regex_captures_codex_zsh_lc_bare_gh_pr() {
        // Codex shell-wraps bare `gh pr …` the same way as `cube pr …`.
        // Without peeling the `-lc` payload, classify's quote strip empties
        // the argument and Layer-1 rejects a real URL as not_a_gh_pr_command.
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!("/bin/zsh -lc 'gh pr create --title t --body b'"),
            &json!("https://github.com/spinyfin/mono/pull/101\n"),
        )
        .expect("codex-shaped gh feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert_eq!(url, "https://github.com/spinyfin/mono/pull/101");
        assert!(
            is_gh_pr_command_str(&feed.command),
            "zsh -lc-wrapped bare gh pr must pass Layer-1 after peel"
        );
    }

    #[test]
    fn driver_feed_plus_shared_regex_matches_claude_bash_helper() {
        let input = json!({ "command": "gh pr create --title t --body b" });
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
        });
        let feed = crate::driver::default_pr_url_capture_feed("Bash", &input, &response).expect("claude feed");
        assert_eq!(
            extract_pr_url_from_text(&feed.output_text),
            extract_pr_url_from_bash_response(&response),
        );
        assert_eq!(is_gh_pr_command_str(&feed.command), is_gh_pr_command(&input));
    }

    #[test]
    fn extract_returns_canonical_form_stripping_trailing_path() {
        // `gh pr view --json url` sometimes emits the URL inside a
        // JSON blob; the URL itself is canonical. Other surfaces
        // (issue comments, PR pages) may include `/files`,
        // `/commits`, `#issuecomment-…`. The regex stops at the
        // PR number so we never bind to a sub-path.
        let response = json!({
            "stdout": "{\"url\":\"https://github.com/spinyfin/mono/pull/458/files#diff-abc\"}",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_falls_back_to_stderr_when_stdout_absent() {
        let response = json!({
            "stdout": "",
            "stderr": "Created pull request: https://github.com/spinyfin/mono/pull/458\n",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_prefers_stdout_over_stderr() {
        // If both surfaces carry a URL — e.g. the worker piped
        // `gh pr create` output through a wrapper that also logged
        // a previously-cached URL to stderr — stdout wins because
        // it's the canonical output of the just-run command.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "https://github.com/spinyfin/mono/pull/100",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn extract_returns_first_match_when_stdout_has_multiple() {
        // A worker that runs `gh pr view 100 && gh pr create` in a
        // single Bash call could surface two URLs. The first is the
        // one we want — chronologically, it's the one printed by
        // the earlier command, but more importantly any later URL
        // in stdout is most often the just-created one's URL
        // followed by a CI status line containing a different
        // checks URL. We don't try to disambiguate; we take the
        // first match deterministically and document that workers
        // should keep `gh pr create` in its own Bash call.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/100\nhttps://github.com/spinyfin/mono/pull/458\n",
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/100"),
        );
    }

    #[test]
    fn extract_returns_none_when_no_url() {
        let response = json!({
            "stdout": "Hello world\n",
            "stderr": "",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_returns_none_when_response_is_not_an_object() {
        let response = json!("just a string");
        assert_eq!(extract_pr_url_from_bash_response(&response), None);

        let response = json!(null);
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_ignores_non_github_pull_urls() {
        // A worker mentioning a pull URL on a different host (e.g.
        // gitlab, gitea) is not a GitHub PR. The engine's binding
        // path is keyed on GitHub repo slugs; non-github URLs must
        // not latch.
        let response = json!({
            "stdout": "https://gitlab.com/x/y/-/merge_requests/123\n",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_ignores_issue_urls() {
        // GitHub issue URLs use `/issues/<N>`, not `/pull/<N>`.
        // The regex is anchored on `/pull/` so they don't match.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/issues/300\n",
        });
        assert_eq!(extract_pr_url_from_bash_response(&response), None);
    }

    #[test]
    fn extract_pulls_url_from_real_gh_pr_create_output() {
        // Reproduces a real PostToolUse `tool_response` shape as
        // observed in `/tmp/boss-engine.log` for Riker's
        // `exec_18af43101ae56430_6` (2026-05-13 23:20:31Z). The
        // body field exists alongside the URL — `gh pr create`
        // only ever prints the URL to stdout, but the surrounding
        // JSON is richer.
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/458",
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": false,
        });
        assert_eq!(
            extract_pr_url_from_bash_response(&response).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    // ── StagedPrUrlCache ──────────────────────────────────────────

    #[test]
    fn cache_records_first_url_for_an_execution() {
        let cache = StagedPrUrlCache::new();
        let outcome = cache.record_if_unset("exec_abc", "https://github.com/spinyfin/mono/pull/458");
        assert_eq!(outcome, StagePrUrlOutcome::Staged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn cache_ignores_subsequent_records_for_same_execution() {
        // The worker that pushed and ran `gh pr create` (URL latched)
        // and later ran `gh pr view <some-other-PR>` in a follow-up
        // Bash call (e.g. inspecting a referenced PR) must not have
        // the staged URL clobbered. First-writer-wins.
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset("exec_abc", "https://github.com/spinyfin/mono/pull/458");
        let outcome = cache.record_if_unset("exec_abc", "https://github.com/spinyfin/mono/pull/999");
        assert_eq!(outcome, StagePrUrlOutcome::AlreadyStaged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/458"),
        );
    }

    #[test]
    fn cache_isolates_executions() {
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset("exec_a", "https://github.com/spinyfin/mono/pull/1");
        cache.record_if_unset("exec_b", "https://github.com/spinyfin/mono/pull/2");
        assert_eq!(
            cache.get("exec_a").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/1"),
        );
        assert_eq!(
            cache.get("exec_b").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/2"),
        );
    }

    #[test]
    fn cache_forget_drops_entry_and_allows_re_record() {
        let cache = StagedPrUrlCache::new();
        cache.record_if_unset("exec_abc", "https://github.com/spinyfin/mono/pull/458");
        cache.forget("exec_abc");
        assert_eq!(cache.get("exec_abc"), None);
        // A fresh record after forget should succeed — useful if
        // the same execution_id gets reused (it shouldn't in prod,
        // but the semantics are: forget clears state).
        let outcome = cache.record_if_unset("exec_abc", "https://github.com/spinyfin/mono/pull/999");
        assert_eq!(outcome, StagePrUrlOutcome::Staged);
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/999"),
        );
    }

    #[test]
    fn cache_forget_is_idempotent() {
        let cache = StagedPrUrlCache::new();
        cache.forget("never-staged");
        cache.forget("never-staged");
        assert_eq!(cache.get("never-staged"), None);
    }

    // ── is_gh_pr_command ──────────────────────────────────────────

    #[test]
    fn gh_pr_create_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({
            "command": "gh pr create --head boss/exec_abc --base main --title 'fix: something'"
        })));
    }

    #[test]
    fn gh_pr_create_with_git_dir_prefix_is_a_gh_pr_command() {
        // Workers use GIT_DIR=.jj/repo/store/git because jj-backed
        // workspaces lack a top-level .git directory.
        assert!(is_gh_pr_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr create --head boss/exec_abc --base main"
        })));
    }

    #[test]
    fn gh_pr_view_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr view"
        })));
    }

    #[test]
    fn gh_pr_list_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({ "command": "gh pr list --state open" })));
    }

    #[test]
    fn gh_pr_edit_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({ "command": "gh pr edit 42 --add-label foo" })));
    }

    #[test]
    fn cube_pr_create_is_a_gh_pr_command() {
        // `cube pr create` outputs a PR URL as its only stdout line and
        // must be captured the same way as `gh pr create`.
        assert!(is_gh_pr_command(&json!({
            "command": "cube pr create --branch boss/exec_abc123_01 --title 'my feature'"
        })));
    }

    #[test]
    fn cube_pr_update_is_a_gh_pr_command() {
        // `cube pr update` also prints the PR URL as its only stdout line.
        assert!(is_gh_pr_command(&json!({
            "command": "cube pr update --branch boss/exec_abc123_01"
        })));
    }

    #[test]
    fn deprecated_cube_pr_ensure_is_a_gh_pr_command() {
        assert!(is_gh_pr_command(&json!({
            "command": "cube pr ensure --branch boss/exec_abc123_01 --title 'my feature'"
        })));
    }

    #[test]
    fn non_gh_command_is_not_a_gh_pr_command() {
        // Bash command that outputs PR URLs (e.g. reading a chore
        // description that mentions a prior PR) must not trigger capture.
        assert!(!is_gh_pr_command(&json!({
            "command": "bossctl task show task_123"
        })));
    }

    #[test]
    fn cat_command_with_pr_url_content_is_not_a_gh_pr_command() {
        assert!(!is_gh_pr_command(&json!({ "command": "cat chore.md" })));
    }

    #[test]
    fn grep_command_is_not_a_gh_pr_command() {
        assert!(!is_gh_pr_command(&json!({
            "command": "grep -r 'pull/' . | head -5"
        })));
    }

    #[test]
    fn gh_issue_is_not_a_gh_pr_command() {
        // `gh issue` is not a PR command.
        assert!(!is_gh_pr_command(&json!({ "command": "gh issue list" })));
    }

    #[test]
    fn missing_command_field_returns_false() {
        assert!(!is_gh_pr_command(&json!({ "timeout": 30000 })));
    }

    #[test]
    fn null_tool_input_returns_false() {
        assert!(!is_gh_pr_command(&json!(null)));
    }

    #[test]
    fn zsh_lc_wrapped_bare_gh_pr_create_is_a_gh_pr_command() {
        // Codex envelope: whole command is `/bin/zsh -lc 'gh pr …'`.
        // Peel must expose the inner gh invocation to classify.
        assert!(is_gh_pr_command_str("/bin/zsh -lc 'gh pr create --title t --body b'"));
        assert!(is_gh_pr_command_str("/bin/zsh -lc 'gh pr view'"));
        assert!(is_gh_pr_command_str("/bin/zsh -lc 'gh pr list --state open'"));
        assert!(is_gh_pr_command_str("/bin/zsh -lc 'gh pr edit 42 --add-label foo'"));
    }

    #[test]
    fn bash_c_and_sh_c_wrapped_gh_pr_are_gh_pr_commands() {
        assert!(is_gh_pr_command_str(r#"bash -c "gh pr create --title t""#));
        assert!(is_gh_pr_command_str("/usr/bin/bash -c 'gh pr view'"));
        assert!(is_gh_pr_command_str("sh -c 'gh pr list'"));
    }

    #[test]
    fn zsh_lc_wrapped_non_gh_is_not_a_gh_pr_command() {
        assert!(!is_gh_pr_command_str("/bin/zsh -lc 'cat chore.md'"));
        assert!(!is_gh_pr_command_str("/bin/zsh -lc 'gh issue list'"));
        assert!(!is_gh_pr_command_str("/bin/zsh -lc 'bossctl task show t'"));
    }

    #[test]
    fn quoted_gh_pr_inside_commit_message_is_not_a_shell_c_peel_false_positive() {
        // Not a shell -c wrapper: peel must not fire, and classify's quote
        // strip must keep the commit-message phrase from matching.
        assert!(!is_gh_pr_command_str(r#"jj describe -m "gh pr create --title t""#));
        assert!(!is_gh_pr_command_str("jj describe -m 'gh pr create'"));
    }

    #[test]
    fn peel_shell_c_payload_extracts_inner_script() {
        assert_eq!(
            peel_shell_c_payload("/bin/zsh -lc 'gh pr create --title t'"),
            Some("gh pr create --title t"),
        );
        assert_eq!(
            peel_shell_c_payload(r#"bash -c "cube pr update --branch b""#),
            Some("cube pr update --branch b"),
        );
        // Not a shell -c wrapper.
        assert_eq!(peel_shell_c_payload("gh pr create --title t"), None);
        // Trailing junk after the quoted payload rejects the peel.
        assert_eq!(peel_shell_c_payload("/bin/zsh -lc 'gh pr create' && echo done"), None,);
    }

    // ── is_revision_push_command ─────────────────────────────────

    #[test]
    fn cube_pr_update_is_a_revision_push_command() {
        // Regression: every worker (including revisions) pushes via `cube pr
        // update`, not bare `jj git push` (blocked by a PreToolUse hook). If
        // this stops matching, the SHA-delta gate's push-evidence check
        // never sees a revision's own push on any Stop after the first,
        // stranding multi-turn revisions in `active` forever.
        assert!(is_revision_push_command(&json!({
            "command": "cube pr update --branch boss/exec_abc123_01"
        })));
    }

    #[test]
    fn cube_pr_update_with_git_dir_prefix_is_a_revision_push_command() {
        assert!(is_revision_push_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git cube pr update --branch boss/exec_abc123_01"
        })));
    }

    #[test]
    fn deprecated_cube_pr_ensure_is_a_revision_push_command() {
        assert!(is_revision_push_command(&json!({
            "command": "cube pr ensure --branch boss/exec_abc123_01"
        })));
    }

    #[test]
    fn jj_git_push_is_still_a_revision_push_command() {
        // Defence-in-depth for any worker prompt that still pushes directly.
        assert!(is_revision_push_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git jj git push -b boss/exec_abc123_01"
        })));
    }

    #[test]
    fn jj_git_push_dry_run_is_not_a_revision_push_command() {
        assert!(!is_revision_push_command(&json!({
            "command": "jj git push -b boss/exec_abc123_01 --dry-run"
        })));
    }

    #[test]
    fn cube_pr_create_is_not_a_revision_push_command() {
        // Revisions never open a new PR (blocked by the OQ3 PreToolUse
        // guard); `cube pr create` staying unmatched here is defensive, not
        // load-bearing.
        assert!(!is_revision_push_command(&json!({
            "command": "cube pr create --branch boss/exec_abc123_01 --title 'my feature'"
        })));
    }

    #[test]
    fn unrelated_command_is_not_a_revision_push_command() {
        assert!(!is_revision_push_command(&json!({ "command": "bazel test //..." })));
    }

    #[test]
    fn missing_command_field_is_not_a_revision_push_command() {
        assert!(!is_revision_push_command(&json!({ "timeout": 30000 })));
    }

    // ── validate_pr_url ───────────────────────────────────────────

    #[test]
    fn validate_rejects_foo_bar_placeholder() {
        // Simulates a worker that emits a foo/bar fixture URL in test
        // output captured by a PostToolUse event.
        let response = json!({
            "stdout": "Pull request created: https://github.com/foo/bar/pull/42",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        let result = validate_pr_url(&extracted, "git@github.com:spinyfin/mono.git");
        assert!(result.is_err(), "foo/bar should be rejected");
        let reason = result.unwrap_err();
        assert!(
            reason.contains("placeholder"),
            "rejection reason should mention placeholder, got: {reason}",
        );
    }

    #[test]
    fn validate_accepts_product_repo_url() {
        let response = json!({
            "stdout": "https://github.com/spinyfin/mono/pull/42",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        assert_eq!(validate_pr_url(&extracted, "git@github.com:spinyfin/mono.git"), Ok(()),);
    }

    #[test]
    fn validate_rejects_octocat_hello_world_placeholder() {
        let response = json!({
            "stdout": "https://github.com/octocat/Hello-World/pull/1",
            "stderr": "",
        });
        let extracted = extract_pr_url_from_bash_response(&response).unwrap();
        let result = validate_pr_url(&extracted, "git@github.com:spinyfin/mono.git");
        assert!(result.is_err(), "octocat/Hello-World should be rejected");
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
