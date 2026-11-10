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
     work out why it is stuck and what, if anything, would move it forward.\n\
     You hold no code checkout and no workspace lease.\n\
     \n\
     ## Read-only mandate (HARD CONSTRAINT)\n\
     \n\
     You can only look and declare. Everything else is denied at the dispatch\n\
     layer (deny-by-default `dontAsk` permission mode plus an explicit deny\n\
     list) — this is enforced, not advisory.\n\
     \n\
     What you can do:\n\
     \n\
     - Read files and search with `Read`, `Grep` and `Glob`.\n\
     - Inspect work state with `{boss} task show` / `{boss} task list` and your\n\
       item's PR with `{boss} pr status`.\n\
     - Inspect pull requests with `gh pr view` and `gh pr checks`.\n\
     - Declare how the run ended with `{boss} propose done`.\n\
     \n\
     What is denied:\n\
     \n\
     - Editing or writing any file (`Edit`, `Write`, `NotebookEdit`).\n\
     - Any `jj`, `git` or `cube` command. There is no repository for you to\n\
       operate on; the item's code lives in other workers' workspaces.\n\
     - Creating, editing, commenting on, reviewing, merging or closing pull\n\
       requests or issues, and `gh api` calls that write.\n\
     \n\
     ## Ending the run\n\
     \n\
     Finish by running `{boss} propose done --outcome <outcome> --summary\n\
     \"<one line>\"` as your last command. Describe what you found in the\n\
     summary. There is no pull request to open and no URL to print.\n\
     \n\
     The engine may refuse `{boss} propose done` for a progression run until\n\
     progression declarations are enabled. If it does, do not retry it and do\n\
     not try other proposal kinds: put your findings in your final message and\n\
     end your turn.\n\
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
    fn claude_md_states_read_only_mandate_and_teaches_no_pr_workflow() {
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
    fn claude_md_tells_the_agent_not_to_retry_a_refused_propose_done() {
        let md = render_progression_claude_md("/scratch/x", "\"$BOSS_BIN\"");
        assert!(md.contains("may refuse `\"$BOSS_BIN\" propose done`"));
        assert!(md.contains("do not retry it"));
        assert!(md.contains("put your findings in your final message and\nend your turn"));
    }
}
