//! Deterministic scan for bare Boss work-item id references (`T<n>`/`P<n>`)
//! in worker-authored PR text and added diff lines — a mechanical assist for
//! the reviewer prompt's agent-isms "Boss-construct references" sub-rule.
//!
//! A real review once reasoned itself out of flagging exactly this: the
//! reviewer read pre-existing `T<n>` leaks already on `main` in the same
//! struct as evidence of a legitimate non-Boss tracker, and read the
//! engine-authored Task description's own bare ids as evidence the ids are
//! ordinary project vocabulary — the inverted inference. This scan removes
//! the judgment call by surfacing every candidate as a forced-disposition
//! line the reviewer must explicitly flag or explain away, rather than a
//! pattern it can reason past silently.
//!
//! Word-boundary matching means a token like `T5Config` or a hex run that
//! happens to contain `T5` mid-sequence does not match — only a standalone
//! `T<digits>`/`P<digits>` run bounded by non-word characters counts.

use std::sync::LazyLock;

use regex::Regex;

/// Matches a bare Boss work-item id: `T` or `P` followed by one or more
/// digits, bounded on both sides by non-word characters.
static BOSS_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[TP]\d+\b").expect("boss id regex compiles"));

/// How many characters of context to include on each side of a match in the
/// reported snippet.
const SNIPPET_RADIUS: usize = 48;

/// A single Boss-construct-id hit, ready to render as a forced-disposition
/// prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BossConstructHit {
    /// Where the id was found: `"<file>:<line>"` for a diff hit, or a fixed
    /// label (`"PR title"`, `"PR description"`) for narrative text.
    pub location: String,
    /// The matched id token, e.g. `"T339"`.
    pub id: String,
    /// A short surrounding snippet (original case) for reviewer orientation.
    pub snippet: String,
}

/// Scan a block of narrative text (PR title or description) for bare
/// `T<n>`/`P<n>` tokens. Every match is reported — narrative text is short
/// enough that dedup would only hide distinct sentences worth separate
/// disposition.
pub fn scan_narrative_text(text: &str, location: &str) -> Vec<BossConstructHit> {
    BOSS_ID_RE
        .find_iter(text)
        .map(|m| BossConstructHit {
            location: location.to_owned(),
            id: m.as_str().to_owned(),
            snippet: snippet_around(text, m.start(), m.end()),
        })
        .collect()
}

/// Scan the *added* lines of a unified diff (`gh pr diff` output) for bare
/// `T<n>`/`P<n>` tokens, resolving each hit to `<file>:<line>` using the
/// diff's own hunk headers. Context and removed lines are not scanned — only
/// lines the PR actually introduces.
pub fn scan_diff_added_lines(diff: &str) -> Vec<BossConstructHit> {
    let mut hits = Vec::new();
    let mut current_file: Option<String> = None;
    let mut next_line: u64 = 0;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            current_file = None;
            next_line = 0;
        } else if line.starts_with("index ") || line.starts_with("--- ") {
            // File-identity lines that precede a hunk; carry no line info.
        } else if let Some(path) = line.strip_prefix("+++ ") {
            current_file = normalize_diff_path(path.trim());
        } else if line.starts_with("@@ ") {
            if let Some(start) = parse_hunk_new_start(line) {
                next_line = start;
            }
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" — not a real line, no counter bump.
        } else if let Some(content) = line.strip_prefix('+') {
            let file = current_file.as_deref().unwrap_or("<unknown file>");
            for m in BOSS_ID_RE.find_iter(content) {
                hits.push(BossConstructHit {
                    location: format!("{file}:{next_line}"),
                    id: m.as_str().to_owned(),
                    snippet: snippet_around(content, m.start(), m.end()),
                });
            }
            next_line += 1;
        } else if line.starts_with('-') {
            // Removed line: does not consume a new-file line number.
        } else {
            // Context line (unchanged, carries into the new file).
            next_line += 1;
        }
    }
    hits
}

/// Strip the `a/`/`b/` prefix `gh pr diff` puts on `+++`/`---` paths. Returns
/// `None` for `/dev/null` (a deleted file has no added lines to attribute).
fn normalize_diff_path(path: &str) -> Option<String> {
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix("b/").unwrap_or(path).to_owned())
}

/// Parse the new-file starting line number out of a hunk header, e.g.
/// `@@ -12,5 +34,7 @@ fn foo() {` → `34`.
fn parse_hunk_new_start(line: &str) -> Option<u64> {
    let plus_idx = line.find('+')?;
    let rest = &line[plus_idx + 1..];
    let end = rest.find([' ', ',']).unwrap_or(rest.len());
    rest[..end].parse::<u64>().ok()
}

