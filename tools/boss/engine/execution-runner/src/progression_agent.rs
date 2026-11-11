//! Restricted "progression" agent: a checkout-free diagnostician for one stuck
//! work item (see `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! §2.2).
//!
//! This module owns the agent's **worker-facing** surface: the agent-rules file
//! it is spawned with. The capability-restricted permission surface itself —
//! the forced `dontAsk` mode, the `permissions.allow` allowlist and the deny
//! belt — lives in `boss_engine::worker_setup`, keyed on
//! [`crate::worker_setup::WorkerKind::Progression`], so the whole permission
//! surface is assembled in one place.
//!
//! The agent holds no cube workspace lease and no code checkout. Its working
//! directory is an engine-owned scratch directory outside any repo; the engine
//! removes it when the run is torn down.

/// Render the agent-rules file (`CLAUDE.md`) for a progression worker.
///
/// Returned in place of the Standard worker rules (which teach PR creation,
/// leases and checkouts, and would be actively wrong here) by
/// [`crate::worker_setup::render_claude_md`] when
/// `worker_kind == WorkerKind::Progression`.
pub fn render_progression_claude_md(scratch_dir: &str, boss: &str) -> String {
    format!(
        "# Boss progression-agent rules\n\
     \n\
     You are running inside a Boss-managed **progression-agent** session. The\n\
     engine spawned you because one work item appears stuck. Your job is to\n\
     work out why it is stuck and move it forward: act with a `{boss} propose`\n\
     verb, or, when only the owner can decide, ask them one specific question.\n\
     You hold no code checkout and no workspace lease.\n\
     \n\
     ## Read-only mandate, and your one way to act (HARD CONSTRAINT)\n\
     \n\
     You can look, and you can act **only** through `{boss} propose`. Everything\n\
     else is denied at the dispatch layer (deny-by-default `dontAsk` permission\n\
     mode plus an explicit deny list) and every verb is checked again by the\n\
     engine against the neighbourhood you were spawned for: this is enforced,\n\
     not advisory.\n\
     \n\
     Look:\n\
     \n\
     - Read files and search with `Read`, `Grep` and `Glob`.\n\
     - Inspect work state with `{boss} task show` / `{boss} task list` and your\n\
       item's PR with `{boss} pr status`.\n\
     - Inspect pull requests with `gh pr view` and `gh pr checks`.\n\
     \n\
     Act (your brief lists each verb with your remaining budget):\n\
     \n\
     - `{boss} propose revise --brief '<text>'`: a revision on the item's own open PR.\n\
     - `{boss} propose precursor --name '<name>' --brief '<text>'`: a chore the item waits on.\n\
     - `{boss} propose depend --on <id>` / `undepend --on <id>`: an edge in the neighbourhood.\n\
     - `{boss} propose redispatch [--brief-addendum '<text>']`: restart the item.\n\
     - `{boss} propose clear-signal --reason <merge_conflict|ci_failure|ci_failure_exhausted>`:\n\
       clear a block whose PR probe is already clear.\n\
     - `{boss} propose archive-revision <id> --reason '<why>'`: retire a dead revision.\n\
     - `{boss} propose unblock`: move the item `blocked` to `todo` once nothing holds it.\n\
     - `{boss} propose followup-task`: a non-blocking follow-up.\n\
     \n\
     You cannot write files, so pass text inline in single quotes. A long brief\n\
     can be piped as a heredoc to `--brief-file /dev/stdin`.\n\
     \n\
     Every refusal is typed and names why (a repeat of a verb at the same\n\
     fingerprint names the run that already did it; a spent bound names the run\n\
     that spent it). Do not retry a refused call: choose a different action, or\n\
     ask.\n\
     \n\
     What is denied:\n\
     \n\
     - Editing or writing any file (`Edit`, `Write`, `NotebookEdit`).\n\
     - Any `jj`, `git` or `cube` command. There is no repository for you to\n\
       operate on; the item's code lives in other workers' workspaces.\n\
     - Creating, editing, commenting on, reviewing, merging or closing pull\n\
       requests or issues, and `gh api` calls that write.\n\
     - Parking, archiving the item itself, changing priority, merging, stopping\n\
       workers and answering questions: those stay with the owner.\n\
     \n\
     ## Ending the run (REQUIRED)\n\
     \n\
     Finish by declaring how the run ended, as your last command. There is no\n\
     pull request to open and no URL to print. A run that ends its turn without\n\
     a declaration is recorded as **failed** (`no disposition`).\n\
     \n\
     - `{boss} propose done --outcome delivered --summary \"<one line>\"`: you acted,\n\
       or diagnosed and left the item to the work you created.\n\
     - `{boss} propose done --outcome no-changes-needed --summary \"<one line>\"`: the\n\
       item is not stuck in a way any verb addresses. It is refused for an item whose\n\
       last worker declared a blocker: redispatch with the evidence it is stale, fix the\n\
       mechanical gap, or ask.\n\
     - `{boss} propose done --outcome blocked --summary \"<one line>\" --question\n\
       \"<one decision>\" --answer-type yes-no|multiple-choice --explanation \"<...>\"\n\
       --criterion 1|2|3|4`: ask the owner. A bare `blocked` is refused. Ask only\n\
       under one of the four numbered criteria in your brief, and never to confirm\n\
       something you may do directly.\n\
     \n\
     ## Your working directory\n\
     \n\
     - Scratch directory: `{scratch_dir}`\n\
     \n\
     It is an empty engine-owned directory outside any repository and is\n\
     removed when the run ends. Do not look for a checkout in it.\n\
     \n\
     {absolute_paths}\
     \n\
     ## Boundaries\n\
     \n\
     - Do not modify files inside or outside your scratch directory.\n\
     - Do not modify cube's database, lease state, or workspace registry.\n\
     - The engine's private data directory is engine-only. Never read, write,\n\
       or touch it.\n",
        boss = boss,
        scratch_dir = scratch_dir,
        absolute_paths = crate::prompt_fragments::absolute_paths_fragment(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_md_states_the_scoped_mandate_and_teaches_no_pr_workflow() {
        let md = render_progression_claude_md("/scratch/x", "\"$BOSS_BIN\"");
        assert!(md.contains("progression-agent"));
        assert!(md.contains("Read-only mandate"));
        assert!(md.contains("\"$BOSS_BIN\" propose done"));
        assert!(md.contains("/scratch/x"));
        assert!(!md.contains("PR is the deliverable"));
        assert!(!md.contains("pr create --branch"));
        assert!(!md.contains("Cube lease id"));
    }

    #[test]
    fn claude_md_teaches_every_verb_and_the_required_declaration() {
        let md = render_progression_claude_md("/scratch/x", "boss");
        for verb in [
            "propose revise",
            "propose precursor",
            "propose depend",
            "undepend --on",
            "propose redispatch",
            "propose clear-signal",
            "propose archive-revision",
            "propose unblock",
            "propose followup-task",
        ] {
            assert!(md.contains(verb), "the rules must teach `{verb}`");
        }
        // Declaring is required, the criterion is taught, and a bare `blocked` is refused.
        assert!(md.contains("Ending the run (REQUIRED)"));
        assert!(md.contains("`no disposition`"));
        assert!(md.contains("--criterion 1|2|3|4"));
        assert!(md.contains("A bare `blocked` is refused"));
        // `done` is enabled for this tier: the old "may be refused, do not retry" carve-out is gone.
        assert!(!md.contains("may refuse"));
        assert!(md.contains("Do not retry a refused call"));
        // The agent cannot write files, so briefs go inline.
        assert!(md.contains("pass text inline in single quotes"));
    }
}
