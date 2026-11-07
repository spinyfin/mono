use std::collections::{BTreeMap, HashMap};

use super::*;

/// Commit identities saved before a fetch or checkout can move local pointers.
pub type PointerSnapshot = Vec<(String, Option<String>)>;

/// Resolve several bookmarks of one repository in a single `jj` invocation.
///
/// Returns every visible commit each named bookmark points at (a conflicted
/// bookmark lists more than one; an absent bookmark is missing from the
/// map). Each record uses one child process for all its bookmarks.
///
/// Bookmark names are matched by the template's `name()` — the literal
/// local bookmark name, never the decorated `name*` / `name??` rendering —
/// against the exact revset selectors, so a name is never inferred from
/// output formatting.
pub(crate) async fn resolve_bookmark_targets(
    jj: &dyn Jj,
    repo: &Path,
    bookmarks: &[String],
) -> Result<HashMap<String, Vec<String>>> {
    if bookmarks.is_empty() {
        return Ok(HashMap::new());
    }
    let revset = bookmarks
        .iter()
        .map(|name| revision(name))
        .collect::<Vec<_>>()
        .join(" | ");
    Ok(bookmark_targets_in(
        &commits_with_bookmarks(jj, repo, &revset).await?,
        bookmarks,
    ))
}

/// Every commit `revset` names, with the local bookmarks it carries, in one
/// `jj` invocation. Hidden commits named explicitly are listed too.
pub(crate) async fn commits_with_bookmarks(
    jj: &dyn Jj,
    repo: &Path,
    revset: &str,
) -> Result<Vec<(String, Vec<String>)>> {
    let output = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "commit_id ++ \"\\t\" ++ local_bookmarks.map(|b| b.name()).join(\"\\t\") ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(output
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let commit = fields.next().filter(|commit| !commit.is_empty())?;
            Some((
                commit.to_owned(),
                fields.filter(|name| !name.is_empty()).map(str::to_owned).collect(),
            ))
        })
        .collect())
}

/// The commits each of `bookmarks` targets among `commits`: a conflicted
/// bookmark lists more than one, an absent bookmark is missing from the map.
pub(crate) fn bookmark_targets_in(
    commits: &[(String, Vec<String>)],
    bookmarks: &[String],
) -> HashMap<String, Vec<String>> {
    let mut targets: HashMap<String, Vec<String>> = HashMap::new();
    for (commit, names) in commits {
        for name in names.iter().filter(|name| bookmarks.contains(name)) {
            targets.entry(name.clone()).or_default().push(commit.clone());
        }
    }
    targets
}

/// The single commit each bookmark targets (`None` when absent). A
/// conflicted bookmark is a real pointer-integrity error, exactly as the
/// one-bookmark resolver reports it.
async fn resolve_single_targets(
    jj: &dyn Jj,
    repo: &Path,
    bookmarks: &[String],
) -> Result<HashMap<String, Option<String>>> {
    let mut targets = resolve_bookmark_targets(jj, repo, bookmarks).await?;
    let mut resolved = HashMap::with_capacity(bookmarks.len());
    for name in bookmarks {
        let commit = match targets.remove(name) {
            None => None,
            Some(commits) if commits.len() == 1 => commits.into_iter().next(),
            Some(commits) => {
                return Err(pointer_integrity_error(format!(
                    "recovery bookmark {name} {UNRESOLVED_MESSAGE}; found {}",
                    commits.len()
                )));
            }
        };
        resolved.insert(name.clone(), commit);
    }
    Ok(resolved)
}

pub async fn snapshot_pointers(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<PointerSnapshot> {
    let names = [record.base(), record.head(), record.publication()];
    let mut resolved = resolve_single_targets(jj, &record.repo_path, &names).await?;
    Ok(names
        .into_iter()
        .map(|name| {
            let commit = resolved.remove(&name).flatten();
            (name, commit)
        })
        .collect())
}

/// Restore the exact pre-checkout pointers, including absent references.
/// Reconciliation must validate these identities, never checkout's replacements.
///
/// One `jj` invocation reads where every pointer is now; a pointer that
/// still targets its saved commit is left alone, pointers that moved to
/// the same commit are set in one invocation, and absent references that
/// reappeared are deleted in one invocation. A pointer whose current state
/// cannot be read as a single commit (conflicted) is always set, which is
/// what resolves the conflict.
///
/// Leaving unmoved pointers alone matters beyond speed: a `bookmark set`
/// that changes nothing still records an operation, and every operation in
/// the shared store writes a full view of all its bookmarks.
pub async fn restore_pointers(jj: &dyn Jj, record: &ExecutionBookmark, saved: &PointerSnapshot) -> Result<()> {
    let names: Vec<String> = saved.iter().map(|(name, _)| name.clone()).collect();
    let current = resolve_bookmark_targets(jj, &record.repo_path, &names).await?;
    let mut sets: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut deletes: Vec<&str> = Vec::new();
    for (name, commit) in saved {
        let now = current.get(name).map(Vec::as_slice).unwrap_or_default();
        match commit {
            Some(commit) => {
                if now != [commit.clone()] {
                    sets.entry(commit.as_str()).or_default().push(name.as_str());
                }
            }
            None => {
                if !now.is_empty() {
                    deletes.push(name.as_str());
                }
            }
        }
    }
    for (commit, names) in sets {
        let mut args = vec!["bookmark", "set"];
        args.extend(names);
        args.extend(["-r", commit, "--allow-backwards"]);
        jj.run(&record.repo_path, &args).await?;
    }
    if !deletes.is_empty() {
        let mut args = vec!["bookmark", "delete"];
        args.extend(deletes);
        jj.run(&record.repo_path, &args).await?;
    }
    Ok(())
}
