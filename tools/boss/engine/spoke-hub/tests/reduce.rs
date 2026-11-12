use boss_protocol::StopReason;
use boss_protocol::{ProposalKind, WorkerEvent};
use boss_protocol_spoke::v1::{self, PaneState, Run, Stamp, record, spoke_frame};
use boss_protocol_spoke::worker_event;
use boss_spoke_hub::projection::reduce::{Observation, PROPOSAL_JSON, ProposalRequest, Reduction, Skip, reduce};
use boss_work_db::SpokeIngressRow;
use prost::Message;
use serde_json::json;

fn run() -> Run {
    Run {
        execution_id: "exec_1".into(),
        run_id: "run_1".into(),
        assignment_generation: 1,
        spawn_token: "token".into(),
    }
}

fn stamp() -> Stamp {
    Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: 7,
        ..Default::default()
    }
}

fn row(sequence: u64, run: Option<Run>, body: record::Body) -> SpokeIngressRow {
    let record = v1::Record {
        sequence,
        observed: Some(stamp()),
        run,
        body: Some(body),
    };
    SpokeIngressRow {
        sequence,
        frame: v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(record)),
        }
        .encode_to_vec(),
        epoch: 1,
        features: String::new(),
        observed_at_ms: 1,
    }
}

fn observations(reduction: Reduction) -> Vec<Observation> {
    match reduction {
        Reduction::Observations(found) => found,
        other => panic!("expected observations, got {other:?}"),
    }
}

fn proposal(kind: ProposalKind, key: &str, wire: v1::ProposalKind, contract: &str) -> v1::Proposal {
    v1::Proposal {
        proposal_id: key.into(),
        kind: wire as i32,
        payload_contract: contract.into(),
        canonical_json: serde_json::to_vec(&json!({
            "kind": kind, "payload": {"outcome": "delivered", "summary": "done"}, "idempotency_key": key,
        }))
        .unwrap(),
        ..Default::default()
    }
}

#[test]
fn an_event_keeps_its_run_order_stamp_and_subagent_origin() {
    let mut wire = worker_event::to_wire(&WorkerEvent::Stop {
        session_id: "s".into(),
        stop_hook_active: false,
        stop_reason: StopReason::Completed,
    });
    wire.from_subagent = true;
    let found = observations(reduce(
        "host-a",
        "journal",
        &row(4, Some(run()), record::Body::Event(wire)),
    ));
    let [Observation::Event(event)] = found.as_slice() else {
        panic!("one event expected: {found:?}")
    };
    assert!(event.from_subagent);
    assert!(matches!(
        event.event,
        WorkerEvent::Stop {
            stop_reason: StopReason::Completed,
            ..
        }
    ));
    assert_eq!((event.origin.sequence, event.origin.observed.wall_unix_ms), (4, 7));
    assert_eq!(
        (event.origin.host_id.as_str(), event.origin.run.execution_id.as_str()),
        ("host-a", "exec_1")
    );
}

#[test]
fn a_standard_proposal_carries_its_kind_payload_and_key() {
    let proposal = proposal(
        ProposalKind::RunDone,
        "auto:run_done:abc",
        v1::ProposalKind::RunDone,
        PROPOSAL_JSON,
    );
    let found = observations(reduce("h", "j", &row(2, Some(run()), record::Body::Proposal(proposal))));
    let [Observation::Proposal(found)] = found.as_slice() else {
        panic!("one proposal expected")
    };
    assert_eq!(found.proposal_id, "auto:run_done:abc");
    assert_eq!(
        found.request,
        ProposalRequest::Standard {
            kind: ProposalKind::RunDone,
            payload: json!({"outcome": "delivered", "summary": "done"}),
            idempotency_key: "auto:run_done:abc".into(),
        }
    );
}

