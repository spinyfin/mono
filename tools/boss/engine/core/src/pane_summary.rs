//! Generate and cache short human-readable pane-titlebar summaries
//! for work items.
//!
//! The macOS app's worker pane titlebars used to show the bare run id
//! (`exec_18ad...`). That's stable for traceability but unreadable
//! at a glance — eight panes on screen looked identical. We now ask
//! Claude (Sonnet — fast and cheap) to compress the work item's name
//! plus description into a short *gerund verb phrase* like
//! `"fixing the fencer scraper"`, which the app renders as a
//! natural-language sentence under the worker's display name
//! (`"Riker is fixing the fencer scraper"`).
//!
//! Phrasing rules: lowercase, no leading subject, present-continuous
//! verb (gerund), aiming for 3–6 words. The prompt allows up to ~7
//! when needed to keep the phrase complete — treating the word count
//! as a hard cap produces garbage like `"persist slot id on"` (cut
//! off mid-preposition).
//!
//! Caching: results are stored in the `pane_summaries` table keyed
//! by work_item_id, alongside a `basis_hash` derived from the inputs
//! we fed to Claude (name + description) and the prompt version.
//! When the work item's name or description changes, *or* when we
//! bump [`PROMPT_VERSION`] after editing the prompt, the basis hash
//! changes and we regenerate on the next spawn. Logs, APIs, and
//! identifiers everywhere else still use the run id — this module
//! only feeds the visual titlebar.
//!
//! Failure modes are silent on purpose. If the API key is missing
//! or the request fails (timeout, transport, 5xx), we fall back to
//! a deterministic local trim of the work item name. That keeps the
//! pane spawn flow on its happy path even when the network or
//! Anthropic is down. The fallback is *not* cached so a later spawn
//! can still call the API and store a real summary.

use std::time::Duration;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::claude_client::{self, CallConfig, Message, MessagesRequest};
use crate::work::{WorkDb, WorkItem};

/// Sonnet 4.6: latest released Sonnet at the time of writing — the
/// design doc explicitly calls it out as the right speed/cost
/// balance for this kind of micro-prompt.
const SUMMARY_MODEL: &str = "claude-sonnet-4-6";
/// 60 tokens covers 3–7 words plus the rare case where Sonnet adds
/// a stray article we'll strip back out. Tight enough that a runaway
/// 20-word summary still gets cut off; loose enough that legitimate
/// 6–7 word phrases (the upper end of what the prompt now permits)
/// don't get truncated mid-word.
const SUMMARY_MAX_TOKENS: u32 = 60;
/// Bump this whenever [`build_prompt`] changes in a way that would
/// produce a different label for the same inputs. It feeds into
/// [`compute_basis`], so bumping it invalidates every cached summary
/// and forces regeneration on the next spawn — the only way to make
/// previously-stored stale labels (e.g. v2 Title Case noun phrases)
/// refresh themselves under the v3 gerund-phrase prompt.
const PROMPT_VERSION: &str = "v3";
/// Hard timeout on the round-trip. Worker spawn is user-visible and
/// we'd rather show the fallback than block the pane on a slow
/// upstream. Sonnet on a tiny prompt typically returns in well
/// under a second.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5);

/// Compute a stable hash of the inputs that, if changed, must
/// invalidate the cached summary. Used as the `basis_hash` column
/// in `pane_summaries`.
pub fn compute_basis(name: &str, description: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PROMPT_VERSION.as_bytes());
    hasher.update([0u8]);
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(description.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Pull the (name, description) pair off whichever variant of
/// [`WorkItem`] the caller has. Tasks and chores share the `Task`
/// shape so they're handled together.
pub fn name_and_description(item: &WorkItem) -> (&str, &str) {
    match item {
        WorkItem::Product(p) => (p.name.as_str(), p.description.as_str()),
        WorkItem::Project(p) => (p.name.as_str(), p.description.as_str()),
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.name.as_str(), t.description.as_str()),
    }
}

/// Returns the work item's id regardless of variant. Lifted out so
/// callers don't have to repeat the match.
pub fn item_id(item: &WorkItem) -> &str {
    match item {
        WorkItem::Product(p) => &p.id,
        WorkItem::Project(p) => &p.id,
        WorkItem::Task(t) | WorkItem::Chore(t) => &t.id,
    }
}

