//! Creation-time (and sweep-shared) comparison-set prefilter for notification
//! dedup.
//!
//! See `tools/boss/docs/designs/notification-dedup-scoring.md` §4 ("Keeping the
//! comparison set tractable"):
//! - Same product only (cross-product dedup is a non-goal).
//! - Open items (`answer_state == "open"`) in non-terminal cards (`state` in
//!   `{"open", "partially_answered"}`) within a recency window.
//! - **Never** partition or filter by `group_id` (card), `association`
//!   (task/project), or `kind`. The motivating near-duplicates are raised by
//!   different tasks in different cards with low lexical overlap, so cross-task,
//!   cross-card items must have a real chance of entering the top-K.
//! - Widened top-K with recency + secondary lexical overlap, and
//!   `same_association` / `same_kind` as mild tiebreakers only.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use boss_protocol::{Attention, AttentionBrief, AttentionGroup, CreateAttentionInput};

use crate::dedup::brief::render_attention_brief;
use crate::work::WorkDb;

/// Default maximum number of open `Attention` briefs forwarded to the dedup
/// LLM in one call (design §4 + R15: `K <= 32`, and `24` leaves headroom for
/// up to `24` `WorkItemBrief`s under the `48`-item combined budget).
pub const DEFAULT_PREFILTER_TOP_K: usize = 24;

/// Default recency window in days for open `Attention` items (design §4).
pub const DEFAULT_RECENCY_WINDOW_DAYS: i64 = 30;

/// Default recency window in seconds (`30` days).
pub const DEFAULT_RECENCY_WINDOW_SECS: i64 = DEFAULT_RECENCY_WINDOW_DAYS * 86_400;

/// Weight of the recency signal in `[0.0, 1.0]`. Keeping recency primary
/// ensures recent cross-task paraphrases with zero lexical overlap still enter
/// the widened top-K even when an older task has accumulated many items.
const RECENCY_WEIGHT: f64 = 1.0;

/// Weight of lexical token overlap in `[0.0, 1.0]`. Secondary to recency and
/// strong enough to pull relevant older cross-task items above unrelated noise,
/// without turning the prefilter into a strict lexical gate.
const LEXICAL_WEIGHT: f64 = 0.6;

/// Mild tiebreaker for matching `kind` (`question` vs `followup`) — never
/// strong enough to block cross-kind matches or outweigh recency/overlap.
const SAME_KIND_TIEBREAKER: f64 = 0.03;

/// Mild tiebreaker for matching `association` (`project_id` or `task_id`) —
/// strictly smaller than meaningful recency or lexical signal so same-task
/// items cannot crowd out cross-task items.
const SAME_ASSOCIATION_TIEBREAKER: f64 = 0.02;

/// Tuning knobs for [`prefilter_open_attentions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefilterConfig {
    /// Maximum number of `AttentionBrief` items returned.
    pub top_k: usize,
    /// Only items created at or after `now_epoch_secs - recency_window_secs`
    /// are eligible.
    pub recency_window_secs: i64,
}

impl Default for PrefilterConfig {
    fn default() -> Self {
        Self {
            top_k: DEFAULT_PREFILTER_TOP_K,
            recency_window_secs: DEFAULT_RECENCY_WINDOW_SECS,
        }
    }
}

/// Candidate descriptor consumed by [`prefilter_open_attentions`]. Supports
/// both unpersisted creation-time [`CreateAttentionInput`]s and persisted
/// [`Attention`] items examined by the startup sweep.
#[derive(Debug, Clone, bon::Builder)]
pub struct PrefilterCandidate<'a> {
    pub product_id: &'a str,
    /// When evaluating an already-persisted item (sweep path), exclude this id
    /// so the item is never compared against itself. `None` at creation time.
    pub exclude_attention_id: Option<&'a str>,
    pub kind: &'a str,
    pub association_project_id: Option<&'a str>,
    pub association_task_id: Option<&'a str>,
    pub source_doc_path: Option<&'a str>,
    pub source_anchor: Option<&'a str>,
    pub prompt_text: Option<&'a str>,
    pub proposed_name: Option<&'a str>,
    pub proposed_description: Option<&'a str>,
    pub rationale: Option<&'a str>,
}

