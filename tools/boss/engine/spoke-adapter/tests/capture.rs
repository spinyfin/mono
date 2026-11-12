//! `SpokeHostAdapter::ctl_capture` against a real hub: its capability gate, its
//! TTL translation and its mapping of the spoke's answer.
//!
//! The production hub (`Contracts::first_release`) requires `worker-api`,
//! `proposal-json` and `artifact`. The production executor Hello does not
//! advertise those — the executor does not implement them — so connecting that
//! Hello to that hub is refused and pane-capture is never negotiated. Tests
//! that need a live stream therefore use a hub derived from first-release with
//! those three names dropped from `required`. They do not add unimplemented
//! features to Hello to make the handshake succeed.
use std::time::Duration;

use boss_execution_contracts::{PaneCaptureOutcome, PaneCaptureRequest};
use boss_protocol_spoke::{
    negotiate::{Contracts, feature},
    v1::{Capture, Command, PaneCapture, Record, ResultCode, Stamp, command, hub_frame, record},
};
use boss_spoke_adapter::SpokeAdapterConfig;

use crate::support::*;
use crate::{lease, spawn, standard};

fn adapter_config() -> SpokeAdapterConfig {
    SpokeAdapterConfig::builder()
        .command_deadline(Duration::from_secs(5))
        .build()
}

/// Production hub contract set. Same as `Contracts::first_release()`.
fn production_hub_contracts() -> Contracts {
    Contracts::first_release()
}

/// A hub that can complete Hello with the production executor: first-release
/// minus the three required features the executor does not implement.
fn hub_that_negotiates_with_executor() -> Contracts {
    let mut contracts = Contracts::first_release();
    contracts
        .required
        .retain(|name| name != feature::WORKER_API && name != feature::PROPOSAL_JSON && name != feature::ARTIFACT);
    contracts
}

/// The production executor Hello. Same list the live spoke sends.
fn production_executor_hello() -> Contracts {
    boss_protocol_spoke::negotiate::executor_contracts()
}

async fn hub_for_executor() -> Fixture {
    Fixture::with_hub_contracts(adapter_config(), hub_that_negotiates_with_executor()).await
}

fn capture_of(command: &Command) -> Option<&Capture> {
    match &command.body {
        Some(command::Body::Capture(capture)) => Some(capture),
        _ => None,
    }
}

fn captures(commands: &[Command]) -> Vec<Capture> {
    commands.iter().filter_map(|c| capture_of(c).cloned()).collect()
}

/// How many commands the hub has stored for the host.
fn stored_commands(fx: &Fixture) -> usize {
    let (mut count, mut after) = (0, 0);
    while let Some(next) = fx.db.host_command_after(HOST, after).unwrap() {
        count += 1;
        after = next.sequence;
    }
    count
}

fn request(ttl_seconds: u32, cancel: bool) -> PaneCaptureRequest {
    PaneCaptureRequest {
        subscription_id: "viewer-1".into(),
        interval_ms: 0,
        max_bytes: 0,
        ttl_seconds,
        cancel,
    }
}

/// A spoke that answers lease, release, spawn and capture.
fn script(capture_code: ResultCode) -> Script {
    Box::new(move |c| match &c.body {
        Some(command::Body::Capture(_)) => Some(refused(c, capture_code, "scripted")),
        _ => standard(c),
    })
}

#[tokio::test]
async fn production_executor_hello_is_accepted_by_the_production_hub() {
    let fx = Fixture::with_hub_contracts(adapter_config(), production_hub_contracts()).await;
    match fx.hello_reply(production_executor_hello().features).await {
        hub_frame::Body::Welcome(_) => {}
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn the_production_hello_negotiates_capture_and_a_capture_command_reaches_the_spoke() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke_contracts(script(ResultCode::Ok), None, 0, production_executor_hello())
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    assert_eq!(fx.manager.host_supports(HOST, feature::PANE_CAPTURE).await, Some(true));

    assert_eq!(
        adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap(),
        PaneCaptureOutcome::Accepted
    );
    let seen = captures(&peer.commands());
    assert_eq!(seen.len(), 1, "the capture command reached the spoke");
    assert_eq!(seen[0].subscription_id, "viewer-1");
    assert_eq!(seen[0].ttl_seconds, 0, "zero asks for the host's default");
}

#[tokio::test]
async fn a_host_that_did_not_negotiate_capture_is_unsupported_and_nothing_is_sent() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let mut hello = production_executor_hello();
    hello.features.retain(|f| f.name != feature::PANE_CAPTURE);
    let peer = fx.spoke_contracts(script(ResultCode::Ok), None, 0, hello).await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    assert_eq!(fx.manager.host_supports(HOST, feature::PANE_CAPTURE).await, Some(false));
    let before = stored_commands(&fx);

    let outcome = adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap();
    assert!(matches!(outcome, PaneCaptureOutcome::Unsupported(_)), "{outcome:?}");
    assert!(captures(&peer.commands()).is_empty());
    assert_eq!(stored_commands(&fx), before, "no command was stored");
}

