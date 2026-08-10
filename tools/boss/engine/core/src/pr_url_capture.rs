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
//! - [`is_pr_url_binding_command`] / [`is_pr_url_binding_command_str`] —
//!   Layer-1 gate so arbitrary Bash/shell output that happens to mention a
//!   PR URL does not bind the wrong PR. A narrower finalization gate keeps
//!   read-only and metadata commands from authorizing a recheck teardown.
//! - [`StagedPrUrlCache`] — a thread-safe `HashMap<execution_id,
//!   StagedPrUrlEntry>` that callers populate from progress events and the
//!   `on_stop` handler reads on Stop. First-writer-wins among observations
//!   of equal strength (armed vs armed, unarmed vs unarmed). A later publish
//!   command may replace a binding-only URL and arm finalization. Each entry
//!   also stamps `staged_at` so the merge-poller recheck can bound mid-turn
//!   deferral.
//!
//! The reconciliation path (`completion::detect_pr` →
//! `jj_candidate_commit_shas` → GitHub commits/{sha}/pulls) is
//! preserved as the engine-restart recovery fallback for primary
//! implementations. If the engine restarts after a worker opened a PR but
//! before Stop fired, the staged URL is lost from this cache (it lives in
//! memory only) and the fallback path runs on the next sweep. Revision
//! executions do not equate either URL channel with completion: they remain
//! live until their own Stop path records the contributed head. The staging
//! cache is the hot path; reconstruction is the primary-implementation cold
//! path.
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

/// Check whether a shell command string is a deliberate `gh pr` / `cube pr`
/// invocation whose output may bind a PR URL to the execution.
///
/// This intentionally accepts `gh pr create|view|list|edit` as well as
/// `cube pr create|update|ensure`: each is a supported source of a PR URL
/// for binding recovery. It does *not* decide whether observing that command
/// may finalize the execution; use [`is_pr_url_finalization_command_str`]
/// for that narrower question. Handles environment-variable prefixes such as
/// `GIT_DIR=.jj/repo/store/git gh pr create ...` via the shared
/// [`classify`] matcher, and Codex-style shell wrappers
/// (`/bin/zsh -lc 'gh pr …'`) via [`peel_shell_c_payload`].
///
/// Use this as the binding gate in the progress-event capture path:
/// arbitrary shell commands whose output happens to contain a PR URL
/// (file reads, test runs, chore descriptions echoed via shell) must not bind
/// a wrong PR against the running execution.
pub fn is_pr_url_binding_command_str(command: &str) -> bool {
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
/// [`is_pr_url_binding_command_str`]. Prefer the string form when the command
/// already came from a [`boss_engine_driver::PrUrlCaptureFeed`].
pub fn is_pr_url_binding_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    is_pr_url_binding_command_str(command)
}

/// Check whether a command that printed a PR URL is evidence that the worker
/// published or pushed that PR and may therefore arm recheck finalization.
///
/// This is deliberately narrower than [`is_pr_url_binding_command_str`].
/// `gh pr view`, `gh pr list`, and `gh pr edit` may all report a useful URL,
/// but none demonstrates that this execution pushed work.
///
/// Built on [`is_revision_push_command_str`] so the two "did this publish?"
/// predicates cannot drift: `cube pr update` / `cube pr ensure` / `jj git
/// push` arm finalization for the same reasons they populate the revision
/// push cache. `jj git push` is included even though it rarely prints a PR
/// URL — when a URL *is* present in the same observation (wrapper scripts,
/// echoed links), the push itself is publish evidence and must arm. Create-
/// shaped commands (`gh pr create`, `cube pr create`) are added on top
/// because they publish a new PR without going through the revision-push
/// path.
pub fn is_pr_url_finalization_command_str(command: &str) -> bool {
    if is_revision_push_command_str(command) {
        return true;
    }
    // `cube pr create` is publish evidence but is not a revision push
    // (revisions push to an existing parent PR via update/ensure / jj).
    if command.contains("cube pr create") {
        return true;
    }
    let command = peel_shell_c_payload(command).unwrap_or(command);
    matches!(classify(command), Some(inv) if inv.noun == GhNoun::Pr && inv.subcommand == "create")
}

