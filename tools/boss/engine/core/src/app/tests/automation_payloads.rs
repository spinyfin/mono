use super::*;
use crate::app::automations::{
    handle_create_automation, handle_disable_automation, handle_get_automation, handle_list_automations,
    handle_update_automation,
};
use crate::test_support::{create_product, seed_daily_automation};
use crate::work::AutomationFireRecord;
use boss_protocol::{AutomationPatch, AutomationRun, AutomationTrigger, CreateAutomationInput};

fn dispatch_for(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-automations")
        .request_id("req-automation")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

fn record_run(state: &ServerState, automation_id: &str, scheduled_for: i64, outcome: &str, detail: &str) {
    state
        .work_db
        .record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation_id)
                .scheduled_for(scheduled_for)
                .started_at(scheduled_for)
                .outcome(outcome)
                .detail(detail)
                .build(),
        )
        .unwrap();
}

/// A brand-new automation has never fired and owns no tasks: the push must
/// say so explicitly (`0` / no run) rather than leaving the app to ask.
#[tokio::test]
async fn automation_created_carries_zero_count_and_no_last_run() {
    let (state, _dir) = test_server_state();
    let product_id = create_product(&state.work_db);
    let sink = make_session_sink();
    let input = CreateAutomationInput::builder()
        .product_id(product_id)
        .name("fresh")
        .trigger(AutomationTrigger::Schedule {
            cron: "0 14 * * *".to_owned(),
            timezone: "UTC".to_owned(),
        })
        .standing_instruction("do the thing")
        .build();

    handle_create_automation(dispatch_for(&state, &sink), FrontendRequest::CreateAutomation { input }).await;

    let FrontendEvent::AutomationCreated {
        automation,
        open_task_count,
        last_run,
    } = sole_response(&sink).await
    else {
        panic!("expected AutomationCreated");
    };
    assert_eq!(automation.name, "fresh");
    assert_eq!(open_task_count, Some(0));
    assert!(last_run.is_none());
}

/// `automation_updated` (update, enable, disable share the builder) and
/// `automation_result` all carry the live open-task count and the newest run
/// — the older run must not win.
#[tokio::test]
async fn updated_and_result_carry_open_task_count_and_newest_run() {
    let (state, _dir) = test_server_state();
    let product_id = create_product(&state.work_db);
    let automation = seed_daily_automation(&state.work_db, &product_id);
    state
        .work_db
        .create_automation_task(&automation.id, "fix the thing", None, &[], &[])
        .unwrap();
    record_run(
        &state,
        &automation.id,
        1_000,
        boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
        "older run",
    );
    record_run(
        &state,
        &automation.id,
        2_000,
        boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
        "newest run",
    );

    let assert_summary = |open_task_count: Option<i64>, last_run: Option<AutomationRun>| {
        assert_eq!(open_task_count, Some(1));
        assert_eq!(
            last_run.expect("last_run must be present").detail.as_deref(),
            Some("newest run")
        );
    };

    let sink = make_session_sink();
    handle_get_automation(
        dispatch_for(&state, &sink),
        FrontendRequest::GetAutomation {
            id: automation.id.clone(),
        },
    )
    .await;
    let FrontendEvent::AutomationResult {
        open_task_count,
        last_run,
        ..
    } = sole_response(&sink).await
    else {
        panic!("expected AutomationResult");
    };
    assert_summary(open_task_count, last_run);

    let sink = make_session_sink();
    handle_update_automation(
        dispatch_for(&state, &sink),
        FrontendRequest::UpdateAutomation {
            id: automation.id.clone(),
            patch: AutomationPatch::builder().name("renamed").build(),
        },
    )
    .await;
    let FrontendEvent::AutomationUpdated {
        automation: updated,
        open_task_count,
        last_run,
    } = sole_response(&sink).await
    else {
        panic!("expected AutomationUpdated");
    };
    assert_eq!(updated.name, "renamed");
    assert_summary(open_task_count, last_run);

    let sink = make_session_sink();
    handle_disable_automation(
        dispatch_for(&state, &sink),
        FrontendRequest::DisableAutomation { id: automation.id },
    )
    .await;
    let FrontendEvent::AutomationUpdated {
        open_task_count,
        last_run,
        ..
    } = sole_response(&sink).await
    else {
        panic!("expected AutomationUpdated");
    };
    assert_summary(open_task_count, last_run);
}

/// The list carries the newest run of each automation that has fired and
/// nothing for one that has not, in the same round trip as the counts.
#[tokio::test]
async fn automations_list_batches_newest_run_per_automation() {
    let (state, _dir) = test_server_state();
    let product_id = create_product(&state.work_db);
    let fired = seed_daily_automation(&state.work_db, &product_id);
    let idle = state
        .work_db
        .create_automation(
            CreateAutomationInput::builder()
                .product_id(product_id.clone())
                .name("idle")
                .trigger(AutomationTrigger::Schedule {
                    cron: "0 15 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                })
                .standing_instruction("never fires")
                .build(),
        )
        .unwrap();
    record_run(
        &state,
        &fired.id,
        1_000,
        boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
        "older run",
    );
    record_run(
        &state,
        &fired.id,
        2_000,
        boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
        "newest run",
    );

    let sink = make_session_sink();
    handle_list_automations(
        dispatch_for(&state, &sink),
        FrontendRequest::ListAutomations { product_id },
    )
    .await;

    let FrontendEvent::AutomationsList {
        automations,
        open_task_counts,
        last_runs,
        ..
    } = sole_response(&sink).await
    else {
        panic!("expected AutomationsList");
    };
    assert_eq!(automations.len(), 2);
    assert_eq!(open_task_counts.len(), 2);
    assert_eq!(last_runs.len(), 1, "only the automation that fired has a last run");
    assert_eq!(last_runs[&fired.id].detail.as_deref(), Some("newest run"));
    assert!(!last_runs.contains_key(&idle.id));
}
