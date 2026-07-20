//! Paraphrase-robust duplicate detection for automation-produced tasks.
//!
//! An automation fires on a cron (~12h). Each fire spawns a fresh,
//! memory-less triage agent against the same standing instruction and the
//! same repository, so the same finding is very likely to be re-derived —
//! and re-filed under a *differently worded* title. The audited collisions
//! all look like this:
//!
//! ```text
//! "Split engine core app.rs (~2548 lines)"
//! "Split engine/core src/app.rs (nearing 3000-line limit)"
//! ```
//!
//! Exact-name matching (the engine's 60-second `check_recent_duplicate`
//! guard) cannot see that, and no time window helps: the two filings are
//! half a day apart by design. What the two DO share — in every audited
//! cluster — is the **thing they are about**: a file path, or a crate /
//! module name. That is what this crate keys on.
//!
//! # The three signals, strongest first
//!
//! 1. **File target** — the first source-file token in the title (falling
//!    back to the description), reduced to `(basename, qualifiers)`:
//!    `engine/core/src/app.rs` → `("app.rs", ["engine", "core"])`, with
//!    boilerplate segments (`src`, `tools`, `lib`, …) dropped. Two file
//!    targets match when the basenames are equal **and** the qualifiers are
//!    compatible — either side may be empty, otherwise the shorter must be
//!    a subsequence of the longer. So `app.rs` (unqualified, as in the
//!    paraphrase above) matches `engine/core/src/app.rs`, while
//!    `tools/cube/src/app.rs` does not.
//! 2. **Module target** — crate / module identifiers named in the title:
//!    backtick-quoted words, `a::b` paths, `snake_case` words, and words
//!    adjacent to "crate"/"module". Collected as a *set*; two fingerprints
//!    match when the sets intersect. This is what catches the `pr_review`
//!    and `metrics` crate-extraction clusters, which name no file at all.
//! 3. **Normalized title** — the secondary, per the brief. Lowercased
//!    content words with sizes/counts and stopwords stripped, compared as
//!    sets so word order does not matter. Deliberately exact set equality
//!    rather than a similarity threshold: signals 1 and 2 do the real work,
//!    and a fuzzy third signal is where false suppressions would come from.
//!
//! Suppressing a *genuine* finding is the expensive error — it silently
//! loses work, whereas a missed duplicate merely costs what the system
//! already costs today. Every rule here is therefore biased toward
//! matching narrowly.
//!
//! # Scope
//!
//! Fingerprints are only ever compared *within one automation*
//! (`tasks.source_automation_id`). Two different automations converging on
//! the same file is out of scope and explicitly allowed — see
//! `WorkDb::create_automation_task`.

use std::collections::BTreeSet;

mod extract;

#[cfg(test)]
mod tests;

use extract::{
    QUALIFIER_NOISE_SEGMENTS, is_file_token, module_candidates_in, normalize_token, split_tokens, strip_token_edges,
};

/// Which signal fired, for the operator-facing suppression trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Both titles name the same source file (signal 1).
    FileTarget,
    /// Both titles name the same crate / module identifier (signal 2).
    ModuleTarget,
    /// Neither names a target, but the normalized content words are
    /// identical as sets (signal 3).
    NormalizedTitle,
}

impl MatchKind {
    /// Stable identifier stored in `automation_dedup_suppressions.matched_on`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileTarget => "file_target",
            Self::ModuleTarget => "module_target",
            Self::NormalizedTitle => "normalized_title",
        }
    }
}

/// A fired signal plus the concrete value that fired it, so the
/// suppression trace can say *why* two rows were judged the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateMatch {
    pub kind: MatchKind,
    /// The shared value — a basename, a module identifier, or the joined
    /// normalized title. Stored verbatim in the suppression row.
    pub key: String,
}

/// A source-file reference reduced to its comparable parts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileTarget {
    /// Lowercased final path segment, e.g. `app.rs`.
    basename: String,
    /// Lowercased directory segments with boilerplate removed, in path
    /// order, e.g. `engine/core/src/app.rs` → `["engine", "core"]`.
    qualifiers: Vec<String>,
}

impl FileTarget {
    /// Same file? Basenames must be equal; qualifiers must not contradict.
    ///
    /// An empty qualifier list is "unspecified", not "root" — a title that
    /// says `app.rs` with no path is exactly the paraphrase case we exist
    /// to catch, so it must stay compatible with any path ending in
    /// `app.rs`. Two non-empty lists are compatible only when the shorter
    /// is a subsequence of the longer, which keeps `engine/core/…/app.rs`
    /// distinct from `tools/cube/…/app.rs`.
    ///
    /// Known limitation: an unqualified `app.rs` matches *any* `app.rs`
    /// within the automation. Within a single automation's standing
    /// instruction that has been the right call on every audited cluster,
    /// and the alternative — inferring qualifiers from bare title words —
    /// is guesswork that fails in both directions.
    fn matches(&self, other: &Self) -> bool {
        self.basename == other.basename && qualifiers_compatible(&self.qualifiers, &other.qualifiers)
    }
}

/// True when `short` appears in `long` in order (not necessarily
/// contiguously), so `["engine", "core"]` is compatible with
/// `["engine", "core", "work"]` but not with `["cube"]`.
fn qualifiers_compatible(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut long_iter = long.iter();
    short.iter().all(|seg| long_iter.any(|candidate| candidate == seg))
}