impl<'a> PrefilterCandidate<'a> {
    /// Build a creation-time candidate view from a [`CreateAttentionInput`] and
    /// its owning `product_id`.
    pub fn from_create_input(product_id: &'a str, input: &'a CreateAttentionInput) -> Self {
        Self {
            product_id,
            exclude_attention_id: None,
            kind: &input.kind,
            association_project_id: non_empty(input.association_project_id.as_deref()),
            association_task_id: non_empty(input.association_task_id.as_deref()),
            source_doc_path: non_empty(input.source_doc_path.as_deref()),
            source_anchor: non_empty(input.source_anchor.as_deref()),
            prompt_text: non_empty(input.prompt_text.as_deref()),
            proposed_name: non_empty(input.proposed_name.as_deref()),
            proposed_description: non_empty(input.proposed_description.as_deref()),
            rationale: non_empty(input.rationale.as_deref()),
        }
    }

    /// Build a candidate view from an already-persisted [`Attention`] and its
    /// owning [`AttentionGroup`] (for sweep or re-evaluation paths).
    pub fn from_existing(attention: &'a Attention, group: &'a AttentionGroup) -> Self {
        Self {
            product_id: &group.product_id,
            exclude_attention_id: Some(&attention.id),
            kind: &group.kind,
            association_project_id: non_empty(group.association_project_id.as_deref()),
            association_task_id: non_empty(group.association_task_id.as_deref()),
            source_doc_path: non_empty(group.source_doc_path.as_deref()),
            source_anchor: non_empty(attention.source_anchor.as_deref()),
            prompt_text: non_empty(attention.prompt_text.as_deref()),
            proposed_name: non_empty(attention.proposed_name.as_deref()),
            proposed_description: non_empty(attention.proposed_description.as_deref()),
            rationale: non_empty(attention.rationale.as_deref()),
        }
    }

    fn tokens(&self) -> BTreeSet<String> {
        tokenize_fields(&[
            self.prompt_text,
            self.proposed_name,
            self.proposed_description,
            self.rationale,
            self.source_anchor,
            self.source_doc_path,
        ])
    }
}

/// Output of [`prefilter_open_attentions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefilterResult {
    /// Selected open `Attention` briefs, ranked highest-priority first, capped
    /// at `config.top_k`.
    pub briefs: Vec<AttentionBrief>,
    /// Total number of open same-product items inside the recency window
    /// before applying the top-K cap.
    pub total_eligible: usize,
    /// Number of eligible items omitted by the top-K cap (`0` when all
    /// eligible items fit).
    pub truncated_count: usize,
}

