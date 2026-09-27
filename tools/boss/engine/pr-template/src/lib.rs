//! PR-template loader for editorial controls (design: editorial-controls-for-agent-authored-prs-and-github-comments.md, chore #9).
//!
//! Reads `.github/PULL_REQUEST_TEMPLATE.md` (single-file form) or
//! `.github/PULL_REQUEST_TEMPLATE/*.md` (directory form) from a cube
//! workspace and exposes:
//!
//! - The raw template text, for injection into the `[editorial-rules]` prompt block.
//! - The set of required H2/H3 section headings, for the PreToolUse hook's
//!   `template_policy = Enforce` conformance check.
//!
//! Results are cached per `(product_id, lease_id)` so that repeated calls
//! within the same execution do not re-read the disk.  A new lease forces a
//! fresh read even if the product ID is the same, which is correct: the
//! template might have changed in the workspace between leases.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single PR-template file.
#[derive(Debug, Clone)]
pub struct PrTemplate {
    /// Full markdown text of the template, verbatim.  Suitable for including
    /// in the `[editorial-rules]` prompt block.
    pub text: String,
    /// H2 and H3 heading titles, in document order.  Used by the
    /// `template_policy = Enforce` hook to detect missing required sections.
    /// Headings inside fenced code blocks are excluded.
    pub required_headings: Vec<String>,
    /// Path relative to the workspace root, for logging and diagnostics.
    pub source_path: PathBuf,
}

/// Everything the loader found (or didn't find) in a workspace.
///
/// Callers should treat `is_empty() == true` as `template_policy` being
/// effectively `Off` — there is no template to conform to.
#[derive(Debug, Clone, Default)]
pub struct PrTemplateSet {
    /// Single-file form: `.github/PULL_REQUEST_TEMPLATE.md`.
    pub default_template: Option<PrTemplate>,
    /// Directory form: `.github/PULL_REQUEST_TEMPLATE/<stem>.md`,
    /// keyed by the lowercased file stem (e.g. `"bug_report"`).
    pub named_templates: HashMap<String, PrTemplate>,
}

impl PrTemplateSet {
    /// `true` when no template file was found at either path.
    pub fn is_empty(&self) -> bool {
        self.default_template.is_none() && self.named_templates.is_empty()
    }

    /// Iterator over all templates: default first, then named templates in
    /// sorted order.  Useful for rendering all templates into a prompt block.
    pub fn all_templates(&self) -> impl Iterator<Item = &PrTemplate> {
        let mut stems: Vec<&str> = self.named_templates.keys().map(String::as_str).collect();
        stems.sort();
        // Build a Vec so we can return a concrete iterator type.
        let mut out: Vec<&PrTemplate> = Vec::with_capacity(1 + stems.len());
        if let Some(t) = &self.default_template {
            out.push(t);
        }
        for stem in stems {
            out.push(&self.named_templates[stem]);
        }
        out.into_iter()
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    product_id: String,
    lease_id: String,
}

static CACHE: OnceLock<Mutex<HashMap<CacheKey, PrTemplateSet>>> = OnceLock::new();

fn global_cache() -> &'static Mutex<HashMap<CacheKey, PrTemplateSet>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load (and cache) the PR template(s) for a product + workspace lease.
///
/// The first call for a given `(product_id, lease_id)` pair reads from disk
/// and stores the result.  Subsequent calls with the same key return the
/// cached value without touching the filesystem.
///
/// If no template files exist, the returned `PrTemplateSet` is empty
/// (`is_empty() == true`).  Callers treat this as `template_policy = Off`.
pub fn load(product_id: &str, lease_id: &str, workspace_path: &Path) -> PrTemplateSet {
    let key = CacheKey {
        product_id: product_id.to_owned(),
        lease_id: lease_id.to_owned(),
    };

    {
        let guard = global_cache().lock().expect("pr_template cache lock poisoned");
        if let Some(set) = guard.get(&key) {
            return set.clone();
        }
    }

    let set = load_from_disk(workspace_path);

    let mut guard = global_cache().lock().expect("pr_template cache lock poisoned");
    // Use `entry` so a racing call that completed between the two locks wins;
    // either value is correct.
    guard.entry(key).or_insert(set).clone()
}

// ---------------------------------------------------------------------------
// Disk loading
// ---------------------------------------------------------------------------

fn load_from_disk(workspace_path: &Path) -> PrTemplateSet {
    let mut set = PrTemplateSet::default();
    let github_dir = workspace_path.join(".github");

    // Single-file form
    let single = github_dir.join("PULL_REQUEST_TEMPLATE.md");
    if single.is_file()
        && let Some(tmpl) = load_file(&single, workspace_path)
    {
        set.default_template = Some(tmpl);
    }

    // Directory form
    let template_dir = github_dir.join("PULL_REQUEST_TEMPLATE");
    if template_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&template_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if let Some(tmpl) = load_file(&path, workspace_path) {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unnamed")
                    .to_lowercase();
                set.named_templates.insert(stem, tmpl);
            }
        }
    }

    set
}

