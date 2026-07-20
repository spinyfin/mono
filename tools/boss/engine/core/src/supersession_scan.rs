//! Deterministic scan for supersession / obsolescence language in
//! worker-authored PR text (PR body, commit messages, PR comments) —
//! incident-002 remediation P3.
//!
//! Incident 002 (`tools/boss/docs/postmortems/incident-002-merge-conflict-\
//! deletion-blessed-by-review.md`): a merge-conflict resolution deleted a
//! merged feature and framed the removal as "supersedes T16's static badge",
//! "now-dead", "orphaned". The claim had no basis in the design doc — the
//! design's §Surfacing specified the two surfaces as complementary siblings.
//!
//! "Supersedes / obsoletes" is a claim **about design**, and claims about
//! design require citations. This module is the deterministic,
//! rationale-independent half of P3: it flags the *presence* of supersession
//! language so the reviewer can be **required** to verify a design-doc
//! citation, rather than relying on the reviewer to happen to notice the
//! narrative. It never decides whether a claim is true — it surfaces claims
//! for verification.
//!
//! Deliberately scoped to the worker's *narrative* surfaces (PR body, commit
//! messages, comments) and NOT the code diff: "replace" is ubiquitous in
//! source, and the failure mode is a rationalising narrative, not code.

/// A single supersession-language hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersessionHit {
    /// The canonical trigger term that matched (lowercased).
    pub term: String,
    /// A short surrounding snippet (lowercased) for reviewer orientation.
    pub snippet: String,
}

/// The supersession / obsolescence trigger phrases, lowercased. These are the
/// terms the postmortem enumerates (§5 P3) plus their obvious inflections.
/// A phrase matches only at word boundaries (see [`is_word_boundary_match`]),
/// so `orphaned` does not fire inside an unrelated longer word.
const TRIGGERS: &[&str] = &[
    "supersede",
    "supersedes",
    "superseded",
    "superseding",
    "obsolete",
    "obsoletes",
    "obsoleted",
    "replaces",
    "replaced",
    "now-dead",
    "now dead",
    "orphan",
    "orphaned",
    "orphans",
];

/// How many characters of context to include on each side of a match in the
/// reported snippet.
const SNIPPET_RADIUS: usize = 48;

/// Scan `text` for supersession / obsolescence language.
///
/// Returns one [`SupersessionHit`] per matched *term* (deduplicated by term,
/// keeping the first occurrence's snippet), ordered by first appearance in the
/// text. An empty result means no supersession narrative is present. Matching
/// is case-insensitive and word-boundary aware.
pub fn scan_supersession_language(text: &str) -> Vec<SupersessionHit> {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let n = lower.len();

    // Collect (start_index, term) matches, then dedup by term preserving order.
    let mut matches: Vec<(usize, &str)> = Vec::new();
    let mut seen_terms: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    // Longest triggers first so `now-dead` is preferred over a bare `dead`-like
    // partial (none today, but keeps the scan stable if TRIGGERS grows).
    let mut ordered: Vec<&str> = TRIGGERS.to_vec();
    ordered.sort_by_key(|t| std::cmp::Reverse(t.len()));

    for &term in &ordered {
        let pat: Vec<char> = term.chars().collect();
        if pat.is_empty() || pat.len() > n {
            continue;
        }
        let mut i = 0;
        while i + pat.len() <= n {
            if lower[i..i + pat.len()] == pat[..] && is_word_boundary_match(&lower, i, pat.len()) {
                matches.push((i, term));
                i += pat.len();
            } else {
                i += 1;
            }
        }
    }

    matches.sort_by_key(|(idx, _)| *idx);

    let mut hits: Vec<SupersessionHit> = Vec::new();
    for (idx, term) in matches {
        if !seen_terms.insert(term) {
            continue;
        }
        hits.push(SupersessionHit {
            term: term.to_owned(),
            snippet: snippet_around(&lower, idx, term.chars().count()),
        });
    }
    hits
}

/// True when the `len`-char match starting at `start` in `chars` is bounded by
/// non-alphanumeric characters on both sides (or the string edge). Hyphenated
/// triggers (`now-dead`) still satisfy this because the boundary check looks at
/// the char just before `start` and just after the match, not the interior.
fn is_word_boundary_match(chars: &[char], start: usize, len: usize) -> bool {
    let before_ok = start == 0 || !chars[start - 1].is_alphanumeric();
    let end = start + len;
    let after_ok = end >= chars.len() || !chars[end].is_alphanumeric();
    before_ok && after_ok
}