/// The comparable shape of one candidate task title (+ description).
///
/// Build with [`fingerprint`]; compare with [`TaskFingerprint::duplicate_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFingerprint {
    file_target: Option<FileTarget>,
    module_targets: BTreeSet<String>,
    title_tokens: BTreeSet<String>,
}

/// Reduce a task's name (and optional description) to its comparable form.
///
/// The **title** is the authoritative source: it is short, on-point, and
/// written to identify the finding. The description is consulted only to
/// recover a file target the title omitted — descriptions are long enough
/// that harvesting module identifiers from them produces noise, not signal.
pub fn fingerprint(name: &str, description: Option<&str>) -> TaskFingerprint {
    let title_tokens_raw = split_tokens(name);

    let file_target = first_file_target(&title_tokens_raw)
        .or_else(|| description.and_then(|body| first_file_target(&split_tokens(body))));

    TaskFingerprint {
        file_target,
        module_targets: module_candidates_in(&title_tokens_raw),
        title_tokens: content_tokens(&title_tokens_raw),
    }
}

impl TaskFingerprint {
    /// Report the strongest signal on which `self` duplicates `other`, or
    /// `None` when the two describe different work.
    ///
    /// Signals are tried strongest-first so the recorded trace names the
    /// most defensible reason. A fingerprint that names a target only ever
    /// matches on that target: if both sides name files and the files
    /// differ, they are different findings and the normalized-title
    /// fallback must not second-guess that.
    pub fn duplicate_of(&self, other: &TaskFingerprint) -> Option<DuplicateMatch> {
        if let (Some(mine), Some(theirs)) = (&self.file_target, &other.file_target) {
            return mine.matches(theirs).then(|| DuplicateMatch {
                kind: MatchKind::FileTarget,
                key: render_file_key(mine, theirs),
            });
        }

        if let Some(shared) = self.module_targets.intersection(&other.module_targets).next() {
            return Some(DuplicateMatch {
                kind: MatchKind::ModuleTarget,
                key: shared.clone(),
            });
        }

        // Only one side names a file, or one side names a module the other
        // does not: the two are not comparable on a target, and asserting
        // sameness from prose alone is exactly the false suppression we
        // must avoid. Fall through to the title check only when *neither*
        // side offered a target at all.
        if self.file_target.is_some()
            || other.file_target.is_some()
            || !self.module_targets.is_empty()
            || !other.module_targets.is_empty()
        {
            return None;
        }

        (!self.title_tokens.is_empty() && self.title_tokens == other.title_tokens).then(|| DuplicateMatch {
            kind: MatchKind::NormalizedTitle,
            key: self.title_tokens.iter().cloned().collect::<Vec<_>>().join(" "),
        })
    }
}

/// Render the shared file key, preferring whichever side carried
/// qualifiers so the trace reads `engine/core/app.rs`, not a bare
/// `app.rs`, when either title bothered to say where the file lives.
fn render_file_key(a: &FileTarget, b: &FileTarget) -> String {
    let qualifiers = if a.qualifiers.len() >= b.qualifiers.len() {
        &a.qualifiers
    } else {
        &b.qualifiers
    };
    if qualifiers.is_empty() {
        return a.basename.clone();
    }
    format!("{}/{}", qualifiers.join("/"), a.basename)
}

/// First token in `tokens` that looks like a source file, reduced to a
/// [`FileTarget`]. "First" is deliberate: titles lead with their subject,
/// and a later path is usually incidental context ("… so it stops
/// tripping `CHECKS.yaml`").
fn first_file_target(tokens: &[String]) -> Option<FileTarget> {
    tokens.iter().find_map(|token| {
        let cleaned = strip_token_edges(token);
        if !is_file_token(cleaned) {
            return None;
        }
        let mut segments: Vec<String> = cleaned
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let basename = segments.pop()?;
        segments.retain(|seg| !QUALIFIER_NOISE_SEGMENTS.contains(&seg.as_str()));
        Some(FileTarget {
            basename,
            qualifiers: segments,
        })
    })
}

/// Words that carry no distinguishing meaning in a task title. Kept
/// short and generic on purpose — dropping domain verbs like "split" or
/// "extract" would collapse genuinely different findings about the same
/// area of the repo.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "in", "into", "is", "it", "its",
    "of", "on", "or", "our", "that", "the", "this", "to", "we", "with",
];

/// Lowercased content words with stopwords and size/count noise removed.
///
/// "Sizes and counts" is implemented as "contains a digit": `~2548`,
/// `3000-line`, and `12h` are all noise, and a source filename that
/// contains a digit is preserved explicitly because it is a target, not a
/// measurement.
fn content_tokens(tokens: &[String]) -> BTreeSet<String> {
    tokens
        .iter()
        .filter_map(|token| {
            let cleaned = strip_token_edges(token);
            if is_file_token(cleaned) {
                return Some(cleaned.to_ascii_lowercase());
            }
            let normalized = normalize_token(cleaned);
            if normalized.is_empty()
                || normalized.chars().any(|c| c.is_ascii_digit())
                || STOPWORDS.contains(&normalized.as_str())
            {
                return None;
            }
            Some(normalized)
        })
        .collect()
}
