//! Integration test: measure and pin the end-to-end engine cost of archiving
//! a project that owns N child tasks, over the real frontend socket.
//!
//! Drives the exact RPC the macOS app sends from the navigator's right-click
//! "Archive" item (`UpdateWorkItem { patch: { status: "archived" } }`) against
//! an in-process engine on a temp socket, with a second client subscribed to
//! the product's work topic — the same shape the app's invalidation
//! subscription takes. It reports, per segment:
//!
//! ```text
//!   rpc          UpdateWorkItem issue → WorkItemUpdated reply
//!   invalidation UpdateWorkItem issue → WorkInvalidated push arrival, stamped
//!                by the watcher's reader task when the push is decoded (not
//!                when the test drains the queue)
//!   work_tree    GetWorkTree issue → WorkTree reply (what the app refetches)
//! ```
//!
//! and pins the contract that archiving a project with N children costs a
//! bounded number of engine pushes: exactly one `WorkItemUpdated` reply and
//! exactly one `WorkInvalidated` push on the product topic, regardless of N.
//! A per-child invalidation (or a per-child `WorkItemUpdated`) would show up
//! here as extra pushes inside the quiet window and fail the test.
//!
//! Engine tracing records correlated monotonic handler-entry, transaction-begin,
//! successful commit, and completed work-invalidated publication boundaries.
//! Only the app's main-actor apply/re-render needs live GUI measurement.
//!
//! Timings are printed (`--test_output=all`), not asserted: wall clock on a
//! shared CI runner is not a stable gate. The event-count pin is the gate.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{
    CreateManyTasksInput, CreateProductInput, CreateProjectInput, CreateTaskInput, FrontendEvent, FrontendRequest,
    Project, ProjectStatus, TopicEventPayload, WorkItem, WorkItemPatch, work_product_topic,
};

mod archive_timing_support;
mod common;
mod watcher_support;
use common::{TestEngine, TestEngineOptions};
use watcher_support::{Watcher, subscribe_watcher};

/// How long the watcher waits for the archive's own invalidation.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// Quiet window after the archive settles in which *no further* push may
/// arrive on the product topic — this is what makes "exactly one" testable.
const QUIET_WINDOW: Duration = Duration::from_millis(400);

struct Seeded {
    product_id: String,
    project: Project,
}

async fn seed(client: &mut BossClient, child_count: usize) -> Result<Seeded> {
    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Archive timing")
                .repo_remote_url("git@example.com:archive/timing.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(product),
        } => product,
        other => return Err(anyhow!("unexpected product-create reply: {other:?}")),
    };
    let project = match client
        .send_request(&FrontendRequest::CreateProject {
            input: CreateProjectInput {
                product_id: product.id.clone(),
                name: "Make tmux the only pane hosting mode".to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
                design_reasoning_effort_xhigh: false,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Project(project),
        } => project,
        other => return Err(anyhow!("unexpected project-create reply: {other:?}")),
    };
    let items: Vec<CreateTaskInput> = (0..child_count)
        .map(|i| {
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(format!("Child task {i}"))
                .description(format!("child {i} of the archive-timing project"))
                .autostart(false)
                .build()
        })
        .collect();
    match client
        .send_request(&FrontendRequest::CreateManyTasks {
            input: CreateManyTasksInput { items },
        })
        .await?
    {
        FrontendEvent::WorkItemsCreated { items } => {
            assert_eq!(items.len(), child_count, "seeded child count");
        }
        other => return Err(anyhow!("unexpected create-many reply: {other:?}")),
    }
    Ok(Seeded {
        product_id: product.id,
        project,
    })
}

/// Drain whatever invalidations the seeding itself produced so the archive
/// measurement starts from a quiet topic.
async fn drain_until_quiet(watcher: &Watcher) {
    while watcher.next_invalidation(QUIET_WINDOW).await.is_ok() {}
}

