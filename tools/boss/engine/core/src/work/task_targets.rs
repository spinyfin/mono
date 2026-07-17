//! `task_targets`: declared (and, later, actual) files/symbols a task
//! touches, and the pre-file dedup gate built on top of them
//! (investigation `automation-duplicate-work-2026-07-14.md` §4 Layer 1).
//!
//! The gate is deliberately high-precision only (§5.2: "start thresholds
//! strict"): it suppresses a candidate automation task only when its
//! declared file set is non-empty and is a subset of (or equal to) an
//! already-open automation-sourced task's declared file set in the same
//! product, *and* the two names/descriptions clear a token-overlap
//! threshold. Same-file-different-work (the #1945/#1955 pattern, doc §1.4)
//! passes through untouched — a fuzzier post-hoc detector is layer 2's job
//! (`merge_poller`), not this gate's.

use super::*;

/// Starting threshold for the name/description token-overlap tie-breaker
/// (Jaccard similarity over lowercased word tokens). File-set subset/
/// equality is the high-precision structural signal; this threshold only
/// guards against two automations that happen to name the same single file
/// while doing genuinely different work. Per doc §5.2, start strict and
/// tune from layer-2 residual data — do not loosen this without evidence.
pub(crate) const DUPLICATE_GATE_TOKEN_OVERLAP_THRESHOLD: f64 = 0.3;

/// Normalize a declared target file path for comparison: trim whitespace
/// and a leading `./`. Paths are otherwise compared verbatim (case-
/// sensitive, no canonicalization) — the gate only needs to catch the
/// exact-path-repeated case the incident demonstrated, not fuzzy path
/// matching.
pub(crate) fn normalize_target_file(raw: &str) -> String {
    raw.trim().trim_start_matches("./").to_owned()
}

/// Lowercased alphanumeric word tokens, 3+ chars, used for the name/
/// description overlap tie-breaker. Short tokens ("a", "to", "in") are
/// dropped as noise.
fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|tok| tok.len() > 2)
        .map(str::to_owned)
        .collect()
}

/// Jaccard similarity between two token sets. `0.0` when either side is
/// empty (an empty description must never look like a "match").
fn token_overlap_ratio(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    intersection as f64 / union as f64
}

/// Insert `task_targets` rows for a freshly created task. Blank entries
/// (after trimming) are dropped rather than stored — an agent-supplied
/// `--target-file ""` declares nothing.
pub(crate) fn insert_task_targets_in_tx(
    conn: &Connection,
    task_id: &str,
    target_files: &[String],
    target_symbols: &[String],
) -> Result<()> {
    let now = now_string();
    for raw in target_files {
        let value = normalize_target_file(raw);
        if value.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO task_targets (id, task_id, kind, value, created_at)
             VALUES (?1, ?2, 'file', ?3, ?4)",
            params![next_id("tgt"), task_id, value, now],
        )?;
    }
    for raw in target_symbols {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO task_targets (id, task_id, kind, value, created_at)
             VALUES (?1, ?2, 'symbol', ?3, ?4)",
            params![next_id("tgt"), task_id, value, now],
        )?;
    }
    Ok(())
}

/// Declared target files for `task_id`, normalized. Empty when the task
/// declared none.
pub(crate) fn declared_target_files_in_tx(conn: &Connection, task_id: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT value FROM task_targets WHERE task_id = ?1 AND kind = 'file'")?;
    let rows = stmt.query_map([task_id], |row| row.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for value in rows {
        set.insert(value?);
    }
    Ok(set)
}

/// The open automation-sourced task a candidate collided with.
pub(crate) struct DuplicateGateBlocker {
    pub task_id: String,
    pub short_id: Option<i64>,
    pub name: String,
}

/// Find an open automation-sourced task (any automation — the cross-
/// automation blindness is exactly the bug, doc §1.3) in `product_id` whose
/// declared target files are a superset of (or equal to) `candidate_files`,
/// with name/description token overlap at or above
/// [`DUPLICATE_GATE_TOKEN_OVERLAP_THRESHOLD`]. Returns the first match
/// (open tasks are scanned oldest-first, so a real collision surfaces
/// against the *original* row, not a later duplicate of it).
///
/// Returns `Ok(None)` immediately when `candidate_files` is empty — an
/// undeclared candidate has nothing structural to compare against and is
/// never gated (high precision only).
pub(crate) fn find_duplicate_gate_blocker(
    conn: &Connection,
    product_id: &str,
    candidate_name: &str,
    candidate_description: &str,
    candidate_files: &HashSet<String>,
) -> Result<Option<DuplicateGateBlocker>> {
    if candidate_files.is_empty() {
        return Ok(None);
    }
    let candidate_tokens = tokenize(&format!("{candidate_name} {candidate_description}"));

    let mut stmt = conn.prepare(
        "SELECT id, short_id, name, description FROM tasks
          WHERE product_id = ?1
            AND source_automation_id IS NOT NULL
            AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
            AND deleted_at IS NULL
          ORDER BY created_at ASC, id ASC",
    )?;
    let rows: Vec<(String, Option<i64>, String, String)> = stmt
        .query_map([product_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (task_id, short_id, name, description) in rows {
        let existing_files = declared_target_files_in_tx(conn, &task_id)?;
        if existing_files.is_empty() || !candidate_files.is_subset(&existing_files) {
            continue;
        }
        let existing_tokens = tokenize(&format!("{name} {description}"));
        if token_overlap_ratio(&candidate_tokens, &existing_tokens) >= DUPLICATE_GATE_TOKEN_OVERLAP_THRESHOLD {
            return Ok(Some(DuplicateGateBlocker {
                task_id,
                short_id,
                name,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_whitespace_and_leading_dot_slash() {
        assert_eq!(
            normalize_target_file("  ./engine/core/src/runner.rs  "),
            "engine/core/src/runner.rs"
        );
        assert_eq!(
            normalize_target_file("engine/core/src/runner.rs"),
            "engine/core/src/runner.rs"
        );
    }

    #[test]
    fn tokenize_drops_short_tokens_and_lowercases() {
        let tokens = tokenize("Dedup runner extract_pr_number on boss_github::pr_url");
        assert!(tokens.contains("dedup"));
        assert!(tokens.contains("runner"));
        assert!(tokens.contains("extract_pr_number"));
        // `::` splits like any other non-alphanumeric separator.
        assert!(tokens.contains("boss_github"));
        assert!(tokens.contains("pr_url"));
        // Below the length-3 floor.
        assert!(!tokens.contains("on"));
    }

    #[test]
    fn token_overlap_ratio_is_symmetric_and_bounded() {
        let a = tokenize("dedup runner extract_pr_number");
        let b = tokenize("route runner pr number parsing through boss_github helper");
        let ratio_ab = token_overlap_ratio(&a, &b);
        let ratio_ba = token_overlap_ratio(&b, &a);
        assert_eq!(ratio_ab, ratio_ba);
        assert!((0.0..=1.0).contains(&ratio_ab));
    }

    #[test]
    fn token_overlap_ratio_zero_when_either_side_empty() {
        let a = tokenize("dedup runner");
        let empty = HashSet::new();
        assert_eq!(token_overlap_ratio(&a, &empty), 0.0);
        assert_eq!(token_overlap_ratio(&empty, &a), 0.0);
    }

    #[test]
    fn identical_text_has_full_overlap() {
        let a = tokenize("fix clippy warnings in the foo crate");
        let b = tokenize("fix clippy warnings in the foo crate");
        assert_eq!(token_overlap_ratio(&a, &b), 1.0);
    }
}
