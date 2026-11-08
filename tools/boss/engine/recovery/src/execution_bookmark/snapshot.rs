use std::collections::HashMap;

use super::*;

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