/// Truncate a work item name to at most 6 words. Used in tests and
/// by the caller to produce a short display label when no API key is
/// available. The result is intentionally NOT lowercased here — the
/// caller decides formatting (e.g. `"<AgentName>: <result>"`).
/// The fallback is *not* cached because a later spawn might be able
/// to reach Claude and generate a proper gerund phrase.
pub fn local_fallback(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let words: Vec<&str> = trimmed.split_whitespace().take(6).collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" "))
}

/// Returns a fixed gerund phrase for conflict-resolution workers, overriding
/// the original task's pane summary so the pane titlebar reads
/// `"<Name> is resolving merge conflicts for <task-name>"` rather than
/// `"<Name> is implementing …"` (the original task's gerund).
///
/// The task name is truncated to 3 words so the combined phrase stays within
/// the 7-word UI guidance. If the task name is empty the shorter
/// `"resolving merge conflicts"` is returned instead.
pub fn conflict_resolution_summary(task_name: &str) -> Option<String> {
    let short: Vec<String> = task_name.split_whitespace().take(3).map(|w| w.to_lowercase()).collect();
    if short.is_empty() {
        Some("resolving merge conflicts".to_owned())
    } else {
        Some(format!("resolving merge conflicts for {}", short.join(" ")))
    }
}

/// Pane summary for `ci_remediation` workers — same rationale as
/// [`conflict_resolution_summary`]. Reads as `"<Name> is fixing CI for
/// <task-name>"` (or the shorter `"fixing CI"` when the task name is
/// empty / unavailable).
pub fn ci_remediation_summary(task_name: &str) -> Option<String> {
    let short: Vec<String> = task_name.split_whitespace().take(3).map(|w| w.to_lowercase()).collect();
    if short.is_empty() {
        Some("fixing CI".to_owned())
    } else {
        Some(format!("fixing CI for {}", short.join(" ")))
    }
}

/// Resolve a summary for a work item, hitting the cache first and
/// falling through to Claude only on a miss or basis change. Errors
/// are swallowed — this function never blocks worker spawn — and a
/// `None` return tells the caller to display the run id as before.
pub async fn get_or_generate(db: &WorkDb, api_key: Option<&str>, work_item: &WorkItem) -> Option<String> {
    let (name, description) = name_and_description(work_item);
    let basis = compute_basis(name, description);
    let id = item_id(work_item);

    match db.get_pane_summary(id) {
        Ok(Some((summary, cached_basis))) if cached_basis == basis => {
            return Some(summary);
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = id,
                ?err,
                "pane_summary: cache lookup failed; will try to regenerate",
            );
        }
    }

    if let Some(api_key) = api_key {
        match claude_short_summary(api_key, name, description).await {
            Ok(summary) => {
                if let Err(err) = db.set_pane_summary(id, &summary, &basis) {
                    tracing::warn!(
                        work_item_id = id,
                        ?err,
                        "pane_summary: failed to cache summary; will retry next spawn",
                    );
                }
                return Some(summary);
            }
            Err(err) => {
                tracing::warn!(
                    work_item_id = id,
                    ?err,
                    "pane_summary: Claude call failed; returning None so UI uses task_title",
                );
            }
        }
    } else {
        tracing::debug!(
            work_item_id = id,
            "pane_summary: no ANTHROPIC_API_KEY in config; returning None so UI uses task_title",
        );
    }

    None
}

