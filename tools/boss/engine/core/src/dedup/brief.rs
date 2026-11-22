//! Pure `Attention` → [`AttentionBrief`] prose renderer shared by the
//! creation-time dedup path and the startup sweep.
//!
//! See `tools/boss/docs/designs/notification-dedup-scoring.md` §1 and §4:
//! the LLM reasons over rendered item content (`prompt_text` /
//! `proposed_name` + `proposed_description` / `rationale`), with
//! `association` and parent `AttentionGroup` surfaced strictly as context
//! (never as a filter).

use boss_protocol::{
    AnswerOptionLabels, Attention, AttentionBrief, AttentionGroup, CANDIDATE_ID, CreateAttentionInput,
};

/// Resolve the display label for an attention's `association` field.
///
/// Prefers an explicit caller-supplied label (e.g. `"Task: Engine core"`) when
/// non-empty, falling back to `"task:<id>"` or `"project:<id>"` from the
/// exclusive association columns, or `"unknown"` when neither is present.
pub fn association_context_label(
    association_project_id: Option<&str>,
    association_task_id: Option<&str>,
    association_label: Option<&str>,
) -> String {
    if let Some(label) = non_empty(association_label) {
        return label.to_owned();
    }
    if let Some(task_id) = non_empty(association_task_id) {
        return format!("task:{task_id}");
    }
    if let Some(project_id) = non_empty(association_project_id) {
        return format!("project:{project_id}");
    }
    "unknown".to_owned()
}

/// Render an existing persisted [`Attention`] and its owning [`AttentionGroup`]
/// into an [`AttentionBrief`].
///
/// Shared by both the creation-time comparison set (for `existing_attentions`)
/// and the startup sweep (for both candidate and comparison-set items).
pub fn render_attention_brief(
    attention: &Attention,
    group: &AttentionGroup,
    association_label: Option<&str>,
) -> AttentionBrief {
    let association = association_context_label(
        group.association_project_id.as_deref(),
        group.association_task_id.as_deref(),
        association_label,
    );
    let group_label = format_persisted_group_label(group);
    let fields = RenderFields {
        kind: &group.kind,
        association: &association,
        group_label: &group_label,
        group_key: None,
        source_kind: non_empty(Some(&group.source_kind)),
        source_doc_path: non_empty(group.source_doc_path.as_deref()),
        source_task_id: non_empty(group.source_task_id.as_deref()),
        association_task_id: non_empty(group.association_task_id.as_deref()),
        source_anchor: non_empty(attention.source_anchor.as_deref()),
        question_type: non_empty(attention.question_type.as_deref()),
        prompt_text: non_empty(attention.prompt_text.as_deref()),
        choice_options: non_empty(attention.choice_options.as_deref()),
        proposed_name: non_empty(attention.proposed_name.as_deref()),
        proposed_description: non_empty(attention.proposed_description.as_deref()),
        proposed_effort: non_empty(attention.proposed_effort.as_deref()),
        proposed_work_kind: non_empty(attention.proposed_work_kind.as_deref()),
        rationale: non_empty(attention.rationale.as_deref()),
    };

    let rendered = render_prose(&fields);

    AttentionBrief {
        attention_id: attention.id.clone(),
        group_id: group.id.clone(),
        kind: group.kind.clone(),
        association,
        rendered,
    }
}

