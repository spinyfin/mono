//! Tests for the wire types re-exported by the `types` facade.

use super::*;
use serde_json::{Value, json};

fn sample_project_json(extra: Value) -> Value {
    let mut base = json!({
        "id": "proj_1",
        "product_id": "prod_1",
        "name": "Demo",
        "slug": "demo",
        "description": "",
        "goal": "",
        "status": "planned",
        "priority": "medium",
        "created_at": "2026-05-11T00:00:00Z",
        "updated_at": "2026-05-11T00:00:00Z",
    });
    if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    base
}

#[test]
fn project_decodes_without_short_id() {
    let raw = sample_project_json(json!({}));
    let project: Project = serde_json::from_value(raw).unwrap();
    assert!(project.short_id.is_none());
}

#[test]
fn project_skips_none_short_id_on_encode() {
    let project: Project = serde_json::from_value(sample_project_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&project).unwrap();
    assert!(!encoded.as_object().unwrap().contains_key("short_id"));
}

#[test]
fn project_roundtrips_with_short_id() {
    let raw = sample_project_json(json!({"short_id": 42}));
    let project: Project = serde_json::from_value(raw).unwrap();
    assert_eq!(project.short_id, Some(42));
    let reencoded = serde_json::to_value(&project).unwrap();
    assert_eq!(reencoded["short_id"], Value::from(42_i64));
    let project2: Project = serde_json::from_value(reencoded).unwrap();
    assert_eq!(project.short_id, project2.short_id);
}

#[test]
fn project_decodes_without_design_doc_fields() {
    let raw = sample_project_json(json!({}));
    let project: Project = serde_json::from_value(raw).unwrap();
    assert!(project.design_doc_repo_remote_url.is_none());
    assert!(project.design_doc_branch.is_none());
    assert!(project.design_doc_path.is_none());
    assert_eq!(project.last_status_actor, "human");
}

#[test]
fn project_skips_none_design_doc_fields_on_encode() {
    let project: Project = serde_json::from_value(sample_project_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&project).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("design_doc_repo_remote_url"));
    assert!(!obj.contains_key("design_doc_branch"));
    assert!(!obj.contains_key("design_doc_path"));
}

#[test]
fn project_roundtrips_with_design_doc_fields() {
    let raw = sample_project_json(json!({
        "design_doc_repo_remote_url": "https://github.com/foo/bar.git",
        "design_doc_branch": "main",
        "design_doc_path": "tools/boss/docs/designs/demo.md",
    }));
    let project: Project = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(
        project.design_doc_repo_remote_url.as_deref(),
        Some("https://github.com/foo/bar.git"),
    );
    assert_eq!(project.design_doc_branch.as_deref(), Some("main"));
    assert_eq!(
        project.design_doc_path.as_deref(),
        Some("tools/boss/docs/designs/demo.md"),
    );

    let reencoded = serde_json::to_value(&project).unwrap();
    let project2: Project = serde_json::from_value(reencoded).unwrap();
    assert_eq!(project.design_doc_repo_remote_url, project2.design_doc_repo_remote_url,);
    assert_eq!(project.design_doc_branch, project2.design_doc_branch);
    assert_eq!(project.design_doc_path, project2.design_doc_path);
}

#[test]
fn set_project_design_doc_input_roundtrips() {
    let input = SetProjectDesignDocInput {
        project_id: "proj_1".into(),
        design_doc_repo_remote_url: None,
        design_doc_branch: None,
        design_doc_path: Some("tools/boss/docs/designs/demo.md".into()),
        unset: false,
    };
    let raw = serde_json::to_value(&input).unwrap();
    let obj = raw.as_object().unwrap();
    assert!(!obj.contains_key("design_doc_repo_remote_url"));
    assert!(!obj.contains_key("design_doc_branch"));
    assert_eq!(obj.get("unset"), Some(&Value::Bool(false)));
    let back: SetProjectDesignDocInput = serde_json::from_value(raw).unwrap();
    assert_eq!(back.project_id, input.project_id);
    assert_eq!(back.design_doc_path, input.design_doc_path);
    assert_eq!(back.unset, input.unset);
}

#[test]
fn set_project_design_doc_input_unset_decodes_without_optional_fields() {
    let raw = json!({
        "project_id": "proj_1",
        "unset": true,
    });
    let parsed: SetProjectDesignDocInput = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.project_id, "proj_1");
    assert!(parsed.unset);
    assert!(parsed.design_doc_path.is_none());
}

#[test]
fn resolved_design_doc_kind_serializes_as_internally_tagged() {
    let same = ResolvedDesignDocKind::SameProduct {
        product_id: "prod_1".into(),
    };
    let raw = serde_json::to_value(&same).unwrap();
    assert_eq!(raw, json!({"type": "same_product", "product_id": "prod_1"}));

    let external = ResolvedDesignDocKind::External;
    let raw = serde_json::to_value(&external).unwrap();
    assert_eq!(raw, json!({"type": "external"}));

    let back: ResolvedDesignDocKind =
        serde_json::from_value(json!({"type": "other_product", "product_id": "prod_2"})).unwrap();
    assert_eq!(
        back,
        ResolvedDesignDocKind::OtherProduct {
            product_id: "prod_2".into(),
        }
    );
}

#[test]
fn project_design_doc_state_roundtrips_all_variants() {
    let not_set = ProjectDesignDocState::NotSet;
    let raw = serde_json::to_value(&not_set).unwrap();
    assert_eq!(raw, json!({"type": "not_set"}));
    assert_eq!(serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(), not_set,);

    let resolved = ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "https://github.com/foo/bar.git".into(),
            branch: "main".into(),
            path: "docs/x.md".into(),
            kind: ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
        },
        workspace_path: Some("/Users/me/Documents/dev/workspaces/mono-agent-001".into()),
        web_url: "https://github.com/foo/bar/blob/main/docs/x.md".into(),
        raw_content_url: Some("https://raw.githubusercontent.com/foo/bar/main/docs/x.md".into()),
    };
    let raw = serde_json::to_value(&resolved).unwrap();
    assert_eq!(raw["type"], "resolved");
    assert_eq!(serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(), resolved,);

    let broken = ProjectDesignDocState::Broken {
        reason: "no repo".into(),
    };
    let raw = serde_json::to_value(&broken).unwrap();
    assert_eq!(raw, json!({"type": "broken", "reason": "no repo"}));
    assert_eq!(serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(), broken,);
}

fn sample_task_json(extra: Value) -> Value {
    let mut base = json!({
        "id": "task_1",
        "product_id": "prod_1",
        "project_id": Value::Null,
        "kind": "chore",
        "name": "Demo",
        "description": "",
        "status": "todo",
        "ordinal": Value::Null,
        "pr_url": Value::Null,
        "deleted_at": Value::Null,
        "created_at": "2026-05-11T00:00:00Z",
        "updated_at": "2026-05-11T00:00:00Z",
    });
    if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    base
}

#[test]
fn task_decodes_without_short_id() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.short_id.is_none());
}

#[test]
fn task_skips_none_short_id_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    assert!(!encoded.as_object().unwrap().contains_key("short_id"));
}

