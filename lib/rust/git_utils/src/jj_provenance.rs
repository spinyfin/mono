//! The rule for deciding that one jj commit is a recorded predecessor of a
//! commit that a head already contains.
//!
//! Matching change ids alone never establish provenance: a divergent variant
//! of a change shares its change id with the commit it competes with. A
//! commit `old` is a proven predecessor only when, among the commits of
//! change `change(old)` inside the head's history, there is exactly one
//! candidate and `old` appears in that candidate's `jj evolog`.
//!
//! cube (synchronous) and the boss engine's recovery (async) both rely on this
//! rule in the same shared jj store, so the revset/template construction and
//! the decision live here as pure functions that take already-run jj output.
//! Neither caller's process runner leaks into this crate.

/// `jj log` / `jj evolog` template printing one commit id per line.
pub const COMMIT_ID_LINES_TEMPLATE: &str = "commit_id ++ \"\\n\"";

/// Revset for the commits of `change_id` that exist inside `scope_revset`
/// (e.g. `::head`). `present` keeps a change unknown to the store from being
/// an error.
pub fn candidate_revset(change_id: &str, scope_revset: &str) -> String {
    format!("present({change_id}) & ({scope_revset})")
}

/// The single candidate commit in `candidates_output` (one id per line).
/// Zero or several candidates means no provenance can be proven.
pub fn sole_candidate(candidates_output: &str) -> Option<&str> {
    let mut seen: Vec<&str> = Vec::new();
    for line in candidates_output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !seen.contains(&line) {
            seen.push(line);
        }
    }
    match seen.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// Whether `commit` is a recorded predecessor in `evolog_output`, the
/// [`COMMIT_ID_LINES_TEMPLATE`] rendering of `jj evolog -r <candidate>`.
pub fn evolution_contains(evolog_output: &str, commit: &str) -> bool {
    evolog_output.lines().any(|line| line.trim() == commit)
}

/// Run the whole check through a synchronous jj runner. `run` receives jj
/// arguments (without any global flags the caller wants to add itself, which
/// it can prepend) and returns stdout.
///
/// Returns the head-side commit `commit` evolved into (or `commit` itself when
/// it is already the candidate), `None` when provenance is not proven. An
/// evolog failure is returned as an error so the caller can say why.
pub fn proven_successor<E>(
    mut run: impl FnMut(&[&str]) -> Result<String, E>,
    change_id: &str,
    scope_revset: &str,
    commit: &str,
) -> Result<Option<String>, E> {
    let revset = candidate_revset(change_id, scope_revset);
    let candidates = run(&["log", "--no-graph", "-r", &revset, "-T", COMMIT_ID_LINES_TEMPLATE])?;
    let Some(candidate) = sole_candidate(&candidates) else {
        return Ok(None);
    };
    if candidate == commit {
        return Ok(Some(candidate.to_owned()));
    }
    let evolution = run(&["evolog", "--no-graph", "-r", candidate, "-T", COMMIT_ID_LINES_TEMPLATE])?;
    Ok(evolution_contains(&evolution, commit).then(|| candidate.to_owned()))
}

/// Whether `commit` is a rewrite (rebase, amend, conflict resolution) of any
/// commit in `originals`: some recorded original appears in its evolution log.
/// This is how a never-published intermediate (a conflicted rebase result) is
/// tied back to the published head it was rewritten from. An evolog failure is
/// returned as an error so the caller can say why.
pub fn is_rewrite_of_any<E>(
    mut run: impl FnMut(&[&str]) -> Result<String, E>,
    commit: &str,
    originals: &[String],
) -> Result<bool, E> {
    if originals.is_empty() {
        return Ok(false);
    }
    let evolution = run(&["evolog", "--no-graph", "-r", commit, "-T", COMMIT_ID_LINES_TEMPLATE])?;
    Ok(originals
        .iter()
        .any(|original| original != commit && evolution_contains(&evolution, original)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_revset_scopes_the_change() {
        assert_eq!(candidate_revset("abc", "::h1 & ::h2"), "present(abc) & (::h1 & ::h2)");
    }

    #[test]
    fn sole_candidate_requires_exactly_one_distinct_commit() {
        assert_eq!(sole_candidate("c1\n"), Some("c1"));
        assert_eq!(sole_candidate("c1\nc1\n"), Some("c1"));
        assert_eq!(sole_candidate(""), None);
        assert_eq!(sole_candidate("c1\nc2\n"), None);
    }

    #[test]
    fn evolution_contains_matches_whole_lines_only() {
        assert!(evolution_contains("new\nold\n", "old"));
        assert!(!evolution_contains("new\nolder\n", "old"));
    }

    #[test]
    fn rewrite_of_any_requires_a_recorded_original_in_the_evolog() {
        let run = |evolog: &'static str| move |_: &[&str]| -> Result<String, ()> { Ok(evolog.to_owned()) };
        let recorded = vec!["orig".to_string()];
        assert_eq!(is_rewrite_of_any(run("new\norig\n"), "new", &recorded), Ok(true));
        assert_eq!(is_rewrite_of_any(run("new\nother\n"), "new", &recorded), Ok(false));
        assert_eq!(is_rewrite_of_any(run("orig\n"), "orig", &recorded), Ok(false));
        assert_eq!(is_rewrite_of_any(run("new\norig\n"), "new", &[]), Ok(false));
    }

    #[test]
    fn proven_successor_decides_from_candidates_and_evolog() {
        let run = |candidates: &'static str, evolog: &'static str| {
            move |args: &[&str]| -> Result<String, ()> {
                Ok(if args[0] == "log" { candidates } else { evolog }.to_owned())
            }
        };
        assert_eq!(
            proven_successor(run("head\n", "head\nold\n"), "ch", "::h", "old"),
            Ok(Some("head".into()))
        );
        assert_eq!(
            proven_successor(run("old\n", ""), "ch", "::h", "old"),
            Ok(Some("old".into()))
        );
        assert_eq!(
            proven_successor(run("head\n", "head\nother\n"), "ch", "::h", "old"),
            Ok(None)
        );
        assert_eq!(
            proven_successor(run("a\nb\n", "a\nold\n"), "ch", "::h", "old"),
            Ok(None)
        );
        assert_eq!(proven_successor(run("", ""), "ch", "::h", "old"), Ok(None));
    }
}
