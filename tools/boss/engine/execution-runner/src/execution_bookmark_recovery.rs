use boss_protocol::WorkExecution;

pub fn worker_instructions(execution: &WorkExecution) -> String {
    let bookmark = format!("boss-recovery/{}", execution.id);
    let publication = format!("boss/{}", execution.id);
    let text = format!(
        "## Execution recovery bookmark\n\nThe engine created and owns recovery bookmark `{bookmark}` and publication bookmark `{publication}` in the shared jj store for this execution, including revisions. Advance both existing bookmarks to `@` before editing a new change (`jj bookmark set {bookmark} {publication} -r @`). jj follows rewrites of that change automatically. After every `jj new`, `jj commit`, split, squash, rebase, or checkout, advance it again before editing; checkpoint file edits with `jj status` and keep the bookmark at the latest work before ending a turn or stopping. Never delete or push the recovery bookmark. Its separate namespace retains work even when cube cleans up a merged publication bookmark. A revision still publishes only to its existing PR branch; its execution bookmark is a local recovery pointer.\n\nThe engine has already positioned this lease. Inspect its current changes before any checkout; do not reset it to main or re-run PR positioning from generic setup instructions. If you produce nothing, leave the engine-created bookmark in place so an empty run remains distinguishable from a missing recovery pointer.\n\n"
    );
    text
}

/// `reconciliation_note` is the persisted [`Reconciliation::note`] when
/// dispatch had to re-anchor the preserved pointers before recovering them.
///
/// [`Reconciliation::note`]: boss_engine_recovery::execution_bookmark::Reconciliation::note
pub fn recovery_instructions(
    recovery: Option<&(String, bool)>,
    report: Option<&boss_engine_recovery::execution_bookmark::RestoreReport>,
    reconciliation_note: Option<&str>,
) -> String {
    let note = reconciliation_note.unwrap_or_default();
    if let Some(report) = report {
        return format!("{}{note}", report.instructions());
    }
    let Some((predecessor, has_work)) = recovery else {
        return note.to_owned();
    };
    let state = if *has_work {
        "Unpublished changes and their history were recovered into this lease. Stay at `@`; do not reset or repeat PR checkout instructions."
    } else {
        "The predecessor has no unpublished changes to recover. The engine positioned this lease normally."
    };
    format!(
        "## EXECUTION BOOKMARK RECOVERY\n\nThe engine resolved predecessor `{predecessor}` through its recorded bookmark in the shared jj store. {state} The old workspace was not used. Inspect `jj status`, `jj diff --stat`, and the inherited history. Re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n{note}"
    )
}