#[test]
fn task_roundtrips_with_short_id() {
    let raw = sample_task_json(json!({"short_id": 99}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.short_id, Some(99));
    let reencoded = serde_json::to_value(&task).unwrap();
    assert_eq!(reencoded["short_id"], Value::from(99_i64));
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.short_id, task2.short_id);
}

#[test]
fn task_decodes_without_repo_remote_url() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.repo_remote_url.is_none());
    assert_eq!(task.created_via, CREATED_VIA_UNKNOWN);
}

#[test]
fn task_skips_none_repo_remote_url_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("repo_remote_url"));
}

#[test]
fn task_roundtrips_with_repo_remote_url() {
    let raw = sample_task_json(json!({
        "repo_remote_url": "https://github.com/foo/bar.git",
    }));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.repo_remote_url.as_deref(), Some("https://github.com/foo/bar.git"),);
    let reencoded = serde_json::to_value(&task).unwrap();
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.repo_remote_url, task2.repo_remote_url);
}

#[test]
fn create_task_input_repo_remote_url_roundtrips() {
    let raw = json!({
        "product_id": "prod_1",
        "project_id": "proj_1",
        "name": "Demo",
        "description": null,
        "repo_remote_url": "git@github.com:foo/bar.git",
    });
    let parsed: CreateTaskInput = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.repo_remote_url.as_deref(), Some("git@github.com:foo/bar.git"),);
    let encoded = serde_json::to_value(&parsed).unwrap();
    assert_eq!(
        encoded["repo_remote_url"],
        Value::String("git@github.com:foo/bar.git".into()),
    );

    let without_field = json!({
        "product_id": "prod_1",
        "project_id": "proj_1",
        "name": "Demo",
        "description": null,
    });
    let parsed_none: CreateTaskInput = serde_json::from_value(without_field).unwrap();
    assert!(parsed_none.repo_remote_url.is_none());
    let encoded_none = serde_json::to_value(&parsed_none).unwrap();
    assert!(!encoded_none.as_object().unwrap().contains_key("repo_remote_url"));
}

#[test]
fn create_chore_input_repo_remote_url_roundtrips() {
    let raw = json!({
        "product_id": "prod_1",
        "name": "Demo",
        "description": null,
        "repo_remote_url": "",
    });
    let parsed: CreateChoreInput = serde_json::from_value(raw).unwrap();
    // Empty string is preserved here; the engine interprets it as
    // "clear" on update verbs but for create it just resolves as
    // not-set / inherit.
    assert_eq!(parsed.repo_remote_url.as_deref(), Some(""));

    let without_field = json!({
        "product_id": "prod_1",
        "name": "Demo",
        "description": null,
    });
    let parsed_none: CreateChoreInput = serde_json::from_value(without_field).unwrap();
    assert!(parsed_none.repo_remote_url.is_none());
    let encoded_none = serde_json::to_value(&parsed_none).unwrap();
    assert!(!encoded_none.as_object().unwrap().contains_key("repo_remote_url"));
}

#[test]
fn resolve_project_design_doc_output_roundtrips() {
    let output = ResolveProjectDesignDocOutput {
        project_id: "proj_1".into(),
        state: ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "https://github.com/foo/bar.git".into(),
                branch: "main".into(),
                path: "docs/x.md".into(),
                kind: ResolvedDesignDocKind::External,
            },
            workspace_path: None,
            web_url: "https://github.com/foo/bar/blob/main/docs/x.md".into(),
            raw_content_url: Some("https://raw.githubusercontent.com/foo/bar/main/docs/x.md".into()),
        },
    };
    let raw = serde_json::to_value(&output).unwrap();
    let back: ResolveProjectDesignDocOutput = serde_json::from_value(raw).unwrap();
    assert_eq!(output, back);
}

// Note: `sample_task_json` is defined earlier in this test module;
// the duplicate that previously sat here was a merge-resolution
// leftover that broke `cargo test -p boss-protocol`. The helper
// above carries the same field set; the timestamp shape change is
// harmless because Task's serde fields accept any string for the
// ISO-8601 columns. See the diagnostics PR description for why
// this one-line cleanup is bundled with the live_status work.

#[test]
fn task_decodes_without_blocked_fields() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.blocked_reason.is_none());
    assert!(task.blocked_attempt_id.is_none());
}

#[test]
fn task_skips_none_blocked_fields_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("blocked_reason"));
    assert!(!obj.contains_key("blocked_attempt_id"));
}

#[test]
fn task_roundtrips_with_blocked_fields() {
    let raw = sample_task_json(json!({
        "status": "blocked",
        "blocked_reason": "merge_conflict",
        "blocked_attempt_id": "conflict_18ab_1",
    }));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(task.blocked_attempt_id.as_deref(), Some("conflict_18ab_1"));

    let reencoded = serde_json::to_value(&task).unwrap();
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.blocked_reason, task2.blocked_reason);
    assert_eq!(task.blocked_attempt_id, task2.blocked_attempt_id);
}

#[test]
fn conflict_resolution_roundtrips_with_all_fields() {
    let attempt = ConflictResolution {
        id: "conflict_18ab_1".into(),
        product_id: "prod_1".into(),
        work_item_id: "task_77".into(),
        pr_url: "https://github.com/foo/bar/pull/243".into(),
        pr_number: 243,
        head_branch: "feat/banana".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("abc123".into()),
        head_sha_before: Some("def456".into()),
        head_sha_after: Some("ghi789".into()),
        status: "succeeded".into(),
        failure_reason: None,
        cube_lease_id: Some("lease_1".into()),
        cube_workspace_id: Some("ws_1".into()),
        worker_id: Some("worker_1".into()),
        conflict_diagnosis: Some("{\"hunks\":1}".into()),
        created_at: "1747000000".into(),
        started_at: Some("1747000010".into()),
        finished_at: Some("1747000100".into()),
        revision_task_id: None,
        event_source: "review_watch".into(),
        conflict_class: Some("semantic".into()),
        resolved_by_rung: Some(3),
        mechanical_rung_in_flight: None,
    };
    let raw = serde_json::to_value(&attempt).unwrap();
    let back: ConflictResolution = serde_json::from_value(raw).unwrap();
    assert_eq!(attempt, back);
}

#[test]
fn conflict_resolution_pending_skips_optional_fields_on_encode() {
    let attempt = ConflictResolution {
        id: "conflict_18ab_2".into(),
        product_id: "prod_1".into(),
        work_item_id: "task_77".into(),
        pr_url: "https://github.com/foo/bar/pull/243".into(),
        pr_number: 243,
        head_branch: "feat/banana".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: None,
        head_sha_before: None,
        head_sha_after: None,
        status: "pending".into(),
        failure_reason: None,
        cube_lease_id: None,
        cube_workspace_id: None,
        worker_id: None,
        conflict_diagnosis: None,
        created_at: "1747000000".into(),
        started_at: None,
        finished_at: None,
        revision_task_id: None,
        event_source: "review_watch".into(),
        conflict_class: None,
        resolved_by_rung: None,
        mechanical_rung_in_flight: None,
    };
    let encoded = serde_json::to_value(&attempt).unwrap();
    let obj = encoded.as_object().unwrap();
    for absent in [
        "base_sha_at_trigger",
        "head_sha_before",
        "head_sha_after",
        "failure_reason",
        "cube_lease_id",
        "cube_workspace_id",
        "worker_id",
        "conflict_diagnosis",
        "started_at",
        "finished_at",
        "conflict_class",
        "resolved_by_rung",
        "mechanical_rung_in_flight",
    ] {
        assert!(!obj.contains_key(absent), "expected {absent} omitted on encode",);
    }
    let back: ConflictResolution = serde_json::from_value(encoded).unwrap();
    assert_eq!(attempt, back);
}