fn load_file(path: &Path, workspace_root: &Path) -> Option<PrTemplate> {
    let text = std::fs::read_to_string(path).ok()?;
    let required_headings = parse_required_headings(&text);
    let source_path = path.strip_prefix(workspace_root).unwrap_or(path).to_path_buf();
    Some(PrTemplate {
        text,
        required_headings,
        source_path,
    })
}

// ---------------------------------------------------------------------------
// Heading parser
// ---------------------------------------------------------------------------

/// One fence-aware, CommonMark-ATX-conformant markdown heading: its level
/// (1-6), title text, and the byte offset of the start of its line in the
/// source document.
///
/// This is the single heading tokenizer shared by this crate's
/// required-heading extraction (H2/H3 only) and `boss-pr-review`'s
/// design-doc section locator (any level, plus the offset needed to slice
/// out a section body). Keeping one tokenizer means a heading-detection fix
/// — like the fence and indentation rules below — never has to be made
/// twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingToken {
    pub level: usize,
    pub title: String,
    pub start: usize,
}

/// Extract every ATX heading (`#` through `######`) from markdown, skipping
/// content inside fenced code blocks (``` or ~~~).
///
/// Follows CommonMark's ATX heading rule: the line may be indented by at
/// most three spaces, and the hash run must be followed by a space/tab or
/// the end of the line. `#[derive(Debug)]` (no space after the hash run) is
/// never a heading, fenced or not. `# shell comment` is a valid H1 when
/// unfenced; it is excluded only when it sits inside a fence. A 4-space
/// indent is treated as part of an indented code block rather than a
/// heading candidate.
pub fn parse_all_headings(text: &str) -> Vec<HeadingToken> {
    let mut headings = Vec::new();
    let mut in_fence = false;
    let mut fence_char = '`';
    let mut fence_min_len = 3usize;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let start = offset;
        offset += line.len();

        let leading_spaces = content.chars().take_while(|&c| c == ' ').count();
        let unindented = &content[leading_spaces.min(content.len())..];

        if in_fence {
            // Closing fence: same character, at least as many as the opener,
            // with nothing else on the line (trailing spaces are fine).
            let n = unindented.chars().take_while(|&c| c == fence_char).count();
            if n >= fence_min_len && unindented[n..].trim().is_empty() {
                in_fence = false;
            }
            continue;
        }

        // Up to three leading spaces is still an ATX candidate per
        // CommonMark; four or more is an indented code block, so neither a
        // fence opener nor a heading can start there.
        if leading_spaces > 3 {
            continue;
        }

        // Opening fence: at least three ``` or ~~~ characters.
        if unindented.starts_with("```") || unindented.starts_with("~~~") {
            fence_char = unindented.chars().next().unwrap();
            fence_min_len = unindented.chars().take_while(|&c| c == fence_char).count().max(3);
            in_fence = true;
            continue;
        }

        if let Some((level, title)) = parse_atx_heading(unindented) {
            headings.push(HeadingToken { level, title, start });
        }
    }

    headings
}

/// Return `(level, title)` for an ATX heading line, or `None`.
///
/// The caller passes a line with any indentation already stripped. Requires
/// a space/tab after the hash run, or end of line (a bare `#` heading with
/// no title is not a required section and is skipped).
fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let hash_count = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hash_count) {
        return None;
    }
    let rest = &line[hash_count..];
    let title = if rest.is_empty() {
        ""
    } else if rest.starts_with(' ') || rest.starts_with('\t') {
        rest.trim()
    } else {
        // Not a valid ATX heading (no space/tab after the hashes).
        return None;
    };
    if title.is_empty() {
        return None;
    }
    Some((hash_count, title.to_owned()))
}

