//! Real CLI -> peer-authenticated gateway -> mTLS hub -> durable projection
//! -> command reply -> CLI. No frontend dispatch is impersonated by the hub.
use super::*;
use boss_protocol_spoke::v1::{self, command, record};
use prost::Message;

#[path = "spoke_requests/support.rs"]
mod support;
use support::*;

#[tokio::test]
async fn online_cli_reads_use_the_bound_run_and_replay_the_stored_reply() {
    let mut fx = Fixture::new().await;
    let context = fx.read(&["context"]).await;
    assert_eq!(context["task"]["id"], fx.work_item_id);
    let context_reply = reply(&fx.effect_payloads()[0]);
    assert!(!context_reply.revision.is_empty());
    assert!(context_reply.observed_unix_ms > 0);
    let body = fx.read(&["pr", "body"]).await;
    assert_eq!(body["title"], "The bound PR");
    assert_eq!(body["body"], "Snapshot before execution");
    let status = fx.read(&["pr", "status", "--refresh"]).await;
    assert_eq!(status["pr_url"], PR);
    assert_eq!(status["refreshed"], true);
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        fx.state
            .work_db
            .get_pr_status_snapshot(&fx.work_item_id)
            .unwrap()
            .unwrap()
            .head_sha
            .as_deref(),
        Some("abc123")
    );
    let before = fx.effect_payloads();
    assert_eq!(before.len(), 3);
    let replay = fx.last_request.clone().unwrap();
    fx.send_record(replay.clone()).await;
    fx.expect_receipt().await;
    fx.projector().drain().await;
    assert_eq!(fx.effect_payloads(), before, "network replay applies nothing twice");

    // A duplicate request in a new journal record still cannot repeat a read
    // or replace its reply, even after the underlying status changes.
    fx.state
        .work_db
        .set_pr_status_observation(
            &fx.work_item_id,
            "MERGEABLE",
            Some("CLEAN"),
            Some("changed-after-reply"),
            &boss_work_db::now_string(),
        )
        .unwrap();
    fx.append(replay.body.unwrap());
    fx.receive_next().await;
    fx.projector().drain().await;
    assert_eq!(
        fx.effect_payloads(),
        before,
        "request identity survives a second sequence"
    );
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reserved_online_methods_return_denied_to_the_actual_cli() {
    let mut fx = Fixture::new().await;
    let cli = fx.start_cli(&["pr", "status", "--refresh"], &fx.execution_id);
    let mut request = fx.next_record().await;
    let Some(record::Body::Request(read)) = &mut request.body else {
        panic!("request")
    };
    read.method = v1::WorkerMethod::ProposalStatus as i32;
    read.proposal_id = "another-proposal".into();
    fx.send_record(request).await;
    fx.expect_receipt().await;
    fx.project_and_reply().await;
    denied(cli.await.unwrap(), "worker_method_denied");
    let effects = fx.effect_payloads();
    let reply = reply(&effects[0]);
    assert_eq!(reply.code, v1::ResultCode::Rejected as i32);
    assert!(reply.result_json.is_empty());
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cross_run_cli_claims_and_forged_host_assignments_never_reach_application() {
    let mut fx = Fixture::new().await;
    denied(
        fx.start_cli(&["context"], "another-execution").await.unwrap(),
        "worker_attribution_mismatch",
    );
    assert_eq!(fx.gateway.journal.lock().unwrap().record_bounds().unwrap().1, 0);
    let cli = fx.start_cli(&["pr", "status", "--refresh"], &fx.execution_id);
    let mut request = fx.next_record().await;
    // A compromised spoke cannot claim another host's valid assignment.
    let db = fx.state.work_db.clone();
    db.set_host_transport("other-host", boss_work_db::HostTransport::SpokeReady)
        .unwrap();
    let other = db.reserve_spoke_assignment("other-execution", "other-host").unwrap();
    request.run = Some(v1::Run {
        execution_id: other.execution_id,
        run_id: other.run_id,
        assignment_generation: other.assignment_generation,
        spawn_token: other.spawn_token,
    });
    fx.send_record(request).await;
    let rejected = fx.next_frame().await.unwrap_err();
    assert_eq!(rejected.code(), tonic::Code::PermissionDenied);
    assert_eq!(db.spoke_journal(HOST).unwrap().unwrap().received, 0);
    fx.projector().drain().await;
    assert!(fx.effect_payloads().is_empty());
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    denied(cli.await.unwrap(), "hub_unavailable");
}

#[tokio::test]
async fn a_new_assignment_denies_an_already_received_cli_read() {
    let mut fx = Fixture::new().await;
    let cli = fx.start_cli(&["pr", "status", "--refresh"], &fx.execution_id);
    fx.receive_next().await;
    let db = fx.state.work_db.clone();
    db.resolve_spoke_assignment(&fx.execution_id, fx.run.assignment_generation, "completed")
        .unwrap();
    db.reserve_spoke_assignment(&fx.execution_id, HOST).unwrap();
    // Only the rejected reply may cross the old assignment's command fence.
    fx.project_and_reply().await;
    let effects = fx.effect_payloads();
    assert_eq!(reply(&effects[0]).code, v1::ResultCode::Rejected as i32);
    assert!(reply(&effects[0]).reason.contains("worker_assignment_denied"));
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    denied(cli.await.unwrap(), "worker_assignment_denied");
}

#[tokio::test]
async fn revocation_between_receipt_and_projection_denies_without_reading() {
    let mut fx = Fixture::new().await;
    let cli = fx.start_cli(&["pr", "status", "--refresh"], &fx.execution_id);
    fx.receive_next().await;
    fx.hub.credentials().revoke(HOST, "test", "host removed").unwrap();
    fx.projector().drain().await;
    let effects = fx.effect_payloads();
    assert_eq!(effects.len(), 1);
    let reply = reply(&effects[0]);
    assert_eq!(reply.code, v1::ResultCode::Rejected as i32);
    assert!(reply.reason.contains("worker_host_denied"));
    assert!(reply.result_json.is_empty());
    assert_eq!(fx.probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    denied(cli.await.unwrap(), "hub_unavailable");
    // Revocation also rejects subsequent requests on the existing stream.
    let next = fx.next_frame().await;
    assert!(
        next.is_err()
            || matches!(next, Ok(None))
            || matches!(
                next,
                Ok(Some(v1::HubFrame {
                    body: Some(v1::hub_frame::Body::Reject(_)),
                    ..
                }))
            )
    );
}

fn reply(bytes: &[u8]) -> v1::WorkerReply {
    match v1::Command::decode(bytes).unwrap().body.unwrap() {
        command::Body::WorkerReply(reply) => reply,
        other => panic!("expected worker reply, got {other:?}"),
    }
}
