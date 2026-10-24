//! Test-only scanner for engine-internal vocabulary in worker-bound text.
//!
//! Workers echo whatever nouns the surrounding prompt uses into PR titles and
//! bodies, commit messages, code comments and docs. Terms like "the brief",
//! "the operator" or "work item" mean nothing to a reader of the PR, so the
//! reviewer flags them. The cheapest fix is to never put those words in front
//! of the worker in the first place; tests across the engine run their
//! worker-bound strings through [`forbidden_prose`] to keep them out.
//!
//! Only prose is checked. Structured markers a worker has to read or echo back
//! to the engine (`chore_implementation` execution kinds, `boss-recovery/`
//! bookmarks, `$BOSS_BIN` verbs, `[blocked]` markers, the `chore` work-kind
//! enum value) are single identifiers or fixed literals; the identifiers never
//! match the word-level scan and the literals are listed in
//! [`STRUCTURED_MARKERS`] and stripped first.

use std::sync::LazyLock;

use regex::Regex;

/// Fixed literals a worker must read or echo verbatim. Each is removed from
/// the text before the prose scan.
pub(crate) const STRUCTURED_MARKERS: &[&str] = &[
    // The `proposed_work_kind` / `--work-kind` enum values accepted by the
    // follow-up proposal verb.
    "one of `task` | `chore` | `project` (defaults to `task`)",
    "one of `task` | `chore` | `project` (defaults to `chore`)",
    // Heading the engine appends to a task description when a question is
    // answered; the prompt has to name it so the worker can find it.
    "`## Operator authorization`",
    // The flag that carries a prerequisite task's description.
    "--prerequisite-brief",
    // The engine's on-disk state directory, which workers are told to keep
    // out of.
    "~/Library/Application Support/Boss/",
];

/// Prose nouns that only make sense inside the engine. Matched as whole
/// words, so identifiers such as `chore_implementation` or `$BOSS_BIN` never
/// match; `Boss` is case-sensitive so `boss-recovery/…` and `boss propose` do
/// not match either.
static FORBIDDEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \b(?i:briefs?|operators?|coordinators?|chores?)\b
        | \b(?i:work[\s-]+items?)\b
        | \bBoss\b
        | \b[TP][0-9]+\b
        ",
    )
    .unwrap()
});

/// Every forbidden-prose hit in `text`, each rendered with the line it is on.
/// Empty when the text is clean.
pub(crate) fn forbidden_prose(text: &str) -> Vec<String> {
    let mut scanned = text.to_owned();
    for marker in STRUCTURED_MARKERS {
        scanned = scanned.replace(marker, "");
    }
    FORBIDDEN
        .find_iter(&scanned)
        .map(|m| {
            let start = scanned[..m.start()].rfind('\n').map_or(0, |i| i + 1);
            let end = scanned[m.end()..].find('\n').map_or(scanned.len(), |i| m.end() + i);
            format!("`{}` in: {}", m.as_str(), &scanned[start..end])
        })
        .collect()
}

/// Assert `text` is clean, naming `label` in the failure.
#[track_caller]
pub(crate) fn assert_no_forbidden_prose(label: &str, text: &str) {
    let hits = forbidden_prose(text);
    assert!(
        hits.is_empty(),
        "{label} leaks internal vocabulary ({} hits):\n{}",
        hits.len(),
        hits.join("\n"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_catches_the_terms_it_exists_for() {
        for sample in [
            "Read the brief carefully.",
            "ask the operator",
            "tell the coordinator",
            "this chore is small",
            "the work item is done",
            "a Work-Item row",
            "Boss's poller",
        ] {
            assert!(!forbidden_prose(sample).is_empty(), "scan missed: {sample}");
        }
        // Short work-item id shapes, assembled at run time so this file does
        // not itself contain one.
        for prefix in ["T", "P"] {
            let sample = format!("see {prefix}{} for context", 1234);
            assert!(!forbidden_prose(&sample).is_empty(), "scan missed: {sample}");
        }
    }

    #[test]
    fn scan_ignores_structured_markers_and_lookalikes() {
        for sample in [
            "- execution kind: `chore_implementation`",
            "bookmark `boss-recovery/exec_abc` and `\"$BOSS_BIN\" propose done`",
            "- `proposed_work_kind` (optional): one of `task` | `chore` | `project` (defaults to `task`).",
            "check for a `## Operator authorization` section",
            "the briefly stated plan, chorewalk, operatorless",
        ] {
            assert_eq!(
                forbidden_prose(sample),
                Vec::<String>::new(),
                "false positive: {sample}"
            );
        }
    }
}
