use git_utils::jj_provenance;

use super::*;

/// Follow a baseline rewrite only along jj's recorded evolution and only
/// to a commit that is an ancestor of every preserved head. A divergent
/// variant with the same change id is not sufficient provenance.
pub(super) async fn evolved_baseline(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    base: &str,
    heads: &Heads,
) -> Result<String> {
    let named = heads.named(record);
    let mut common = String::new();
    for (_, head) in &named {
        if !common.is_empty() {
            common.push_str(" & ");
        }
        common.push_str(&format!("::{head}"));
    }
    let change = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                base,
                "-T",
                "change_id",
            ],
        )
        .await?;
    let candidates = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &jj_provenance::candidate_revset(&change, &common),
                "-T",
                jj_provenance::COMMIT_ID_LINES_TEMPLATE,
            ],
        )
        .await?;
    if let Some(candidate) = jj_provenance::sole_candidate(&candidates) {
        if candidate == base {
            return Ok(base.to_owned());
        }
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
                    jj_provenance::COMMIT_ID_LINES_TEMPLATE,
                ],
            )
            .await?;
        if jj_provenance::evolution_contains(&evolution, base) {
            return Ok(candidate.to_owned());
        }
    }
    Ok(base.to_owned())
}

/// Checkpoint the effective baseline after a rewrite. jj moves bookmarks
/// during rebase; the returned commit refreshes the durable database record.
pub async fn checkpoint_baseline(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let pointers = heal_baseline(jj, record).await?;
    Ok(pointers.base)
}

/// The commit a fresh baseline goes on: the parent of `@`, skipping empty
/// undescribed positioning commits. Cube's `workspace goto` leaves `@` as
/// an empty child of the PR head and `change create` adds the titled change
/// on top, so `@-` is then a throwaway commit the worker will squash or
/// abandon, and `jj abandon` deletes the bookmarks on the commit it drops.
/// Skipping it changes nothing in `base..head` (it is empty) and puts the
/// baseline on the commit cube actually positioned on. Only inspect emptiness
/// above trunk; the common ancestor is an inclusive floor, even if trunk has
/// advanced since positioning. Its parent may be outside a shallow clone.
pub(super) async fn durable_baseline(jj: &dyn Jj, workspace: &Path) -> Result<String> {
    let output = jj
        .run(
            workspace,
            &[
                "log",
                "--no-graph",
                "-r",
                "heads(heads(::trunk() & ::@-) | ((trunk()..@-) ~ (empty() & description(exact:\"\"))))",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => return Err(baseline_error(jj, workspace, error).await),
    };
    let ids: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    ensure!(
        ids.len() == 1,
        "expected exactly one durable baseline commit below @; found {}",
        ids.len()
    );
    Ok(ids[0].to_owned())
}

// Read jj's Git-store location rather than assuming a colocated checkout.
async fn baseline_error(jj: &dyn Jj, workspace: &Path, error: anyhow::Error) -> anyhow::Error {
    let boundaries = async {
        let repo = jj.shared_repo(workspace).await?;
        let store = repo.join(".jj/repo/store");
        let target = std::fs::read_to_string(store.join("git_target"))?;
        let shallow = store.join(target.trim()).join("shallow");
        let boundaries = std::fs::read_to_string(&shallow)?;
        Ok::<_, anyhow::Error>((shallow, boundaries))
    }
    .await;
    match boundaries {
        Ok((path, ids)) if !ids.trim().is_empty() => error.context(format!(
            "cannot compute execution baseline in shallow repository; shallow boundaries in {}: {}",
            path.display(),
            ids.split_whitespace().collect::<Vec<_>>().join(", ")
        )),
        _ => error.context("cannot compute execution baseline above trunk"),
    }
}

#[cfg(test)]
mod tests;
