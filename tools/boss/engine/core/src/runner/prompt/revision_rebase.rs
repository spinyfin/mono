//! Shared revision rebase and publication contract.

pub(crate) fn revision_conflict_notice(
    db: &crate::work::WorkDb,
    execution: &crate::work::WorkExecution,
) -> anyhow::Result<String> {
    let root = db
        .get_revision_chain_root_task_id(&execution.work_item_id)
        .unwrap_or_else(|| execution.work_item_id.clone());
    let Some(snapshot) = db.get_pr_status_snapshot(&root)? else {
        return Ok(String::new());
    };
    if snapshot.mergeable.as_deref() != Some("CONFLICTING") && snapshot.merge_state_status.as_deref() != Some("DIRTY") {
        return Ok(String::new());
    }
    let files = db.revision_conflicting_files(&execution.work_item_id)?;
    let files = if files.is_empty() {
        "The engine has no conflicting-file list; obtain it from the mandatory rebase.".to_owned()
    } else {
        format!("Previously diagnosed conflicting files: {}.", files.join(", "))
    };
    Ok(format!(
        "**Conflict known at dispatch:** the engine's last observation of this PR reports a conflict with its base branch. {files} Rebase first, preserve both sides, and verify the pushed PR is mergeable before declaring delivery.\n\n"
    ))
}

pub(super) const REBASE_FIRST: &str = r#"
**Start every revision by rebasing, including clean PRs and recovered runs.** Inspect the preserved work first and record the full working-copy commit id. Before editing the requested change, run:

```
"$CUBE_BIN" workspace local-rebase --pr <bound-pr-number> --json
```

This resolves the actual GitHub remote and the PR's actual base branch (including a parent PR branch for stacked PRs), fetches that remote, and rebases the preserved working-copy history onto the freshly fetched tracking ref. It preserves published `pr/<n>` bookmarks and never publishes, repositions the working copy, or collapses commits. Do not use plain `"$CUBE_BIN" workspace rebase`: it can push immediately or collapse intermediate commits. Advance both execution recovery bookmarks after the rebase.

Resolve every conflicted commit from the base upward, integrating the PR's intent and the base branch's changes. Never take either side wholesale, drop commits, squash, or reword existing descriptions. Use full commit ids to identify conflicted ancestors without divergent change-id ambiguity. Keep a local bookmark on the rebased working-copy tip before editing any ancestor, and return via that bookmark after resolving: descendants get new commit ids, while the bookmark follows those rewrites. Never return to a pre-resolution commit id. Check every commit:

```
jj log -r '<destination>..@' -T 'commit_id ++ " conflicts=" ++ conflict ++ "\n"'
```

Substitute the exact `destination` from the local-rebase command's JSON output; it uses the freshly fetched PR base and GitHub remote, not a local mirror. If resolution requires an unavailable decision, report a blocker naming the conflicting files. Then do the revision work and run the existing pre-push build gate on the rebased result, even when the rebase looked clean.

"#;

pub(super) const VERIFY_MERGEABLE: &str = "8. **Completion requires a mergeable pushed PR.** Read `gh pr view` with the bound PR number and `-R <owner/repo> --json headRefOid,mergeable,mergeStateStatus` after pushing, and verify the head matches the commit you published. If mergeability is `UNKNOWN`, retry after 3 seconds and then 6 seconds (at most three reads). Only `MERGEABLE` permits delivery; an unknown, failed, or timed-out read is not success. If `CONFLICTING`, rebase onto the fresh PR base again, resolve, validate and push before completing. If it cannot be resolved, report a blocker naming the conflicting files. The engine independently verifies live mergeability with bounded retries and rejects delivery while it remains conflicting or unknown.\n";