/// Build a trimmed, single-line snippet of ~`SNIPPET_RADIUS` chars on each side
/// of the `[start, start+len)` match, collapsing interior newlines to spaces.
fn snippet_around(chars: &[char], start: usize, len: usize) -> String {
    let lo = start.saturating_sub(SNIPPET_RADIUS);
    let hi = (start + len + SNIPPET_RADIUS).min(chars.len());
    let mut s: String = chars[lo..hi]
        .iter()
        .map(|&c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    if lo > 0 {
        out.push('…');
    }
    out.push_str(&s);
    if hi < chars.len() {
        out.push('…');
    }
    out
}

/// Format each hit as a single reviewer-facing bullet line
/// (`**term** — "snippet"`). This is the deterministic scan's serialisable
/// output, carried into the reviewer prompt context.
pub fn hit_lines(hits: &[SupersessionHit]) -> Vec<String> {
    hits.iter()
        .map(|h| format!("**{}** — \"{}\"", h.term, h.snippet))
        .collect()
}

/// Re-export of the reviewer-prompt block renderer for these hits. The
/// rendering moved to `boss-pr-review` (it is reviewer-prompt text, and lives
/// next to the prompt that interpolates it); the deterministic scan that
/// produces the hit lines stays here.
pub use boss_pr_review::render_supersession_flag_block;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_the_incident_002_narrative() {
        let body = "Kept T17's clickable Insights badge + drill-down modal, which \
                    supersedes T16's static bucket badge. Removed the now-dead T16 \
                    wiring this obsoletes.";
        let hits = scan_supersession_language(body);
        let terms: Vec<&str> = hits.iter().map(|h| h.term.as_str()).collect();
        assert!(terms.contains(&"supersedes"), "must flag 'supersedes': {terms:?}");
        assert!(terms.contains(&"now-dead"), "must flag 'now-dead': {terms:?}");
        assert!(terms.contains(&"obsoletes"), "must flag 'obsoletes': {terms:?}");
    }

    #[test]
    fn flags_orphaned_and_replaces() {
        let hits = scan_supersession_language("This component is orphaned; the modal replaces it.");
        let terms: Vec<&str> = hits.iter().map(|h| h.term.as_str()).collect();
        assert!(terms.contains(&"orphaned"));
        assert!(terms.contains(&"replaces"));
    }

    #[test]
    fn no_false_positive_on_clean_text() {
        let hits = scan_supersession_language("Add a new recommendation badge and wire the pre-fetch into PlanPageV2.");
        assert!(hits.is_empty(), "clean text must not flag: {hits:?}");
    }

    #[test]
    fn word_boundary_prevents_substring_false_positives() {
        // "orphanage" contains "orphan" but is not a supersession claim.
        let hits = scan_supersession_language("The orphanage software is unrelated.");
        assert!(
            hits.iter().all(|h| h.term != "orphan"),
            "must not flag 'orphan' inside 'orphanage': {hits:?}",
        );
    }

    #[test]
    fn dedups_by_term_but_keeps_distinct_terms() {
        let hits = scan_supersession_language("supersedes here and supersedes there and obsoletes elsewhere");
        let supersedes = hits.iter().filter(|h| h.term == "supersedes").count();
        assert_eq!(supersedes, 1, "term deduped: {hits:?}");
        assert!(hits.iter().any(|h| h.term == "obsoletes"));
    }

    #[test]
    fn case_insensitive() {
        let hits = scan_supersession_language("This SUPERSEDES the old flow.");
        assert!(hits.iter().any(|h| h.term == "supersedes"));
    }

    #[test]
    fn render_block_empty_when_no_hits() {
        assert_eq!(render_supersession_flag_block(&[]), "");
    }

    #[test]
    fn render_block_lists_flags_and_demands_citation() {
        let hits = scan_supersession_language("supersedes the badge");
        let block = render_supersession_flag_block(&hit_lines(&hits));
        assert!(block.contains("Supersession-claim citation check"));
        assert!(block.contains("design doc"));
        assert!(block.contains("regression"));
        assert!(block.contains("supersedes"));
    }

    #[test]
    fn snippet_is_single_line() {
        let hits = scan_supersession_language("line one\nthis supersedes\nthat old thing\nmore");
        assert!(!hits.is_empty());
        assert!(!hits[0].snippet.contains('\n'));
    }
}
