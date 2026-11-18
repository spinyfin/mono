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
//! or the request fails after a single bounded retry of transient
//! errors (timeout, transport, 429, 5xx), we fall back to a
//! deterministic local trim of the work item name. That keeps the
//! pane spawn flow on its happy path even when the network or
//! Anthropic is down. The fallback is *not* cached so a later spawn
//! can still call the API and store a real summary. Auth / other 4xx
//! and decode errors are not retried — they will not heal on a second
//! attempt and would only add spawn latency.

use std::time::Duration;

use anyhow::Result;
use sha2::{Digest, Sha256};

use boss_claude_client::{self as claude_client, CallConfig, Message, MessagesRequest, RetryPolicy};
use boss_engine_utility_model::{UtilityCall, UtilityModel, UtilityTask};
use boss_protocol::{ExecutionKind, ReviewStage};
use boss_work_db::{WorkDb, WorkItem};

// The model is no longer pinned here: it comes from the `UtilityModel`
// provider via `UtilityTask::PaneSummary`, whose default is Sonnet 4.6 —
// the design doc explicitly calls that out as the right speed/cost balance
// for this kind of micro-prompt. An operator can retarget it
// (`BOSS_UTILITY_MODEL_PANE_SUMMARY`) independently of any work item's driver.

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
/// Hard timeout on a single round-trip attempt. Worker spawn is
/// user-visible and we'd rather show the fallback than block the pane
/// on a slow upstream. Sonnet on a tiny prompt typically returns in
/// well under a second. Applied per attempt (not across the whole
/// retry budget) — see [`SUMMARY_ATTEMPTS`].
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5);
/// Total attempts for the pane-summary utility-model call. One retry
/// only: transport blips / 429 / 5xx are the common transient failures
/// that a single re-issue absorbs, and this call sits on the
/// tmux worker spawn path so every extra attempt adds directly to
/// spawn latency. Non-transient errors (auth, other 4xx, decode) are
/// never retried — see [`claude_client::ClaudeError::is_retryable`].
///
/// Wall-clock ceiling:
/// `SUMMARY_ATTEMPTS * SUMMARY_TIMEOUT + (SUMMARY_ATTEMPTS - 1) * SUMMARY_BACKOFF`
/// = 2 × 5s + 250ms = **10.25s** worst case (both attempts timing out).
/// Happy path stays under a second; a transport fail that recovers on
/// the second attempt typically costs a few hundred ms of backoff plus
/// one short model RTT.
const SUMMARY_ATTEMPTS: u32 = 2;
/// Backoff before the single retry. Short on purpose: this is spawn-
/// path, and the blips we care about (connection reset, brief 5xx)
/// recover in tens of milliseconds — a long pause only inflates pane
/// open latency without improving absorb rate.
const SUMMARY_BACKOFF: Duration = Duration::from_millis(250);

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

/// Words that make a poor endpoint for a truncated name fragment:
/// articles, prepositions, conjunctions, and copulas. When a fragment
/// must be cut, we walk the cut point back off any of these so the
/// result doesn't stop mid-clause (e.g. on "the", "for", "is").
/// Pronouns, negation, and auxiliary verbs also need their following clause.
const DANGLING_STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "for", "to", "with", "by", "into", "onto", "on", "in", "and", "or", "but", "from", "as",
    "at", "is", "are", "was", "were", "be", "been", "that", "which", "this", "these", "those", "its", "their", "it",
    "not", "so", "if", "when", "than", "then", "about", "over", "under", "via", "per", "has", "have", "had", "does",
    "do", "nothing",
];

/// Clause punctuation that should not sit immediately before an ellipsis,
/// and that should not mask a dangling stopword (`"for,"`).
const TRAILING_CLAUSE_PUNCT: [char; 7] = [',', ';', ':', '-', '\u{2014}', '\u{2013}', '&'];

fn strip_trailing_clause_punct(word: &str) -> &str {
    word.trim_end_matches(TRAILING_CLAUSE_PUNCT)
}

/// A trailing possessive (`"engine's"`) is just as bad a stopping point
/// as a bare preposition — it reads as a dangling clause fragment.
fn ends_in_possessive(word: &str) -> bool {
    word.ends_with("'s") || word.ends_with("\u{2019}s")
}

