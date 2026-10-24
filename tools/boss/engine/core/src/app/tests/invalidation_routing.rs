//! Board invalidations are routed by class (hub-and-spoke design §2.4): a
//! runtime-class push names cards whose runtime fields changed and costs the
//! engine nothing beyond the push, while a structural push keeps the
//! product-wide reconcile and tells subscribers to re-read the whole board.
//!
//! These tests drive the real emitters and handlers and observe three things
//! a consumer or an operator depends on: the `class` on the wire, the
//! counters, and whether the product was reconciled.

use super::*;
use crate::app::handler_helpers::{publish_runtime_work_invalidation, publish_work_invalidation};
use crate::app::work_items::{handle_get_work_runtime_patch, handle_set_task_doc_pointer};
use crate::board_invalidation_metrics as metrics;
use crate::coordinator::ExecutionPublisher;
use crate::test_support::create_test_product_with_repo;
use boss_protocol::{
    CreateProjectInput, CreateTaskInput, InvalidationClass, SetTaskDocPointerInput, TopicEventPayload, WorkItem,
    work_product_topic,
};

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Seeded {
    state: Arc<ServerState>,
    #[builder(name = dir)]
    _dir: tempfile::TempDir,
    product_id: String,
    task_id: String,
    topic: String,
    watcher: Arc<SessionSink>,
}

/// A product with one dispatchable project task whose execution row has not
/// been minted: the first reconcile mints it, so "was the product reconciled"
/// is visible as "does the task have an execution".
async fn seeded() -> Seeded {
    let (state, dir) = test_server_state();
    let product = create_test_product_with_repo(&state.work_db, "InvalidationRouting", Some("git@example.com:r/r.git"));
    let project = state
        .work_db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Routing project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .expect("create project");
    let task = state
        .work_db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id)
                .name("Routed task")
                .autostart(true)
                .build(),
        )
        .expect("create task");
    assert!(
        state.work_db.list_executions(Some(&task.id)).unwrap().is_empty(),
        "seeding must leave the product unreconciled"
    );
    let topic = work_product_topic(&product.id);
    let watcher = make_session_sink();
    state
        .topic_broker
        .register_session("session-watcher", watcher.clone())
        .await;
    state
        .topic_broker
        .subscribe("session-watcher", std::slice::from_ref(&topic))
        .await;
    Seeded::builder()
        .state(state)
        .dir(dir)
        .product_id(product.id)
        .task_id(task.id)
        .topic(topic)
        .watcher(watcher)
        .build()
}

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, request_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-routing")
        .request_id(request_id)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// The `(class, reason, item_ids)` of a work invalidation envelope.
fn invalidation_of(envelope: FrontendEventEnvelope) -> (InvalidationClass, String, Vec<String>) {
    match envelope.payload {
        FrontendEvent::TopicEvent {
            event:
                TopicEventPayload::WorkInvalidated {
                    class,
                    reason,
                    item_ids,
                    ..
                },
            ..
        } => (class, reason, item_ids),
        other => panic!("expected only work invalidations, got {other:?}"),
    }
}

/// The next work invalidation the watcher is sent. Pushes on one topic that
/// are published before the session drains fold into one (see the coalescing
/// tests below), so a test that wants to see each push takes it before
/// publishing the next.
async fn next_push(watcher: &Arc<SessionSink>) -> (InvalidationClass, String, Vec<String>) {
    invalidation_of(watcher.next().await.expect("a push is queued"))
}

/// The `(class, reason, item_ids)` of every work invalidation still queued.
async fn pushed(watcher: &Arc<SessionSink>) -> Vec<(InvalidationClass, String, Vec<String>)> {
    watcher.close();
    let mut pushes = Vec::new();
    while let Some(envelope) = watcher.next().await {
        pushes.push(invalidation_of(envelope));
    }
    pushes
}

