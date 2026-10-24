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