/// Build the prompt for Claude. Pulled out as a free function so
/// tests can pin the exact wording — drift here changes summary
/// style across all panes.
fn build_prompt(name: &str, description: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You rewrite an engineering task title as a short verb phrase that describes \
         what an engineer is currently doing. The phrase will be inserted into the \
         sentence \"<Name> is ___.\" rendered under a worker pane in a developer UI.\n\
         \n\
         Rules:\n\
         - Start with a present-continuous verb (gerund ending in \"-ing\"): \
         \"fixing\", \"adding\", \"refactoring\", \"investigating\", \"wiring up\", etc.\n\
         - Lowercase. No leading subject (do NOT include the engineer's name or \"is\"). \
         No quotes, no trailing period, no explanation.\n\
         - Aim for 3-6 words. The word count is GUIDANCE, not a hard cap: stretch to 7 \
         if a shorter version would drop a key noun or end on a dangling preposition or \
         article. Coherence matters more than brevity.\n\
         - Never end on a preposition (\"on\", \"in\", \"to\", \"of\", \"for\", \"with\", \
         \"by\", \"into\", \"onto\"), a conjunction, or an article (\"the\", \"a\", \"an\").\n\
         \n\
         Examples:\n\
         - Input: \"Fix bossctl stubs and agent stop\"\n\
           GOOD: \"fixing bossctl and agent stops\"\n\
           GOOD: \"fixing bossctl stubs and agent stop\"\n\
           BAD:  \"Fixing Bossctl Stubs and Agent Stop\"  (title case)\n\
           BAD:  \"is fixing bossctl stubs\"               (includes \"is\")\n\
           BAD:  \"fix bossctl stubs\"                     (imperative, not gerund)\n\
         - Input: \"Persist allocated slot id onto run record (fix agent_id always = worker-1)\"\n\
           GOOD: \"persisting allocated slot ids on runs\"\n\
           GOOD: \"persisting slot ids on run records\"\n\
           BAD:  \"persisting slot id on\"                 (ends on preposition)\n\
           BAD:  \"persist slot id\"                       (imperative, not gerund)\n\
         - Input: \"Render agent activity summary as natural-language sentence\"\n\
           GOOD: \"rendering agent activity as a sentence\"\n\
           GOOD: \"rewording the agent activity line\"\n\n",
    );
    prompt.push_str("Task name:\n");
    prompt.push_str(name);
    prompt.push('\n');
    if !description.trim().is_empty() {
        prompt.push_str("\nTask description:\n");
        // Cap the description so a runaway design doc doesn't blow
        // the prompt up. 600 chars is enough to disambiguate similar
        // titles without paying for a long context window.
        let truncated: String = description.chars().take(600).collect();
        prompt.push_str(&truncated);
        prompt.push('\n');
    }
    prompt.push_str("\nVerb phrase:");
    prompt
}

/// Ask Claude for a short gerund summary via the shared [`crate::claude_client`]
/// pipeline and pull the first text block out of the response. Errors are
/// bucketed into `anyhow` because the caller (`get_or_generate`) only logs them.
pub async fn claude_short_summary(api_key: &str, name: &str, description: &str) -> Result<String> {
    let request = MessagesRequest::builder()
        .model(SUMMARY_MODEL)
        .max_tokens(SUMMARY_MAX_TOKENS)
        .messages(vec![Message::user(build_prompt(name, description))])
        .build();
    let config = CallConfig::new(SUMMARY_TIMEOUT);

    let response = claude_client::send_messages(api_key, &request, &config).await?;
    let cleaned = clean_summary(response.first_text().unwrap_or_default());
    if cleaned.is_empty() {
        anyhow::bail!("anthropic returned an empty summary");
    }
    Ok(cleaned)
}