#[tokio::test]
async fn a_runtime_mutation_push_skips_the_reconcile_and_a_structural_one_keeps_it() {
    let seeded = seeded().await;
    let state = &seeded.state;

    publish_runtime_work_invalidation(
        state,
        "session-routing",
        "req-runtime",
        vec![seeded.topic.clone()],
        "task_doc_pointer_set",
        Some(seeded.product_id.clone()),
        vec![seeded.task_id.clone()],
    )
    .await;
    assert!(
        state.work_db.list_executions(Some(&seeded.task_id)).unwrap().is_empty(),
        "a runtime-class mutation must not pay a product-wide reconcile"
    );
    let runtime_push = next_push(&seeded.watcher).await;

    publish_work_invalidation(
        state,
        "session-routing",
        "req-structural",
        vec![seeded.topic.clone()],
        "work_item_updated",
        Some(seeded.product_id.clone()),
        vec![seeded.task_id.clone()],
    )
    .await;
    assert!(
        !state.work_db.list_executions(Some(&seeded.task_id)).unwrap().is_empty(),
        "a structural mutation still reconciles the product's executions"
    );

    assert_eq!(
        runtime_push,
        (
            InvalidationClass::Runtime,
            "task_doc_pointer_set".to_owned(),
            vec![seeded.task_id.clone()]
        )
    );
    assert_eq!(
        next_push(&seeded.watcher).await,
        (
            InvalidationClass::Structural,
            "work_item_updated".to_owned(),
            vec![seeded.task_id.clone()]
        )
    );
    assert_eq!(
        metrics::pushes(&state.metrics, metrics::SOURCE_MUTATION, InvalidationClass::Runtime),
        1
    );
    assert_eq!(
        metrics::pushes(&state.metrics, metrics::SOURCE_MUTATION, InvalidationClass::Structural),
        1
    );
}

#[tokio::test]
async fn the_doc_pointer_handler_answers_and_pushes_runtime_class_without_reconciling() {
    let seeded = seeded().await;
    let sink = make_session_sink();
    let ctx = dispatch(&seeded.state, &sink, "req-set-doc");
    handle_set_task_doc_pointer(
        ctx,
        FrontendRequest::SetTaskDocPointer {
            input: SetTaskDocPointerInput {
                task_id: seeded.task_id.clone(),
                doc_path: Some("docs/routing.md".to_owned()),
                ..Default::default()
            },
        },
    )
    .await;

    sink.close();
    match sink.next().await.expect("the handler must answer").payload {
        FrontendEvent::WorkItemUpdated {
            item: WorkItem::Task(task),
        } => assert_eq!(task.id, seeded.task_id),
        other => panic!("expected WorkItemUpdated(task), got {other:?}"),
    }
    assert_eq!(
        pushed(&seeded.watcher).await,
        vec![(
            InvalidationClass::Runtime,
            "task_doc_pointer_set".to_owned(),
            vec![seeded.task_id.clone()]
        )]
    );
    assert!(
        seeded
            .state
            .work_db
            .list_executions(Some(&seeded.task_id))
            .unwrap()
            .is_empty(),
        "setting a doc pointer must not reconcile the product"
    );
}

#[tokio::test]
async fn the_broker_publisher_tags_each_push_with_its_class_and_counts_it() {
    let seeded = seeded().await;
    let publisher = BrokerExecutionPublisher {
        work_db: seeded.state.work_db.clone(),
        topic_broker: seeded.state.topic_broker.clone(),
        work_revision: seeded.state.work_revision.clone(),
        metrics: seeded.state.metrics.clone(),
        kick: std::sync::OnceLock::new(),
    };

    let mut classes = Vec::new();
    publisher
        .publish_work_item_changed(&seeded.product_id, &seeded.task_id, "pr_merged")
        .await;
    classes.push(next_push(&seeded.watcher).await);
    publisher
        .publish_work_item_runtime_changed(&seeded.product_id, &seeded.task_id, "pr_poll_state_updated")
        .await;
    classes.push(next_push(&seeded.watcher).await);
    publisher
        .publish_work_item_runtime_changed(&seeded.product_id, &seeded.task_id, "dispatch_wait_changed")
        .await;
    classes.push(next_push(&seeded.watcher).await);

    let classes: Vec<_> = classes
        .iter()
        .map(|(class, reason, _)| (*class, reason.as_str()))
        .collect();
    assert_eq!(
        classes,
        vec![
            (InvalidationClass::Structural, "pr_merged"),
            (InvalidationClass::Runtime, "pr_poll_state_updated"),
            (InvalidationClass::Runtime, "dispatch_wait_changed"),
        ],
        "the plain publish is the full-board invalidation; only the runtime variant is runtime-class"
    );
    let registry = &seeded.state.metrics;
    assert_eq!(
        metrics::pushes(registry, metrics::SOURCE_EXECUTION, InvalidationClass::Structural),
        1
    );
    assert_eq!(
        metrics::pushes(registry, metrics::SOURCE_EXECUTION, InvalidationClass::Runtime),
        2
    );
    assert_eq!(metrics::full_assemblies(registry), 0, "publishing assembles nothing");
}