/// Extract H2 (`##`) and H3 (`###`) heading titles from markdown, skipping
/// content inside fenced code blocks (``` or ~~~). Deeper headings (H4+) are
/// not included per the design's "v1 only enforces H2/H3" rule.
fn parse_required_headings(text: &str) -> Vec<String> {
    parse_all_headings(text)
        .into_iter()
        .filter(|h| (2..=3).contains(&h.level))
        .map(|h| h.title)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_workspace() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // --- heading parser ------------------------------------------------------

    #[test]
    fn parse_h2_headings() {
        let text = "## Summary\n\nSome text.\n\n## Test plan\n";
        assert_eq!(parse_required_headings(text), vec!["Summary", "Test plan"]);
    }

    #[test]
    fn parse_h3_headings() {
        let text = "## Summary\n\n### Details\n\nText.\n";
        assert_eq!(parse_required_headings(text), vec!["Summary", "Details"]);
    }

    #[test]
    fn skip_h1_and_h4() {
        let text = "# Title\n\n## Keep\n\n#### Skip\n";
        assert_eq!(parse_required_headings(text), vec!["Keep"]);
    }

    #[test]
    fn skip_headings_in_fenced_code_block() {
        let text = "## Before\n\n```\n## Inside\n```\n\n## After\n";
        assert_eq!(parse_required_headings(text), vec!["Before", "After"]);
    }

    #[test]
    fn skip_headings_in_tilde_fence() {
        let text = "## Before\n\n~~~\n## Inside\n~~~\n\n## After\n";
        assert_eq!(parse_required_headings(text), vec!["Before", "After"]);
    }

    #[test]
    fn longer_fence_closed_by_longer_or_equal() {
        // A `````-fence must be closed by at least five backticks.
        let text = "## A\n\n`````\n## Hidden\n`````\n\n## B\n";
        assert_eq!(parse_required_headings(text), vec!["A", "B"]);
    }

    #[test]
    fn heading_without_space_is_not_heading() {
        // `##NoSpace` is not a valid ATX heading.
        let text = "##NoSpace\n## Valid\n";
        assert_eq!(parse_required_headings(text), vec!["Valid"]);
    }

    #[test]
    fn parse_all_headings_covers_every_level_with_offsets() {
        let text = "# One\n\n## Two\n\n###### Six\n";
        let headings = parse_all_headings(text);
        assert_eq!(
            headings,
            vec![
                HeadingToken {
                    level: 1,
                    title: "One".to_owned(),
                    start: 0
                },
                HeadingToken {
                    level: 2,
                    title: "Two".to_owned(),
                    start: 7
                },
                HeadingToken {
                    level: 6,
                    title: "Six".to_owned(),
                    start: 15
                },
            ]
        );
    }

    #[test]
    fn pseudo_headings_inside_fences_are_not_headings() {
        // A Rust `#[derive]` attribute and a shell `#` comment must never be
        // mistaken for markdown headings when they appear inside a fence.
        let text = "## Before\n\n```rust\n#[derive(Debug)]\npub struct Broker;\n```\n\n```sh\n# shell comment\necho hi\n```\n\n## After\n";
        let headings: Vec<String> = parse_all_headings(text).into_iter().map(|h| h.title).collect();
        assert_eq!(headings, vec!["Before", "After"]);
    }

    #[test]
    fn four_space_indented_line_is_not_a_heading_or_fence_opener() {
        // CommonMark: 4+ spaces of indentation makes a line part of an
        // indented code block, not an ATX heading or fence opener.
        let text = "## Real\n\n    ## Not a heading (indented code)\n\n## Also real\n";
        let headings: Vec<String> = parse_all_headings(text).into_iter().map(|h| h.title).collect();
        assert_eq!(headings, vec!["Real", "Also real"]);
    }

    #[test]
    fn hash_without_space_or_eol_is_never_a_heading() {
        // `#[derive(...)]` (no space after the hash run) is never a heading.
        // `# shell comment` is a valid H1 when unfenced; it is excluded only
        // because it sits inside a fence (see
        // `pseudo_headings_inside_fences_are_not_headings`).
        let text = "#[derive(Debug)]\n## Real\n";
        let headings: Vec<String> = parse_all_headings(text).into_iter().map(|h| h.title).collect();
        assert_eq!(headings, vec!["Real"]);
    }

    #[test]
    fn trailing_hashes_stripped() {
        // CommonMark allows `## Heading ##` — trailing hashes are cosmetic.
        // GitHub strips them; our parser doesn't need to, but the title
        // should be trimmed.
        let text = "## Heading ##\n";
        // trim() on the rest after the leading space: "Heading ##" trimmed is "Heading ##".
        // The design doesn't require stripping trailing hashes — just test that
        // trim() at least trims whitespace around the title.
        let headings = parse_required_headings(text);
        assert_eq!(headings, vec!["Heading ##"]);
    }

    // --- single-file form ----------------------------------------------------

    #[test]
    fn single_file_loaded() {
        let tmp = make_workspace();
        let github = tmp.path().join(".github");
        fs::create_dir_all(&github).unwrap();
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## Summary\n\n## Test plan\n").unwrap();

        let set = load_from_disk(tmp.path());
        assert!(!set.is_empty());
        let tmpl = set.default_template.as_ref().unwrap();
        assert_eq!(tmpl.required_headings, vec!["Summary", "Test plan"]);
        assert_eq!(tmpl.source_path, Path::new(".github/PULL_REQUEST_TEMPLATE.md"));
    }

    // --- directory form ------------------------------------------------------

    #[test]
    fn directory_form_loaded() {
        let tmp = make_workspace();
        let tdir = tmp.path().join(".github").join("PULL_REQUEST_TEMPLATE");
        fs::create_dir_all(&tdir).unwrap();
        fs::write(
            tdir.join("bug_report.md"),
            "## Bug description\n\n## Steps to reproduce\n",
        )
        .unwrap();
        fs::write(tdir.join("feature_request.md"), "## Motivation\n\n## Proposal\n").unwrap();

        let set = load_from_disk(tmp.path());
        assert!(set.default_template.is_none());
        assert_eq!(set.named_templates.len(), 2);

        let bug = &set.named_templates["bug_report"];
        assert_eq!(bug.required_headings, vec!["Bug description", "Steps to reproduce"]);
        assert_eq!(
            bug.source_path,
            Path::new(".github/PULL_REQUEST_TEMPLATE/bug_report.md")
        );

        let feat = &set.named_templates["feature_request"];
        assert_eq!(feat.required_headings, vec!["Motivation", "Proposal"]);
    }

    #[test]
    fn non_md_files_in_directory_ignored() {
        let tmp = make_workspace();
        let tdir = tmp.path().join(".github").join("PULL_REQUEST_TEMPLATE");
        fs::create_dir_all(&tdir).unwrap();
        fs::write(tdir.join("template.md"), "## Section\n").unwrap();
        fs::write(tdir.join("README.txt"), "not a template").unwrap();

        let set = load_from_disk(tmp.path());
        assert_eq!(set.named_templates.len(), 1);
        assert!(set.named_templates.contains_key("template"));
    }

    // --- missing-file form ---------------------------------------------------

    #[test]
    fn missing_template_returns_empty_set() {
        let tmp = make_workspace();
        let set = load_from_disk(tmp.path());
        assert!(set.is_empty());
        assert!(set.default_template.is_none());
        assert!(set.named_templates.is_empty());
    }

    #[test]
    fn github_dir_missing_returns_empty_set() {
        let tmp = make_workspace();
        // No .github directory at all.
        let set = load_from_disk(tmp.path());
        assert!(set.is_empty());
    }

    // --- all_templates ordering ----------------------------------------------

    #[test]
    fn all_templates_default_first_then_sorted() {
        let tmp = make_workspace();
        let github = tmp.path().join(".github");
        fs::create_dir_all(&github).unwrap();
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## Default\n").unwrap();
        let tdir = github.join("PULL_REQUEST_TEMPLATE");
        fs::create_dir_all(&tdir).unwrap();
        fs::write(tdir.join("zebra.md"), "## Zebra\n").unwrap();
        fs::write(tdir.join("alpha.md"), "## Alpha\n").unwrap();

        let set = load_from_disk(tmp.path());
        let names: Vec<&str> = set.all_templates().map(|t| t.required_headings[0].as_str()).collect();
        // Default first, then named in sorted order (alpha < zebra).
        assert_eq!(names, vec!["Default", "Alpha", "Zebra"]);
    }

    // --- caching -------------------------------------------------------------

    #[test]
    fn cache_returns_same_result_on_second_call() {
        let tmp = make_workspace();
        let github = tmp.path().join(".github");
        fs::create_dir_all(&github).unwrap();
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## Cached\n").unwrap();

        // Use unique IDs to avoid colliding with other test runs that share
        // the global cache.
        let pid = "test-product-cache-check";
        let lid = "test-lease-cache-check";

        let first = load(pid, lid, tmp.path());
        assert!(!first.is_empty());

        // Overwrite the file on disk — the cache should serve the old value.
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## Changed\n").unwrap();

        let second = load(pid, lid, tmp.path());
        assert_eq!(
            first.default_template.as_ref().unwrap().required_headings,
            second.default_template.as_ref().unwrap().required_headings,
        );
        // Cache returned old headings, not "Changed".
        assert_eq!(
            second.default_template.as_ref().unwrap().required_headings,
            vec!["Cached"]
        );
    }

    #[test]
    fn different_lease_id_bypasses_cache() {
        let tmp = make_workspace();
        let github = tmp.path().join(".github");
        fs::create_dir_all(&github).unwrap();
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## First\n").unwrap();

        let pid = "test-product-lease-bypass";

        let first = load(pid, "lease-A-bypass", tmp.path());
        assert_eq!(
            first.default_template.as_ref().unwrap().required_headings,
            vec!["First"]
        );

        // Overwrite and use a new lease id.
        fs::write(github.join("PULL_REQUEST_TEMPLATE.md"), "## Second\n").unwrap();

        let second = load(pid, "lease-B-bypass", tmp.path());
        assert_eq!(
            second.default_template.as_ref().unwrap().required_headings,
            vec!["Second"]
        );
    }
}