async fn fetch_work_tree(client: &mut BossClient, product_id: &str) -> Result<(Duration, usize, usize)> {
    let started = Instant::now();
    let reply = client
        .send_request(&FrontendRequest::GetWorkTree {
            product_id: product_id.to_owned(),
            fetch_seq: None,
        })
        .await?;
    let elapsed = started.elapsed();
    match reply {
        FrontendEvent::WorkTree { tasks, .. } => {
            let bytes = serde_json::to_vec(&tasks)?.len();
            Ok((elapsed, tasks.len(), bytes))
        }
        other => Err(anyhow!("unexpected get_work_tree reply: {other:?}")),
    }
}

struct ArchiveTimings {
    rpc: Duration,
    invalidation: Duration,
    work_tree_before: Duration,
    work_tree_after: Duration,
    /// `(tasks in the tree, serialized task bytes)` of the post-archive fetch.
    tree_size: (usize, usize),
}

async fn measure(child_count: usize, boundaries: &archive_timing_support::Boundaries) -> Result<ArchiveTimings> {
    // On-disk SQLite, as in production — a `:memory:` DB would hide the
    // per-transaction fsync that an on-disk cascade pays per row.
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let seeded = seed(&mut client, child_count).await?;
    let topic = work_product_topic(&seeded.product_id);
    let watcher = subscribe_watcher(engine.socket_str(), topic.clone()).await?;
    drain_until_quiet(&watcher).await;

    let (work_tree_before, _, _) = fetch_work_tree(&mut client, &seeded.product_id).await?;

    let started = Instant::now();
    let reply = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: seeded.project.id.clone(),
            patch: WorkItemPatch::builder().status("archived").build(),
        })
        .await?;
    let received = Instant::now();
    let rpc = received.duration_since(started);
    boundaries.report(started, received, child_count);
    match reply {
        FrontendEvent::WorkItemUpdated {
            item: WorkItem::Project(project),
        } => {
            assert_eq!(project.id, seeded.project.id);
            assert_eq!(project.status, ProjectStatus::Archived);
        }
        other => return Err(anyhow!("unexpected archive reply: {other:?}")),
    }

    // The archive's own invalidation: one push, naming the project.
    let push = watcher.next_invalidation(PUSH_TIMEOUT).await?;
    let invalidation = push.received_at.saturating_duration_since(started);
    assert_eq!(push.topic, topic);
    match push.event {
        TopicEventPayload::WorkInvalidated {
            reason,
            product_id,
            item_ids,
        } => {
            assert_eq!(reason, "work_item_updated");
            assert_eq!(product_id.as_deref(), Some(seeded.product_id.as_str()));
            assert_eq!(item_ids, vec![seeded.project.id.clone()]);
        }
        other => return Err(anyhow!("unexpected push payload: {other:?}")),
    }

    // Pin: nothing else is pushed for the archive. A per-child cascade
    // (per-row invalidation, per-row execution churn) would land here.
    if let Ok(extra) = watcher.next_invalidation(QUIET_WINDOW).await {
        return Err(anyhow!(
            "archiving a project with {child_count} children pushed a second invalidation \
             on {}: {:?}",
            extra.topic,
            extra.event
        ));
    }

    let (work_tree_after, tree_tasks, tree_bytes) = fetch_work_tree(&mut client, &seeded.product_id).await?;
    assert_eq!(tree_tasks, child_count, "archiving the project must not drop its tasks");

    Ok(ArchiveTimings {
        rpc,
        invalidation,
        work_tree_before,
        work_tree_after,
        tree_size: (tree_tasks, tree_bytes),
    })
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::test]
async fn archive_boundaries_and_bounded_pushes_at_product_scales() -> Result<()> {
    // One subscriber and sequential requests: each captured handler span
    // belongs to the one archive under measurement, never a sibling fixture.
    let boundaries = archive_timing_support::Boundaries::install();
    for children in [50, 500, 1949] {
        let t = measure(children, &boundaries).await?;
        eprintln!(
            "archive[N={children}] rpc={:.3}ms invalidation={:.3}ms work_tree(before)={:.3}ms work_tree(after)={:.3}ms tree_tasks={} tree_task_bytes={}",
            ms(t.rpc),
            ms(t.invalidation),
            ms(t.work_tree_before),
            ms(t.work_tree_after),
            t.tree_size.0,
            t.tree_size.1
        );
    }
    Ok(())
}