fn is_dangling(word: &str) -> bool {
    let word = strip_trailing_clause_punct(word);
    word.is_empty() || ends_in_possessive(word) || DANGLING_STOPWORDS.contains(&word.to_lowercase().as_str())
}

/// Whether `text` ends inside an open quote or bracket. An apostrophe
/// between two alphanumerics (`"don't"`, `"engine's"`) is not a quote.
fn has_open_delimiter(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut depth: i32 = 0;
    let mut in_double = false;
    let mut in_single = false;
    for (i, &c) in chars.iter().enumerate() {
        let prev_alnum = i > 0 && chars[i - 1].is_alphanumeric();
        let next_alnum = chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
        match c {
            '(' | '[' | '{' if !in_double && !in_single => depth += 1,
            ')' | ']' | '}' if !in_double && !in_single => depth = (depth - 1).max(0),
            '"' if !in_single => in_double = !in_double,
            '\u{201c}' if !in_single => in_double = true,
            '\u{201d}' if !in_single => in_double = false,
            '\'' | '\u{2018}' | '\u{2019}' if prev_alnum && next_alnum => {}
            '\'' | '\u{2019}'
                if !in_single
                    && i > 0
                    && matches!(chars[i - 1], 's' | 'S')
                    && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) => {}
            '\'' if !in_double => in_single = !in_single,
            '\u{2018}' if !in_double => in_single = true,
            '\u{2019}' if !in_double => in_single = false,
            _ => {}
        }
    }
    depth > 0 || in_double || in_single
}

/// A word that ends in the middle of an identifier or path (`"foo::"`,
/// `"src/"`, `"snake_"`, `"key="`).
fn ends_mid_identifier(word: &str) -> bool {
    word.ends_with("::") || word.ends_with(['/', '\\', '_', '=', '(', '[', '{', '<', '#', '@'])
}

/// Take up to `max_words` words from `task_name` for embedding in a
/// derived pane-title phrase, preserving the name's original casing —
/// the borrowed name is a proper noun / identifier fragment, not the
/// fixed verb phrase it's spliced into, so it must not be lowercased.
///
/// A name that already fits within `max_words` is returned whole and
/// unmarked. For a shortened name, walk the cut point back past:
/// - dangling articles, prepositions, conjunctions, copulas, or possessives;
/// - words ending mid-identifier;
/// - fragments with an open quote or bracket.
///
/// Strip trailing clause punctuation and append "…" to mark the abbreviation.
///
/// Returns `None` when the name is empty or reduces to nothing but
/// dangling words, so the caller can fall back to its fixed short
/// phrase instead of an empty or unreadable fragment.
fn name_fragment(task_name: &str, max_words: usize) -> Option<String> {
    let trimmed = task_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() <= max_words {
        return Some(words.join(" "));
    }
    let mut candidate = &words[..max_words];
    while let Some(last) = candidate.last() {
        if is_dangling(last) || ends_mid_identifier(last) || has_open_delimiter(&candidate.join(" ")) {
            candidate = &candidate[..candidate.len() - 1];
        } else {
            break;
        }
    }
    if candidate.is_empty() {
        return None;
    }
    let mut fragment = candidate.join(" ");
    fragment.truncate(fragment.trim_end_matches(TRAILING_CLAUSE_PUNCT).len());
    if fragment.is_empty() {
        return None;
    }
    Some(format!("{fragment}…"))
}

/// Overall word budget for a derived title (verb plus whatever is spliced in),
/// matching the ~7-word UI guidance for pane titles.
const DERIVED_TITLE_WORD_BUDGET: usize = 7;

/// How one derived execution kind words its pane title.
struct DerivedPhrase {
    /// Short verb used before `": <cached summary>"`.
    cached_verb: &'static str,
    /// Verb phrase spliced directly before the item name when nothing is cached.
    name_verb: &'static str,
    /// Shown when the name offers no legible fragment.
    fallback: &'static str,
    /// Word cap on the name fragment in the name-based phrase.
    name_words: usize,
}

