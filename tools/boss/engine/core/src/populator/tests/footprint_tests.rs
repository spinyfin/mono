use super::*;

const FIELDS: &str = "Major deliverables:
1. Implement the capability and its regression tests.
Likely changed files:
- `src/handler.rs`
Estimated size: ~1 files / ~100 changed lines";

pub(super) fn estimated_doc(output: &PlannerOutput) -> String {
    if !output.breakdown_found || output.tasks.is_empty() {
        return "# Design without proposed work".to_owned();
    }
    let mut doc = "## Proposed implementation task breakdown\n".to_owned();
    for task in &output.tasks {
        doc.push_str(&format!("### {}\n{}\n{FIELDS}\n", task.name, task.description));
    }
    doc
}

#[tokio::test]
async fn rejects_bad_footprints_before_planning_and_sends_specific_corrections() {
    for (fields, reason) in [
        (
            FIELDS.replace("Estimated size: ~1 files / ~100 changed lines", ""),
            "missing Estimated size",
        ),
        (
            FIELDS.replace("Likely changed files:\n- `src/handler.rs`\n", ""),
            "missing Likely changed files",
        ),
        (
            FIELDS.replace("~1 files /", "~26 files /"),
            "estimated 26 files / 100 changed lines exceeds limit",
        ),
        (
            FIELDS.replace("~100 changed", "~1,501 changed"),
            "estimated 1 files / 1501 changed lines exceeds limit",
        ),
        (
            FIELDS.replace("1. Implement", "2. Extra\n3. Another\n4. Last\n1. Implement"),
            "4 major deliverables exceeds",
        ),
    ] {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let doc = format!("## Proposed implementation task breakdown\n### Handler\n{fields}");
        // This failure would be PlannerFailed if validation called the model.
        let steps = steps_with(DocFetchOutcomeKind::Content(doc), PlannerOutcomeKind::NoApiKey);
        let publisher = RecordingPublisher::default();
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
            &publisher,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::RejectedBadGraph);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        let attention = populate_attention_member(&db, &product_id, &design_id);
        let body = attention.proposed_description.unwrap();
        assert!(body.contains(&format!("Handler: {reason}")), "{body}");
        assert!(body.contains("Return the design doc to its author"));
        assert!(body.contains("never splits entries"));
        assert_eq!(publisher.attentions_created().await, 1);
        assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
    }
}

#[tokio::test]
async fn task_brief_preserves_author_files_and_estimate() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let output = plan_output(vec![ptask("handler", "Handler")], vec![], Confidence::High, true);
    let steps = steps_with(
        DocFetchOutcomeKind::Content(format!(
            "## Proposed implementation task breakdown\n### Handler\n{FIELDS}"
        )),
        PlannerOutcomeKind::Success(output),
    );
    let outcome = Populator::run(
        &db,
        &steps,
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
        &RecordingPublisher::default(),
    )
    .await;
    assert!(matches!(outcome, PopulateOutcome::Staged { created: 1, .. }));
    let task = db
        .list_tasks(&product_id, Some(&project_id), None, false)
        .unwrap()
        .into_iter()
        .find(|task| task.name == "Handler")
        .unwrap();
    assert!(task.description.contains("Likely changed files:\n- `src/handler.rs`"));
    assert!(
        task.description
            .contains("Estimated size: ~1 files / ~100 changed lines")
    );
}

#[test]
fn parses_entries_and_reports_all_invalid_entries() {
    let doc = format!("## Proposed implementation task breakdown\n### Handler\n{FIELDS}");
    let parsed = crate::planner::validate_breakdown_footprints(&doc).unwrap();
    assert_eq!(parsed[0].0, "Handler");
    assert_eq!(parsed[0].1.files, 1);
    assert_eq!(parsed[0].1.likely_changed_files, ["`src/handler.rs`"]);
    let errors = crate::planner::validate_breakdown_footprints(
        "## Proposed implementation task breakdown\n### First\nScope: in-scope\n### Second\nScope: deferred",
    )
    .unwrap_err();
    assert!(errors.contains("First:"));
    assert!(errors.contains("Second:"));
}

#[test]
fn output_cannot_invent_rename_or_split_entries() {
    let footprints = crate::planner::validate_breakdown_footprints(&format!(
        "## Proposed implementation task breakdown\n### Handler\n{FIELDS}"
    ))
    .unwrap();
    for tasks in [
        vec![ptask("invented", "Different name")],
        vec![ptask("first", "Handler"), ptask("split", "Handler")],
    ] {
        let mut output = plan_output(tasks, vec![], Confidence::High, true);
        assert!(crate::planner::attach_task_footprints(&mut output, &footprints).is_err());
    }
}