#[test]
fn effort_level_parses_all_five_values() {
    use std::str::FromStr;
    assert_eq!(EffortLevel::from_str("trivial").unwrap(), EffortLevel::Trivial);
    assert_eq!(EffortLevel::from_str("small").unwrap(), EffortLevel::Small);
    assert_eq!(EffortLevel::from_str("medium").unwrap(), EffortLevel::Medium);
    assert_eq!(EffortLevel::from_str("large").unwrap(), EffortLevel::Large);
    assert_eq!(EffortLevel::from_str("max").unwrap(), EffortLevel::Max);
}

#[test]
fn effort_level_rejects_unknown_values() {
    use std::str::FromStr;
    let err = EffortLevel::from_str("galaxybrain").unwrap_err();
    assert!(err.contains("galaxybrain"));
    assert!(err.contains("trivial"));
    assert!(err.contains("max"));
}

#[test]
fn effort_level_serializes_as_lowercase_string() {
    let encoded = serde_json::to_value(EffortLevel::Large).unwrap();
    assert_eq!(encoded, Value::String("large".into()));
    let back: EffortLevel = serde_json::from_value(Value::String("trivial".into())).unwrap();
    assert_eq!(back, EffortLevel::Trivial);
}

#[test]
fn task_decodes_without_effort_or_model_fields() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.effort_level.is_none());
    assert!(task.model_override.is_none());
}

#[test]
fn task_skips_none_effort_and_model_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("effort_level"));
    assert!(!obj.contains_key("model_override"));
}

#[test]
fn task_roundtrips_with_effort_and_model_set() {
    let raw = sample_task_json(json!({
        "effort_level": "large",
        "model_override": "claude-opus-4-7",
    }));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.effort_level, Some(EffortLevel::Large));
    assert_eq!(task.model_override.as_deref(), Some("claude-opus-4-7"));

    let reencoded = serde_json::to_value(&task).unwrap();
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.effort_level, task2.effort_level);
    assert_eq!(task.model_override, task2.model_override);
}

fn sample_product_json(extra: Value) -> Value {
    let mut base = json!({
        "id": "prod_1",
        "name": "Boss",
        "slug": "boss",
        "description": "",
        "repo_remote_url": Value::Null,
        "status": "active",
        "created_at": "1747000000",
        "updated_at": "1747000000",
    });
    if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    base
}

#[test]
fn product_decodes_without_default_model() {
    let raw = sample_product_json(json!({}));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert!(product.default_model.is_none());
}

#[test]
fn product_roundtrips_with_default_model() {
    let raw = sample_product_json(json!({"default_model": "sonnet"}));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert_eq!(product.default_model.as_deref(), Some("sonnet"));
    let encoded = serde_json::to_value(&product).unwrap();
    assert_eq!(encoded["default_model"], Value::String("sonnet".into()));
}

#[test]
fn product_decodes_without_design_repo() {
    let raw = sample_product_json(json!({}));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert!(product.design_repo.is_none());
}

#[test]
fn product_roundtrips_with_design_repo() {
    let raw = sample_product_json(json!({"design_repo": "https://github.com/linkedin-sandbox/bduff.git"}));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert_eq!(
        product.design_repo.as_deref(),
        Some("https://github.com/linkedin-sandbox/bduff.git"),
    );
    let encoded = serde_json::to_value(&product).unwrap();
    assert_eq!(
        encoded["design_repo"],
        Value::String("https://github.com/linkedin-sandbox/bduff.git".into()),
    );
}

#[test]
fn product_skips_none_design_repo_on_encode() {
    let product: Product = serde_json::from_value(sample_product_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&product).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("design_repo"));
}

#[test]
fn product_decodes_without_external_tracker_fields() {
    let raw = sample_product_json(json!({}));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert!(product.external_tracker_kind.is_none());
    assert!(product.external_tracker_config.is_none());
}

#[test]
fn product_skips_none_external_tracker_fields_on_encode() {
    let product: Product = serde_json::from_value(sample_product_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&product).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("external_tracker_kind"));
    assert!(!obj.contains_key("external_tracker_config"));
}

#[test]
fn product_roundtrips_with_external_tracker_fields() {
    let config = json!({"org": "spinyfin", "repo": "mono", "project_number": 1});
    let raw = sample_product_json(json!({
        "external_tracker_kind": "github",
        "external_tracker_config": config.clone(),
    }));
    let product: Product = serde_json::from_value(raw).unwrap();
    assert_eq!(product.external_tracker_kind.as_deref(), Some("github"));
    assert_eq!(product.external_tracker_config.as_ref().unwrap()["org"], "spinyfin");

    let reencoded = serde_json::to_value(&product).unwrap();
    let product2: Product = serde_json::from_value(reencoded).unwrap();
    assert_eq!(product.external_tracker_kind, product2.external_tracker_kind);
    assert_eq!(product.external_tracker_config, product2.external_tracker_config);
}

#[test]
fn task_decodes_without_ci_attempt_fields() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.ci_attempt_budget.is_none());
    assert_eq!(task.ci_attempts_used, 0);
    assert!(task.blocked_signals.is_empty());
}

#[test]
fn task_skips_default_ci_attempt_fields_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("ci_attempt_budget"));
    // `ci_attempts_used` and `blocked_signals` carry zero/empty
    // defaults rather than `Option::None`, so they round-trip
    // through the wire as concrete values. `serde(default)` on the
    // decode side is what makes the omitted-from-payload shape
    // legal.
    assert_eq!(obj.get("ci_attempts_used"), Some(&Value::from(0_i64)));
    assert_eq!(obj.get("blocked_signals"), Some(&Value::Array(Vec::new())),);
}

#[test]
fn task_roundtrips_with_ci_attempt_fields_set() {
    let raw = sample_task_json(json!({
        "ci_attempt_budget": 5,
        "ci_attempts_used": 2,
        "blocked_signals": [
            {
                "work_item_id": "task_1",
                "reason": "ci_failure",
                "attempt_id": "ci_18ab_1",
                "created_at": "1747000000",
                "cleared_at": Value::Null,
            },
            {
                "work_item_id": "task_1",
                "reason": "merge_conflict",
                "attempt_id": "conflict_18ab_1",
                "created_at": "1747000010",
            },
        ],
    }));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.ci_attempt_budget, Some(5));
    assert_eq!(task.ci_attempts_used, 2);
    assert_eq!(task.blocked_signals.len(), 2);
    assert_eq!(task.blocked_signals[0].reason, "ci_failure");
    assert_eq!(task.blocked_signals[0].attempt_id.as_deref(), Some("ci_18ab_1"),);

    let reencoded = serde_json::to_value(&task).unwrap();
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.ci_attempt_budget, task2.ci_attempt_budget);
    assert_eq!(task.ci_attempts_used, task2.ci_attempts_used);
    assert_eq!(task.blocked_signals, task2.blocked_signals);
}

