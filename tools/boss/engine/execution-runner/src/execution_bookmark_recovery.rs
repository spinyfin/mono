pub fn worker_instructions() -> &'static str {
    "## Workspace state\n\nThe engine has already positioned this lease. Inspect its current changes before any checkout; do not reset it to main or re-run PR positioning from generic setup instructions. Checkpoint file edits with `jj status` before ending a turn or stopping. The engine preserves unpublished work from the workspace when the execution ends.\n\n"
}

pub fn recovery_instructions(
    recovery: Option<&(String, bool)>,
    report: Option<&boss_engine_recovery::execution_bookmark::RestoreReport>,
) -> String {
    if let Some(report) = report {
        return report.instructions();
    }
    let Some((predecessor, has_work)) = recovery else {
        return String::new();
    };
    let state = if *has_work {
        "Unpublished changes and their history were recovered into this lease. Stay at `@`; do not reset or repeat PR checkout instructions."
    } else {
        "The predecessor has no unpublished changes to recover. The engine positioned this lease normally."
    };
    format!(
        "## EXECUTION BOOKMARK RECOVERY\n\nThe engine resolved predecessor `{predecessor}` through its recorded bookmark in the shared jj store. {state} The old workspace was not used. Inspect `jj status`, `jj diff --stat`, and the inherited history. Re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n"
    )
}