#[tokio::test]
async fn the_runtime_patch_handler_serves_named_cards_reports_absent_ids_and_counts_the_patch() {
    let seeded = seeded().await;
    let sink = make_session_sink();
    let ctx = dispatch(&seeded.state, &sink, "req-patch");
    handle_get_work_runtime_patch(
        ctx,
        FrontendRequest::GetWorkRuntimePatch {
            product_id: seeded.product_id.clone(),
            item_ids: vec![seeded.task_id.clone(), "task_missing".to_owned()],
        },
    )
    .await;

    sink.close();
    let envelope = sink.next().await.expect("the handler must answer");
    assert!(
        envelope.revision.is_some(),
        "the reply is stamped with the revision read before the snapshot"
    );
    match envelope.payload {
        FrontendEvent::WorkRuntimePatch {
            product_id,
            items,
            absent,
        } => {
            assert_eq!(product_id, seeded.product_id);
            assert_eq!(items.len(), 1);
            assert!(matches!(&items[0].item, WorkItem::Task(task) if task.id == seeded.task_id));
            assert_eq!(items[0].runtime.work_item_id, seeded.task_id);
            assert_eq!(absent, vec!["task_missing".to_owned()]);
        }
        other => panic!("expected WorkRuntimePatch, got {other:?}"),
    }
    let registry = &seeded.state.metrics;
    assert_eq!(metrics::runtime_patches(registry), 1);
    assert_eq!(registry.counter_value(metrics::RUNTIME_PATCH_CARDS), Some(1));
    assert_eq!(metrics::full_assemblies(registry), 0, "a patch is not a board assembly");
}

// --- Per-session coalescing --------------------------------------------------
//
// A session's outbound queue folds a pending push into the next one for the
// same topic. The class and the named cards are what the app routes its refetch
// on, so folding must not lose either.

fn invalidation_push(
    topic: &str,
    product_id: &str,
    reason: &str,
    class: InvalidationClass,
    item_ids: &[&str],
) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::TopicEvent {
        topic: topic.to_owned(),
        revision: 1,
        origin_session_id: String::new(),
        origin_request_id: None,
        event: TopicEventPayload::WorkInvalidated {
            reason: reason.to_owned(),
            product_id: Some(product_id.to_owned()),
            item_ids: item_ids.iter().map(|id| (*id).to_owned()).collect(),
            class,
        },
    })
}

fn only_pending(queue: &mut SessionQueue) -> (InvalidationClass, String, Vec<String>) {
    assert_eq!(queue.items.len(), 1, "same-topic pushes share one queue slot");
    match queue.pop_front().expect("one pending push").payload {
        FrontendEvent::TopicEvent {
            event:
                TopicEventPayload::WorkInvalidated {
                    class,
                    reason,
                    item_ids,
                    ..
                },
            ..
        } => (class, reason, item_ids),
        other => panic!("expected a work invalidation, got {other:?}"),
    }
}

