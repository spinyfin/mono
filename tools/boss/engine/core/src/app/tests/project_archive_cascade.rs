//! Pins the engine-side cost of archiving a project that owns many child
//! tasks — the navigator's right-click "Archive" gesture, which arrives as
//! `UpdateWorkItem { patch: { status: "archived" } }` on the project id.
//!
//! The contract under test is that the cascade is bounded independently
//! of the child count:
//!
//! - exactly one `WorkItemUpdated` reply to the requesting session;
//! - exactly one `WorkInvalidated` push on the product's work topic (the
//!   subscription the macOS app holds), naming only the project;
//! - a bounded number of row writes — the project row and its status
//!   audit row — never one per child.
//!
//! A regression that turned the archive into a per-child cascade (per-row
//! invalidation, per-row execution churn, per-row status writes) fails
//! one of these three assertions at `CHILD_COUNT = 50`.

use super::*;
use crate::app::work_items::apply_work_item_patch;
use crate::test_support::create_test_product_with_repo;
use boss_protocol::{
    CreateManyTasksInput, CreateProjectInput, CreateTaskInput, Project, ProjectStatus, TopicEventPayload, WorkItem,
    WorkItemPatch, work_product_topic,
};

const CHILD_COUNT: usize = 50;

/// Rows the archive is allowed to write: the project row itself plus the
/// `project_property_audit` row recording the status transition.
const MAX_ROW_WRITES: u64 = 2;

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-archive")
        .request_id("req-archive-project")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

fn seed_project_with_children(state: &ServerState) -> (String, Project) {
    let product = create_test_product_with_repo(
        &state.work_db,
        "ArchiveCascade",
        Some("git@example.com:archive/cascade.git"),
    );
    let project = state
        .work_db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Make tmux the only pane hosting mode".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .expect("create project");
    let items = (0..CHILD_COUNT)
        .map(|i| {
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(format!("Child task {i}"))
                .description(format!("child {i}"))
                .autostart(false)
                .build()
        })
        .collect();
    let created = state
        .work_db
        .create_many_tasks(CreateManyTasksInput { items })
        .expect("create children");
    assert_eq!(created.len(), CHILD_COUNT);
    // Bring the product's execution rows to steady state first: the
    // handler's `publish_work_invalidation` reconciles the whole product on
    // every update, and the *first* reconcile after seeding mints the
    // children's execution rows. That is seeding cost, not archive cost —
    // the measurement below must see only what the archive itself writes.
    state
        .work_db
        .reconcile_product_executions(&product.id)
        .expect("steady-state reconcile");
    (product.id, project)
}

#[tokio::test]
async fn archiving_a_project_with_many_children_is_one_reply_one_push_and_bounded_writes() {
    let (state, _dir) = test_server_state();
    let (product_id, project) = seed_project_with_children(&state);
    assert_ne!(
        project.status,
        ProjectStatus::Archived,
        "the archive below must be a real transition"
    );

    // A second session subscribed to the product's work topic — the
    // app's own invalidation subscription — so pushes are observable.
    let topic = work_product_topic(&product_id);
    let watcher = make_session_sink();
    state
        .topic_broker
        .register_session("session-watcher", watcher.clone())
        .await;
    state
        .topic_broker
        .subscribe("session-watcher", std::slice::from_ref(&topic))
        .await;

    let writes_before = state.work_db.connect().expect("db").total_changes();

    let sink = make_session_sink();
    let ctx = dispatch(&state, &sink);
    apply_work_item_patch(
        ctx,
        project.id.clone(),
        WorkItemPatch::builder().status("archived").build(),
        None,
    )
    .await;

    let writes_after = state.work_db.connect().expect("db").total_changes();

    // Exactly one reply, and it is the archived project.
    sink.close();
    let reply = sink.next().await.expect("handler must answer the request").payload;
    match reply {
        FrontendEvent::WorkItemUpdated {
            item: WorkItem::Project(updated),
        } => {
            assert_eq!(updated.id, project.id);
            assert_eq!(updated.status, ProjectStatus::Archived);
        }
        other => panic!("expected WorkItemUpdated(project), got {other:?}"),
    }
    assert!(
        sink.next().await.is_none(),
        "archiving a project with {CHILD_COUNT} children must send exactly one reply"
    );

    // Exactly one push on the product topic, naming only the project.
    watcher.close();
    let push = watcher.next().await.expect("one WorkInvalidated push").payload;
    match push {
        FrontendEvent::TopicEvent {
            topic: pushed_topic,
            event:
                TopicEventPayload::WorkInvalidated {
                    reason,
                    product_id: pushed_product,
                    item_ids,
                },
            ..
        } => {
            assert_eq!(pushed_topic, topic);
            assert_eq!(reason, "work_item_updated");
            assert_eq!(pushed_product.as_deref(), Some(product_id.as_str()));
            assert_eq!(item_ids, vec![project.id.clone()]);
        }
        other => panic!("expected WorkInvalidated push, got {other:?}"),
    }
    assert!(
        watcher.next().await.is_none(),
        "archiving a project with {CHILD_COUNT} children must publish exactly one invalidation"
    );

    // Bounded writes: the project row and its audit row, never per child.
    let row_writes = writes_after - writes_before;
    assert!(
        row_writes <= MAX_ROW_WRITES,
        "archiving a project with {CHILD_COUNT} children wrote {row_writes} rows; \
         expected at most {MAX_ROW_WRITES} (project row + status audit row)"
    );

    // And the children are untouched: still present, still `todo`.
    let children = state
        .work_db
        .list_tasks(&product_id, Some(&project.id), None, false)
        .expect("list children");
    assert_eq!(children.len(), CHILD_COUNT);
    assert!(
        children.iter().all(|t| t.status == boss_protocol::TaskStatus::Todo),
        "archiving the project must not rewrite its children's status"
    );
}
