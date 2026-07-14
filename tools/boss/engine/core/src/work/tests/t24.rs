//! Tests for the `deferred_scope` attention item closure paths:
//! `resolve_deferred_scope_attention` ("accept"),
//! `create_task_from_deferred_scope_attention` ("create task"), and
//! `list_open_deferred_scope_attentions_for_product` (kanban badge/popup
//! source). See `crate::deferred_scope` for the marker contract these
//! items are filed from.

use super::*;

fn deferred_scope_body(marker_line: &str) -> String {
    format!(
        "Worker deferred part of this task's scope on its Stop boundary.\n\n\
         Marker (verbatim):\n\n```\n{marker_line}\n```"
    )
}

/// Helper: file an open `deferred_scope` attention item against `execution_id`.
fn file_deferred_scope_attention(db: &WorkDb, execution_id: &str, marker_line: &str) -> WorkAttentionItem {
    db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution_id.to_owned()),
        work_item_id: None,
        kind: crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Worker deferred scope".to_owned(),
        body_markdown: deferred_scope_body(marker_line),
        resolved_at: None,
    })
    .unwrap()
}

#[test]
fn resolve_deferred_scope_attention_accepts_an_open_item() {
    let db = WorkDb::open(temp_db_path("dsa-accept")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-accept");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Source chore");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let attn = file_deferred_scope_attention(
        &db,
        &execution.id,
        "[deferred-scope] summary=\"a\" reason=\"needs plumbing\"",
    );

    let resolved = db.resolve_deferred_scope_attention(&attn.id).unwrap();
    assert_eq!(resolved.status, "accepted");
    assert!(resolved.resolved_at.is_some());
    assert_eq!(resolved.converted_task_id, None);
}

#[test]
fn resolve_deferred_scope_attention_rejects_wrong_kind() {
    let db = WorkDb::open(temp_db_path("dsa-wrong-kind")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-wrong-kind");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Source chore");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let attn = db
        .create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: "worker_escalation".to_owned(),
            status: None,
            title: "Worker requested an effort escalation".to_owned(),
            body_markdown: "body".to_owned(),
            resolved_at: None,
        })
        .unwrap();

    let err = db.resolve_deferred_scope_attention(&attn.id).unwrap_err();
    assert!(
        err.to_string().contains("not a deferred_scope item"),
        "unexpected error: {err}"
    );
}

#[test]
fn resolve_deferred_scope_attention_rejects_already_closed_item() {
    let db = WorkDb::open(temp_db_path("dsa-double-close")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-double-close");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Source chore");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let attn = file_deferred_scope_attention(
        &db,
        &execution.id,
        "[deferred-scope] summary=\"a\" reason=\"needs plumbing\"",
    );

    db.resolve_deferred_scope_attention(&attn.id).unwrap();
    let err = db.resolve_deferred_scope_attention(&attn.id).unwrap_err();
    assert!(err.to_string().contains("not open"), "unexpected error: {err}");
}

#[test]
fn create_task_from_deferred_scope_attention_files_a_followup_with_provenance() {
    let db = WorkDb::open(temp_db_path("dsa-create-task")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-create-task");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Source chore");
    let pr_url = "https://github.com/spinyfin/mono/pull/4242";
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, pr_url],
        )
        .unwrap();
    }
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let attn = file_deferred_scope_attention(
        &db,
        &execution.id,
        "[deferred-scope] summary=\"T11 data plumbing\" reason=\"needs a new ingestion pipeline\"",
    );

    let (updated_item, new_task) = db.create_task_from_deferred_scope_attention(&attn.id).unwrap();

    assert_eq!(updated_item.status, "converted");
    assert!(updated_item.resolved_at.is_some());
    assert_eq!(updated_item.converted_task_id.as_deref(), Some(new_task.id.as_str()));

    assert_eq!(new_task.product_id, product.id);
    assert_eq!(new_task.kind, TaskKind::Followup);
    assert!(
        new_task.name.contains("T11 data plumbing"),
        "task name should carry the deferred summary: {}",
        new_task.name
    );
    assert!(
        new_task.description.contains("needs a new ingestion pipeline"),
        "task description should carry the deferred reason: {}",
        new_task.description
    );
    assert!(
        new_task.description.contains(&chore.id),
        "task description should carry provenance to the source task: {}",
        new_task.description
    );

    let source_task = db.get_work_item(&chore.id).unwrap();
    let source_short_id = match source_task {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.short_id,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(new_task.origin_task_short_id, source_short_id);
    assert_eq!(new_task.origin_pr_number, Some(4242));

    // Second conversion attempt must fail: the item is no longer open.
    let err = db.create_task_from_deferred_scope_attention(&attn.id).unwrap_err();
    assert!(err.to_string().contains("not open"), "unexpected error: {err}");
}

#[test]
fn create_task_from_deferred_scope_attention_tolerates_a_malformed_marker() {
    let db = WorkDb::open(temp_db_path("dsa-malformed")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-malformed");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Source chore");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    // No summary=/reason= fields at all — mirrors a malformed `[deferred-scope]` marker.
    let attn = file_deferred_scope_attention(&db, &execution.id, "[deferred-scope]");

    let (_, new_task) = db.create_task_from_deferred_scope_attention(&attn.id).unwrap();
    assert!(new_task.name.starts_with("Deferred: "));
    assert!(new_task.description.contains("not parseable"));
}

#[test]
fn list_open_deferred_scope_attentions_for_product_excludes_closed_and_other_kinds() {
    let db = WorkDb::open(temp_db_path("dsa-list")).unwrap();
    let product = create_test_product_named(&db, "Boss-dsa-list");
    let other_product = create_test_product_named(&db, "Boss-dsa-list-other");

    let chore_a = create_test_chore_manual(&db, product.id.clone(), "Chore A");
    let exec_a = create_ready_chore_execution(&db, chore_a.id.clone());
    let open_item = file_deferred_scope_attention(&db, &exec_a.id, "[deferred-scope] summary=\"a\" reason=\"ra\"");

    let chore_b = create_test_chore_manual(&db, product.id.clone(), "Chore B");
    let exec_b = create_ready_chore_execution(&db, chore_b.id.clone());
    let closed_item = file_deferred_scope_attention(&db, &exec_b.id, "[deferred-scope] summary=\"b\" reason=\"rb\"");
    db.resolve_deferred_scope_attention(&closed_item.id).unwrap();

    // A non-`deferred_scope` open item on the same product must not leak in.
    db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(exec_a.id.clone()),
        work_item_id: None,
        kind: "worker_escalation".to_owned(),
        status: None,
        title: "Worker requested an effort escalation".to_owned(),
        body_markdown: "body".to_owned(),
        resolved_at: None,
    })
    .unwrap();

    // An open `deferred_scope` item on a different product must not leak in.
    let chore_c = create_test_chore_manual(&db, other_product.id.clone(), "Chore C");
    let exec_c = create_ready_chore_execution(&db, chore_c.id.clone());
    file_deferred_scope_attention(&db, &exec_c.id, "[deferred-scope] summary=\"c\" reason=\"rc\"");

    let items = db.list_open_deferred_scope_attentions_for_product(&product.id).unwrap();
    assert_eq!(items.len(), 1, "expected exactly one open item; got {items:?}");
    assert_eq!(items[0].item.id, open_item.id);
    assert_eq!(items[0].source_work_item_id, chore_a.id);
}
