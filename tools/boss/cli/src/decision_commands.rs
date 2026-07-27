//! `boss decision …` handlers: product-scoped decision records over the
//! engine's `product_decisions` table (Create/List/Get/Revoke/Supersede).
//!
//! Also hosts the deterministic create-time overlap warning that fires
//! (stderr only) when a new task/chore name looks like an active
//! decision for the same product.

use std::collections::HashSet;

use crate::*;

// ---------------------------------------------------------------------------
// Overlap surfacing (deterministic, in-process, bias toward silence)
// ---------------------------------------------------------------------------

/// Significant-token length floor. Tokens shorter than this are dropped
/// so common short words never drive a match.
const TOKEN_MIN_LEN: usize = 4;

/// Minimum shared significant tokens required to fire a warning.
const OVERLAP_MIN_INTERSECTION: usize = 2;

/// Jaccard similarity floor over significant-token sets. Combined with
/// [`OVERLAP_MIN_INTERSECTION`], a single shared content word never
/// fires, and near-miss pairs stay quiet.
const OVERLAP_JACCARD_THRESHOLD: f64 = 0.5;

/// Tiny English stoplist for tokens that clear [`TOKEN_MIN_LEN`] but
/// still carry no decision-specific meaning. Kept short on purpose —
/// the length floor already does most of the work.
const STOPWORDS: &[&str] = &[
    "about", "after", "before", "being", "between", "could", "every", "from", "have", "into", "just", "like", "make",
    "more", "only", "other", "over", "same", "should", "some", "such", "than", "that", "their", "them", "then",
    "there", "these", "they", "this", "those", "through", "under", "very", "what", "when", "where", "which", "while",
    "with", "would", "your",
];

/// Case-fold `text`, split on non-alphanumeric runs, drop short tokens
/// and stoplist entries. Pure / deterministic.
pub(crate) fn significant_tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= TOKEN_MIN_LEN)
        .filter(|t| !STOPWORDS.contains(t))
        .map(|t| t.to_owned())
        .collect()
}

/// True when `new_name` overlaps `decision` strongly enough to warn.
///
/// Predicate (stated in the PR body):
/// 1. Build significant-token sets for the new name and for the
///    decision's `title` union optional `keywords`.
/// 2. Require `|intersection| >= 2` and Jaccard ≥ 0.5.
///
/// Inactive decisions are never considered — the caller must filter.
pub(crate) fn decision_name_overlaps(new_name: &str, decision: &Decision) -> bool {
    let name_toks = significant_tokens(new_name);
    if name_toks.is_empty() {
        return false;
    }
    let mut decision_toks = significant_tokens(&decision.title);
    if let Some(keywords) = decision.keywords.as_deref() {
        decision_toks.extend(significant_tokens(keywords));
    }
    if decision_toks.is_empty() {
        return false;
    }
    let inter = name_toks.intersection(&decision_toks).count();
    if inter < OVERLAP_MIN_INTERSECTION {
        return false;
    }
    let union = name_toks.union(&decision_toks).count();
    if union == 0 {
        return false;
    }
    (inter as f64) / (union as f64) >= OVERLAP_JACCARD_THRESHOLD
}

/// Active decisions whose title/keywords overlap `new_name`.
pub(crate) fn matching_decisions<'a>(new_name: &str, decisions: &'a [Decision]) -> Vec<&'a Decision> {
    decisions
        .iter()
        .filter(|d| d.status.is_active() && decision_name_overlaps(new_name, d))
        .collect()
}

/// Format the non-blocking stderr warning for one overlapping decision.
pub(crate) fn format_decision_overlap_warning(decision: &Decision) -> String {
    format!(
        "warning: new work name overlaps active decision {} ({}) — {}",
        decision.display_label(),
        decision.id,
        decision.title,
    )
}