#[test]
fn proposals_the_hub_cannot_read_are_refused_not_dropped() {
    let refused = |proposal: v1::Proposal| {
        let found = observations(reduce("h", "j", &row(2, Some(run()), record::Body::Proposal(proposal))));
        let [Observation::Proposal(found)] = found.as_slice() else {
            panic!("one proposal expected")
        };
        match &found.request {
            ProposalRequest::Refused(reason) => reason.clone(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    };
    assert!(
        refused(proposal(
            ProposalKind::RunDone,
            "k",
            v1::ProposalKind::RunDone,
            "guide-outcome/1"
        ))
        .contains("guide-outcome/1")
    );
    // Wire kind and payload kind must agree.
    assert!(
        refused(proposal(
            ProposalKind::RunDone,
            "k",
            v1::ProposalKind::Wait,
            PROPOSAL_JSON
        ))
        .contains("wire kind")
    );
    // The id is the idempotency key.
    let mut mismatched = proposal(ProposalKind::RunDone, "k", v1::ProposalKind::RunDone, PROPOSAL_JSON);
    mismatched.proposal_id = "different".into();
    assert!(refused(mismatched).contains("idempotency key"));
    // A review guide needs the host's git store.
    assert!(
        refused(proposal(
            ProposalKind::ReviewGuide,
            "k",
            v1::ProposalKind::OtherValidated,
            PROPOSAL_JSON
        ))
        .contains("review guides")
    );
    let mut garbage = proposal(ProposalKind::RunDone, "k", v1::ProposalKind::RunDone, PROPOSAL_JSON);
    garbage.canonical_json = b"{not json".to_vec();
    assert!(refused(garbage).contains("unreadable"));
}

#[test]
fn a_pane_and_every_member_of_an_inventory_page_are_pane_observations() {
    let pane = |life: v1::PaneLife| PaneState {
        life: life as i32,
        boot_id: "boot".into(),
        ..Default::default()
    };
    let one = observations(reduce(
        "h",
        "j",
        &row(1, Some(run()), record::Body::Pane(pane(v1::PaneLife::Alive))),
    ));
    assert!(matches!(one.as_slice(), [Observation::Pane(_)]));
    let mut second = run();
    second.execution_id = "exec_2".into();
    let page = record::Body::Inventory(v1::InventorySnapshot {
        runs: vec![
            v1::RunObservation {
                run: Some(run()),
                pane: Some(pane(v1::PaneLife::Exited)),
            },
            v1::RunObservation {
                run: Some(second),
                pane: Some(pane(v1::PaneLife::Missing)),
            },
        ],
        last_page: true,
        ..Default::default()
    });
    let found = observations(reduce("h", "j", &row(2, None, page)));
    let ids: Vec<_> = found.iter().map(|o| o.origin().run.execution_id.as_str()).collect();
    assert_eq!(ids, ["exec_1", "exec_2"]);
    // Every member shares the record's sequence.
    assert!(found.iter().all(|o| o.origin().sequence == 2));
}

#[test]
fn records_owned_by_other_consumers_or_settled_at_ingress_carry_nothing() {
    let capacity = record::Body::Capacity(v1::Capacity {
        generation: 1,
        total_slots: 1,
        ..Default::default()
    });
    assert_eq!(
        reduce("h", "j", &row(1, None, capacity)),
        Reduction::Skip(Skip::OtherConsumer("capacity"))
    );
    let capture = record::Body::Capture(v1::PaneCapture {
        subscription_id: "view".into(),
        ..Default::default()
    });
    assert_eq!(
        reduce("h", "j", &row(2, Some(run()), capture)),
        Reduction::Skip(Skip::LiveOnly)
    );
    let result = record::Body::Result(v1::CommandResult {
        command_id: "c".into(),
        code: v1::ResultCode::Ok as i32,
        ..Default::default()
    });
    assert_eq!(
        reduce("h", "j", &row(3, Some(run()), result)),
        Reduction::Skip(Skip::SettledAtIngress)
    );
}

#[test]
fn unreadable_frames_are_quarantined_never_applied() {
    let stored = |frame: Vec<u8>| SpokeIngressRow {
        sequence: 1,
        frame,
        epoch: 1,
        features: String::new(),
        observed_at_ms: 1,
    };
    assert!(matches!(
        reduce("h", "j", &stored(vec![0xff, 0xff, 0xff])),
        Reduction::Quarantine(_)
    ));
    // A hello is a transport control, never a journaled record.
    let hello = v1::SpokeFrame {
        body: Some(spoke_frame::Body::Hello(v1::Hello::default())),
    };
    assert!(matches!(
        reduce("h", "j", &stored(hello.encode_to_vec())),
        Reduction::Quarantine(_)
    ));
    // A record without the run its kind requires fails validation.
    let bare = row(
        1,
        None,
        record::Body::Pane(PaneState {
            life: v1::PaneLife::Alive as i32,
            ..Default::default()
        }),
    );
    assert!(matches!(reduce("h", "j", &bare), Reduction::Quarantine(_)));
    // A record whose sequence is not its ingress sequence is not trusted.
    let mut moved = row(
        5,
        Some(run()),
        record::Body::Pane(PaneState {
            life: v1::PaneLife::Alive as i32,
            boot_id: "boot".into(),
            ..Default::default()
        }),
    );
    moved.sequence = 6;
    assert!(matches!(reduce("h", "j", &moved), Reduction::Quarantine(_)));
    // An event with malformed tool JSON is unreadable.
    let bad = row(
        1,
        Some(run()),
        record::Body::Event(v1::WorkerEvent {
            session_id: "s".into(),
            event: Some(v1::worker_event::Event::PostToolUse(v1::ToolUse {
                name: "Bash".into(),
                input_json: b"{broken".to_vec(),
                response_json: Vec::new(),
            })),
            ..Default::default()
        }),
    );
    assert!(matches!(reduce("h", "j", &bad), Reduction::Quarantine(_)));
}