/// Pure comparison-set prefilter over an in-memory slice of
/// `(Attention, AttentionGroup)` pairs.
///
/// Filters to open items in non-terminal groups within the same product and
/// recency window, ranks by recency + secondary lexical overlap with mild
/// `same_kind` / `same_association` tiebreakers, caps at `config.top_k`, and
/// renders each survivor into an [`AttentionBrief`].
pub fn prefilter_open_attentions(
    candidate: &PrefilterCandidate<'_>,
    pool: &[(Attention, AttentionGroup)],
    association_labels: Option<&HashMap<String, String>>,
    config: &PrefilterConfig,
    now_epoch_secs: i64,
) -> PrefilterResult {
    let window_secs = config.recency_window_secs.max(0);
    let cutoff = now_epoch_secs.saturating_sub(window_secs);
    let candidate_tokens = candidate.tokens();

    let mut scored: Vec<ScoredItem<'_>> = Vec::new();
    for (attention, group) in pool {
        if group.product_id != candidate.product_id {
            continue;
        }
        if attention.answer_state != "open" {
            continue;
        }
        if !matches!(group.state.as_str(), "open" | "partially_answered") {
            continue;
        }
        if candidate.exclude_attention_id == Some(attention.id.as_str()) {
            continue;
        }
        let Some(created_secs) = parse_timestamp_secs(&attention.created_at) else {
            tracing::warn!(
                attention_id = %attention.id,
                created_at = %attention.created_at,
                "dedup prefilter: skipping attention with unparseable created_at",
            );
            continue;
        };
        if created_secs < cutoff {
            continue;
        }

        let recency_score = if window_secs == 0 {
            1.0
        } else {
            let age = (now_epoch_secs - created_secs).max(0) as f64;
            (1.0 - (age / window_secs as f64)).clamp(0.0, 1.0)
        };
        let item_tokens = item_semantic_tokens(attention, group);
        let lexical_score = dice_similarity(&candidate_tokens, &item_tokens);
        let same_kind = if group.kind == candidate.kind {
            SAME_KIND_TIEBREAKER
        } else {
            0.0
        };
        let same_assoc = if has_same_association(candidate, group) {
            SAME_ASSOCIATION_TIEBREAKER
        } else {
            0.0
        };
        let rank_score = recency_score * RECENCY_WEIGHT + lexical_score * LEXICAL_WEIGHT + same_kind + same_assoc;

        scored.push(ScoredItem {
            attention,
            group,
            created_secs,
            rank_score,
        });
    }

    scored.sort_by(|a, b| {
        b.rank_score
            .total_cmp(&a.rank_score)
            .then_with(|| b.created_secs.cmp(&a.created_secs))
            .then_with(|| a.attention.id.cmp(&b.attention.id))
    });

    let total_eligible = scored.len();
    let keep = total_eligible.min(config.top_k);
    let truncated_count = total_eligible.saturating_sub(keep);

    let briefs = scored
        .into_iter()
        .take(keep)
        .map(|entry| {
            let assoc_label = resolve_label(entry.group, association_labels);
            render_attention_brief(entry.attention, entry.group, assoc_label)
        })
        .collect();

    PrefilterResult {
        briefs,
        total_eligible,
        truncated_count,
    }
}

/// Fetch open attentions for `candidate.product_id` from [`WorkDb`] and run
/// [`prefilter_open_attentions`].
pub fn prefilter_open_attentions_from_db(
    work_db: &WorkDb,
    candidate: &PrefilterCandidate<'_>,
    association_labels: Option<&HashMap<String, String>>,
    config: &PrefilterConfig,
    now_epoch_secs: i64,
) -> Result<PrefilterResult> {
    let pool = work_db.list_open_attentions_for_product(candidate.product_id)?;
    Ok(prefilter_open_attentions(
        candidate,
        &pool,
        association_labels,
        config,
        now_epoch_secs,
    ))
}

struct ScoredItem<'a> {
    attention: &'a Attention,
    group: &'a AttentionGroup,
    created_secs: i64,
    rank_score: f64,
}

fn resolve_label<'a>(
    group: &AttentionGroup,
    association_labels: Option<&'a HashMap<String, String>>,
) -> Option<&'a str> {
    let labels = association_labels?;
    if let Some(task_id) = non_empty(group.association_task_id.as_deref())
        && let Some(label) = labels.get(task_id)
    {
        return Some(label.as_str());
    }
    if let Some(project_id) = non_empty(group.association_project_id.as_deref())
        && let Some(label) = labels.get(project_id)
    {
        return Some(label.as_str());
    }
    None
}

fn has_same_association(candidate: &PrefilterCandidate<'_>, group: &AttentionGroup) -> bool {
    let same_task = candidate.association_task_id.is_some()
        && candidate.association_task_id == non_empty(group.association_task_id.as_deref());
    let same_project = candidate.association_project_id.is_some()
        && candidate.association_project_id == non_empty(group.association_project_id.as_deref());
    same_task || same_project
}

fn item_semantic_tokens(attention: &Attention, group: &AttentionGroup) -> BTreeSet<String> {
    tokenize_fields(&[
        non_empty(attention.prompt_text.as_deref()),
        non_empty(attention.proposed_name.as_deref()),
        non_empty(attention.proposed_description.as_deref()),
        non_empty(attention.rationale.as_deref()),
        non_empty(attention.source_anchor.as_deref()),
        non_empty(group.source_doc_path.as_deref()),
    ])
}

fn tokenize_fields(fields: &[Option<&str>]) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    for field in fields.iter().flatten() {
        for raw in field.split(|c: char| !c.is_alphanumeric()) {
            let lower = raw.to_ascii_lowercase();
            if lower.len() >= 2 && !is_stopword(&lower) {
                tokens.insert(lower);
            }
        }
    }
    tokens
}