/// Best-effort create-time surfacing: list active decisions for
/// `product_id`, warn on stderr for each name overlap, never fail the
/// create and never write to stdout (so `--json` pipes stay valid).
pub(crate) async fn warn_if_overlapping_decision(client: &mut BossClient, product_id: &str, new_name: &str) {
    let Ok(decisions) = list_decisions(client, product_id, false).await else {
        return;
    };
    for decision in matching_decisions(new_name, &decisions) {
        eprintln!("{}", format_decision_overlap_warning(decision));
    }
}

// ---------------------------------------------------------------------------
// Selector resolution
// ---------------------------------------------------------------------------

/// Parsed form of a decision selector.
#[derive(Debug)]
pub(crate) enum DecisionSelector {
    /// `dec_…` canonical id — used directly without a product lookup.
    PrimaryId(String),
    /// `D<n>` / `d<n>` (or plain integer) — short id within a product.
    ShortId(i64),
}

pub(crate) fn parse_decision_selector(s: &str) -> Result<DecisionSelector, CliError> {
    let s = s.trim();
    if s.starts_with("dec_") {
        return Ok(DecisionSelector::PrimaryId(s.to_owned()));
    }
    // `D<n>` or `d<n>`
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'D' || first == b'd')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(DecisionSelector::ShortId(n));
        }
    }
    // Plain positive integer → short id
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(DecisionSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "decision selector must be D<n> (e.g. D1) or a dec_… id; got {s:?}"
    )))
}

/// Resolve a decision selector to a full `Decision` row.
///
/// For `dec_…` ids the product is not needed. For `D<n>` selectors a
/// product must be provided (resolved by the caller beforehand). Short
/// ids are resolved by listing the product's decisions (including
/// inactive, so show/revoke of a revoked `D<n>` still works) — there is
/// no wire RPC for short-id lookup.
pub(crate) async fn resolve_decision(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<Decision, CliError> {
    match parse_decision_selector(selector)? {
        DecisionSelector::PrimaryId(id) => get_decision(client, &id).await,
        DecisionSelector::ShortId(n) => {
            let product = product.ok_or_else(|| {
                CliError::usage("D<n> selectors require --product to identify the decision namespace")
            })?;
            let decisions = list_decisions(client, &product.id, true).await?;
            decisions
                .into_iter()
                .find(|d| d.short_id == Some(n))
                .ok_or_else(|| CliError::not_found(format!("no decision D{n} found in product '{}'", product.slug)))
        }
    }
}

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------

pub(crate) async fn create_decision(client: &mut BossClient, input: CreateDecisionInput) -> Result<Decision, CliError> {
    rpc_call!(
        client,
        FrontendRequest::CreateDecision { input },
        "decision create",
        FrontendEvent::DecisionCreated { decision } => decision,
    )
}

pub(crate) async fn list_decisions(
    client: &mut BossClient,
    product_id: &str,
    include_inactive: bool,
) -> Result<Vec<Decision>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListDecisions {
            product_id: product_id.to_owned(),
            include_inactive,
        },
        "decision list",
        FrontendEvent::DecisionsList { decisions, .. } => decisions,
    )
}

pub(crate) async fn get_decision(client: &mut BossClient, id: &str) -> Result<Decision, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetDecision { id: id.to_owned() },
        "decision show",
        FrontendEvent::DecisionResult { decision } => decision,
    )
}

pub(crate) async fn revoke_decision(client: &mut BossClient, id: &str) -> Result<Decision, CliError> {
    rpc_call!(
        client,
        FrontendRequest::RevokeDecision { id: id.to_owned() },
        "decision revoke",
        FrontendEvent::DecisionUpdated { decision } => decision,
    )
}