/// Strip whitespace, surrounding quotes, and trailing punctuation
/// from the model's reply; lowercase the first word in case Sonnet
/// slipped a capital in; strip a leading `"is "` if the model copied
/// the example sentence framing back at us; and clamp to 7 words as
/// a safety net against runaway output. Sonnet reliably follows the
/// format instruction but small style strays shouldn't bleed into
/// the titlebar.
///
/// The 7-word ceiling matches the upper bound the prompt allows;
/// hard-clamping lower would re-introduce the truncation bug we
/// fixed in v2 (a coherent phrase chopped mid-thought becomes
/// incoherent).
fn clean_summary(raw: &str) -> String {
    let stripped = crate::json_extract::strip_wrapping_quotes(raw);
    let words: Vec<&str> = stripped.split_whitespace().take(7).collect();
    if words.is_empty() {
        return String::new();
    }
    let mut joined = words.join(" ");
    // Defensive: prompt forbids a leading subject/copula but if Sonnet
    // ever returns "is fixing X", strip the "is " so the surrounding
    // sentence reads "Riker is fixing X" rather than "Riker is is
    // fixing X".
    if let Some(rest) = joined.strip_prefix("is ") {
        joined = rest.to_owned();
    } else if let Some(rest) = joined.strip_prefix("Is ") {
        joined = rest.to_owned();
    }
    // Lowercase the very first character so the phrase reads
    // mid-sentence even if Sonnet capitalized it.
    let mut chars = joined.chars();
    match chars.next() {
        Some(c) => {
            let mut out: String = c.to_lowercase().collect();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkDb;
    use boss_protocol::{Task, TaskKind, TaskStatus};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_task(id: &str, name: &str, description: &str) -> WorkItem {
        WorkItem::Task(
            Task::builder()
                .id(id)
                .product_id("prod-1")
                .kind(TaskKind::Task)
                .name(name)
                .description(description)
                .status(TaskStatus::Active)
                .created_at("2026-01-01T00:00:00Z")
                .updated_at("2026-01-01T00:00:00Z")
                .build(),
        )
    }

    #[test]
    fn basis_hash_is_stable_for_same_inputs() {
        let a = compute_basis("Fix fencer scraper", "Scraper hits 429s on big tournaments");
        let b = compute_basis("Fix fencer scraper", "Scraper hits 429s on big tournaments");
        assert_eq!(a, b);
    }

    #[test]
    fn basis_hash_differs_when_name_changes() {
        let a = compute_basis("Fix fencer scraper", "desc");
        let b = compute_basis("Fix fencer scraper v2", "desc");
        assert_ne!(a, b);
    }

    #[test]
    fn basis_hash_differs_when_description_changes() {
        let a = compute_basis("name", "Old description");
        let b = compute_basis("name", "New description");
        assert_ne!(a, b);
    }

    #[test]
    fn basis_hash_does_not_collide_when_separator_moves() {
        // Without the explicit zero-byte separator, ("ab", "c") and
        // ("a", "bc") would hash the same. Make sure we keep them
        // distinct.
        let a = compute_basis("ab", "c");
        let b = compute_basis("a", "bc");
        assert_ne!(a, b);
    }

    #[test]
    fn local_fallback_truncates_to_first_six_words() {
        // The fallback trims to six words but preserves original case
        // since the caller renders it as "<Name>: <phrase>" (no "is").
        assert_eq!(
            local_fallback("Show short task summary in agent pane titlebar").as_deref(),
            Some("Show short task summary in agent"),
        );
    }

    #[test]
    fn local_fallback_returns_short_input_unchanged() {
        assert_eq!(local_fallback("Fix Fencer").as_deref(), Some("Fix Fencer"));
    }

    #[test]
    fn local_fallback_handles_empty() {
        assert_eq!(local_fallback("").as_deref(), None);
        assert_eq!(local_fallback("   ").as_deref(), None);
    }

    #[test]
    fn conflict_resolution_summary_uses_first_three_words_of_task_name() {
        assert_eq!(
            conflict_resolution_summary("Implementing app + engine resolution path").as_deref(),
            Some("resolving merge conflicts for implementing app +"),
        );
    }

    #[test]
    fn conflict_resolution_summary_lowercases_task_name_fragment() {
        assert_eq!(
            conflict_resolution_summary("Fix The Fencer Scraper").as_deref(),
            Some("resolving merge conflicts for fix the fencer"),
        );
    }

    #[test]
    fn conflict_resolution_summary_handles_short_task_name() {
        assert_eq!(
            conflict_resolution_summary("Fix it").as_deref(),
            Some("resolving merge conflicts for fix it"),
        );
    }

    #[test]
    fn conflict_resolution_summary_handles_empty_task_name() {
        assert_eq!(
            conflict_resolution_summary("").as_deref(),
            Some("resolving merge conflicts"),
        );
        assert_eq!(
            conflict_resolution_summary("   ").as_deref(),
            Some("resolving merge conflicts"),
        );
    }

    #[test]
    fn clean_summary_strips_quotes_and_periods() {
        assert_eq!(clean_summary("\"fixing fencer scraper.\""), "fixing fencer scraper",);
        assert_eq!(
            clean_summary("  fixing the pane titlebar  "),
            "fixing the pane titlebar",
        );
    }

    #[test]
    fn clean_summary_lowercases_leading_capital() {
        // Sonnet sometimes capitalizes despite the lowercase rule —
        // make sure the first character is forced lowercase so the
        // phrase reads mid-sentence after "<Name> is ".
        assert_eq!(clean_summary("Fixing the bossctl stubs"), "fixing the bossctl stubs",);
    }

    #[test]
    fn clean_summary_strips_leading_is_copula() {
        // Defensive: if the model echoed the framing back at us
        // ("is fixing X"), the surrounding sentence would read
        // "<Name> is is fixing X". Strip the leading copula.
        assert_eq!(clean_summary("is fixing the bossctl stubs"), "fixing the bossctl stubs",);
        assert_eq!(clean_summary("Is fixing the bossctl stubs"), "fixing the bossctl stubs",);
    }

    #[test]
    fn clean_summary_clamps_to_seven_words() {
        // The prompt allows up to 7 words for coherence, so the
        // safety clamp matches that ceiling. Anything beyond is a
        // runaway response we'd rather truncate than display.
        assert_eq!(
            clean_summary("one two three four five six seven eight nine"),
            "one two three four five six seven",
        );
    }

    #[test]
    fn clean_summary_keeps_six_word_phrases_intact() {
        // Regression guard: clamping lower would re-introduce the
        // truncation bug from v1 — a coherent phrase chopped mid-
        // thought becomes incoherent.
        assert_eq!(
            clean_summary("persisting slot ids on the run record"),
            "persisting slot ids on the run record",
        );
    }

    #[test]
    fn clean_summary_returns_empty_for_empty_input() {
        assert_eq!(clean_summary(""), "");
        assert_eq!(clean_summary("   "), "");
    }

    #[tokio::test]
    async fn cache_hit_returns_stored_summary_without_calling_api() {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let item = sample_task("task-1", "Fix fencer scraper", "desc");
        let basis = compute_basis("Fix fencer scraper", "desc");
        db.set_pane_summary("task-1", "fixing fencer scraper", &basis).unwrap();

        // No API key, but there IS a cache hit — the cached gerund
        // summary is returned. Cache is checked before the key path,
        // so a previously-computed gerund still surfaces even without
        // an API key present on this spawn.
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), Some("fixing fencer scraper"));
    }

    #[tokio::test]
    async fn cache_invalidates_when_basis_changes() {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let stale_basis = compute_basis("old name", "old desc");
        db.set_pane_summary("task-1", "stale summary", &stale_basis).unwrap();

        // Same id, different name → cache should miss. With no API
        // key, get_or_generate now returns None (the engine passes
        // the raw task name separately as task_title for the UI).
        let item = sample_task("task-1", "New Name Goes Here", "new desc");
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), None);
    }

    #[tokio::test]
    async fn no_api_key_returns_none() {
        // When no API key is present, get_or_generate returns None.
        // The engine passes the raw work-item name as task_title in
        // the spawn request; the UI renders it as "<Name>: <title>"
        // rather than the gerund "<Name> is <phrase>" form.
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let item = sample_task("task-1", "Show short task summary in agent pane", "");
        let summary = get_or_generate(&db, None, &item).await;
        assert_eq!(summary.as_deref(), None);
    }

    #[tokio::test]
    async fn api_response_flows_through_the_shared_pipeline() {
        // pane_summary now shares the engine-wide `claude_client` pipeline.
        // Drive a realistic Anthropic response through it with the exact
        // request `claude_short_summary` builds (model, max_tokens, prompt),
        // and confirm the shared client sets the auth/version headers and that
        // `clean_summary` post-processes the first text block. The caching half
        // is covered by `cache_hit_returns_stored_summary_*`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", claude_client::ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "fixing the pane titlebar"}],
            })))
            .mount(&server)
            .await;

        let request = MessagesRequest::builder()
            .model(SUMMARY_MODEL)
            .max_tokens(SUMMARY_MAX_TOKENS)
            .messages(vec![Message::user(build_prompt("name", "desc"))])
            .build();
        let config = CallConfig::new(SUMMARY_TIMEOUT).with_endpoint(format!("{}/v1/messages", server.uri()));
        let response = claude_client::send_messages("test-key", &request, &config)
            .await
            .expect("mock success");
        assert_eq!(response.first_text(), Some("fixing the pane titlebar"));
        assert_eq!(
            clean_summary(response.first_text().unwrap()),
            "fixing the pane titlebar"
        );
    }
}