#[tokio::test]
async fn a_disconnected_host_is_unreachable() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke_contracts(script(ResultCode::Ok), None, 0, production_executor_hello())
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    peer.disconnect();
    let deadline = tokio::time::Instant::now() + STEP;
    while fx.manager.host_supports(HOST, feature::PANE_CAPTURE).await.is_some() {
        assert!(tokio::time::Instant::now() < deadline, "the stream never closed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let outcome = adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap();
    assert!(matches!(outcome, PaneCaptureOutcome::Unreachable(_)), "{outcome:?}");
}

#[tokio::test]
async fn ttls_are_translated_for_the_wire() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke_contracts(script(ResultCode::Ok), None, 0, production_executor_hello())
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();

    for request in [
        request(0, false),
        request(1, false),
        request(30, false),
        request(30, true),
    ] {
        adapter.capture_worker_pane("exec_1", &request).await.unwrap();
    }
    let ttls: Vec<u32> = captures(&peer.commands()).iter().map(|c| c.ttl_seconds).collect();
    // Default, raised above the cancel value, as asked, and the cancel value.
    assert_eq!(
        ttls,
        vec![0, 2, 30, boss_protocol_spoke::limits::CANCEL_CAPTURE_TTL_SECONDS]
    );
}

#[tokio::test]
async fn the_spokes_answer_maps_to_an_outcome() {
    for (code, expect) in [
        (ResultCode::Ok, "accepted"),
        (ResultCode::Absent, "unreachable"),
        (ResultCode::Rejected, "refused"),
        (ResultCode::Unknown, "unreachable"),
    ] {
        let fx = hub_for_executor().await;
        fx.ready(HOST);
        let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
        let _peer = fx
            .spoke_contracts(script(code), None, 0, production_executor_hello())
            .await;
        lease(&adapter).await;
        spawn(&adapter).await.unwrap();
        let outcome = adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap();
        if code == ResultCode::Absent {
            assert!(
                matches!(&outcome, PaneCaptureOutcome::Unreachable(reason) if reason.contains("reports the worker has finished"))
            );
        }
        let got = match outcome {
            PaneCaptureOutcome::Accepted => "accepted",
            PaneCaptureOutcome::Ended => "ended",
            PaneCaptureOutcome::Refused(_) => "refused",
            PaneCaptureOutcome::Unreachable(_) => "unreachable",
            PaneCaptureOutcome::Unsupported(_) => "unsupported",
        };
        assert_eq!(got, expect, "{code:?}");
    }
}

#[tokio::test]
async fn an_absent_answer_while_the_assignment_is_open_is_not_ended() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx
        .spoke_contracts(script(ResultCode::Absent), None, 0, production_executor_hello())
        .await;
    lease(&adapter).await;
    let outcome = adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap();
    assert!(
        matches!(outcome, PaneCaptureOutcome::Unreachable(_)),
        "an open assignment must keep renewing, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_capture_record_on_a_negotiated_stream_reaches_the_viewer_feed() {
    let fx = hub_for_executor().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let mut feed = fx.manager.subscribe_captures();
    let peer = fx
        .spoke_contracts(script(ResultCode::Ok), None, 0, production_executor_hello())
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    adapter.capture_worker_pane("exec_1", &request(0, false)).await.unwrap();
    let capture = peer
        .commands()
        .into_iter()
        .find(|c| capture_of(c).is_some())
        .expect("capture command");
    peer.send_record(Record {
        observed: Some(Stamp {
            boot_id: "boot".into(),
            wall_unix_ms: 9,
            ..Default::default()
        }),
        run: capture.run.clone(),
        body: Some(record::Body::Capture(PaneCapture {
            subscription_id: "viewer-1".into(),
            text_utf8: b"live \x1b[31mred".to_vec(),
            truncated: false,
        })),
        ..Default::default()
    })
    .await;
    let seen = tokio::time::timeout(STEP, feed.recv()).await.unwrap().unwrap();
    assert_eq!(seen.subscription_id, "viewer-1");
    assert_eq!(seen.text, "live \x1b[31mred");
    assert_eq!(seen.observed_unix_ms, 9);
    assert!(!seen.truncated);
}