impl DerivedPhrase {
    /// Prefer the item's cached implementer summary (`"reviewing: fixing
    /// queued-reason labels on cards"`); otherwise splice a shortened,
    /// case-preserved fragment of `task_name` (see [`name_fragment`]) after the
    /// verb phrase, or use the fixed `fallback` when the name is empty or has
    /// no legible fragment.
    fn render(&self, task_name: &str, cached_summary: Option<&str>) -> String {
        if let Some(cached) = cached_summary {
            let budget = DERIVED_TITLE_WORD_BUDGET.saturating_sub(self.cached_verb.split_whitespace().count());
            if let Some(fragment) = name_fragment(cached, budget.max(1)) {
                return format!("{}: {fragment}", self.cached_verb);
            }
        }
        match name_fragment(task_name, self.name_words) {
            Some(fragment) => format!("{} {fragment}", self.name_verb),
            None => self.fallback.to_owned(),
        }
    }
}

/// Read the work item's cached implementer summary without writing it. The
/// derived kinds share the implementer's `work_item_id`, so they may only
/// *read* this slot — writing it would flap the implementer's pane. Returns
/// `None` when nothing is cached or the lookup fails.
pub fn cached_summary(db: &WorkDb, work_item_id: &str) -> Option<String> {
    match db.get_pane_summary(work_item_id) {
        Ok(Some((summary, _basis))) if !summary.trim().is_empty() => Some(summary),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(work_item_id, ?err, "pane_summary: cache read failed for derived title");
            None
        }
    }
}

/// Pane summary for conflict-resolution workers, overriding the original
/// task's gerund so the title reads `"<Name> is resolving conflicts: <summary>"`
/// (cached implementer summary) or `"... resolving merge conflicts for
/// <task-name>"` (name fallback, name capped to 3 words).
pub fn conflict_resolution_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "resolving conflicts",
            name_verb: "resolving merge conflicts for",
            fallback: "resolving merge conflicts",
            name_words: 3,
        }
        .render(task_name, cached_summary),
    )
}

/// Pane summary for `ci_remediation` workers — same rationale as
/// [`conflict_resolution_summary`]. Reads as `"fixing CI: <summary>"` or
/// `"fixing CI for <task-name>"`, or the bare `"fixing CI"`.
pub fn ci_remediation_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "fixing CI",
            name_verb: "fixing CI for",
            fallback: "fixing CI",
            name_words: 3,
        }
        .render(task_name, cached_summary),
    )
}

/// Pane summary for `pr_review` workers. A reviewer's `work_item_id` is bound
/// to the *reviewed* task, so this only reads that row's cached gerund (never
/// [`get_or_generate`], which would overwrite it). Reads as `"reviewing:
/// <summary>"` or `"reviewing the PR for <task-name>"` (name capped to 3 words).
pub fn pr_review_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "reviewing",
            name_verb: "reviewing the PR for",
            fallback: "reviewing a pull request",
            name_words: 3,
        }
        .render(task_name, cached_summary),
    )
}

/// Pane summary for the consolidating agent of an AI review. Distinct from
/// [`pr_review_summary`] so the pane does not read as one more reviewer: it
/// reads `"consolidating review: <summary>"` or `"consolidating review
/// findings for <task-name>"` (name capped to 2 words).
pub fn pr_review_consolidation_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "consolidating review",
            name_verb: "consolidating review findings for",
            fallback: "consolidating review findings",
            name_words: 2,
        }
        .render(task_name, cached_summary),
    )
}

/// Pane summary for `answer_agent` workers. The synthetic work item's
/// name/description are derived straight from the comment being answered
/// (up to 600 raw chars of someone else's text as the "description") —
/// feeding either through [`get_or_generate`] risks steering Claude toward
/// the comment's own verb instead of "answering". A fixed derived phrase
/// sidesteps the model call entirely.
pub fn answer_agent_summary() -> Option<String> {
    Some("answering a doc comment".to_owned())
}

/// Pane summary for `progression` agents. The run's `work_item_id` is the stuck
/// item itself, so the cached implementer gerund is read but never written.
/// Reads as `"unsticking: <summary>"`, `"unsticking <task-name>"`, or the
/// bare `"unsticking work"`.
pub fn progression_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "unsticking",
            name_verb: "unsticking",
            fallback: "unsticking work",
            name_words: 3,
        }
        .render(task_name, cached_summary),
    )
}