fn dice_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f64;
    if shared == 0.0 {
        0.0
    } else {
        (2.0 * shared) / ((a.len() + b.len()) as f64)
    }
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "an" | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "has"
            | "have"
            | "in"
            | "into"
            | "is"
            | "it"
            | "its"
            | "not"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "their"
            | "this"
            | "to"
            | "was"
            | "were"
            | "will"
            | "with"
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Parse an engine timestamp (`now_string()` epoch seconds, or RFC 3339).
fn parse_timestamp_secs(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<i64>() {
        return Some(secs);
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore, create_test_product_named, open_db};

    const NOW: i64 = 1_700_000_000;

    fn make_group(
        id: &str,
        product_id: &str,
        kind: &str,
        state: &str,
        task_id: Option<&str>,
        project_id: Option<&str>,
    ) -> AttentionGroup {
        AttentionGroup::builder()
            .id(id)
            .product_id(product_id)
            .created_at("1699900000")
            .grouping_key(format!("{kind}|{id}"))
            .kind(kind)
            .source_kind("task_transcript")
            .state(state)
            .maybe_association_task_id(task_id.map(str::to_owned))
            .maybe_association_project_id(project_id.map(str::to_owned))
            .build()
    }

    fn make_followup(id: &str, group_id: &str, answer_state: &str, created_at: &str, name: &str) -> Attention {
        Attention::builder()
            .id(id)
            .group_id(group_id)
            .ordinal(1)
            .answer_state(answer_state)
            .created_at(created_at)
            .proposed_name(name)
            .proposed_description(format!("Description for {name}"))
            .build()
    }

    fn make_question(id: &str, group_id: &str, created_at: &str, prompt: &str) -> Attention {
        Attention::builder()
            .id(id)
            .group_id(group_id)
            .ordinal(1)
            .answer_state("open")
            .created_at(created_at)
            .question_type("prompt")
            .prompt_text(prompt)
            .build()
    }

    #[test]
    fn empty_pool_returns_empty_result() {
        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_1")
            .proposed_name("Add index")
            .build();
        let cand = PrefilterCandidate::from_create_input("prod_1", &input);
        let res = prefilter_open_attentions(&cand, &[], None, &PrefilterConfig::default(), NOW);
        assert!(res.briefs.is_empty());
        assert_eq!(res.total_eligible, 0);
        assert_eq!(res.truncated_count, 0);
    }

    #[test]
    fn excludes_other_products_terminal_states_self_and_expired_items() {
        let g_open = make_group("atg_open", "prod_1", "followup", "open", Some("task_1"), None);
        let g_partial = make_group(
            "atg_partial",
            "prod_1",
            "followup",
            "partially_answered",
            Some("task_2"),
            None,
        );
        let g_dismissed = make_group("atg_dis", "prod_1", "followup", "dismissed", Some("task_3"), None);
        let g_actioned = make_group("atg_act", "prod_1", "followup", "actioned", Some("task_4"), None);
        let g_other_prod = make_group("atg_p2", "prod_2", "followup", "open", Some("task_5"), None);

        let recent = (NOW - 3600).to_string();
        let exact_boundary = (NOW - DEFAULT_RECENCY_WINDOW_SECS).to_string();
        let too_old = (NOW - DEFAULT_RECENCY_WINDOW_SECS - 1).to_string();

        let pool = vec![
            (
                make_followup("atn_keep_1", "atg_open", "open", &recent, "Keep recent"),
                g_open.clone(),
            ),
            (
                make_followup(
                    "atn_keep_boundary",
                    "atg_partial",
                    "open",
                    &exact_boundary,
                    "Boundary item",
                ),
                g_partial.clone(),
            ),
            (
                make_followup("atn_self", "atg_open", "open", &recent, "Self item"),
                g_open.clone(),
            ),
            (
                make_followup("atn_answered", "atg_partial", "answered", &recent, "Answered"),
                g_partial.clone(),
            ),
            (
                make_followup("atn_merged", "atg_open", "merged", &recent, "Merged"),
                g_open.clone(),
            ),
            (
                make_followup("atn_dismissed_grp", "atg_dis", "open", &recent, "Dismissed group"),
                g_dismissed,
            ),
            (
                make_followup("atn_actioned_grp", "atg_act", "open", &recent, "Actioned group"),
                g_actioned,
            ),
            (
                make_followup("atn_other_prod", "atg_p2", "open", &recent, "Other product"),
                g_other_prod,
            ),
            (
                make_followup("atn_too_old", "atg_open", "open", &too_old, "Stale item"),
                g_open,
            ),
        ];

        let self_item = pool[2].0.clone();
        let self_group = pool[2].1.clone();
        let cand = PrefilterCandidate::from_existing(&self_item, &self_group);

        let res = prefilter_open_attentions(&cand, &pool, None, &PrefilterConfig::default(), NOW);
        let ids: Vec<&str> = res.briefs.iter().map(|b| b.attention_id.as_str()).collect();
        assert_eq!(ids, vec!["atn_keep_1", "atn_keep_boundary"]);
        assert_eq!(res.total_eligible, 2);
        assert_eq!(res.truncated_count, 0);
    }

    #[test]
    fn cross_task_cross_card_zero_lexical_overlap_enters_top_k_ahead_of_older_same_task_items() {
        let same_task_group = make_group("atg_same", "prod_1", "followup", "open", Some("task_same"), None);
        let cross_task_group = make_group("atg_cross", "prod_1", "followup", "open", Some("task_other"), None);

        let mut pool = Vec::new();
        // Fill > K older items all in the candidate's own task/card.
        for i in 0..30 {
            let ts = (NOW - 10 * 86_400 - i * 60).to_string();
            pool.push((
                make_followup(
                    &format!("atn_old_same_{i:02}"),
                    "atg_same",
                    "open",
                    &ts,
                    &format!("Unrelated older work {i}"),
                ),
                same_task_group.clone(),
            ));
        }

        // Add a recent cross-task, cross-card paraphrase with ZERO shared words.
        let cross_ts = (NOW - 300).to_string();
        pool.push((
            make_followup(
                "atn_cross_paraphrase",
                "atg_cross",
                "open",
                &cross_ts,
                "Speed up startup query scan",
            ),
            cross_task_group,
        ));

        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_same")
            .proposed_name("Add index on created_at")
            .proposed_description("Migration lacks btree coverage")
            .build();
        let cand = PrefilterCandidate::from_create_input("prod_1", &input);

        let config = PrefilterConfig {
            top_k: 24,
            recency_window_secs: DEFAULT_RECENCY_WINDOW_SECS,
        };
        let res = prefilter_open_attentions(&cand, &pool, None, &config, NOW);
        assert_eq!(res.total_eligible, 31);
        assert_eq!(res.briefs.len(), 24);
        assert_eq!(res.truncated_count, 7);
        assert_eq!(
            res.briefs[0].attention_id, "atn_cross_paraphrase",
            "recent cross-task item must enter top-K ahead of older same-task items"
        );
    }

    #[test]
    fn cross_task_lexical_match_outranks_same_task_unrelated_item_at_same_age() {
        let same_group = make_group("atg_same", "prod_1", "followup", "open", Some("task_a"), None);
        let cross_group = make_group("atg_other", "prod_1", "question", "open", None, Some("proj_b"));

        let ts = (NOW - 7_200).to_string();
        let pool = vec![
            (
                make_followup("atn_same_unrelated", "atg_same", "open", &ts, "Refactor log formatting"),
                same_group,
            ),
            (
                make_question(
                    "atn_cross_match",
                    "atg_other",
                    &ts,
                    "Should we add an index on created_at for attentions?",
                ),
                cross_group,
            ),
        ];

        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_a")
            .proposed_name("Add index on created_at")
            .proposed_description("Full scan on attentions table")
            .build();
        let cand = PrefilterCandidate::from_create_input("prod_1", &input);

        let res = prefilter_open_attentions(&cand, &pool, None, &PrefilterConfig::default(), NOW);
        let ids: Vec<&str> = res.briefs.iter().map(|b| b.attention_id.as_str()).collect();
        assert_eq!(ids, vec!["atn_cross_match", "atn_same_unrelated"]);
    }

    #[test]
    fn same_association_acts_as_mild_tiebreaker_when_recency_and_lexical_match() {
        let same_group = make_group("atg_same", "prod_1", "followup", "open", Some("task_a"), None);
        let other_group = make_group("atg_other", "prod_1", "followup", "open", Some("task_b"), None);

        let ts = (NOW - 1_000).to_string();
        let pool = vec![
            (
                make_followup("atn_a_other", "atg_other", "open", &ts, "Completely different topic"),
                other_group,
            ),
            (
                make_followup("atn_z_same", "atg_same", "open", &ts, "Another unrelated item"),
                same_group,
            ),
        ];

        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_a")
            .proposed_name("Index migration")
            .build();
        let cand = PrefilterCandidate::from_create_input("prod_1", &input);

        let res = prefilter_open_attentions(&cand, &pool, None, &PrefilterConfig::default(), NOW);
        let ids: Vec<&str> = res.briefs.iter().map(|b| b.attention_id.as_str()).collect();
        assert_eq!(ids, vec!["atn_z_same", "atn_a_other"]);
    }

    #[test]
    fn supports_rfc3339_timestamps_and_association_labels() {
        let group = make_group("atg_1", "prod_1", "followup", "open", Some("task_7"), None);
        let rfc3339 = chrono::DateTime::from_timestamp(NOW - 60, 0).unwrap().to_rfc3339();
        let pool = vec![(
            make_followup("atn_1", "atg_1", "open", &rfc3339, "Add retry jitter"),
            group,
        )];

        let mut labels = HashMap::new();
        labels.insert("task_7".to_owned(), "T7: Retry client".to_owned());

        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_other")
            .proposed_name("Retry jitter")
            .build();
        let cand = PrefilterCandidate::from_create_input("prod_1", &input);

        let res = prefilter_open_attentions(&cand, &pool, Some(&labels), &PrefilterConfig::default(), NOW);
        assert_eq!(res.briefs.len(), 1);
        assert_eq!(res.briefs[0].association, "T7: Retry client");
    }

    #[test]
    fn db_prefilter_shares_path_for_creation_and_sweep() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "dedup-prefilter");
        let chore_a = create_test_chore(&db, product.id.clone(), "Chore A");
        let chore_b = create_test_chore(&db, product.id.clone(), "Chore B");

        let (a1, g1) = db
            .create_attention(
                CreateAttentionInput::builder()
                    .kind("followup")
                    .association_task_id(&chore_a.id)
                    .source_kind("task_transcript")
                    .source_task_id(&chore_a.id)
                    .proposed_name("Add created_at index")
                    .proposed_description("Avoid full scans")
                    .build(),
            )
            .unwrap();
        let (a2, _) = db
            .create_attention(
                CreateAttentionInput::builder()
                    .kind("followup")
                    .association_task_id(&chore_b.id)
                    .source_kind("task_transcript")
                    .source_task_id(&chore_b.id)
                    .proposed_name("Speed up attention lookup")
                    .proposed_description("Index created_at")
                    .build(),
            )
            .unwrap();

        let now = crate::work::now_string().parse::<i64>().unwrap() + 5;
        // Creation-time candidate sees both open items across chore A and chore B.
        let new_cand_input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id(&chore_b.id)
            .proposed_name("Missing index on attentions")
            .build();
        let create_res = prefilter_open_attentions_from_db(
            &db,
            &PrefilterCandidate::from_create_input(&product.id, &new_cand_input),
            None,
            &PrefilterConfig::default(),
            now,
        )
        .unwrap();
        assert_eq!(create_res.briefs.len(), 2);

        // Sweep-path candidate excludes itself and keeps the peer in the other card.
        let sweep_res = prefilter_open_attentions_from_db(
            &db,
            &PrefilterCandidate::from_existing(&a1, &g1),
            None,
            &PrefilterConfig::default(),
            now,
        )
        .unwrap();
        let sweep_ids: Vec<&str> = sweep_res.briefs.iter().map(|b| b.attention_id.as_str()).collect();
        assert_eq!(sweep_ids, vec![a2.id.as_str()]);
    }
}
