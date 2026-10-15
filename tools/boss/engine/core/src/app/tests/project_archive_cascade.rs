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
//!   audit row — never one per child;
//! - a bounded number of transactions and SQL statements, identical at
//!   50 and 500 children. Transactions are counted at the connection
//!   (every `BEGIN` and every commit during the handler), so a per-child
//!   read or no-op transaction — invisible to the row-write count — fails
//!   too.
//!
//! A regression that turned the archive into a per-child cascade (per-row
//! invalidation, per-row execution churn, per-row status writes, per-row
//! reads) fails one of these assertions.

use super::*;
use crate::app::work_items::apply_work_item_patch;
use crate::test_support::create_test_product_with_repo;
use boss_protocol::{
    CreateManyTasksInput, CreateProjectInput, CreateTaskInput, Project, ProjectStatus, TopicEventPayload, WorkItem,
    WorkItemPatch, work_product_topic,
};
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Child counts the cascade is exercised at. The transaction and statement
/// counts must be the same at both, so any per-child work shows up as a
/// difference no matter what absolute bound is chosen.
const CHILD_COUNTS: [usize; 2] = [50, 500];

/// Write transactions the handler may open: the project update itself plus
/// the product reconcile run by `publish_work_invalidation`. A fixed bound
/// that does not depend on the child count.
const MAX_TRANSACTIONS: usize = 4;

static BEGINS: AtomicUsize = AtomicUsize::new(0);
static STATEMENTS: AtomicUsize = AtomicUsize::new(0);

fn count_statement(event: TraceEvent<'_>) {
    if let TraceEvent::Stmt(_, sql) = event {
        STATEMENTS.fetch_add(1, Ordering::SeqCst);
        if sql
            .trim_start()
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("begin"))
        {
            BEGINS.fetch_add(1, Ordering::SeqCst);
        }
    }
}

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

fn seed_project_with_children(state: &ServerState, child_count: usize) -> (String, Project) {
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
    let items = (0..child_count)
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
    assert_eq!(created.len(), child_count);
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

/// What the handler cost at one child count.
struct Cost {
    transactions: usize,
    commits: usize,
    statements: usize,
}

#[tokio::test]
async fn archiving_a_project_with_many_children_is_one_reply_one_push_and_bounded_writes() {
    let costs: Vec<Cost> = {
        let mut costs = Vec::new();
        for child_count in CHILD_COUNTS {
            costs.push(archive_with_children(child_count).await);
        }
        costs
    };
    let (small, large) = (&costs[0], &costs[1]);
    for (child_count, cost) in CHILD_COUNTS.iter().zip(&costs) {
        assert!(cost.transactions >= 1, "the archive must open a transaction");
        assert!(
            cost.transactions <= MAX_TRANSACTIONS,
            "archiving a project with {child_count} children opened {} transactions; \
             expected at most {MAX_TRANSACTIONS} (project update + invalidation reconcile)",
            cost.transactions
        );
        assert!(
            cost.commits <= MAX_TRANSACTIONS,
            "archiving a project with {child_count} children committed {} times; \
             expected at most {MAX_TRANSACTIONS}",
            cost.commits
        );
    }
    assert_eq!(
        small.transactions, large.transactions,
        "transaction count must not grow with the child count ({} -> {})",
        small.transactions, large.transactions
    );
    assert_eq!(
        small.commits, large.commits,
        "commit count must not grow with the child count ({} -> {})",
        small.commits, large.commits
    );
    assert_eq!(
        small.statements, large.statements,
        "SQL statement count must not grow with the child count ({} -> {})",
        small.statements, large.statements
    );
}

async fn archive_with_children(child_count: usize) -> Cost {
    let (state, _dir) = test_server_state();
    let (product_id, project) = seed_project_with_children(&state, child_count);
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

    // Count transactions on the one shared connection while the handler
    // runs. Installed after seeding so seeding cost is not counted.
    let commits = Arc::new(AtomicUsize::new(0));
    BEGINS.store(0, Ordering::SeqCst);
    STATEMENTS.store(0, Ordering::SeqCst);
    let writes_before = {
        let conn = state.work_db.connect().expect("db");
        let commit_counter = commits.clone();
        conn.commit_hook(Some(move || {
            commit_counter.fetch_add(1, Ordering::SeqCst);
            false
        }));
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        conn.total_changes()
    };

    let sink = make_session_sink();
    let ctx = dispatch(&state, &sink);
    apply_work_item_patch(
        ctx,
        project.id.clone(),
        WorkItemPatch::builder().status("archived").build(),
        None,
        false,
    )
    .await;

    let writes_after = {
        let conn = state.work_db.connect().expect("db");
        conn.trace_v2(TraceEventCodes::empty(), None);
        conn.commit_hook(None::<fn() -> bool>);
        conn.total_changes()
    };
    let cost = Cost {
        transactions: BEGINS.load(Ordering::SeqCst),
        commits: commits.load(Ordering::SeqCst),
        statements: STATEMENTS.load(Ordering::SeqCst),
    };

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
        "archiving a project with {child_count} children must send exactly one reply"
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
                    class,
                },
            ..
        } => {
            assert_eq!(pushed_topic, topic);
            assert_eq!(reason, "work_item_updated");
            assert_eq!(
                class,
                crate::protocol::InvalidationClass::Structural,
                "an archive cascade moves cards, so it must stay a full-board invalidation"
            );
            assert_eq!(pushed_product.as_deref(), Some(product_id.as_str()));
            assert_eq!(item_ids, vec![project.id.clone()]);
        }
        other => panic!("expected WorkInvalidated push, got {other:?}"),
    }
    assert!(
        watcher.next().await.is_none(),
        "archiving a project with {child_count} children must publish exactly one invalidation"
    );

    // Bounded writes: the project row and its audit row, never per child.
    let row_writes = writes_after - writes_before;
    assert!(
        row_writes <= MAX_ROW_WRITES,
        "archiving a project with {child_count} children wrote {row_writes} rows; \
         expected at most {MAX_ROW_WRITES} (project row + status audit row)"
    );

    // And the children are untouched: still present, still `todo`.
    let children = state
        .work_db
        .list_tasks(&product_id, Some(&project.id), None, false)
        .expect("list children");
    assert_eq!(children.len(), child_count);
    assert!(
        children.iter().all(|t| t.status == boss_protocol::TaskStatus::Todo),
        "archiving the project must not rewrite its children's status"
    );
    cost
}