/// Render an unpersisted creation-time [`CreateAttentionInput`] into an
/// [`AttentionBrief`] carrying the [`CANDIDATE_ID`] sentinel.
pub fn render_candidate_brief(input: &CreateAttentionInput, association_label: Option<&str>) -> AttentionBrief {
    let association = association_context_label(
        input.association_project_id.as_deref(),
        input.association_task_id.as_deref(),
        association_label,
    );
    let group_id = non_empty(input.group_id.as_deref()).unwrap_or("(pending)").to_owned();
    let group_label = if group_id == "(pending)" {
        "pending card".to_owned()
    } else {
        group_id.clone()
    };
    let fields = RenderFields {
        kind: &input.kind,
        association: &association,
        group_label: &group_label,
        group_key: non_empty(input.group_key.as_deref()),
        source_kind: non_empty(input.source_kind.as_deref()),
        source_doc_path: non_empty(input.source_doc_path.as_deref()),
        source_task_id: non_empty(input.source_task_id.as_deref()),
        association_task_id: non_empty(input.association_task_id.as_deref()),
        source_anchor: non_empty(input.source_anchor.as_deref()),
        question_type: non_empty(input.question_type.as_deref()),
        prompt_text: non_empty(input.prompt_text.as_deref()),
        choice_options: non_empty(input.choice_options.as_deref()),
        proposed_name: non_empty(input.proposed_name.as_deref()),
        proposed_description: non_empty(input.proposed_description.as_deref()),
        proposed_effort: non_empty(input.proposed_effort.as_deref()),
        proposed_work_kind: non_empty(input.proposed_work_kind.as_deref()),
        rationale: non_empty(input.rationale.as_deref()),
    };

    let rendered = render_prose(&fields);

    AttentionBrief {
        attention_id: CANDIDATE_ID.to_owned(),
        group_id,
        kind: input.kind.clone(),
        association,
        rendered,
    }
}

#[derive(bon::Builder)]
struct RenderFields<'a> {
    kind: &'a str,
    association: &'a str,
    group_label: &'a str,
    group_key: Option<&'a str>,
    source_kind: Option<&'a str>,
    source_doc_path: Option<&'a str>,
    source_task_id: Option<&'a str>,
    association_task_id: Option<&'a str>,
    source_anchor: Option<&'a str>,
    question_type: Option<&'a str>,
    prompt_text: Option<&'a str>,
    choice_options: Option<&'a str>,
    proposed_name: Option<&'a str>,
    proposed_description: Option<&'a str>,
    proposed_effort: Option<&'a str>,
    proposed_work_kind: Option<&'a str>,
    rationale: Option<&'a str>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

fn format_persisted_group_label(group: &AttentionGroup) -> String {
    match group.short_id {
        Some(short_id) => format!("A{short_id} ({})", group.id),
        None => group.id.clone(),
    }
}

fn render_prose(fields: &RenderFields<'_>) -> String {
    let mut lines: Vec<String> = Vec::new();

    match fields.kind {
        "question" => render_question_lines(fields, &mut lines),
        "followup" => render_followup_lines(fields, &mut lines),
        _ => render_fallback_lines(fields, &mut lines),
    }

    lines.push(render_context_line(fields));
    lines.join("\n  ")
}

fn render_question_lines(fields: &RenderFields<'_>, lines: &mut Vec<String>) {
    let prompt = fields.prompt_text.unwrap_or("(no prompt)");
    match fields.question_type {
        Some(qtype) => lines.push(format!("Question ({qtype}): {prompt}")),
        None => lines.push(format!("Question: {prompt}")),
    }
    if let Some(raw_choices) = fields.choice_options {
        lines.push(format!("Options: {}", format_choice_options(raw_choices)));
    }
    if let Some(anchor) = fields.source_anchor {
        lines.push(format!("Anchor: {anchor}"));
    }
    if let Some(rationale) = fields.rationale {
        lines.push(format!("Rationale: {rationale}"));
    }
}

fn render_followup_lines(fields: &RenderFields<'_>, lines: &mut Vec<String>) {
    let name = fields.proposed_name.unwrap_or("(untitled followup)");
    let mut meta: Vec<&str> = Vec::new();
    if let Some(work_kind) = fields.proposed_work_kind {
        meta.push(work_kind);
    }
    if let Some(effort) = fields.proposed_effort {
        meta.push(effort);
    }
    if meta.is_empty() {
        lines.push(format!("Followup: {name}"));
    } else {
        lines.push(format!("Followup ({}): {name}", meta.join(", ")));
    }
    if let Some(desc) = fields.proposed_description {
        lines.push(format!("Description: {desc}"));
    }
    if let Some(rationale) = fields.rationale {
        lines.push(format!("Rationale: {rationale}"));
    }
    if let Some(anchor) = fields.source_anchor {
        lines.push(format!("Anchor: {anchor}"));
    }
}

