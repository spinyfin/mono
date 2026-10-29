//! Bind author-declared footprints to the planner's one-entry/one-task output.

use boss_engine_planner_validation::footprint::{TaskFootprint, parse_task_footprint};
use boss_protocol::PlannerOutput;

use super::{extract_breakdown_entries, extract_breakdown_section};

pub(crate) fn validate_breakdown_footprints(doc: &str) -> Result<Vec<(String, TaskFootprint)>, String> {
    let entries = extract_breakdown_entries(doc);
    if entries.is_empty() && extract_breakdown_section(doc).is_some() {
        return Err("breakdown has no parseable entries; use ### <task name> headings".to_owned());
    }
    let mut footprints = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        match parse_task_footprint(&entry.body) {
            Ok(footprint) => footprints.push((entry.title, footprint)),
            Err(reason) => errors.push(format!("- {}: {reason}", entry.title)),
        }
    }
    if errors.is_empty() {
        Ok(footprints)
    } else {
        Err(errors.join("\n"))
    }
}

pub(crate) fn attach_task_footprints(
    output: &mut PlannerOutput,
    footprints: &[(String, TaskFootprint)],
) -> Result<(), String> {
    let mut used = std::collections::HashSet::new();
    for task in &mut output.tasks {
        let matches: Vec<_> = footprints
            .iter()
            .filter(|(title, _)| title.trim() == task.name.trim())
            .collect();
        if matches.len() != 1 || !used.insert(task.name.trim().to_owned()) {
            return Err(format!(
                "{}: task must match exactly one design entry by title; invented, renamed, or split entries are not accepted",
                task.name
            ));
        }
        // Creation lifts the effort audit into columns and drops its footer.
        // Keep worker-facing content before that footer so it survives insert.
        let at = task
            .description
            .find(super::EFFORT_AUDIT_PREFIX)
            .unwrap_or(task.description.len());
        task.description.insert_str(
            at,
            &format!(
                "\n\nExpected implementation footprint (from the design):\n{}\n\n",
                matches[0].1.brief()
            ),
        );
    }
    Ok(())
}
