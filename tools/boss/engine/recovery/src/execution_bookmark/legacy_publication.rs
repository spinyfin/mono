//! Publication evidence and removal of unreferenced published heads.
use super::*;

/// Commit ids named by one abandon, with its revset repeated for the safety
/// predicate: both copies stay well below a remote shell's 128 KiB limit.
const ABANDON_BATCH: usize = 800;

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

fn rows(text: &str) -> Vec<(&str, &str)> {
    text.lines().filter_map(|r| r.split_once(' ')).collect()
}

/// Include evolution predecessors even across change identities (`duplicate`
/// records such edges). A successor edit never becomes published merely by
/// sharing a published change's identity.
///
/// The walk is bounded: one `jj evolog` per published commit that is not on
/// trunk (remote branches and recorded pushed heads, the commits this engine's
/// workers could have rewritten before pushing), plus trunk commits sharing a
/// change identity with an unpublished candidate. Trunk history arrives by
/// fetch and has no local predecessors, so walking all of it would cost
/// thousands of processes and prove nothing. A commit already seen as a
/// predecessor is skipped, since its evolution is part of the one that listed
/// it. The walk stops as soon as no candidate remains.
pub(super) async fn evidence(jj: &dyn Jj, repo: &Path, heads: &[String]) -> Result<Publication> {
    let template = "commit_id ++ \" \" ++ change_id ++ \"\\n\"";
    let log = super::super::preserved::log;
    let trunk = log(jj, repo, "::trunk()", template).await?;
    let mut off_trunk = log(jj, repo, "::remote_bookmarks() ~ ::trunk()", template).await?;
    for chunk in heads.chunks(REVSET_BATCH) {
        let revs = chunk
            .iter()
            .filter(|h| is_commit_id(h))
            .map(|h| format!("present({h})"))
            .collect::<Vec<_>>();
        if !revs.is_empty() {
            off_trunk.push_str(&log(jj, repo, &format!("::({}) ~ ::trunk()", revs.join(" | ")), template).await?);
        }
    }
    let trunk = rows(&trunk);
    let off_trunk = rows(&off_trunk);
    let mut published: HashSet<String> = trunk.iter().chain(&off_trunk).map(|(id, _)| (*id).to_owned()).collect();
    let all = log(jj, repo, "all() ~ empty()", template).await?;
    let all = rows(&all);
    let changes: HashSet<_> = all
        .iter()
        .filter(|(id, _)| !published.contains(*id))
        .map(|(_, change)| *change)
        .collect();
    let mut candidates: HashSet<_> = all
        .iter()
        .filter(|(id, _)| !published.contains(*id))
        .map(|(id, _)| *id)
        .collect();
    let targets = off_trunk
        .iter()
        .chain(trunk.iter().filter(|(_, change)| changes.contains(change)))
        .map(|(id, _)| *id);
    let mut seen = HashSet::new();
    for id in targets {
        if candidates.is_empty() {
            break;
        }
        if !seen.insert(id.to_owned()) {
            continue;
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
            seen.insert(predecessor.to_owned());
        }
    }
    Ok(Publication { published })
}

/// Hide the bookmark-less commits whose whole stack is published: everything
/// not named, not a working copy, not trunk, and neither an ancestor nor a
/// descendant of unpublished non-empty work. Commits are read at `read`'s
/// operation, children first, and abandoned in batches through `live`, at the
/// current operation, with the safety predicate repeated in the revset: a
/// commit that gained a child since the read is skipped rather than abandoned,
/// so no concurrent worker's commit is ever rebased.
pub(super) async fn hide_published(
    read: &dyn Jj,
    live: &dyn Jj,
    repo: &Path,
    work: &[String],
    report: &mut LegacyCleanupReport,
) -> Result<()> {
    let eligible = "all() ~ ::(bookmarks() | remote_bookmarks() | working_copies() | trunk())";
    let mut hideable = commit_lines(read, repo, eligible).await?;
    for batch in work.chunks(REVSET_BATCH) {
        if hideable.is_empty() {
            return Ok(());
        }
        let revs = batch.iter().map(|c| format!("present({c})")).collect::<Vec<_>>();
        let keep: HashSet<_> = commit_lines(
            read,
            repo,
            &format!("({eligible}) & ::descendants({})", revs.join(" | ")),
        )
        .await?
        .into_iter()
        .collect();
        hideable.retain(|id| !keep.contains(id));
    }
    for batch in hideable.chunks(ABANDON_BATCH) {
        let set = batch
            .iter()
            .map(|c| format!("present({c})"))
            .collect::<Vec<_>>()
            .join(" | ");
        let safe = format!(
            "({set}) ~ ::(descendants({set}) ~ ({set})) ~ ::(bookmarks() | remote_bookmarks() | working_copies() | trunk())"
        );
        live.run(
            repo,
            &[
                "--ignore-working-copy",
                "--ignore-immutable",
                "abandon",
                "--summary",
                &safe,
            ],
        )
        .await?;
        report.operations += 1;
    }
    Ok(())
}
