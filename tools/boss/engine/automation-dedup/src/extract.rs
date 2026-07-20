//! Lexical helpers: turning free-form title/description prose into the
//! tokens [`crate::fingerprint`] compares.
//!
//! Everything here is deliberately dumb string work. Task titles are one
//! line of human-ish English written by an LLM; the value is in picking
//! out the handful of tokens that identify *what the finding is about*,
//! not in parsing the sentence.

use std::collections::BTreeSet;

/// Path segments that say nothing about *which* file is meant — they
/// appear in every path in the repo. Dropping them lets a title that
/// says `engine/core/app.rs` match one that says `engine/core/src/app.rs`.
pub(crate) const QUALIFIER_NOISE_SEGMENTS: &[&str] = &["src", "source", "sources", "tools", "lib", "libs", "crates"];

/// Extensions that make a token a source-file reference. Broad enough to
/// cover every language in the repo; anything not listed (`v1.2`, `3.5x`)
/// falls through and is treated as prose.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "swift", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "kt", "kts", "c", "h", "cc", "cpp", "hpp", "m",
    "mm", "sh", "bash", "zsh", "toml", "yaml", "yml", "json", "md", "sql", "proto", "bzl", "bazel", "gradle",
];

/// Words that mark their neighbours as crate / module identifiers, so
/// "Extract `metrics` crate" yields `metrics` even though the word itself
/// carries no `_` or `::` to give it away.
const MODULE_ANCHOR_WORDS: &[&str] = &["crate", "crates", "module", "modules"];

/// Anchor-adjacent words that are grammar, not identifiers. Without this,
/// "extract into its own crate" would offer `own` as a module target.
const ANCHOR_ADJACENT_NOISE: &[&str] = &[
    "a",
    "an",
    "the",
    "its",
    "it",
    "own",
    "new",
    "this",
    "that",
    "separate",
    "dedicated",
    "into",
    "from",
    "to",
    "of",
    "in",
    "and",
    "or",
    "as",
    "per",
];

/// Split prose into whitespace-delimited tokens, preserving the
/// intra-token punctuation (`/`, `.`, `_`, `:`, `-`) that path and module
/// references are built from.
pub(crate) fn split_tokens(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// Trim the punctuation that wraps a token in prose — backticks, quotes,
/// brackets, and trailing sentence punctuation — without touching the
/// separators inside it. `` (`engine/core/app.rs`), `` becomes
/// `engine/core/app.rs`.
pub(crate) fn strip_token_edges(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '*'
                | '~'
                | '<'
                | '>'
                | '.'
                | '-'
        )
    })
}

/// Does this token name a source file? True when its final `.`-suffix is
/// a known source extension, so `app.rs` and `engine/core/src/app.rs`
/// qualify but `3000-line` and `v1.2` do not.
pub(crate) fn is_file_token(token: &str) -> bool {
    let basename = token.rsplit('/').next().unwrap_or(token);
    let Some((stem, ext)) = basename.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Reduce a prose word to its comparable form: lowercased, with anything
/// that is not alphanumeric / `_` / `-` removed. Returns an empty string
/// for tokens that are pure punctuation.
pub(crate) fn normalize_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Collect the crate / module identifiers a title names.
///
/// Four shapes count, and every one of them is a deliberate signal that
/// the author meant a specific code unit rather than an English word:
///
/// - **backtick-quoted** — `` `metrics` ``; the author marked it as code.
/// - **`::`-qualified** — `work::automations`, reduced to its last segment
///   so `crate::work` and `work` converge.
/// - **`snake_case`** — `pr_review`; no English word carries an underscore.
/// - **anchor-adjacent** — the word beside "crate"/"module".
///
/// Returned as a set and matched by intersection, so a title naming two
/// units matches another naming either of them.
///
/// Title-only by design: descriptions are long enough that these shapes
/// appear incidentally (in a quoted error, a path, a code sample), and a
/// single stray `snake_case` word shared by two unrelated descriptions
/// would suppress a real finding.
pub(crate) fn module_candidates_in(tokens: &[String]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();

    for (index, token) in tokens.iter().enumerate() {
        let cleaned = strip_token_edges(token);

        // File references are targets, but they are signal 1's job — and
        // letting `app.rs` through here as `app` would make it collide
        // with an unrelated `app` module.
        if is_file_token(cleaned) {
            continue;
        }

        if token.starts_with('`')
            && let Some(identifier) = as_identifier(cleaned)
        {
            found.insert(identifier);
            continue;
        }

        if cleaned.contains("::")
            && let Some(last) = cleaned.rsplit("::").next()
            && let Some(identifier) = as_identifier(last)
        {
            found.insert(identifier);
            continue;
        }

        if cleaned.contains('_')
            && let Some(identifier) = as_identifier(cleaned)
        {
            found.insert(identifier);
            continue;
        }

        if MODULE_ANCHOR_WORDS.contains(&normalize_token(cleaned).as_str()) {
            found.extend(anchor_neighbours(tokens, index));
        }
    }

    found
}

/// The identifier-shaped words immediately before and after an
/// anchor word, skipping grammar. Both sides are checked because English
/// puts the identifier on either one: "the `metrics` crate" versus
/// "crate `metrics`".
fn anchor_neighbours(tokens: &[String], anchor_index: usize) -> Vec<String> {
    let mut neighbours = Vec::new();
    let before = anchor_index.checked_sub(1).and_then(|i| tokens.get(i));
    let after = tokens.get(anchor_index + 1);

    for neighbour in [before, after].into_iter().flatten() {
        let cleaned = strip_token_edges(neighbour);
        if is_file_token(cleaned) {
            continue;
        }
        let Some(identifier) = as_identifier(cleaned) else {
            continue;
        };
        if ANCHOR_ADJACENT_NOISE.contains(&identifier.as_str()) || MODULE_ANCHOR_WORDS.contains(&identifier.as_str()) {
            continue;
        }
        neighbours.push(identifier);
    }
    neighbours
}

/// Accept `token` as a code identifier, or reject it.
///
/// Identifiers are alphanumeric plus `_`/`-`, start with a letter, and are
/// at least three characters — short enough tokens ("id", "fn", "a") are
/// noise however they are punctuated. A token containing a digit is still
/// fine (`utf8_decoder`), but one that is *only* digits is not.
fn as_identifier(token: &str) -> Option<String> {
    let normalized = normalize_token(token);
    if normalized.len() < 3 {
        return None;
    }
    if !normalized.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(normalized)
}
