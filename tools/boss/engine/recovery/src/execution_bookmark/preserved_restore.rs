//! Restage preserved changes without manufacturing new change identities.
use super::preserved::{RecordedPresence, log, operations, resolve_recorded_presence, unpublished};
use super::*;

async fn recorded_candidates(jj: &dyn Jj, record: &ExecutionBookmark, recorded: &PreservedTip) -> Result<Vec<String>> {
    match resolve_recorded_presence(jj, &record.repo_path, recorded).await? {
        RecordedPresence::Visible(visible) => {
            Ok(visible.lines().filter(|id| !id.is_empty()).map(str::to_owned).collect())
        }
        // Abandoned or squashed: the recorded commit still exists but has no
        // visible successor. Do not restage it.
        RecordedPresence::Hidden => Ok(Vec::new()),
        RecordedPresence::Absent => Err(pointer_integrity_error(format!(
            "{} is missing and recorded commit {} is not visible",
            preserved_name(&record.execution_id),
            recorded.commit_id
        ))),
    }
}

/// Prefer a variant in the PR history, then the newest visible variant in
/// jj's evolution log and operation journal. Unrelated changes are undecidable.
async fn select_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    base: &str,
    recorded: Option<&PreservedTip>,
) -> Result<Option<(String, Vec<String>)>> {
    let pointer = revision(&preserved_name(&record.execution_id));
    let output = log(jj, &record.repo_path, &pointer, "commit_id ++ \"\\n\"").await?;
    let candidates: Vec<String> = if output.trim().is_empty() {
        let Some(recorded) = recorded else {
            return Ok(None);
        };
        recorded_candidates(jj, record, recorded).await?
    } else {
        output.lines().filter(|id| !id.is_empty()).map(str::to_owned).collect()
    };
    match candidates.as_slice() {
        [] => return Ok(None),
        [tip] => return Ok(Some((tip.clone(), candidates))),
        _ => {}
    }
    let candidate_rev = candidates.join("|");
    let ancestors = log(
        jj,
        &record.repo_path,
        &format!("({candidate_rev}) & ::{base}"),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    if let Some(tip) = ancestors.lines().next() {
        return Ok(Some((tip.to_owned(), candidates)));
    }
    let changes = log(
        jj,
        &record.repo_path,
        &format!("({candidate_rev})"),
        "change_id ++ \"\\n\"",
    )
    .await?;
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
                return Ok(Some((candidate.clone(), candidates.clone())));
            }
        }
        // Concurrent evolution has multiple roots. The operation journal
        // orders those updates, without trusting author/committer clocks.
        let change = changes.iter().next().unwrap();
        let mut latest: Option<((i64, u32), String)> = None;
        for (seconds, nanos, operation) in operations(jj, &record.repo_path).await? {
            if latest.as_ref().is_some_and(|(time, _)| *time > (seconds, nanos)) {
                return Ok(latest.map(|(_, selected)| (selected, candidates.clone())));
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
                && candidates.iter().any(|candidate| candidate == tip)
            {
                if latest.as_ref().is_some_and(|(_, selected)| selected != tip) {
                    break;
                }
                latest = Some(((seconds, nanos), (*tip).to_owned()));
            }
        }
        if let Some((_, tip)) = latest {
            return Ok(Some((tip, candidates)));
        }
    }
    Err(pointer_integrity_error(format!(
        "{} has no decidable preserved variant",
        preserved_name(&record.execution_id)
    )))
}

/// The destination is the PR head or trunk already positioned by cube.
/// When more than one preserved variant exists, this step points
/// `preserved/<exec>` at the selected commit (`jj bookmark set --allow-backwards`)
/// and archives unselected unpublished variants so they remain named.
pub async fn restore_preserved(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
) -> Result<Option<RestoreReport>> {
    if jj.shared_repo(workspace).await? != record.repo_path {
        return Err(pointer_integrity_error("preserved work belongs to another store"));
    }
    let positioned = jj
        .run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
        .await?;
    let positioned = positioned.trim();
    let empty = log(jj, &record.repo_path, &format!("{positioned} & empty()"), "commit_id").await?;
    let parents = log(jj, &record.repo_path, &format!("{positioned}-"), "commit_id ++ \"\\n\"").await?;
    // Only a single-parent empty commit is a disposable positioning child;
    // an empty merge may itself be the intended destination.
    let discard_positioning = !empty.is_empty() && parents.lines().count() == 1;
    let target = if discard_positioning {
        parents.trim().to_owned()
    } else {
        positioned.to_owned()
    };
    let base = target.trim();
    let Some((tip, candidates)) = select_tip(jj, record, base, recorded).await? else {
        return Ok(None);
    };
    let discarded: Vec<_> = candidates
        .iter()
        .filter(|candidate| *candidate != &tip)
        .cloned()
        .collect();
    let mut unrestaged = Vec::new();
    for candidate in &discarded {
        let unique = log(
            jj,
            &record.repo_path,
            &format!("{} ~ ::{tip}", unpublished(candidate)),
            "commit_id ++ \"\\n\"",
        )
        .await?;
        if unique.trim().is_empty() {
            continue;
        }
        let archive = preserved_archive_name(&record.execution_id, candidate);
        jj.run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "bookmark",
                "set",
                &archive,
                "-r",
                candidate,
                "--allow-backwards",
            ],
        )
        .await?;
        unrestaged.push(format!("{candidate} ({archive})"));
    }
    if !discarded.is_empty() {
        jj.run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "bookmark",
                "set",
                &preserved_name(&record.execution_id),
                "-r",
                &tip,
                "--allow-backwards",
            ],
        )
        .await?;
    }
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
        if discard_positioning {
            let remaining = log(
                jj,
                &record.repo_path,
                &format!("present({positioned}) & empty()"),
                "commit_id",
            )
            .await?;
            if !remaining.is_empty() {
                jj.run(workspace, &["abandon", positioned]).await?;
            }
        }
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
            .discarded_tips(discarded.join("\n"))
            .unrestaged_unpublished(unrestaged.join("\n"))
            .build(),
    ))
}