fn render_fallback_lines(fields: &RenderFields<'_>, lines: &mut Vec<String>) {
    if let Some(prompt) = fields.prompt_text {
        lines.push(prompt.to_owned());
    }
    if let Some(name) = fields.proposed_name {
        lines.push(name.to_owned());
    }
    if let Some(desc) = fields.proposed_description {
        lines.push(desc.to_owned());
    }
    if let Some(rationale) = fields.rationale {
        lines.push(format!("Rationale: {rationale}"));
    }
}

fn render_context_line(fields: &RenderFields<'_>) -> String {
    let mut parts = vec![
        format!("association={}", fields.association),
        format!("group={}", fields.group_label),
    ];
    if let Some(group_key) = fields.group_key {
        parts.push(format!("group_key={group_key}"));
    }
    if let Some(source_kind) = fields.source_kind {
        let mut source = source_kind.to_owned();
        if let Some(doc_path) = fields.source_doc_path {
            source.push_str(&format!(":{doc_path}"));
        } else if let Some(source_task) = fields.source_task_id
            && fields.association_task_id != Some(source_task)
        {
            source.push_str(&format!(":{source_task}"));
        }
        parts.push(format!("source={source}"));
    }
    format!("Context: {}", parts.join(" | "))
}

fn format_choice_options(raw: &str) -> String {
    if let Ok(options) = serde_json::from_str::<Vec<String>>(raw) {
        let cleaned: Vec<&str> = options.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        if !cleaned.is_empty() {
            return cleaned.join(" | ");
        }
    }
    if let Ok(labels) = serde_json::from_str::<AnswerOptionLabels>(raw) {
        return format!("{} / {}", labels.yes.trim(), labels.no.trim());
    }
    raw.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_group(kind: &str) -> AttentionGroup {
        AttentionGroup::builder()
            .id("atg_10")
            .short_id(12)
            .product_id("prod_1")
            .created_at("1700000000")
            .grouping_key("question|proj_1|doc:docs/design.md")
            .kind(kind)
            .source_kind("design_doc")
            .source_doc_path("tools/boss/docs/designs/dedup.md")
            .association_project_id("proj_1")
            .build()
    }

    #[test]
    fn renders_question_with_choices_anchor_association_and_group_context() {
        let group = sample_group("question");
        let attention = Attention::builder()
            .id("atn_42")
            .group_id("atg_10")
            .ordinal(1)
            .created_at("1700000100")
            .question_type("multiple_choice")
            .prompt_text("Which index strategy should we use?")
            .choice_options(r#"["B-tree", "Partial index"]"#)
            .source_anchor("data-model")
            .rationale("Queries filter by product_id and state")
            .build();

        let brief = render_attention_brief(&attention, &group, Some("Notification dedup project"));
        assert_eq!(brief.attention_id, "atn_42");
        assert_eq!(brief.group_id, "atg_10");
        assert_eq!(brief.kind, "question");
        assert_eq!(brief.association, "Notification dedup project");
        assert!(
            brief
                .rendered
                .contains("Question (multiple_choice): Which index strategy should we use?")
        );
        assert!(brief.rendered.contains("Options: B-tree | Partial index"));
        assert!(brief.rendered.contains("Anchor: data-model"));
        assert!(
            brief
                .rendered
                .contains("Rationale: Queries filter by product_id and state")
        );
        assert!(brief.rendered.contains("association=Notification dedup project"));
        assert!(brief.rendered.contains("group=A12 (atg_10)"));
        assert!(
            brief
                .rendered
                .contains("source=design_doc:tools/boss/docs/designs/dedup.md")
        );
    }

    #[test]
    fn renders_labelled_yes_no_options_cleanly() {
        let group = sample_group("question");
        let labels = serde_json::to_string(&AnswerOptionLabels::split_or_bypass()).unwrap();
        let attention = Attention::builder()
            .id("atn_43")
            .group_id("atg_10")
            .ordinal(2)
            .created_at("1700000100")
            .question_type("yes_no")
            .prompt_text("Split the PR?")
            .choice_options(labels)
            .build();

        let brief = render_attention_brief(&attention, &group, None);
        assert_eq!(brief.association, "project:proj_1");
        assert!(brief.rendered.contains("Options: Split / Bypass limit"));
    }

    #[test]
    fn renders_followup_with_task_association_and_parent_group() {
        let group = AttentionGroup::builder()
            .id("atg_22")
            .product_id("prod_1")
            .created_at("1700000000")
            .grouping_key("followup|task_9")
            .kind("followup")
            .source_kind("task_transcript")
            .source_task_id("task_9")
            .association_task_id("task_9")
            .build();
        let attention = Attention::builder()
            .id("atn_99")
            .group_id("atg_22")
            .ordinal(1)
            .created_at("1700000200")
            .proposed_name("Add index on created_at")
            .proposed_description("Avoid full table scans during startup sweep.")
            .proposed_work_kind("task")
            .proposed_effort("small")
            .rationale("Observed sequential scan in query plan")
            .build();

        let brief = render_attention_brief(&attention, &group, None);
        assert_eq!(brief.attention_id, "atn_99");
        assert_eq!(brief.group_id, "atg_22");
        assert_eq!(brief.kind, "followup");
        assert_eq!(brief.association, "task:task_9");
        assert!(
            brief
                .rendered
                .contains("Followup (task, small): Add index on created_at")
        );
        assert!(
            brief
                .rendered
                .contains("Description: Avoid full table scans during startup sweep.")
        );
        assert!(
            brief
                .rendered
                .contains("Rationale: Observed sequential scan in query plan")
        );
        assert!(brief.rendered.contains("association=task:task_9"));
        assert!(brief.rendered.contains("group=atg_22"));
    }

    #[test]
    fn renders_candidate_create_input_with_sentinel_id() {
        let input = CreateAttentionInput::builder()
            .kind("followup")
            .association_task_id("task_5")
            .source_kind("task_transcript")
            .source_task_id("task_5")
            .proposed_name("Add index for attention-items query")
            .proposed_description("Startup scan reads whole table.")
            .build();

        let brief = render_candidate_brief(&input, None);
        assert_eq!(brief.attention_id, CANDIDATE_ID);
        assert_eq!(brief.group_id, "(pending)");
        assert_eq!(brief.kind, "followup");
        assert_eq!(brief.association, "task:task_5");
        assert!(brief.rendered.contains("Followup: Add index for attention-items query"));
        assert!(brief.rendered.contains("Description: Startup scan reads whole table."));
        assert!(brief.rendered.contains("association=task:task_5"));
        assert!(brief.rendered.contains("group=pending card"));
    }

    #[test]
    fn candidate_with_group_key_only_keeps_pending_group_id() {
        let input = CreateAttentionInput::builder()
            .kind("followup")
            .group_key("followup|task_5|retry-jitter")
            .association_task_id("task_5")
            .proposed_name("Add retry jitter")
            .build();

        let brief = render_candidate_brief(&input, None);
        assert_eq!(brief.attention_id, CANDIDATE_ID);
        assert_eq!(brief.group_id, "(pending)");
        assert!(brief.rendered.contains("group=pending card"));
        assert!(brief.rendered.contains("group_key=followup|task_5|retry-jitter"));
    }
}