#[test]
fn blocked_signal_skips_optional_fields_on_encode() {
    let signal = BlockedSignal {
        work_item_id: "task_1".into(),
        reason: "dependency".into(),
        attempt_id: None,
        created_at: "1747000000".into(),
        cleared_at: None,
    };
    let encoded = serde_json::to_value(&signal).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("attempt_id"));
    assert!(!obj.contains_key("cleared_at"));
    let back: BlockedSignal = serde_json::from_value(encoded).unwrap();
    assert_eq!(signal, back);
}

#[test]
fn ci_remediation_roundtrips_with_all_fields() {
    let attempt = CiRemediation {
        id: "ci_18ab_1".into(),
        product_id: "prod_1".into(),
        work_item_id: "task_77".into(),
        pr_url: "https://github.com/foo/bar/pull/647".into(),
        pr_number: 647,
        head_branch: "feat/banana".into(),
        head_sha_at_trigger: "abc123".into(),
        head_sha_after: Some("def456".into()),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[{\"name\":\"test\"}]".into(),
        triage_class: Some("tractable".into()),
        log_excerpt: Some("error: ...".into()),
        status: "succeeded".into(),
        failure_reason: None,
        cube_lease_id: Some("lease_1".into()),
        cube_workspace_id: Some("ws_1".into()),
        worker_id: Some("worker_1".into()),
        created_at: "1747000000".into(),
        started_at: Some("1747000010".into()),
        finished_at: Some("1747000100".into()),
        failure_kind: Some("pr_branch_ci".into()),
        before_commit_sha: None,
        revision_task_id: None,
    };
    let raw = serde_json::to_value(&attempt).unwrap();
    let back: CiRemediation = serde_json::from_value(raw).unwrap();
    assert_eq!(attempt, back);
}

#[test]
fn task_decodes_without_external_ref() {
    let raw = sample_task_json(json!({}));
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.external_ref.is_none());
}

#[test]
fn task_skips_none_external_ref_on_encode() {
    let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
    let encoded = serde_json::to_value(&task).unwrap();
    assert!(!encoded.as_object().unwrap().contains_key("external_ref"));
}

#[test]
fn task_roundtrips_with_external_ref() {
    let raw = sample_task_json(json!({
        "external_ref": {
            "kind": "github",
            "canonical_id": "spinyfin/mono#560",
            "raw": {"issue_number": 560, "project_item_id": "PVTI_abc"},
            "web_url": "https://github.com/spinyfin/mono/issues/560",
            "synced_at": "1747000100",
        },
    }));
    let task: Task = serde_json::from_value(raw).unwrap();
    let ext = task.external_ref.as_ref().unwrap();
    assert_eq!(ext.kind, "github");
    assert_eq!(ext.canonical_id, "spinyfin/mono#560");
    assert_eq!(ext.web_url, "https://github.com/spinyfin/mono/issues/560");
    assert_eq!(ext.synced_at.as_deref(), Some("1747000100"));
    assert!(ext.unbound_at.is_none());

    let reencoded = serde_json::to_value(&task).unwrap();
    let task2: Task = serde_json::from_value(reencoded).unwrap();
    assert_eq!(task.external_ref, task2.external_ref);
}

#[test]
fn work_item_external_ref_skips_optional_fields_on_encode() {
    let ext = WorkItemExternalRef {
        kind: "github".into(),
        canonical_id: "spinyfin/mono#560".into(),
        raw: json!({"issue_number": 560}),
        web_url: "https://github.com/spinyfin/mono/issues/560".into(),
        synced_at: None,
        unbound_at: None,
    };
    let encoded = serde_json::to_value(&ext).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("synced_at"));
    assert!(!obj.contains_key("unbound_at"));
    let back: WorkItemExternalRef = serde_json::from_value(encoded).unwrap();
    assert_eq!(ext, back);
}

#[test]
fn set_product_external_tracker_input_roundtrips() {
    let input = SetProductExternalTrackerInput {
        product_id: "prod_1".into(),
        kind: Some("github".into()),
        config: Some(json!({"org": "spinyfin", "repo": "mono", "project_number": 1})),
        unset: false,
    };
    let raw = serde_json::to_value(&input).unwrap();
    let back: SetProductExternalTrackerInput = serde_json::from_value(raw).unwrap();
    assert_eq!(back.product_id, "prod_1");
    assert_eq!(back.kind.as_deref(), Some("github"));
    assert!(!back.unset);
}

#[test]
fn set_product_external_tracker_input_unset_skips_kind_and_config() {
    let input = SetProductExternalTrackerInput {
        product_id: "prod_1".into(),
        kind: None,
        config: None,
        unset: true,
    };
    let encoded = serde_json::to_value(&input).unwrap();
    let obj = encoded.as_object().unwrap();
    assert!(!obj.contains_key("kind"));
    assert!(!obj.contains_key("config"));
    assert_eq!(obj["unset"], Value::Bool(true));
}

#[test]
fn link_external_ref_input_roundtrips() {
    let input = LinkExternalRefInput {
        work_item_id: "task_1".into(),
        kind: "github".into(),
        canonical_id: "spinyfin/mono#560".into(),
    };
    let raw = serde_json::to_value(&input).unwrap();
    assert_eq!(raw["work_item_id"], Value::String("task_1".into()));
    assert_eq!(raw["kind"], Value::String("github".into()));
    assert_eq!(raw["canonical_id"], Value::String("spinyfin/mono#560".into()));
    let back: LinkExternalRefInput = serde_json::from_value(raw).unwrap();
    assert_eq!(back, input);
}

#[test]
fn ci_remediation_pending_skips_optional_fields_on_encode() {
    let attempt = CiRemediation {
        id: "ci_18ab_2".into(),
        product_id: "prod_1".into(),
        work_item_id: "task_77".into(),
        pr_url: "https://github.com/foo/bar/pull/648".into(),
        pr_number: 648,
        head_branch: "feat/coconut".into(),
        head_sha_at_trigger: "abc123".into(),
        head_sha_after: None,
        attempt_kind: "retrigger".into(),
        consumes_budget: 0,
        failed_checks: "[]".into(),
        triage_class: None,
        log_excerpt: None,
        status: "pending".into(),
        failure_reason: None,
        cube_lease_id: None,
        cube_workspace_id: None,
        worker_id: None,
        created_at: "1747000000".into(),
        started_at: None,
        finished_at: None,
        failure_kind: None,
        before_commit_sha: None,
        revision_task_id: None,
    };
    let encoded = serde_json::to_value(&attempt).unwrap();
    let obj = encoded.as_object().unwrap();
    for absent in [
        "head_sha_after",
        "triage_class",
        "log_excerpt",
        "failure_reason",
        "cube_lease_id",
        "cube_workspace_id",
        "worker_id",
        "started_at",
        "finished_at",
        "failure_kind",
        "before_commit_sha",
        "revision_task_id",
    ] {
        assert!(!obj.contains_key(absent), "expected {absent} omitted on encode",);
    }
    let back: CiRemediation = serde_json::from_value(encoded).unwrap();
    assert_eq!(attempt, back);
}