/// Derived phrase for [`ExecutionKind::PrReviewGuide`]. `task_name` is the
/// owning root task's name (not the synthetic row's `Review guide: <PR
/// title>`) and `cached_summary` is that owning task's cache slot. Reads as
/// `"review guide: <summary>"`, `"generating a review guide for
/// <task-name>"` (name capped to 2 words), or the bare phrase when the series
/// has no owning task.
pub fn review_guide_summary(task_name: &str, cached_summary: Option<&str>) -> Option<String> {
    Some(
        DerivedPhrase {
            cached_verb: "review guide",
            name_verb: "generating a review guide for",
            fallback: "generating a review guide",
            name_words: 2,
        }
        .render(task_name, cached_summary),
    )
}

/// Route an execution kind to a *derived* pane-titlebar phrase, bypassing
/// [`get_or_generate`] entirely — or `None` to say "fall through to the
/// cached/LLM path".
///
/// `pane_summaries` is keyed by `work_item_id` alone, and `PrReview`,
/// `ConflictResolution`, `CiRemediation` and `Progression` executions bind
/// `work_item_id` to the implementer's own row. Routing them through
/// `get_or_generate` would overwrite the implementer's cached summary and
/// flap both panes. They instead read the stored phrase through [`cached_summary`]
/// and never write it; no model call is made. `AnswerAgent`'s
/// synthetic row has a different problem (its description is someone else's
/// comment text), so it keeps a fixed phrase.
///
/// Matches [`ExecutionKind`] exhaustively per its doc comment so a new
/// variant forces a decision here rather than silently falling through.
pub fn derived_title_summary(
    kind: &ExecutionKind,
    review_stage: Option<ReviewStage>,
    task_name: &str,
    cached_summary: Option<&str>,
) -> Option<Option<String>> {
    match kind {
        ExecutionKind::CiRemediation => Some(ci_remediation_summary(task_name, cached_summary)),
        ExecutionKind::ConflictResolution => Some(conflict_resolution_summary(task_name, cached_summary)),
        ExecutionKind::PrReview if review_stage == Some(ReviewStage::Consolidation) => {
            Some(pr_review_consolidation_summary(task_name, cached_summary))
        }
        ExecutionKind::PrReview => Some(pr_review_summary(task_name, cached_summary)),
        ExecutionKind::AnswerAgent => Some(answer_agent_summary()),
        ExecutionKind::PrReviewGuide => Some(review_guide_summary(task_name, cached_summary)),
        ExecutionKind::AutomationTriage
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::TaskImplementation => None,
        ExecutionKind::Progression => Some(progression_summary(task_name, cached_summary)),
    }
}

/// Resolve the pane spawner's title, including review-guide ownership and the
/// boundary between read-only derived titles and model-generated summaries.
pub async fn title_for_spawn(
    db: &WorkDb,
    utility: &dyn UtilityModel,
    kind: &ExecutionKind,
    review_stage: Option<ReviewStage>,
    execution_work_item_id: &str,
    work_item: &WorkItem,
) -> Option<String> {
    let owner = (kind == &ExecutionKind::PrReviewGuide).then(|| db.owning_work_item_id(execution_work_item_id));
    let owner_item = owner.as_deref().and_then(|id| db.get_work_item(id).ok());
    let subject = if owner.is_some() {
        owner_item
            .as_ref()
            .map(name_and_description)
            .map(|(name, _)| name)
            .unwrap_or_default()
    } else {
        name_and_description(work_item).0
    };
    let cached = cached_summary(db, owner.as_deref().unwrap_or_else(|| item_id(work_item)));
    match derived_title_summary(kind, review_stage, subject, cached.as_deref()) {
        Some(summary) => summary,
        None => get_or_generate(db, utility, work_item).await,
    }
}

/// Resolve a summary for a work item, hitting the cache first and
/// falling through to Claude only on a miss or basis change. Errors
/// are swallowed — this function never blocks worker spawn — and a
/// `None` return tells the caller to display the run id as before.
pub async fn get_or_generate(db: &WorkDb, utility: &dyn UtilityModel, work_item: &WorkItem) -> Option<String> {
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

    match utility.resolve(UtilityTask::PaneSummary) {
        Ok(call) => match claude_short_summary(&call, name, description).await {
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
                    "pane_summary: utility-model call failed; returning None so UI uses task_title",
                );
            }
        },
        Err(err) => {
            tracing::debug!(
                work_item_id = id,
                %err,
                "pane_summary: no utility-model credential; returning None so UI uses task_title",
            );
        }
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

