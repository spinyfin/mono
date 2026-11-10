//! Publication evidence and removal of unreferenced published heads.
use super::*;

/// Publication evidence is fixed at the initial snapshot. Later commits are
/// conservatively unpublished, so refreshing the visible heads never requires
/// re-reading every evolution log after each removal batch.
pub(super) struct Publication {
    published: HashSet<String>,
}

impl Publication {
    pub(super) async fn unpublished(&self, jj: &dyn Jj, repo: &Path) -> Result<Vec<String>> {
        Ok(commit_lines(jj, repo, "all() ~ empty()")
            .await?
            .into_iter()
            .filter(|id| !self.published.contains(id))
            .collect())
    }
}

/// Include evolution predecessors even across change identities (`duplicate`
/// records such edges). A successor edit never becomes published merely by
/// sharing a published change's identity.
pub(super) async fn evidence(jj: &dyn Jj, repo: &Path, heads: &[String]) -> Result<Publication> {
    let template = "commit_id ++ \" \" ++ change_id ++ \"\\n\"";
    let mut evidence = super::super::preserved::log(jj, repo, "::remote_bookmarks() | ::trunk()", template).await?;
    for chunk in heads.chunks(REVSET_BATCH) {
        let revs = chunk
            .iter()
            .filter(|h| is_commit_id(h))
            .map(|h| format!("present({h})"))
            .collect::<Vec<_>>();
        if !revs.is_empty() {
            evidence.push_str(
                &super::super::preserved::log(jj, repo, &format!("::({})", revs.join(" | ")), template).await?,
            );
        }
    }
    let evidence: BTreeSet<_> = evidence.lines().filter_map(|r| r.split_once(' ')).collect();
    let mut published: HashSet<String> = evidence.iter().map(|(id, _)| (*id).to_owned()).collect();
    let rows = super::super::preserved::log(jj, repo, "all() ~ empty()", template).await?;
    let rows: Vec<_> = rows.lines().filter_map(|r| r.split_once(' ')).collect();
    let changes: HashSet<_> = rows
        .iter()
        .filter(|(id, _)| !published.contains(*id))
        .map(|(_, change)| *change)
        .collect();
    let mut candidates: HashSet<_> = rows
        .iter()
        .filter(|(id, _)| !published.contains(*id))
        .map(|(id, _)| *id)
        .collect();
    let mut evidence: Vec<_> = evidence.into_iter().collect();
    // Same-identity rewrites are common, but duplicates have another identity:
    // prioritize matches without excluding any published commit's evolog.
    evidence.sort_by_key(|(id, change)| (!changes.contains(change), *id));
    for (id, _) in evidence {
        if candidates.is_empty() {
            break;
        }
        let evolution = jj
            .run(
                repo,
                &[
                    "--ignore-working-copy",
                    "evolog",
                    "--no-graph",
                    "-r",
                    id,
                    "-T",
                    "commit_id ++ \"\\n\"",
                ],
            )
            .await?;
        for predecessor in evolution.lines() {
            candidates.remove(predecessor);
            published.insert(predecessor.to_owned());
        }
    }
    Ok(Publication { published })
}

/// Hide only bookmark-less heads whose entire non-empty ancestry is published.
/// Keep all working copies and named history. Abandoning leaves (rather than
/// interior commits) cannot rewrite any surviving descendants.
pub(super) async fn hide_with_evidence(
    jj: &dyn Jj,
    repo: &Path,
    work: &[String],
    report: &mut LegacyCleanupReport,
) -> Result<bool> {
    let eligible = "heads(all()) ~ ::(bookmarks() | remote_bookmarks() | working_copies() | trunk())";
    let mut safe: BTreeSet<String> = commit_lines(jj, repo, eligible).await?.into_iter().collect();
    for batch in work.chunks(REVSET_BATCH) {
        if safe.is_empty() {
            return Ok(false);
        }
        let unsafe_heads =
            commit_lines(jj, repo, &format!("({eligible}) & descendants({})", batch.join(" | "))).await?;
        for head in unsafe_heads {
            safe.remove(&head);
        }
    }
    if safe.is_empty() {
        return Ok(false);
    }
    // The caller refreshes evidence and the operation after every batch.
    let revs = safe.into_iter().take(REVSET_BATCH).collect::<Vec<_>>().join(" | ");
    jj.run(
        repo,
        &[
            "--ignore-working-copy",
            "--ignore-immutable",
            "abandon",
            "--summary",
            &revs,
        ],
    )
    .await?;
    report.operations += 1;
    Ok(true)
}