/// Claude-shaped wrapper for [`is_pr_url_finalization_command_str`]. Prefer
/// the string form when the command already came from a
/// [`boss_engine_driver::PrUrlCaptureFeed`].
pub fn is_pr_url_finalization_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    is_pr_url_finalization_command_str(command)
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

/// Outcome of [`StagedPrUrlCache::record_command_observation`].
///
/// Callers use this to log at `info` only when the cache actually changed
/// (new bind or newly armed) and at `debug` for no-op re-observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCommandObservationOutcome {
    /// No entry existed; a new one was created (bound, maybe armed).
    Bound,
    /// An existing unarmed entry was promoted to armed (URL may have been
    /// replaced by the publish observation).
    Armed,
    /// Equal-strength re-observation; the entry was left unchanged.
    Unchanged,
}

/// One staged PR URL plus the wall-clock instant it was first recorded.
/// The merge-poller recheck path uses [`StagedPrUrlEntry::staged_at`] to
/// bound mid-turn deferral: a staged URL is evidence the worker *has* a
/// PR, not that it is *done*, so a live mid-turn worker must not be
/// finalized immediately — but a worker that never reaches a Stop still
/// needs a finite bound.
#[derive(Debug, Clone)]
pub struct StagedPrUrlEntry {
    pub pr_url: String,
    pub staged_at: Instant,
    /// Whether the command that bound this URL also demonstrated that the
    /// worker published or pushed it. Binding alone is not permission for the
    /// merge-poller recheck or Stop staged arm to finalize and reap.
    pub finalization_armed: bool,
}