#[test]
fn a_runtime_push_folded_into_a_pending_structural_push_stays_structural() {
    // Structural first, runtime second: replacing would deliver "runtime" and
    // the app would patch a card while the board's membership had changed.
    for (first, second) in [
        (InvalidationClass::Structural, InvalidationClass::Runtime),
        (InvalidationClass::Runtime, InvalidationClass::Structural),
        (InvalidationClass::Structural, InvalidationClass::Structural),
    ] {
        let mut queue = SessionQueue::new();
        queue.enqueue(invalidation_push("work.product.p", "p", "first", first, &["task_a"]));
        assert_eq!(
            queue.enqueue(invalidation_push("work.product.p", "p", "second", second, &["task_b"])),
            EnqueueOutcome::Coalesced
        );
        let (class, reason, item_ids) = only_pending(&mut queue);
        assert_eq!(class, InvalidationClass::Structural, "{first:?} then {second:?}");
        assert_eq!(reason, "second", "the newer push's reason is kept");
        assert_eq!(item_ids, vec!["task_a", "task_b"]);
    }
}

#[test]
fn runtime_pushes_folded_together_stay_runtime_and_name_every_card_once() {
    let mut queue = SessionQueue::new();
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "dispatch_claimed",
        InvalidationClass::Runtime,
        &["task_a", "task_b"],
    ));
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "pr_poll_state_updated",
        InvalidationClass::Runtime,
        &["task_b", "task_c"],
    ));
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "pr_poll_state_updated",
        InvalidationClass::Runtime,
        &["task_a"],
    ));
    let (class, reason, item_ids) = only_pending(&mut queue);
    assert_eq!(class, InvalidationClass::Runtime);
    assert_eq!(reason, "pr_poll_state_updated");
    assert_eq!(item_ids, vec!["task_a", "task_b", "task_c"]);
}

#[test]
fn a_fold_naming_more_cards_than_a_patch_is_worth_becomes_structural() {
    let cap = boss_protocol::MAX_RUNTIME_PATCH_ITEMS;
    let ids: Vec<String> = (0..=cap).map(|i| format!("task_{i}")).collect();
    let (head, tail) = ids.split_at(cap / 2);
    let head: Vec<&str> = head.iter().map(String::as_str).collect();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();

    let mut queue = SessionQueue::new();
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "a",
        InvalidationClass::Runtime,
        &head,
    ));
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "b",
        InvalidationClass::Runtime,
        &tail,
    ));
    let (class, _, item_ids) = only_pending(&mut queue);
    assert_eq!(
        class,
        InvalidationClass::Structural,
        "{} cards exceed the cap of {cap}",
        ids.len()
    );
    assert_eq!(
        item_ids, ids,
        "promotion keeps the ids: executions consumers still use them"
    );
}

#[test]
fn a_fold_across_products_becomes_structural() {
    let mut queue = SessionQueue::new();
    queue.enqueue(invalidation_push(
        "shared",
        "p",
        "a",
        InvalidationClass::Runtime,
        &["task_a"],
    ));
    queue.enqueue(invalidation_push(
        "shared",
        "q",
        "b",
        InvalidationClass::Runtime,
        &["task_b"],
    ));
    assert_eq!(only_pending(&mut queue).0, InvalidationClass::Structural);
}

#[test]
fn a_different_kind_of_topic_event_still_replaces_the_pending_one() {
    let mut queue = SessionQueue::new();
    queue.enqueue(invalidation_push(
        "work.product.p",
        "p",
        "a",
        InvalidationClass::Runtime,
        &["task_a"],
    ));
    let resync = FrontendEventEnvelope::push(FrontendEvent::TopicEvent {
        topic: "work.product.p".to_owned(),
        revision: 2,
        origin_session_id: String::new(),
        origin_request_id: None,
        event: TopicEventPayload::ResyncRequired,
    });
    assert_eq!(queue.enqueue(resync), EnqueueOutcome::Coalesced);
    assert!(matches!(
        queue.pop_front().expect("one pending push").payload,
        FrontendEvent::TopicEvent {
            event: TopicEventPayload::ResyncRequired,
            ..
        }
    ));
}
