//! Restage preserved changes without manufacturing new change identities.
use super::preserved::{log, operations, unpublished};
use super::*;

/// Prefer a variant in the PR history, then the newest visible variant in
/// jj's evolution log and operation journal. Unrelated changes are undecidable.
async fn select_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    base: &str,
    recorded: Option<&PreservedTip>,
) -> Result<Option<String>> {
    let pointer = revision(&preserved_name(&record.execution_id));
    let output = log(jj, &record.repo_path, &pointer, "commit_id ++ \"\\n\"").await?;
    let candidates: Vec<_> = output.lines().collect();
    match candidates.as_slice() {
        [] => {
            let Some(recorded) = recorded else {
                return Ok(None);
            };
            let present = log(
                jj,
                &record.repo_path,
                &format!("present({})", recorded.commit_id),
                "commit_id ++ \"\\n\"",
            )
            .await
            .unwrap_or_default();
            if present.lines().any(|id| id == recorded.commit_id) {
                return Ok(Some(recorded.commit_id.clone()));
            }
            return Err(pointer_integrity_error(format!(
                "{} is missing and recorded commit {} is not visible",
                preserved_name(&record.execution_id),
                recorded.commit_id
            )));
        }
        [tip] => return Ok(Some((*tip).to_owned())),
        _ => {}
    }
    let ancestors = log(
        jj,
        &record.repo_path,
        &format!("{pointer} & ::{base}"),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    if let Some(tip) = ancestors.lines().next() {
        return Ok(Some(tip.to_owned()));
    }
    let changes = log(jj, &record.repo_path, &pointer, "change_id ++ \"\\n\"").await?;
    let changes: std::collections::HashSet<_> = changes.lines().collect();
    if changes.len() == 1 {
        // evolog takes one commit, even for divergent changes. Prefer a
        // variant that evolved from all other sides when that is available.
        for candidate in &candidates {
            let evolution = jj
                .run(
                    &record.repo_path,
                    &[
                        "--ignore-working-copy",
                        "evolog",
                        "--no-graph",
                        "-r",
                        candidate,
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                )
                .await?;
            if candidates.iter().all(|other| evolution.lines().any(|id| id == *other)) {
                return Ok(Some((*candidate).to_owned()));
            }
        }
        // Concurrent evolution has multiple roots. The operation journal
        // orders those updates, without trusting author/committer clocks.
        let change = changes.iter().next().unwrap();
        let mut latest: Option<((i64, u32), String)> = None;
        for (seconds, nanos, operation) in operations(jj, &record.repo_path).await? {
            if latest.as_ref().is_some_and(|(time, _)| *time > (seconds, nanos)) {
                return Ok(latest.map(|(_, tip)| tip));
            }
            let visible = jj
                .run(
                    &record.repo_path,
                    &[
                        "--ignore-working-copy",
                        "--at-op",
                        &operation,
                        "log",
                        "--no-graph",
                        "-r",
                        &format!("present({change})"),
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                )
                .await?;
            let visible: Vec<_> = visible.lines().collect();
            if let [tip] = visible.as_slice()
                && candidates.contains(tip)
            {
                if latest.as_ref().is_some_and(|(_, selected)| selected != tip) {
                    break;
                }
                latest = Some(((seconds, nanos), (*tip).to_owned()));
            }
        }
    }
    Err(pointer_integrity_error(format!(
        "{} has no decidable preserved variant",
        preserved_name(&record.execution_id)
    )))
}

/// The destination is the PR head or trunk already positioned by cube.
/// The preserved bookmark follows jj's rewrite naturally; no explicit pointer
/// movement, baseline proof, reconciliation or duplication is involved.
pub async fn restore_preserved(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
) -> Result<Option<RestoreReport>> {
    if jj.shared_repo(workspace).await? != record.repo_path {
        return Err(pointer_integrity_error("preserved work belongs to another store"));
    }
    let base = jj
        .run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
        .await?;
    let base = base.trim();
    let Some(tip) = select_tip(jj, record, base, recorded).await? else {
        return Ok(None);
    };
    let sources = format!("({}) ~ ::{base}", unpublished(&tip));
    let commits = log(
        jj,
        &record.repo_path,
        &sources,
        "change_id ++ \" \" ++ description.first_line() ++ \"\\n\"",
    )
    .await?;
    if !commits.is_empty() {
        let change = log(jj, &record.repo_path, &tip, "change_id").await?;
        // Include empty connectors in the roots; exclude published history.
        let roots = format!("roots(::{tip} ~ (::remote_bookmarks() | ::{base}))");
        jj.run(workspace, &["rebase", "-s", &roots, "-d", base]).await?;
        let evolved = log(
            jj,
            &record.repo_path,
            &format!("{change} & {base}::"),
            "commit_id ++ \"\\n\"",
        )
        .await?;
        let mut selected = None;
        for candidate in evolved.lines() {
            let history = jj
                .run(
                    &record.repo_path,
                    &[
                        "--ignore-working-copy",
                        "evolog",
                        "--no-graph",
                        "-r",
                        candidate,
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                )
                .await?;
            if history.lines().any(|version| version == tip) {
                selected = Some(candidate);
                break;
            }
        }
        let selected = selected
            .ok_or_else(|| pointer_integrity_error("rebased preserved change has no identifiable evolution"))?;
        jj.run(workspace, &["edit", selected]).await?;
    }
    let conflicts = jj
        .run(
            workspace,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("({base}..@) & conflicts()"),
                "-T",
                "change_id ++ \" \" ++ description.first_line() ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(Some(
        RestoreReport::builder()
            .pointer(preserved_name(&record.execution_id))
            .commits(commits)
            .base_sha(base)
            .inherited_base(base)
            .conflicts(conflicts)
            .build(),
    ))
}
