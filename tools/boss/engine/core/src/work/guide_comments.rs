//! Immutable authored guide context layered onto the existing comment store.
use super::*;
use boss_protocol::GuideCommentContext;

#[cfg(test)]
#[path = "guide_comments_tests.rs"]
mod tests;
use crate::comments_anchor::{CommentFuzzyConfig, resolve_anchor};

pub(super) fn context_for_create(
    conn: &Connection,
    input: &CreateCommentInput,
    guide_version_id: Option<&str>,
) -> Result<Option<GuideCommentContext>> {
    if input.artifact_kind != "pr_review_guide" {
        anyhow::ensure!(guide_version_id.is_none(), "guide version requires a guide artifact");
        return Ok(None);
    }
    let version = guide_version_id.context("guide comment requires a version")?;
    let context = conn
        .query_row(
            "SELECT v.id, v.comparison_id, c.packet_hash, c.observed_base_sha, c.merge_base_sha, c.head_sha
         FROM pr_review_guide_versions v JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
         WHERE v.id = ?1 AND v.series_id = ?2 AND c.series_id = v.series_id",
            params![version, input.artifact_id],
            |row| {
                Ok(GuideCommentContext {
                    version_id: row.get(0)?,
                    comparison_id: row.get(1)?,
                    packet_hash: row.get(2)?,
                    base_sha: row.get(3)?,
                    merge_base_sha: row.get(4)?,
                    head_sha: row.get(5)?,
                })
            },
        )
        .optional()?
        .context("guide version does not belong to this series")?;
    Ok(Some(context))
}

impl WorkDb {
    /// Reuse exact/fuzzy matching only within the selected immutable version.
    /// Display resolution never rewrites the original quote, hash, or status.
    pub fn resolve_guide_comments(
        &self,
        series_id: &str,
        version_id: Option<&str>,
        plain_text: &str,
        config: &CommentFuzzyConfig,
    ) -> Result<Vec<ResolvedComment>> {
        let version_id = version_id.context("guide anchor resolution requires a version")?;
        let version = self
            .get_pr_review_guide_version(version_id)?
            .context("unknown guide version")?;
        anyhow::ensure!(
            version.series_id == series_id,
            "guide version does not belong to this series"
        );
        Ok(self
            .list_comments("pr_review_guide", series_id, false)?
            .into_iter()
            .filter(|comment| {
                comment
                    .guide_context
                    .as_ref()
                    .is_some_and(|c| c.version_id == version_id)
            })
            .map(|comment| {
                let resolution = super::comments::wire_resolution(resolve_anchor(plain_text, &comment.anchor, config));
                ResolvedComment { comment, resolution }
            })
            .collect())
    }
}