/// Build a trimmed, single-line snippet of ~`SNIPPET_RADIUS` chars on each
/// side of the `[start, end)` byte range in `text`, collapsing interior
/// whitespace.
fn snippet_around(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let char_start = text[..start].chars().count();
    let char_end = text[..end].chars().count();
    let lo = char_start.saturating_sub(SNIPPET_RADIUS);
    let hi = (char_end + SNIPPET_RADIUS).min(chars.len());
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

/// Format each hit as a single reviewer-facing fragment (`` `id` at location
/// — "snippet" ``). This is the deterministic scan's serialisable output,
/// carried into the reviewer prompt context.
pub fn hit_lines(hits: &[BossConstructHit]) -> Vec<String> {
    hits.iter()
        .map(|h| format!("`{}` at {} — \"{}\"", h.id, h.location, h.snippet))
        .collect()
}

// Rendering of the reviewer-prompt block for these hits lives in
// `boss-pr-review` (it is reviewer-prompt text, and lives next to the
// prompt that interpolates it); the deterministic scan that produces the
// hit lines stays here. See `boss_pr_review::render_boss_construct_sweep_block`.

#[cfg(test)]
mod tests {
    use super::*;
    use boss_pr_review::render_boss_construct_sweep_block;

    #[test]
    fn scans_added_diff_lines_and_resolves_file_line() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                     index 1111111..2222222 100644\n\
                     --- a/src/lib.rs\n\
                     +++ b/src/lib.rs\n\
                     @@ -10,3 +10,4 @@ fn foo() {\n\
                      unchanged line\n\
                     -old line\n\
                     +// T339 originally chose not to serialize this; reversed here\n\
                      trailing context\n";
        let hits = scan_diff_added_lines(diff);
        assert_eq!(hits.len(), 1, "expected exactly one hit: {hits:?}");
        assert_eq!(hits[0].id, "T339");
        assert_eq!(hits[0].location, "src/lib.rs:11");
    }

    #[test]
    fn does_not_scan_removed_or_context_lines() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                     index 1111111..2222222 100644\n\
                     --- a/src/lib.rs\n\
                     +++ b/src/lib.rs\n\
                     @@ -1,2 +1,2 @@\n\
                     -removed T100 reference\n\
                      context mentions T200 too\n";
        let hits = scan_diff_added_lines(diff);
        assert!(hits.is_empty(), "removed/context lines must not be scanned: {hits:?}");
    }

    #[test]
    fn tracks_line_numbers_across_multiple_added_lines() {
        let diff = "diff --git a/a.rs b/a.rs\n\
                     index 1111111..2222222 100644\n\
                     --- a/a.rs\n\
                     +++ b/a.rs\n\
                     @@ -1,0 +1,3 @@\n\
                     +first line\n\
                     +second T15 line\n\
                     +third P7 line\n";
        let hits = scan_diff_added_lines(diff);
        let locations: Vec<&str> = hits.iter().map(|h| h.location.as_str()).collect();
        assert_eq!(locations, vec!["a.rs:2", "a.rs:3"]);
    }

    #[test]
    fn scans_narrative_text_for_pr_title_and_body() {
        let title_hits = scan_narrative_text("Fix T191 handling", "PR title");
        assert_eq!(title_hits.len(), 1);
        assert_eq!(title_hits[0].location, "PR title");
        assert_eq!(title_hits[0].id, "T191");

        let body_hits = scan_narrative_text("This reverses T339's earlier choice.", "PR description");
        assert_eq!(body_hits.len(), 1);
        assert_eq!(body_hits[0].id, "T339");
    }

    #[test]
    fn no_false_positive_on_type_names_or_hex_runs() {
        let hits = scan_narrative_text(
            "Uses T5Config and a hex id abcT5f9 plus commit deadbeefT1 — none are work items.",
            "PR description",
        );
        assert!(hits.is_empty(), "must not flag ids embedded in longer tokens: {hits:?}");
    }

    #[test]
    fn no_false_positive_on_clean_text() {
        let hits = scan_narrative_text("Adds retry/backoff to the client.", "PR description");
        assert!(hits.is_empty());
    }

    #[test]
    fn dev_null_diff_target_produces_no_hits() {
        let diff = "diff --git a/gone.rs b/gone.rs\n\
                     deleted file mode 100644\n\
                     index 1111111..0000000\n\
                     --- a/gone.rs\n\
                     +++ /dev/null\n\
                     @@ -1,2 +0,0 @@\n\
                     -removed T9 line\n\
                     -another removed line\n";
        let hits = scan_diff_added_lines(diff);
        assert!(hits.is_empty(), "a pure deletion has no added lines: {hits:?}");
    }

    #[test]
    fn render_block_empty_when_no_hits() {
        assert_eq!(render_boss_construct_sweep_block(&[]), "");
    }

    #[test]
    fn render_block_lists_candidates_and_demands_disposition() {
        let hits = scan_narrative_text("Fix T191 handling", "PR title");
        let block = render_boss_construct_sweep_block(&hit_lines(&hits));
        assert!(block.contains("Boss work-item id sweep"));
        assert!(block.contains("T191"));
        assert!(block.contains("PR title"));
        assert!(block.contains("you must either flag it as a finding or state why it is not one"));
    }
}
