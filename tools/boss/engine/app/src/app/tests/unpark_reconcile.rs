//! The RPC half of the un-park reconciliation: un-parking through
//! `UpdateWorkItem { parked: false }` (what `ParkWorkItem` becomes) traces the
//! database steps before it replies and runs the GitHub-touching steps as a
//! background job. The steps themselves are tested in `unpark_reconcile` in the
//! engine core crate; this proves the request handler reaches them.

use async_trait::async_trait;
use boss_engine_merge_poller::{MergeProbe, PrLifecycleProbe, PrLifecycleState, PrReviewState};
use boss_protocol::{WorkItem, WorkItemPatch};

use super::*;
use crate::app::work_items::apply_work_item_patch;

/// A probe that reports every PR merged, so the job's refresh step has an
/// observable effect (`in_review` → `done`) with no GitHub in the loop.
struct MergedProbe;

#[async_trait]
impl MergeProbe for MergedProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        Ok(PrLifecycleProbe::builder()
            .url(url.to_owned())
            .state(PrLifecycleState::Merged)
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .base_ref_oid("base-1".to_owned())
            .head_ref_oid("head-1".to_owned())
            .build())
    }
}

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, request_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-unpark")
        .request_id(request_id)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn patch(state: &Arc<ServerState>, id: &str, patch: WorkItemPatch, request_id: &str) -> WorkItem {
    let sink = make_session_sink();
    apply_work_item_patch(dispatch(state, &sink, request_id), id.to_owned(), patch, None).await;
    sink.close();
    match sink.next().await.expect("the handler must answer").payload {
        FrontendEvent::WorkItemUpdated { item } => item,
        other => panic!("expected WorkItemUpdated, got {other:?}"),
    }
}

fn state_with_probe(probe: Arc<dyn MergeProbe>) -> (Arc<ServerState>, tempfile::TempDir) {
    test_server_state_with_overrides(ServerStateOverrides {
        merge_probe: Some(probe),
        ..Default::default()
    })
}

fn task_of(state: &ServerState, id: &str) -> boss_protocol::Task {
    match state.work_db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn parking_lines(task: &boss_protocol::Task) -> Vec<String> {
    task.description
        .lines()
        .filter(|line| line.starts_with("[parking]"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn unparking_over_the_rpc_traces_the_database_steps_and_runs_the_job() {
    let (state, _dir) = state_with_probe(Arc::new(MergedProbe));
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let chore = boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, product.id, "Ship it");
    state
        .work_db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/example/repo/pull/77".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    patch(&state, &chore.id, WorkItemPatch::builder().parked(true).build(), "park").await;
    assert!(task_of(&state, &chore.id).parked_at.is_some());
    assert!(
        parking_lines(&task_of(&state, &chore.id)).is_empty(),
        "parking leaves no reconciliation trail"
    );

    let WorkItem::Chore(reply) = patch(
        &state,
        &chore.id,
        WorkItemPatch::builder().parked(false).build(),
        "unpark",
    )
    .await
    else {
        panic!("expected a chore");
    };

    assert!(reply.parked_at.is_none(), "the reply is the un-parked item");
    // Steps 1 and 2 are traced before the reply goes out.
    let at_reply = parking_lines(&task_of(&state, &chore.id));
    assert!(at_reply.len() >= 2, "{at_reply:#?}");
    assert!(
        at_reply[0].contains("clear_park: un-parked by human"),
        "{}",
        at_reply[0]
    );
    assert!(at_reply[1].contains("clear_halt_stamps: "), "{}", at_reply[1]);

    // Steps 3 to 7 run in the background: the merged PR takes the merged path.
    let mut finished = None;
    for _ in 0..500 {
        let task = task_of(&state, &chore.id);
        if parking_lines(&task)
            .iter()
            .any(|line| line.contains("progression_grace"))
        {
            finished = Some(task);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let task = finished.unwrap_or_else(|| {
        panic!(
            "the background job must run to its last step; the trail so far: {:#?}",
            parking_lines(&task_of(&state, &chore.id))
        )
    });
    assert_eq!(
        task.status,
        boss_protocol::TaskStatus::Done,
        "the PR merged while the item was parked"
    );
    let lines = parking_lines(&task);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("refresh_pr: the PR merged while the item was parked")),
        "{lines:#?}"
    );
    assert_eq!(
        state.work_db.progression_grace_waived_at(&chore.id).unwrap(),
        None,
        "a finished item has nothing for progression to look at"
    );
}

#[tokio::test]
async fn parking_and_unparking_a_project_over_the_rpc_reconciles_each_child() {
    let (state, _dir) = state_with_probe(Arc::new(MergedProbe));
    let product = boss_engine_runtime::test_support::create_test_product_with_repo(
        &state.work_db,
        "Wave",
        Some("git@example.com:example/repo.git"),
    );
    let project = state
        .work_db
        .create_project(
            boss_protocol::CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("wave")
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    let children: Vec<String> = (0..3)
        .map(|n| {
            let task = state
                .work_db
                .create_task(
                    boss_protocol::CreateTaskInput::builder()
                        .product_id(product.id.clone())
                        .project_id(project.id.clone())
                        .name(format!("child {n}"))
                        .autostart(false)
                        .build(),
                )
                .unwrap();
            state
                .work_db
                .update_work_item(
                    &task.id,
                    WorkItemPatch {
                        status: Some("in_review".into()),
                        pr_url: Some(format!("https://github.com/example/repo/pull/{}", 80 + n)),
                        ..WorkItemPatch::default()
                    },
                )
                .unwrap();
            task.id
        })
        .collect();
    patch(
        &state,
        &project.id,
        WorkItemPatch::builder().parked(true).build(),
        "park",
    )
    .await;

    patch(
        &state,
        &project.id,
        WorkItemPatch::builder().parked(false).build(),
        "unpark",
    )
    .await;

    for child in &children {
        let mut done = false;
        for _ in 0..500 {
            if task_of(&state, child).status == boss_protocol::TaskStatus::Done {
                done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            done,
            "{child}: the merged PR was routed to done by the project's job; trail: {:#?}",
            parking_lines(&task_of(&state, child))
        );
        let task = task_of(&state, child);
        assert!(task.parked_at.is_none());
        let lines = parking_lines(&task);
        assert!(
            lines
                .first()
                .is_some_and(|line| line.contains("clear_park: un-parked with project")),
            "{lines:#?}"
        );
    }
}
