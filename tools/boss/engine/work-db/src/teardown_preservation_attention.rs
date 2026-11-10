use crate::*;

const START: &str = "<!-- teardown-preservation -->";
const END: &str = "<!-- /teardown-preservation -->";

/// Older engines appended teardown failures as ordinary inspection reasons.
/// Remove only their known wording and empty generated evidence, retaining
/// every independent reason or commit that was filed alongside them.
fn strip_teardown_verdict(body: &mut String) -> bool {
    let mut found = false;
    if let Some(start) = body.find(START)
        && let Some(end) = body[start..].find(END)
    {
        body.replace_range(start..start + end + END.len(), "");
        found = true;
    }
    let legacy_start = "Teardown could not preserve this run's workspace edits (";
    let legacy_end = "example with `jj op log`)";
    while let Some(start) = body.find(legacy_start) {
        let Some(end) = body[start..].find(legacy_end) else {
            break;
        };
        let end = start + end + legacy_end.len();
        let empty_evidence =
            ". Existing preserved commit: `none`. Nothing was deleted. New workspace tip: unavailable.";
        let end = if body[end..].starts_with(empty_evidence) {
            end + empty_evidence.len()
        } else {
            end
        };
        let start = if body[..start].ends_with("- ") {
            start - 2
        } else {
            start
        };
        body.replace_range(start..end, "");
        found = true;
    }
    found
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Replace only teardown's current verdict, keeping independent inspection
    /// evidence open. A successful retry resolves a teardown-only item.
    pub fn refresh_teardown_preservation_attention(
        &self,
        execution_id: &str,
        failure: Option<&str>,
        lease_outcome: &str,
    ) -> Result<()> {
        let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
        let existing = self
            .list_attention_items(execution_id)?
            .into_iter()
            .find(|item| item.kind == kind && item.status == "open");
        let mut independent = existing.map(|item| item.body_markdown).unwrap_or_default();
        let had_teardown = strip_teardown_verdict(&mut independent);
        if failure.is_none() && !had_teardown {
            return Ok(());
        }
        let verdict = match failure {
            Some(reason) => reason.to_owned(),
            None => {
                let preserved = match self.preserved_tip(execution_id)? {
                    Some(tip) => format!(
                        "`preserved/{execution_id}` retains commit `{}` (change `{}`, {} unpublished commits)",
                        tip.commit_id, tip.change_id, tip.count
                    ),
                    None => "no unpublished work required a preserved bookmark".to_owned(),
                };
                format!("Teardown preservation succeeded: {preserved}. Lease outcome: {lease_outcome}.")
            }
        };
        let body = format!("{}\n{START}\n{verdict}\n{END}", independent.trim());
        self.upsert_execution_attention(execution_id, kind, "Preserved work needs inspection", &body)?;
        if failure.is_none() && independent.trim().is_empty() {
            self.resolve_attention_kind_for_execution(execution_id, kind)?;
        }
        Ok(())
    }
}