/// Ask for a short gerund summary via the shared [`boss_claude_client`]
/// pipeline, addressed at whatever the [`UtilityModel`] provider resolved, and
/// pull the first text block out of the response. Transient failures
/// (transport, timeout, 429, 5xx) are retried once under a tight spawn-path
/// budget — see [`SUMMARY_ATTEMPTS`] / [`SUMMARY_BACKOFF`]. Auth failures,
/// other 4xx, and decode errors fail fast without retry. Errors are bucketed
/// into `anyhow` because the caller (`get_or_generate`) only logs them; the
/// final failure after the budget is exhausted still surfaces so the caller
/// can WARN.
pub async fn claude_short_summary(call: &UtilityCall, name: &str, description: &str) -> Result<String> {
    let request = MessagesRequest::builder()
        .model(call.model.clone())
        .max_tokens(SUMMARY_MAX_TOKENS)
        .messages(vec![Message::user(build_prompt(name, description))])
        .build();
    // Explicit spawn-path policy rather than `CallConfig::new`'s default:
    // pin the attempt count and short backoff so a future default change
    // cannot quietly inflate (or strip) retry on the critical spawn path.
    let config = CallConfig::new(SUMMARY_TIMEOUT)
        .with_retry(RetryPolicy::new(SUMMARY_ATTEMPTS, SUMMARY_BACKOFF, SUMMARY_BACKOFF))
        .with_endpoint(call.endpoint.clone());

    let response = claude_client::send_messages(&call.api_key, &request, &config).await?;
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
    let stripped = boss_engine_utils::json_extract::strip_wrapping_quotes(raw);
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
mod derived_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{Task, TaskKind, TaskStatus};
    use boss_work_db::WorkDb;
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A provider that resolves no credential for any task, so
    /// `get_or_generate` takes the no-credential branch without a network
    /// call — the analogue of the `None` API key these tests used to pass.
    fn keyless_utility() -> boss_engine_utility_model::AnthropicUtilityModel {
        boss_engine_utility_model::AnthropicUtilityModel::from_lookup(None, |_| None)
    }

    pub(super) fn sample_task(id: &str, name: &str, description: &str) -> WorkItem {
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
    fn conflict_resolution_summary_walks_back_off_a_dangling_word_and_marks_the_cut() {
        assert_eq!(
            conflict_resolution_summary("Implementing app and engine resolution path", None).as_deref(),
            Some("resolving merge conflicts for Implementing app…"),
        );
    }

    #[test]
    fn conflict_resolution_summary_preserves_task_name_casing() {
        assert_eq!(
            conflict_resolution_summary("Fix The Fencer Scraper", None).as_deref(),
            Some("resolving merge conflicts for Fix The Fencer…"),
        );
    }

    #[test]
    fn conflict_resolution_summary_passes_short_task_name_through_unmarked() {
        assert_eq!(
            conflict_resolution_summary("Fix it", None).as_deref(),
            Some("resolving merge conflicts for Fix it"),
        );
    }

    #[test]
    fn conflict_resolution_summary_handles_empty_task_name() {
        assert_eq!(
            conflict_resolution_summary("", None).as_deref(),
            Some("resolving merge conflicts"),
        );
        assert_eq!(
            conflict_resolution_summary("   ", None).as_deref(),
            Some("resolving merge conflicts"),
        );
    }

    #[test]
    fn ci_remediation_summary_caps_at_three_words_preserving_case_and_marking_the_cut() {
        assert_eq!(
            ci_remediation_summary("Fix flaky fencer scraper test", None).as_deref(),
            Some("fixing CI for Fix flaky fencer…"),
        );
    }

    #[test]
    fn ci_remediation_summary_handles_empty_task_name() {
        assert_eq!(ci_remediation_summary("", None).as_deref(), Some("fixing CI"));
        assert_eq!(ci_remediation_summary("   ", None).as_deref(), Some("fixing CI"));
    }

    #[test]
    fn pr_review_summary_caps_at_three_words_to_stay_within_the_seven_word_budget() {
        assert_eq!(
            pr_review_summary("Address automated PR review findings", None).as_deref(),
            Some("reviewing the PR for Address automated PR…"),
        );
    }

    #[test]
    fn pr_review_summary_handles_empty_task_name() {
        assert_eq!(pr_review_summary("", None).as_deref(), Some("reviewing a pull request"));
        assert_eq!(
            pr_review_summary("   ", None).as_deref(),
            Some("reviewing a pull request")
        );
    }

    /// A long sentence-shaped name whose first three words end on a
    /// possessive: the cut point walks back past `the` and `from` so the
    /// fragment stops on the identifier rather than mid-clause, and the
    /// identifier's casing survives.
    #[test]
    fn pr_review_summary_walks_back_off_a_trailing_possessive() {
        assert_eq!(
            pr_review_summary(
                "SessionEnd from the engine's own reap resurrects abandoned executions into unreapable phantoms",
                None
            )
            .as_deref(),
            Some("reviewing the PR for SessionEnd…"),
        );
    }

    #[test]
    fn review_guide_summary_names_the_owning_task_and_falls_back_without_one() {
        assert_eq!(
            review_guide_summary("Header metadata for panes", None).as_deref(),
            Some("generating a review guide for Header metadata…"),
        );
        assert_eq!(
            review_guide_summary("", None).as_deref(),
            Some("generating a review guide")
        );
    }

    #[test]
    fn pr_review_summary_trims_trailing_punctuation_before_the_ellipsis() {
        assert_eq!(
            pr_review_summary("Fix the flake, then rerun CI", None).as_deref(),
            Some("reviewing the PR for Fix the flake…"),
        );
        assert_eq!(
            pr_review_summary("Read wire path: movement on the extra words", None).as_deref(),
            Some("reviewing the PR for Read wire path…"),
        );
    }

    #[test]
    fn pr_review_summary_treats_punctuated_stopwords_as_dangling() {
        assert_eq!(
            pr_review_summary("Ship feature for, extra words remaining", None).as_deref(),
            Some("reviewing the PR for Ship feature…"),
        );
    }

    #[test]
    fn consolidation_stage_gets_its_own_pane_summary() {
        let summary = |stage| {
            derived_title_summary(&ExecutionKind::PrReview, stage, "Fix flaky test", None)
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            summary(Some(ReviewStage::Consolidation)),
            "consolidating review findings for Fix flaky…"
        );
        assert_eq!(
            summary(Some(ReviewStage::Reviewers)),
            "reviewing the PR for Fix flaky test"
        );
        assert_eq!(summary(None), "reviewing the PR for Fix flaky test");
        assert_eq!(
            pr_review_consolidation_summary("", None).as_deref(),
            Some("consolidating review findings")
        );
    }

    #[test]
    fn answer_agent_summary_is_fixed() {
        assert_eq!(answer_agent_summary().as_deref(), Some("answering a doc comment"));
    }

    #[test]
    fn derived_title_summary_routes_cache_hazard_kinds_to_derived_phrases() {
        assert_eq!(
            derived_title_summary(&ExecutionKind::CiRemediation, None, "Fix flaky test", None)
                .unwrap()
                .as_deref(),
            Some("fixing CI for Fix flaky test"),
        );
        assert_eq!(
            derived_title_summary(&ExecutionKind::ConflictResolution, None, "Fix flaky test", None)
                .unwrap()
                .as_deref(),
            Some("resolving merge conflicts for Fix flaky test"),
        );
        assert_eq!(
            derived_title_summary(&ExecutionKind::PrReview, None, "Fix flaky test", None)
                .unwrap()
                .as_deref(),
            Some("reviewing the PR for Fix flaky test"),
        );
        assert_eq!(
            derived_title_summary(&ExecutionKind::AnswerAgent, None, "Answer comment: anything", None)
                .unwrap()
                .as_deref(),
            Some("answering a doc comment"),
        );
        assert_eq!(
            derived_title_summary(&ExecutionKind::Progression, None, "Fix flaky test", None)
                .unwrap()
                .as_deref(),
            Some("unsticking Fix flaky test"),
        );
        assert_eq!(
            derived_title_summary(&ExecutionKind::Progression, None, "", None)
                .unwrap()
                .as_deref(),
            Some("unsticking work"),
        );
    }

    #[test]
    fn derived_title_summary_falls_through_for_llm_backed_kinds() {
        for kind in [
            ExecutionKind::AutomationTriage,
            ExecutionKind::ChoreImplementation,
            ExecutionKind::InvestigationImplementation,
            ExecutionKind::ProductDesign,
            ExecutionKind::ProjectDesign,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::TaskImplementation,
        ] {
            assert_eq!(
                derived_title_summary(&kind, None, "Fix flaky test", None),
                None,
                "{kind:?} should fall through to get_or_generate",
            );
        }
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

        // No credential, but there IS a cache hit — the cached gerund
        // summary is returned. Cache is checked before the provider is
        // consulted, so a previously-computed gerund still surfaces even when
        // the utility model is unconfigured on this spawn.
        let summary = get_or_generate(&db, &keyless_utility(), &item).await;
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
        let summary = get_or_generate(&db, &keyless_utility(), &item).await;
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
        let summary = get_or_generate(&db, &keyless_utility(), &item).await;
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

        // Drive the real entry point through a provider pointed at the mock,
        // so the endpoint/model/credential all come from the seam rather than
        // from constants this test re-declares.
        let provider =
            boss_engine_utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
                .with_endpoint(format!("{}/v1/messages", server.uri()));
        let call = provider.resolve(UtilityTask::PaneSummary).expect("key was supplied");
        assert_eq!(call.model, UtilityTask::PaneSummary.default_model());

        let summary = claude_short_summary(&call, "name", "desc").await.expect("mock success");
        assert_eq!(summary, "fixing the pane titlebar");
    }

    /// Transient transport-class failure (HTTP 503 stands in for the same
    /// `is_retryable` branch as a connection reset / timeout): first attempt
    /// fails, second succeeds, and the gerund is returned. Without the
    /// spawn-path retry this would permanently cost the pane its title.
    #[tokio::test]
    async fn retries_transient_5xx_then_returns_gerund() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "fixing the fencer scraper"}],
            })))
            .mount(&server)
            .await;

        let provider =
            boss_engine_utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
                .with_endpoint(format!("{}/v1/messages", server.uri()));
        let call = provider.resolve(UtilityTask::PaneSummary).expect("key was supplied");

        let summary = claude_short_summary(&call, "Fix fencer scraper", "desc")
            .await
            .expect("retry should absorb the transient 503");
        assert_eq!(summary, "fixing the fencer scraper");
    }

    /// Auth / other 4xx must fail fast: a 401 followed by a 200 that a retry
    /// would reach proves we surface the auth error without a second attempt
    /// (and without adding spawn latency for a failure that will not heal).
    #[tokio::test]
    async fn does_not_retry_auth_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid x-api-key"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "should not be reached"}],
            })))
            .mount(&server)
            .await;

        let provider =
            boss_engine_utility_model::AnthropicUtilityModel::from_lookup(Some("bad-key".to_owned()), |_| None)
                .with_endpoint(format!("{}/v1/messages", server.uri()));
        let call = provider.resolve(UtilityTask::PaneSummary).expect("key was supplied");

        let err = claude_short_summary(&call, "name", "desc")
            .await
            .expect_err("401 must fail without retrying into the 200");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("401") || msg.contains("invalid x-api-key") || msg.contains("anthropic returned"),
            "expected auth/API error to surface, got: {msg}",
        );
    }

    /// Connection refused is the live-observed failure mode
    /// (`transport error: error sending request for url …`). Pointing at an
    /// unbound port produces that class of error; with `SUMMARY_ATTEMPTS = 2`
    /// the client retries once then surfaces the transport error — never a
    /// success, and never more than the pinned attempt count.
    #[tokio::test]
    async fn transport_error_exhausts_budget_then_surfaces() {
        let provider =
            boss_engine_utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
                .with_endpoint("http://127.0.0.1:1/v1/messages");
        let call = provider.resolve(UtilityTask::PaneSummary).expect("key was supplied");

        let started = std::time::Instant::now();
        let err = claude_short_summary(&call, "name", "desc")
            .await
            .expect_err("unreachable endpoint must fail");
        let elapsed = started.elapsed();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("transport error") || msg.contains("error sending request"),
            "expected transport error, got: {msg}",
        );
        // Two connection-refused attempts + one short backoff should finish
        // well under a second — and far under the 10.25s wall-clock ceiling.
        // A hang or an accidental raise of SUMMARY_TIMEOUT would blow this.
        assert!(
            elapsed < Duration::from_secs(3),
            "transport-error budget should fail fast, took {elapsed:?}",
        );
    }
}