/// In-memory `execution_id → pr_url` staging cache. Populated by the
/// `PostToolUse` hook dispatcher when a Bash event surfaces a PR
/// URL; consumed by `WorkerCompletionHandler::on_stop` on the
/// matching Stop hook.
///
/// First-writer-wins among observations of equal strength. A later publish
/// command (`finalization_armed = true`) outranks a binding-only entry: it
/// overwrites the URL, refreshes `staged_at`, and arms finalization. Equal-
/// strength armed observations keep the first URL.
#[derive(Debug, Default)]
pub struct StagedPrUrlCache {
    inner: Mutex<HashMap<String, StagedPrUrlEntry>>,
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
             guard.insert(
                 execution_id.to_owned(),
                 StagedPrUrlEntry {
                     pr_url: pr_url.to_owned(),
                     staged_at: Instant::now(),
                    finalization_armed: true,
                 },
             );
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
            .map(|entry| entry.pr_url.clone())
    }

    /// Read the complete staged entry without removing it.
    pub fn get_entry(&self, execution_id: &str) -> Option<StagedPrUrlEntry> {
        self.inner
            .lock()
            .expect("StagedPrUrlCache mutex poisoned")
            .get(execution_id)
            .cloned()
    }

    /// Bind a URL observed in command output, optionally granting
    /// finalization permission when that command published or pushed work.
    ///
    /// Equal-strength observations are first-writer-wins on the URL. A later
    /// publishing command (`finalization_armed = true`) outranks a binding-
    /// only entry: it overwrites `pr_url`, refreshes `staged_at`, and sets
    /// the armed flag. A read-only/metadata command can never downgrade an
    /// already-published observation.
    pub fn record_command_observation(
        &self,
        execution_id: &str,
        pr_url: &str,
        finalization_armed: bool,
    ) -> RecordCommandObservationOutcome {
        let mut guard = self.inner.lock().expect("StagedPrUrlCache mutex poisoned");
        match guard.get_mut(execution_id) {
            Some(entry) if finalization_armed && !entry.finalization_armed => {
                // Publish evidence outranks inspection: replace the bound URL.
                entry.pr_url = pr_url.to_owned();
                entry.staged_at = Instant::now();
                entry.finalization_armed = true;
                RecordCommandObservationOutcome::Armed
            }
            Some(entry) if finalization_armed => {
                // Already armed: keep first URL, no-op.
                let _ = entry;
                RecordCommandObservationOutcome::Unchanged
            }
            Some(_) => RecordCommandObservationOutcome::Unchanged,
            None => {
                guard.insert(
                    execution_id.to_owned(),
                    StagedPrUrlEntry {
                        pr_url: pr_url.to_owned(),
                        staged_at: Instant::now(),
                        finalization_armed,
                    },
                );
                RecordCommandObservationOutcome::Bound
            }
        }
    }

    /// Backdate a staged entry for a deterministic deferral-horizon test.
    #[cfg(test)]
    pub fn backdate_for_test(&self, execution_id: &str, age: std::time::Duration) {
        let mut guard = self.inner.lock().expect("StagedPrUrlCache mutex poisoned");
        if let Some(entry) = guard.get_mut(execution_id) {
            entry.staged_at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        }
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
        // → is_pr_url_binding_command_str. No GitHub poll.
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!("/bin/zsh -lc 'cube pr create --branch boss/exec_x --title t'"),
            &json!("Opening https://github.com/spinyfin/mono/pull/99\n"),
        )
        .expect("codex-shaped feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert_eq!(url, "https://github.com/spinyfin/mono/pull/99");
        assert!(is_pr_url_binding_command_str(&feed.command));
    }

    #[test]
    fn codex_cell_harness_wait_continuation_reaches_the_primary_capture_path() {
        // The whole primary path for a Codex worker whose `cube pr create`
        // outlives its JS cell's yield window: rollout records → the
        // driver's rollout progress normaliser → `pr_url_capture_feed` →
        // this module's extraction and Layer-1 gates.
        //
        // The URL only ever appears on the output of a *`wait`* record. Before
        // the cell correlation landed, the record fed to capture held the
        // `Script running with cell ID 1` placeholder and the record holding
        // the URL was attributed to a tool named `wait` and dropped — so this
        // whole path produced nothing and success depended on the worker
        // optionally writing the fallback artifact file.
        use crate::driver::{DriverRegistry, ProgressSessionConfig, ProgressStreamSource};

        let registry = DriverRegistry::default();
        let driver = registry.get("codex").expect("codex driver is registered");
        let mut session = driver
            .progress_session(&ProgressSessionConfig {
                run_id: None,
                identity_store: None,
                source: ProgressStreamSource::AgentJsonlFile,
                transcript_path: None,
                resume_state: None,
            })
            .expect("codex declares a rollout progress session");

        let script = concat!(
            r#"const r = await tools.exec_command({"cmd":"cube pr create --branch boss/exec_x --title t","#,
            r#""workdir":"/ws","yield_time_ms":30000,"max_output_tokens":2000});"#,
            "\ntext(JSON.stringify(r));"
        );
        for record in [
            json!({"type":"session_meta","payload":{"id":"thread-cell","cwd":"/ws"}}),
            json!({
                "type":"response_item",
                "payload":{"type":"custom_tool_call","name":"exec","call_id":"call-pr","input":script}
            }),
            json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":"call-pr",
                    "output":"Script running with cell ID 1\nWall time 11.1 seconds\nOutput:\n"
                }
            }),
            json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"wait",
                    "call_id":"call-wait",
                    "arguments":"{\"cell_id\":\"1\",\"yield_time_ms\":30000}"
                }
            }),
        ] {
            session
                .normalize_progress_events(&record)
                .expect("every record in the observed sequence must normalise");
        }

        let chunk = r#"{"chunk_id":"ab","exit_code":0,"output":"https://github.com/spinyfin/mono/pull/9\n"}"#;
        let events = session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-wait",
                    "output":[
                        {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                        {"type":"input_text","text":chunk}
                    ]
                }
            }))
            .expect("the wait continuation must normalise");

        let Some(crate::protocol::WorkerEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            ..
        }) = events.first()
        else {
            panic!("the wait continuation must produce a tool observation, got {events:?}");
        };
        let feed = driver
            .pr_url_capture_feed(tool_name, tool_input, tool_response)
            .expect("a correlated Bash observation feeds capture");
        assert_eq!(
            extract_pr_url_from_text(&feed.output_text).as_deref(),
            Some("https://github.com/spinyfin/mono/pull/9"),
        );
        assert!(
            is_pr_url_binding_command_str(&feed.command),
            "Layer-1 must see the originating `cube pr create`, not the cell script: {:?}",
            feed.command
        );
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
            is_pr_url_binding_command_str(&feed.command),
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
        assert_eq!(
            is_pr_url_binding_command_str(&feed.command),
            is_pr_url_binding_command(&input)
        );
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

    // ── PR URL binding and finalization gates ───────────────────────

    #[test]
    fn read_only_and_metadata_commands_bind_without_arming_finalization() {
        for command in ["gh pr view 1342", "gh pr edit 1342 --add-label bug"] {
            assert!(is_pr_url_binding_command_str(command), "{command} must bind its URL");
            assert!(
                !is_pr_url_finalization_command_str(command),
                "{command} must not arm finalization"
            );
        }

        let cache = StagedPrUrlCache::new();
        cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/458", false);
        let entry = cache.get_entry("exec_abc").expect("URL must be bound");
        assert_eq!(entry.pr_url, "https://github.com/spinyfin/mono/pull/458");
        assert!(!entry.finalization_armed);
    }

    #[test]
    fn publishing_commands_bind_and_arm_finalization() {
        for command in [
            "gh pr create --title t",
            "cube pr create --branch boss/exec_x",
            "cube pr update --branch boss/exec_x",
            "cube pr ensure --branch boss/exec_x",
            "jj git push --bookmark boss/exec_x",
        ] {
            assert!(
                is_pr_url_finalization_command_str(command),
                "{command} must arm finalization"
            );
        }
        // Create/update/ensure also bind; bare jj git push is publish evidence
        // for arming but is not a URL-binding command on its own.
        for command in [
            "gh pr create --title t",
            "cube pr create --branch boss/exec_x",
            "cube pr update --branch boss/exec_x",
            "cube pr ensure --branch boss/exec_x",
        ] {
            assert!(is_pr_url_binding_command_str(command), "{command} must bind its URL");
        }
        assert!(!is_pr_url_binding_command_str("jj git push --bookmark boss/exec_x"));
    }

    #[test]
    fn a_later_publishing_command_arms_an_existing_binding() {
        let cache = StagedPrUrlCache::new();
        assert_eq!(
            cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/458", false),
            RecordCommandObservationOutcome::Bound,
        );
        assert_eq!(
            cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/458", true),
            RecordCommandObservationOutcome::Armed,
        );
        assert!(cache.get_entry("exec_abc").expect("bound URL").finalization_armed);
    }

    #[test]
    fn publish_observation_replaces_a_binding_only_url() {
        // `gh pr view` of a related PR latches pull/100 unarmed; a later
        // `cube pr create` that prints pull/200 must overwrite, not merely
        // arm the wrong URL.
        let cache = StagedPrUrlCache::new();
        cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/100", false);
        assert_eq!(
            cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/200", true),
            RecordCommandObservationOutcome::Armed,
        );
        let entry = cache.get_entry("exec_abc").expect("bound URL");
        assert_eq!(entry.pr_url, "https://github.com/spinyfin/mono/pull/200");
        assert!(entry.finalization_armed);
    }

    #[test]
    fn equal_strength_observations_keep_first_url() {
        let cache = StagedPrUrlCache::new();
        cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/100", true);
        assert_eq!(
            cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/200", true),
            RecordCommandObservationOutcome::Unchanged,
        );
        assert_eq!(
            cache.get("exec_abc").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/100"),
        );
        cache.record_command_observation("exec_b", "https://github.com/spinyfin/mono/pull/1", false);
        assert_eq!(
            cache.record_command_observation("exec_b", "https://github.com/spinyfin/mono/pull/2", false),
            RecordCommandObservationOutcome::Unchanged,
        );
        assert_eq!(
            cache.get("exec_b").as_deref(),
            Some("https://github.com/spinyfin/mono/pull/1"),
        );
    }

    #[test]
    fn capture_path_gh_pr_view_binds_without_arming() {
        // Dispatcher wiring: feed → extract → both gates → record.
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!({ "command": "gh pr view 100" }),
            &json!({
                "stdout": "https://github.com/spinyfin/mono/pull/100\n",
                "stderr": "",
            }),
        )
        .expect("view feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert!(is_pr_url_binding_command_str(&feed.command));
        assert!(!is_pr_url_finalization_command_str(&feed.command));
        let cache = StagedPrUrlCache::new();
        let outcome =
            cache.record_command_observation("exec_abc", &url, is_pr_url_finalization_command_str(&feed.command));
        assert_eq!(outcome, RecordCommandObservationOutcome::Bound);
        let entry = cache.get_entry("exec_abc").expect("bound");
        assert_eq!(entry.pr_url, "https://github.com/spinyfin/mono/pull/100");
        assert!(!entry.finalization_armed);
    }

    #[test]
    fn capture_path_gh_pr_edit_binds_without_arming() {
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!({ "command": "gh pr edit 100 --add-label bug" }),
            &json!({
                "stdout": "https://github.com/spinyfin/mono/pull/100\n",
                "stderr": "",
            }),
        )
        .expect("edit feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert!(is_pr_url_binding_command_str(&feed.command));
        assert!(!is_pr_url_finalization_command_str(&feed.command));
        let cache = StagedPrUrlCache::new();
        cache.record_command_observation("exec_abc", &url, is_pr_url_finalization_command_str(&feed.command));
        assert!(!cache.get_entry("exec_abc").expect("bound").finalization_armed);
    }

    #[test]
    fn capture_path_cube_pr_create_binds_and_arms() {
        let feed = crate::driver::default_pr_url_capture_feed(
            "Bash",
            &json!({ "command": "cube pr create --branch boss/exec_x --title t" }),
            &json!({
                "stdout": "https://github.com/spinyfin/mono/pull/200\n",
                "stderr": "",
            }),
        )
        .expect("create feed");
        let url = extract_pr_url_from_text(&feed.output_text).expect("url");
        assert!(is_pr_url_binding_command_str(&feed.command));
        assert!(is_pr_url_finalization_command_str(&feed.command));
        let cache = StagedPrUrlCache::new();
        // Prior binding-only observation of an unrelated PR.
        cache.record_command_observation("exec_abc", "https://github.com/spinyfin/mono/pull/100", false);
        let outcome =
            cache.record_command_observation("exec_abc", &url, is_pr_url_finalization_command_str(&feed.command));
        assert_eq!(outcome, RecordCommandObservationOutcome::Armed);
        let entry = cache.get_entry("exec_abc").expect("bound");
        assert_eq!(entry.pr_url, "https://github.com/spinyfin/mono/pull/200");
        assert!(entry.finalization_armed);
    }

    #[test]
    fn binding_gate_handles_shell_wrappers_and_rejects_non_pr_commands() {
        assert!(is_pr_url_binding_command_str("/bin/zsh -lc 'gh pr view'"));
        assert!(is_pr_url_binding_command_str(
            "/usr/bin/bash -c 'gh pr edit 42 --body b'"
        ));
        assert!(is_pr_url_finalization_command_str(
            r#"bash -c "gh pr create --title t""#
        ));
        assert!(!is_pr_url_binding_command_str("/bin/zsh -lc 'cat chore.md'"));
        assert!(!is_pr_url_binding_command_str(
            r#"jj describe -m "gh pr create --title t""#
        ));
        // Shell-wrapper peel must still see a real `gh pr list` inside.
        assert!(is_pr_url_binding_command_str("sh -c 'gh pr list'"));
        // Single-quoted commit message must not false-positive.
        assert!(!is_pr_url_binding_command_str("jj describe -m 'gh pr create'"));
        assert!(!is_pr_url_binding_command(&json!({ "timeout": 30000 })));
    }

    #[test]
    fn finalization_gate_rejects_false_positives_and_accepts_jj_git_push() {
        // Commit-message phrase is not publish evidence.
        assert!(!is_pr_url_finalization_command_str(r#"jj describe -m "gh pr create""#));
        assert!(!is_pr_url_finalization_command_str("jj describe -m 'gh pr create'"));
        assert!(!is_pr_url_finalization_command_str("gh pr view 42"));
        assert!(!is_pr_url_finalization_command_str("gh pr list"));
        assert!(!is_pr_url_finalization_command_str("gh pr edit 42 --body b"));
        // Align with is_revision_push_command_str: a real push arms.
        assert!(is_pr_url_finalization_command_str("jj git push --bookmark b"));
        assert!(!is_pr_url_finalization_command_str(
            "jj git push --dry-run --bookmark b"
        ));
        // Wrapper parity with the binding pair.
        assert_eq!(
            is_pr_url_finalization_command_str("gh pr create --title t"),
            is_pr_url_finalization_command(&json!({ "command": "gh pr create --title t" })),
        );
        assert_eq!(
            is_pr_url_finalization_command_str("gh pr view 1"),
            is_pr_url_finalization_command(&json!({ "command": "gh pr view 1" })),
        );
        assert!(!is_pr_url_finalization_command(&json!({ "timeout": 30000 })));
    }

    #[test]
    fn binding_gate_accepts_create_with_environment_prefix() {
        assert!(is_pr_url_binding_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr create --head boss/exec_abc --base main"
        })));
    }

    #[test]
    fn binding_gate_accepts_view_and_list() {
        assert!(is_pr_url_binding_command(&json!({
            "command": "GIT_DIR=.jj/repo/store/git gh pr view"
        })));
        assert!(is_pr_url_binding_command(
            &json!({ "command": "gh pr list --state open" })
        ));
    }

    #[test]
    fn binding_gate_rejects_non_pr_commands() {
        for command in [
            "bossctl task show task_123",
            "cat chore.md",
            "grep -r 'pull/' . | head -5",
            "gh issue list",
            // zsh-wrapped non-PR commands must still reject.
            "/bin/zsh -lc 'gh issue list'",
            "/bin/zsh -lc 'bossctl task show t'",
        ] {
            assert!(!is_pr_url_binding_command_str(command), "must reject {command}");
            assert!(!is_pr_url_binding_command(&json!({ "command": command })));
        }
        assert!(!is_pr_url_binding_command(&json!(null)));
    }

    #[test]
    fn shell_wrappers_preserve_the_split_gate() {
        let view = "/bin/zsh -lc 'gh pr view'";
        let edit = "/bin/zsh -lc 'gh pr edit 42 --add-label foo'";
        let create = "/bin/zsh -lc 'gh pr create --title t --body b'";
        for command in [view, edit, create] {
            assert!(is_pr_url_binding_command_str(command));
        }
        assert!(!is_pr_url_finalization_command_str(view));
        assert!(!is_pr_url_finalization_command_str(edit));
        assert!(is_pr_url_finalization_command_str(create));
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