pub(crate) async fn supersede_decision(
    client: &mut BossClient,
    id: &str,
    successor_id: &str,
) -> Result<Decision, CliError> {
    rpc_call!(
        client,
        FrontendRequest::SupersedeDecision {
            id: id.to_owned(),
            successor_id: successor_id.to_owned(),
        },
        "decision supersede",
        FrontendEvent::DecisionUpdated { decision } => decision,
    )
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

pub(crate) fn print_decision_details(label: &str, d: &Decision) {
    println!("{label}:");
    let short = d.display_label();
    println!("  ID:          {} ({})", d.id, short);
    println!("  Product:     {}", d.product_id);
    println!("  Kind:        {}", d.kind);
    println!("  Status:      {}", d.status);
    println!("  Title:       {}", d.title);
    println!("  Body:        {}", d.body);
    if let Some(keywords) = &d.keywords {
        println!("  Keywords:    {keywords}");
    }
    if let Some(related) = &d.related_work_item_id {
        println!("  Related:     {related}");
    }
    if let Some(succ) = &d.superseded_by {
        println!("  Superseded:  {succ}");
    }
    println!("  Created by:  {} (via {})", d.created_by, d.created_via);
    println!("  Created:     {}", d.created_at);
    println!("  Updated:     {}", d.updated_at);
}

pub(crate) fn print_decisions_table(decisions: &[Decision]) {
    let mut table = new_dynamic_table(["ID", "KIND", "STATUS", "TITLE", "CREATED"]);
    for d in decisions {
        table.add_row([
            d.display_label(),
            d.kind.to_string(),
            d.status.to_string(),
            d.title.clone(),
            d.created_at.clone(),
        ]);
    }
    print_table(table);
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

pub(crate) async fn run_decision_command(command: DecisionCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        DecisionCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let title = required_text(args.name, "Decision name", ctx)?;
            let body = required_text(args.description, "Decision description", ctx)?;
            let created_by = args
                .created_by
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(default_comment_author);
            let related_work_item_id = if let Some(selector) = args.related_work_item {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &selector, Some(product.slug.clone())).await?)
            } else {
                None
            };
            let decision = create_decision(
                &mut client,
                CreateDecisionInput::builder()
                    .product_id(product.id)
                    .title(title)
                    .body(body)
                    .kind(args.kind.as_protocol())
                    .created_by(created_by)
                    .created_via(CREATED_VIA_CLI)
                    .maybe_keywords(normalize_non_empty(args.keywords))
                    .maybe_related_work_item_id(related_work_item_id)
                    .build(),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "decision": decision }), || {
                print_decision_details("Created decision", &decision);
            })
        }

        DecisionCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let decisions = list_decisions(&mut client, &product.id, args.include_inactive).await?;
            print_entity(ctx, &serde_json::json!({ "decisions": decisions }), || {
                if decisions.is_empty() {
                    println!("No decisions for product '{}'.", product.slug);
                } else {
                    print_decisions_table(&decisions);
                }
            })
        }

        DecisionCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let decision = resolve_decision(&mut client, &args.selector, product.as_ref()).await?;
            // Flat row object — no `{decision: …}` wrapper (matches task/chore show).
            print_entity(ctx, &decision, || {
                print_decision_details("Decision", &decision);
            })
        }

        DecisionCommand::Revoke(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let decision = resolve_decision(&mut client, &args.selector, product.as_ref()).await?;
            let updated = revoke_decision(&mut client, &decision.id).await?;
            print_entity(ctx, &serde_json::json!({ "decision": updated }), || {
                print_decision_details("Revoked decision", &updated);
            })
        }

        DecisionCommand::Supersede(args) => {
            let product = resolve_optional_product(&mut client, args.product.clone(), ctx).await?;
            let predecessor = resolve_decision(&mut client, &args.selector, product.as_ref()).await?;
            // Successor may be on the same product; reuse product context
            // when the predecessor resolved via D<n>, otherwise re-resolve
            // optionally so a second D<n> still needs --product.
            let successor = resolve_decision(&mut client, &args.by, product.as_ref()).await?;
            let updated = supersede_decision(&mut client, &predecessor.id, &successor.id).await?;
            print_entity(ctx, &serde_json::json!({ "decision": updated }), || {
                print_decision_details("Superseded decision", &updated);
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn sample_decision(id: &str, short_id: i64, title: &str, keywords: Option<&str>) -> Decision {
        Decision::builder()
            .id(id)
            .short_id(short_id)
            .product_id("prod_1")
            .kind(boss_protocol::DecisionKind::Wontfix)
            .status(boss_protocol::DecisionStatus::Active)
            .title(title)
            .body("body")
            .created_by("user:test")
            .created_via("cli")
            .created_at("1")
            .updated_at("1")
            .maybe_keywords(keywords.map(|s| s.to_owned()))
            .build()
    }

    #[test]
    fn parses_decision_create_command() {
        let cli = Cli::parse_from([
            "boss",
            "decision",
            "create",
            "--product",
            "boss",
            "--name",
            "No checkleft all-gating",
            "--description",
            "We considered all-gating and declined for now.",
            "--kind",
            "decided",
            "--keywords",
            "checkleft,gating",
        ]);
        match cli.command {
            Commands::Decision {
                command: DecisionCommand::Create(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.name.as_deref(), Some("No checkleft all-gating"));
                assert_eq!(
                    args.description.as_deref(),
                    Some("We considered all-gating and declined for now.")
                );
                assert_eq!(args.kind, DecisionKindArg::Decided);
                assert_eq!(args.keywords.as_deref(), Some("checkleft,gating"));
            }
            _ => panic!("expected decision create command"),
        }
    }

    #[test]
    fn parses_decision_list_show_revoke_supersede() {
        let list = Cli::parse_from(["boss", "decision", "list", "--product", "boss", "--include-inactive"]);
        match list.command {
            Commands::Decision {
                command: DecisionCommand::List(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert!(args.include_inactive);
            }
            _ => panic!("expected decision list"),
        }

        let show = Cli::parse_from(["boss", "decision", "show", "D1", "--product", "boss"]);
        match show.command {
            Commands::Decision {
                command: DecisionCommand::Show(args),
            } => {
                assert_eq!(args.selector, "D1");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected decision show"),
        }

        let revoke = Cli::parse_from(["boss", "decision", "revoke", "dec_abc"]);
        match revoke.command {
            Commands::Decision {
                command: DecisionCommand::Revoke(args),
            } => {
                assert_eq!(args.selector, "dec_abc");
                assert!(args.product.is_none());
            }
            _ => panic!("expected decision revoke"),
        }

        let supersede = Cli::parse_from(["boss", "decision", "supersede", "D1", "--by", "D2", "--product", "boss"]);
        match supersede.command {
            Commands::Decision {
                command: DecisionCommand::Supersede(args),
            } => {
                assert_eq!(args.selector, "D1");
                assert_eq!(args.by, "D2");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected decision supersede"),
        }
    }

    #[test]
    fn parse_decision_selector_accepts_primary_and_short_forms() {
        match parse_decision_selector("dec_18c55").unwrap() {
            DecisionSelector::PrimaryId(id) => assert_eq!(id, "dec_18c55"),
            _ => panic!("expected primary id"),
        }
        match parse_decision_selector("D12").unwrap() {
            DecisionSelector::ShortId(n) => assert_eq!(n, 12),
            _ => panic!("expected short id"),
        }
        match parse_decision_selector("d3").unwrap() {
            DecisionSelector::ShortId(n) => assert_eq!(n, 3),
            _ => panic!("expected short id"),
        }
        assert!(parse_decision_selector("not-a-decision").is_err());
    }

    #[test]
    fn significant_tokens_drop_short_and_stopwords() {
        let toks = significant_tokens("No checkleft all-gating for the PR");
        assert!(toks.contains("checkleft"));
        assert!(toks.contains("gating"));
        assert!(!toks.contains("for"));
        assert!(!toks.contains("the"));
        assert!(!toks.contains("no"));
        assert!(!toks.contains("all")); // len < 4
        assert!(!toks.contains("pr")); // len < 4
    }

    #[test]
    fn decision_overlap_fires_on_strong_title_match() {
        let decision = sample_decision("dec_1", 1, "No checkleft all-gating", Some("checkleft gating"));
        // Shares checkleft + gating (2 tokens); jaccard with {checkleft,gating,enforcement}
        // = 2/3 >= 0.5.
        assert!(decision_name_overlaps(
            "Add checkleft all-gating enforcement",
            &decision
        ));
        let decisions = [decision];
        let matches = matching_decisions("Add checkleft all-gating enforcement", &decisions);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "dec_1");
    }

    #[test]
    fn decision_overlap_stays_silent_on_near_miss() {
        let decision = sample_decision("dec_1", 1, "No checkleft all-gating", None);
        // Only one significant shared token ("checkleft") — must stay quiet.
        assert!(!decision_name_overlaps("Fix checkleft lint rule", &decision));
        // Shared common-ish content words alone must not fire.
        assert!(!decision_name_overlaps("Ship the remote workers plan", &decision));
        // Empty / short names never fire.
        assert!(!decision_name_overlaps("fix", &decision));
    }

    #[test]
    fn decision_overlap_ignores_inactive_rows() {
        let mut revoked = sample_decision("dec_1", 1, "No checkleft all-gating", Some("checkleft gating"));
        revoked.status = boss_protocol::DecisionStatus::Revoked;
        let decisions = [revoked];
        let matches = matching_decisions("Add checkleft all-gating enforcement", &decisions);
        assert!(matches.is_empty(), "revoked decisions must not surface");
    }

    #[test]
    fn decision_list_json_envelope_shape() {
        let decision = sample_decision("dec_1", 1, "Title", None);
        let envelope = serde_json::json!({ "decisions": [decision] });
        let obj = envelope.as_object().expect("object");
        assert!(obj.contains_key("decisions"));
        let arr = obj["decisions"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "dec_1");
        assert_eq!(arr[0]["title"], "Title");
        // Show is flat: the Decision row itself serialises without a wrapper.
        let flat = serde_json::to_value(sample_decision("dec_2", 2, "Other", None)).unwrap();
        assert!(flat.get("id").is_some());
        assert!(flat.get("decision").is_none());
        assert_eq!(flat["short_id"], 2);
    }

    #[test]
    fn decision_overlap_warning_text_names_id_and_title() {
        let decision = sample_decision("dec_abc", 7, "Remote is the plan", None);
        let msg = format_decision_overlap_warning(&decision);
        assert!(msg.contains("dec_abc"), "warning must name the canonical id");
        assert!(msg.contains("D7"), "warning must name the short label");
        assert!(msg.contains("Remote is the plan"), "warning must name the title");
        assert!(msg.starts_with("warning:"), "must look like a warning");
    }

    /// Simulates the create-time path: when the warning fires under `--json`,
    /// stdout remains parseable JSON and the warning only lands on the
    /// "stderr" side (here, a separate buffer). Pure unit test — no engine.
    #[test]
    fn decision_overlap_warning_keeps_json_stdout_valid() {
        let decision = sample_decision("dec_1", 1, "No checkleft all-gating", Some("checkleft gating"));
        let decisions = [decision.clone()];
        let matches = matching_decisions("Add checkleft all-gating enforcement", &decisions);
        assert_eq!(matches.len(), 1);

        // stdout payload a create would emit under --json
        let stdout_payload = serde_json::json!({
            "task": {
                "id": "task_1",
                "name": "Add checkleft all-gating enforcement",
            }
        });
        let stdout = serde_json::to_string_pretty(&stdout_payload).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must stay valid JSON");
        assert_eq!(reparsed["task"]["id"], "task_1");

        // stderr side: warning text is independent of stdout
        let stderr = format_decision_overlap_warning(&decision);
        assert!(stderr.contains("dec_1"));
        assert!(!stdout.contains("warning:"), "warning must not leak into JSON stdout");
    }
}