#[test]
fn github_auth_state_dto_disconnected_roundtrips() {
    let state = GitHubAuthStateDto::Disconnected;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "disconnected");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_requesting_code_roundtrips() {
    let state = GitHubAuthStateDto::RequestingCode;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "requesting_code");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_pending_user_auth_roundtrips() {
    let state = GitHubAuthStateDto::PendingUserAuth {
        user_code: "WDJB-MJHT".into(),
        verification_uri: "https://github.com/login/device".into(),
        verification_uri_complete: Some("https://github.com/login/device?user_code=WDJB-MJHT".into()),
        expires_at: 1_748_000_000,
        interval_seconds: 5,
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "pending_user_auth");
    assert_eq!(raw["user_code"], "WDJB-MJHT");
    assert_eq!(raw["interval_seconds"], 5);
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_pending_user_auth_skips_none_complete_uri() {
    let state = GitHubAuthStateDto::PendingUserAuth {
        user_code: "WDJB-MJHT".into(),
        verification_uri: "https://github.com/login/device".into(),
        verification_uri_complete: None,
        expires_at: 1_748_000_000,
        interval_seconds: 5,
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert!(!raw.as_object().unwrap().contains_key("verification_uri_complete"));
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_authorized_roundtrips() {
    let state = GitHubAuthStateDto::Authorized {
        login: "octocat".into(),
        granted_scopes: vec!["repo".into(), "project".into()],
        org_state: OrgAuthState::Ok,
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "authorized");
    assert_eq!(raw["login"], "octocat");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_expired_roundtrips() {
    let state = GitHubAuthStateDto::Expired;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "expired");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_denied_roundtrips() {
    let state = GitHubAuthStateDto::Denied;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "denied");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_error_roundtrips() {
    let state = GitHubAuthStateDto::Error {
        message: "network error fetching device code".into(),
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "error");
    assert_eq!(raw["message"], "network error fetching device code");
    let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn org_auth_state_ok_roundtrips() {
    let state = OrgAuthState::Ok;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "ok");
    let back: OrgAuthState = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn org_auth_state_needs_org_approval_roundtrips() {
    let state = OrgAuthState::NeedsOrgApproval {
        request_url: "https://github.com/orgs/spinyfin/policies/applications".into(),
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "needs_org_approval");
    assert_eq!(
        raw["request_url"],
        "https://github.com/orgs/spinyfin/policies/applications"
    );
    let back: OrgAuthState = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn org_auth_state_needs_sso_roundtrips() {
    let state = OrgAuthState::NeedsSso {
        sso_url: "https://github.com/orgs/spinyfin/sso".into(),
    };
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "needs_sso");
    assert_eq!(raw["sso_url"], "https://github.com/orgs/spinyfin/sso");
    let back: OrgAuthState = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn org_auth_state_unknown_roundtrips() {
    let state = OrgAuthState::Unknown;
    let raw = serde_json::to_value(&state).unwrap();
    assert_eq!(raw["type"], "unknown");
    let back: OrgAuthState = serde_json::from_value(raw).unwrap();
    assert_eq!(state, back);
}

#[test]
fn github_auth_state_dto_authorized_with_org_states_roundtrips() {
    let states = vec![
        OrgAuthState::Ok,
        OrgAuthState::NeedsOrgApproval {
            request_url: "https://example.com/approve".into(),
        },
        OrgAuthState::NeedsSso {
            sso_url: "https://example.com/sso".into(),
        },
        OrgAuthState::Unknown,
    ];
    for org_state in states {
        let auth = GitHubAuthStateDto::Authorized {
            login: "user".into(),
            granted_scopes: vec!["repo".into()],
            org_state: org_state.clone(),
        };
        let raw = serde_json::to_value(&auth).unwrap();
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(auth, back);
    }
}

#[test]
fn automation_trigger_schedule_roundtrips() {
    let trigger = AutomationTrigger::Schedule {
        cron: "0 14 * * 1-5".to_owned(),
        timezone: "America/Los_Angeles".to_owned(),
    };
    let encoded = serde_json::to_value(&trigger).unwrap();
    assert_eq!(encoded["kind"], "schedule");
    assert_eq!(encoded["cron"], "0 14 * * 1-5");
    assert_eq!(encoded["timezone"], "America/Los_Angeles");
    let back: AutomationTrigger = serde_json::from_value(encoded).unwrap();
    assert_eq!(trigger, back);
}

#[test]
fn automation_roundtrips() {
    let trigger = AutomationTrigger::Schedule {
        cron: "0 2 * * *".to_owned(),
        timezone: "UTC".to_owned(),
    };
    let automation = Automation::builder()
        .id("auto_1")
        .product_id("prod_1")
        .name("Nightly lint")
        .trigger(trigger)
        .standing_instruction("Fix clippy warnings if any exist")
        .created_at("1700000000")
        .updated_at("1700000000")
        .build();
    assert_eq!(automation.open_task_limit, 1);
    assert!(automation.enabled);
    assert_eq!(automation.created_via, CREATED_VIA_UNKNOWN);
    let encoded = serde_json::to_value(&automation).unwrap();
    let back: Automation = serde_json::from_value(encoded).unwrap();
    assert_eq!(automation.id, back.id);
    assert_eq!(automation.open_task_limit, back.open_task_limit);
}

#[test]
fn automation_run_roundtrips() {
    let run = AutomationRun::builder()
        .id("run_1")
        .automation_id("auto_1")
        .scheduled_for("1700000000")
        .started_at("1700000001")
        .outcome("skipped")
        .detail("no clippy warnings found")
        .build();
    let encoded = serde_json::to_value(&run).unwrap();
    let back: AutomationRun = serde_json::from_value(encoded).unwrap();
    assert_eq!(run.id, back.id);
    assert_eq!(run.outcome, back.outcome);
    assert_eq!(run.detail, back.detail);
    assert!(back.produced_task_id.is_none());
}

#[test]
fn task_source_automation_id_defaults_to_none() {
    let raw = json!({
        "id": "task_1",
        "product_id": "prod_1",
        "project_id": null,
        "kind": "chore",
        "name": "Fix lint",
        "description": "",
        "status": "todo",
        "ordinal": null,
        "pr_url": null,
        "deleted_at": null,
        "created_at": "1700000000",
        "updated_at": "1700000000",
    });
    let task: Task = serde_json::from_value(raw).unwrap();
    assert!(task.source_automation_id.is_none());
}

#[test]
fn task_source_automation_id_roundtrips() {
    let raw = json!({
        "id": "task_1",
        "product_id": "prod_1",
        "project_id": null,
        "kind": "chore",
        "name": "Fix lint",
        "description": "",
        "status": "todo",
        "ordinal": null,
        "pr_url": null,
        "deleted_at": null,
        "created_at": "1700000000",
        "updated_at": "1700000000",
        "source_automation_id": "auto_1",
    });
    let task: Task = serde_json::from_value(raw).unwrap();
    assert_eq!(task.source_automation_id.as_deref(), Some("auto_1"));
    let encoded = serde_json::to_value(&task).unwrap();
    assert_eq!(encoded["source_automation_id"], "auto_1");
}

#[test]
fn is_known_created_via_recognises_engine_trigger_prefixes() {
    // Exact-match values
    assert!(is_known_created_via(CREATED_VIA_CLI));
    assert!(is_known_created_via(CREATED_VIA_ENGINE_AUTO));
    assert!(is_known_created_via(CREATED_VIA_UNKNOWN));

    // Prefix-based values — engine-triggered revisions
    assert!(is_known_created_via("merge-conflict:crz_abc123"));
    assert!(is_known_created_via("ci-fix:crm_def456"));
    // Pre-existing prefix used by Source B
    assert!(is_known_created_via("pr-comment:owner/repo#42:comment_id"));

    // Unknown values still return false
    assert!(!is_known_created_via("something_undocumented"));
    assert!(!is_known_created_via(""));
}

// ── TaskKind / ExecutionKind round-trip tests ────────────────────────────

#[test]
fn task_kind_display_and_parse_are_inverses() {
    let all = [
        TaskKind::Chore,
        TaskKind::Design,
        TaskKind::Followup,
        TaskKind::Investigation,
        TaskKind::ProjectTask,
        TaskKind::Revision,
        TaskKind::Task,
    ];
    for kind in &all {
        let s = kind.to_string();
        let back: TaskKind = s
            .parse()
            .unwrap_or_else(|e| panic!("TaskKind::from_str({s:?}) failed: {e}"));
        assert_eq!(*kind, back, "round-trip failed for {kind:?}");
    }
}

#[test]
fn task_kind_serde_uses_wire_strings() {
    assert_eq!(serde_json::to_string(&TaskKind::Chore).unwrap(), r#""chore""#);
    assert_eq!(serde_json::to_string(&TaskKind::Design).unwrap(), r#""design""#);
    assert_eq!(serde_json::to_string(&TaskKind::Followup).unwrap(), r#""followup""#);
    assert_eq!(
        serde_json::to_string(&TaskKind::Investigation).unwrap(),
        r#""investigation""#
    );
    assert_eq!(
        serde_json::to_string(&TaskKind::ProjectTask).unwrap(),
        r#""project_task""#
    );
    assert_eq!(serde_json::to_string(&TaskKind::Revision).unwrap(), r#""revision""#);
    assert_eq!(serde_json::to_string(&TaskKind::Task).unwrap(), r#""task""#);

    let chore: TaskKind = serde_json::from_str(r#""chore""#).unwrap();
    assert_eq!(chore, TaskKind::Chore);
    let followup: TaskKind = serde_json::from_str(r#""followup""#).unwrap();
    assert_eq!(followup, TaskKind::Followup);
    let project_task: TaskKind = serde_json::from_str(r#""project_task""#).unwrap();
    assert_eq!(project_task, TaskKind::ProjectTask);
}

#[test]
fn execution_kind_display_and_parse_are_inverses() {
    let all = [
        ExecutionKind::AnswerAgent,
        ExecutionKind::AutomationTriage,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::CiRemediation,
        ExecutionKind::ConflictResolution,
        ExecutionKind::InvestigationImplementation,
        ExecutionKind::PrReview,
        ExecutionKind::ProductDesign,
        ExecutionKind::ProjectDesign,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::TaskImplementation,
    ];
    for kind in &all {
        let s = kind.to_string();
        let back: ExecutionKind = s
            .parse()
            .unwrap_or_else(|e| panic!("ExecutionKind::from_str({s:?}) failed: {e}"));
        assert_eq!(*kind, back, "round-trip failed for {kind:?}");
    }
}

#[test]
fn execution_kind_serde_uses_wire_strings() {
    assert_eq!(
        serde_json::to_string(&ExecutionKind::AnswerAgent).unwrap(),
        r#""answer_agent""#
    );
    assert_eq!(
        serde_json::to_string(&ExecutionKind::ChoreImplementation).unwrap(),
        r#""chore_implementation""#
    );
    assert_eq!(
        serde_json::to_string(&ExecutionKind::RevisionImplementation).unwrap(),
        r#""revision_implementation""#
    );
    assert_eq!(
        serde_json::to_string(&ExecutionKind::InvestigationImplementation).unwrap(),
        r#""investigation_implementation""#
    );
    assert_eq!(
        serde_json::to_string(&ExecutionKind::ProjectDesign).unwrap(),
        r#""project_design""#
    );
    assert_eq!(
        serde_json::to_string(&ExecutionKind::PrReview).unwrap(),
        r#""pr_review""#
    );

    let task_impl: ExecutionKind = serde_json::from_str(r#""task_implementation""#).unwrap();
    assert_eq!(task_impl, ExecutionKind::TaskImplementation);
    let chore_impl: ExecutionKind = serde_json::from_str(r#""chore_implementation""#).unwrap();
    assert_eq!(chore_impl, ExecutionKind::ChoreImplementation);
}

#[test]
fn execution_kind_constants_match_enum() {
    assert_eq!(
        EXECUTION_KIND_AUTOMATION_TRIAGE,
        ExecutionKind::AutomationTriage.as_str()
    );
    assert_eq!(EXECUTION_KIND_PR_REVIEW, ExecutionKind::PrReview.as_str());
}

/// Every `ExecutionStatus` variant. The exhaustive match below is the
/// tripwire: adding a variant to the enum without adding it here is a
/// compile error, which forces the classification tests to reckon with
/// the new state instead of silently ignoring it.
fn all_execution_statuses() -> Vec<ExecutionStatus> {
    use ExecutionStatus::*;
    let all = vec![
        Queued,
        Ready,
        WaitingDependency,
        Running,
        WaitingHuman,
        WaitingReview,
        WaitingMerge,
        Completed,
        Failed,
        Abandoned,
        Cancelled,
        Orphaned,
    ];
    for status in &all {
        match status {
            Queued | Ready | WaitingDependency | Running | WaitingHuman | WaitingReview | WaitingMerge | Completed
            | Failed | Abandoned | Cancelled | Orphaned => {}
        }
    }
    all
}

#[test]
fn execution_status_is_terminal_marks_only_closed_states() {
    use ExecutionStatus::*;
    for status in [Completed, Failed, Abandoned, Cancelled, Orphaned] {
        assert!(status.is_terminal(), "{status} should be terminal");
    }
    for status in [
        Queued,
        Ready,
        WaitingDependency,
        Running,
        WaitingHuman,
        WaitingReview,
        WaitingMerge,
    ] {
        assert!(!status.is_terminal(), "{status} should not be terminal");
    }
}

#[test]
fn execution_status_is_live_marks_only_active_states() {
    use ExecutionStatus::*;
    for status in [Running, WaitingHuman] {
        assert!(status.is_live(), "{status} should be live");
    }
    for status in [
        Queued,
        Ready,
        WaitingDependency,
        WaitingReview,
        WaitingMerge,
        Completed,
        Failed,
        Abandoned,
        Cancelled,
        Orphaned,
    ] {
        assert!(!status.is_live(), "{status} should not be live");
    }
}

#[test]
fn execution_status_can_reconcile_marks_only_pre_dispatch_states() {
    use ExecutionStatus::*;
    for status in [Queued, Ready, WaitingDependency] {
        assert!(status.can_reconcile(), "{status} should be reconcilable");
    }
    for status in [
        Running,
        WaitingHuman,
        WaitingReview,
        WaitingMerge,
        Completed,
        Failed,
        Abandoned,
        Cancelled,
        Orphaned,
    ] {
        assert!(!status.can_reconcile(), "{status} should not be reconcilable");
    }
}

#[test]
fn execution_status_classifications_are_mutually_exclusive() {
    for status in all_execution_statuses() {
        let count = [status.is_terminal(), status.is_live(), status.can_reconcile()]
            .into_iter()
            .filter(|&b| b)
            .count();
        assert!(
            count <= 1,
            "{status} is in more than one classification (terminal/live/reconcile)"
        );
    }
}

#[test]
fn execution_status_waiting_review_and_merge_are_unclassified() {
    // WaitingReview/WaitingMerge are intentionally in none of the three
    // buckets: the work is done from the engine's dispatch perspective but
    // not yet closed, and nothing to reconcile. Pin that gap so a future
    // reclassification is a deliberate, test-visible change.
    for status in [ExecutionStatus::WaitingReview, ExecutionStatus::WaitingMerge] {
        assert!(!status.is_terminal(), "{status} should not be terminal");
        assert!(!status.is_live(), "{status} should not be live");
        assert!(!status.can_reconcile(), "{status} should not be reconcilable");
    }
}

/// Every `TaskStatus` variant, with the same compile-time tripwire as
/// [`all_execution_statuses`].
fn all_task_statuses() -> Vec<TaskStatus> {
    use TaskStatus::*;
    let all = vec![Todo, Active, Blocked, InReview, Done, Archived, Cancelled];
    for status in &all {
        match status {
            Todo | Active | Blocked | InReview | Done | Archived | Cancelled => {}
        }
    }
    all
}

#[test]
fn task_status_is_terminal_marks_only_closed_states() {
    use TaskStatus::*;
    for status in [Done, Archived, Cancelled] {
        assert!(status.is_terminal(), "{status} should be terminal");
    }
    for status in [Todo, Active, Blocked, InReview] {
        assert!(!status.is_terminal(), "{status} should not be terminal");
    }
}

#[test]
fn task_status_is_live_marks_only_in_progress_states() {
    use TaskStatus::*;
    for status in [Active, InReview] {
        assert!(status.is_live(), "{status} should be live");
    }
    for status in [Todo, Blocked, Done, Archived, Cancelled] {
        assert!(!status.is_live(), "{status} should not be live");
    }
}

#[test]
fn task_status_classifications_are_mutually_exclusive() {
    for status in all_task_statuses() {
        assert!(
            !(status.is_terminal() && status.is_live()),
            "{status} is both terminal and live"
        );
    }
}

#[test]
fn task_status_display_label_renames_board_columns() {
    assert_eq!(TaskStatus::Todo.display_label(), "backlog");
    assert_eq!(TaskStatus::Active.display_label(), "doing");
    assert_eq!(TaskStatus::InReview.display_label(), "review");
}

#[test]
fn task_status_display_label_matches_as_str_for_unrenamed_states() {
    for status in [
        TaskStatus::Blocked,
        TaskStatus::Done,
        TaskStatus::Archived,
        TaskStatus::Cancelled,
    ] {
        assert_eq!(
            status.display_label(),
            status.as_str(),
            "{status} board label should match its stored name"
        );
    }
}

#[test]
fn task_status_display_label_diverges_only_for_renamed_states() {
    // The renamed trio must differ from `as_str`; everything else must
    // agree. This catches both a missing rename and an accidental one.
    use TaskStatus::*;
    for status in all_task_statuses() {
        let renamed = matches!(status, Todo | Active | InReview);
        assert_eq!(
            status.display_label() != status.as_str(),
            renamed,
            "{status} rename divergence did not match expectation"
        );
    }
}

fn sample_work_execution() -> WorkExecution {
    WorkExecution::builder()
        .id("exec_1")
        .work_item_id("wi_1")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@example.com:foo.git")
        .created_at("1747000000")
        .build()
}

#[test]
fn work_execution_epoch_accessors_parse_stored_strings() {
    let mut exec = sample_work_execution();
    exec.started_at = Some("1747000010".into());
    exec.finished_at = Some("1747000100".into());
    assert_eq!(exec.started_epoch(), Some(1747000010));
    assert_eq!(exec.finished_epoch(), Some(1747000100));
    assert_eq!(exec.created_epoch(), Some(1747000000));
}

// ── WorkerProposal / ProposalKind / ProposalState round-trip tests ──────

#[test]
fn proposal_kind_display_and_parse_are_inverses() {
    for kind in ProposalKind::ALL {
        let s = kind.to_string();
        let back: ProposalKind = s
            .parse()
            .unwrap_or_else(|e| panic!("ProposalKind::from_str({s:?}) failed: {e}"));
        assert_eq!(*kind, back, "round-trip failed for {kind:?}");
    }
}

#[test]
fn proposal_kind_serde_uses_wire_strings() {
    assert_eq!(
        serde_json::to_string(&ProposalKind::EffortEscalation).unwrap(),
        r#""effort_escalation""#
    );
    assert_eq!(
        serde_json::to_string(&ProposalKind::FollowupTask).unwrap(),
        r#""followup_task""#
    );
    assert_eq!(
        serde_json::to_string(&ProposalKind::AutomationOutcome).unwrap(),
        r#""automation_outcome""#
    );
    let pr_created: ProposalKind = serde_json::from_str(r#""pr_created""#).unwrap();
    assert_eq!(pr_created, ProposalKind::PrCreated);
}

#[test]
fn proposal_state_display_and_parse_are_inverses() {
    for state in ProposalState::ALL {
        let s = state.to_string();
        let back: ProposalState = s
            .parse()
            .unwrap_or_else(|e| panic!("ProposalState::from_str({s:?}) failed: {e}"));
        assert_eq!(*state, back, "round-trip failed for {state:?}");
    }
}

#[test]
fn proposal_state_defaults_to_proposed() {
    assert_eq!(ProposalState::default(), ProposalState::Proposed);
}

#[test]
fn proposal_kind_from_str_rejects_unknown_values() {
    assert!("nope".parse::<ProposalKind>().is_err());
}

#[test]
fn proposal_state_from_str_rejects_unknown_values() {
    assert!("nope".parse::<ProposalState>().is_err());
}

#[test]
fn proposal_decider_display_and_parse_are_inverses() {
    for decider in ProposalDecider::ALL {
        let s = decider.to_string();
        let back: ProposalDecider = s
            .parse()
            .unwrap_or_else(|e| panic!("ProposalDecider::from_str({s:?}) failed: {e}"));
        assert_eq!(*decider, back, "round-trip failed for {decider:?}");
    }
}

#[test]
fn proposal_decider_from_str_rejects_unknown_values() {
    assert!("nope".parse::<ProposalDecider>().is_err());
}

fn sample_worker_proposal() -> WorkerProposal {
    WorkerProposal::builder()
        .id("prp_1")
        .execution_id("exec_1")
        .created_at("1747000000")
        .idempotency_key("exec_1:blocked:abc123")
        .kind(ProposalKind::Blocked)
        .payload_json(r#"{"reason":"bazel build wedged"}"#)
        .build()
}

#[test]
fn worker_proposal_defaults_state_to_proposed_and_omits_optional_fields() {
    let proposal = sample_worker_proposal();
    assert_eq!(proposal.state, ProposalState::Proposed);
    let encoded = serde_json::to_value(&proposal).unwrap();
    let obj = encoded.as_object().unwrap();
    for absent in [
        "applied_ref",
        "decided_at",
        "decided_by",
        "decision_reason",
        "work_item_id",
    ] {
        assert!(!obj.contains_key(absent), "expected {absent} omitted on encode");
    }
    assert_eq!(obj["state"], "proposed");
}

#[test]
fn worker_proposal_roundtrips_with_every_field_set() {
    let proposal = WorkerProposal::builder()
        .id("prp_2")
        .execution_id("exec_2")
        .created_at("1747000000")
        .idempotency_key("exec_2:pr_created:def456")
        .kind(ProposalKind::PrCreated)
        .payload_json(r#"{"pr_url":"https://github.com/foo/bar/pull/1"}"#)
        .state(ProposalState::Applied)
        .applied_ref("task_9")
        .decided_at("1747000100")
        .decided_by(ProposalDecider::Policy)
        .decision_reason("verified PR URL and branch match")
        .work_item_id("task_9")
        .build();
    let encoded = serde_json::to_value(&proposal).unwrap();
    let back: WorkerProposal = serde_json::from_value(encoded).unwrap();
    assert_eq!(proposal, back);
}

#[test]
fn automation_outcome_payload_tags_on_outcome() {
    let produced = AutomationOutcomeProposalPayload::ProducedTask {
        task_id: "task_1".into(),
    };
    let raw = serde_json::to_value(&produced).unwrap();
    assert_eq!(raw, json!({"outcome": "produced_task", "task_id": "task_1"}));
    let back: AutomationOutcomeProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(produced, back);

    let skip = AutomationOutcomeProposalPayload::Skip {
        reason: "repo is clean".into(),
    };
    let raw = serde_json::to_value(&skip).unwrap();
    assert_eq!(raw, json!({"outcome": "skip", "reason": "repo is clean"}));
    let back: AutomationOutcomeProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(skip, back);
}

#[test]
fn followup_task_payload_roundtrips_with_and_without_optional_fields() {
    let full = FollowupTaskProposalPayload {
        proposed_description: "Add retry/backoff".into(),
        proposed_name: "Add retry to X client".into(),
        rationale: "Observed transient 5xx flakes".into(),
        proposed_effort: Some(EffortLevel::Small),
        proposed_work_kind: Some("chore".into()),
    };
    let raw = serde_json::to_value(&full).unwrap();
    let back: FollowupTaskProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(full, back);

    let minimal = FollowupTaskProposalPayload {
        proposed_description: "Add retry/backoff".into(),
        proposed_name: "Add retry to X client".into(),
        rationale: "Observed transient 5xx flakes".into(),
        proposed_effort: None,
        proposed_work_kind: None,
    };
    let raw = serde_json::to_value(&minimal).unwrap();
    let obj = raw.as_object().unwrap();
    assert!(!obj.contains_key("proposed_effort"));
    assert!(!obj.contains_key("proposed_work_kind"));
    let back: FollowupTaskProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(minimal, back);
}

#[test]
fn pr_created_payload_roundtrips() {
    let payload = PrCreatedProposalPayload {
        pr_url: "https://github.com/foo/bar/pull/1".into(),
        branch: Some("boss/exec_1".into()),
    };
    let raw = serde_json::to_value(&payload).unwrap();
    let back: PrCreatedProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(payload, back);
}

#[test]
fn attention_payload_roundtrips_and_omits_kind_when_none() {
    let full = AttentionProposalPayload {
        body_markdown: "please check this".into(),
        title: "Needs a decision".into(),
        attention_kind: Some("question".into()),
    };
    let raw = serde_json::to_value(&full).unwrap();
    let back: AttentionProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(full, back);

    let minimal = AttentionProposalPayload {
        body_markdown: "please check this".into(),
        title: "Needs a decision".into(),
        attention_kind: None,
    };
    let raw = serde_json::to_value(&minimal).unwrap();
    let obj = raw.as_object().unwrap();
    assert!(
        !obj.contains_key("attention_kind"),
        "attention_kind should be omitted when None"
    );
    let back: AttentionProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(minimal, back);
}

#[test]
fn effort_escalation_payload_roundtrips_and_encodes_effort_level_wire_string() {
    let payload = EffortEscalationProposalPayload {
        reason: "bazel build wedged".into(),
        requested_level: EffortLevel::Large,
    };
    let raw = serde_json::to_value(&payload).unwrap();
    assert_eq!(raw["requested_level"], "large");
    let back: EffortEscalationProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(payload, back);
}

#[test]
fn blocked_payload_roundtrips() {
    let payload = BlockedProposalPayload {
        reason: "waiting on operator approval to force-push".into(),
    };
    let raw = serde_json::to_value(&payload).unwrap();
    let back: BlockedProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(payload, back);
}

#[test]
fn deferred_scope_payload_roundtrips() {
    let payload = DeferredScopeProposalPayload {
        summary: "wiring for the third data source".into(),
        reason: "needs a new ingestion pipeline; out of scope for this task".into(),
    };
    let raw = serde_json::to_value(&payload).unwrap();
    let back: DeferredScopeProposalPayload = serde_json::from_value(raw).unwrap();
    assert_eq!(payload, back);
}

#[test]
fn work_execution_epoch_accessors_none_when_unset_or_unparseable() {
    let mut exec = sample_work_execution();
    // Unset optional timestamps.
    assert!(exec.started_at.is_none());
    assert!(exec.finished_at.is_none());
    assert_eq!(exec.started_epoch(), None);
    assert_eq!(exec.finished_epoch(), None);

    // Unparseable values yield None rather than panicking.
    exec.started_at = Some("not-a-number".into());
    exec.finished_at = Some("".into());
    exec.created_at = "2026-05-11T00:00:00Z".into();
    assert_eq!(exec.started_epoch(), None);
    assert_eq!(exec.finished_epoch(), None);
    assert_eq!(exec.created_epoch(), None);
}
