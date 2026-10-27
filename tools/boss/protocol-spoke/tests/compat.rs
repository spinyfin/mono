//! Old/new peer compatibility.
//!
//! checkleft's `proto/breaking` checks that the schema evolves compatibly;
//! it cannot prove that a reader behaves correctly when it meets something it
//! has never seen. These tests play both roles with hand-declared prost types:
//! an "old peer" that knows only the stable handshake subset and a "new peer"
//! that has added fields, an enum value and a oneof variant after this build.
//! They exercise the rules in README.md: unknown fields are tolerated, absent
//! optional fields take their documented defaults, unknown enum numbers and
//! oneof variants are never success, and the handshake stays readable.

use boss_protocol_spoke::limits::{Limits, MAX_FRAME_BYTES};
use boss_protocol_spoke::negotiate::{Contracts, REQUIRED_FEATURES, Rejection, feature, reject_code};
use boss_protocol_spoke::v1;
use boss_protocol_spoke::{Validate, ValidationError};
use prost::Message;

/// What a build that predates every later addition knows about the handshake.
mod old_peer {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Reject {
        #[prost(string, tag = "1")]
        pub code: String,
        #[prost(string, tag = "2")]
        pub diagnostic: String,
        #[prost(uint32, tag = "3")]
        pub supported_min_epoch: u32,
        #[prost(uint32, tag = "4")]
        pub supported_max_epoch: u32,
    }
}

/// A later build: the same fields plus additions this build has never heard of.
mod new_peer {
    use boss_protocol_spoke::v1;

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Telemetry {
        #[prost(string, tag = "1")]
        pub note: String,
    }

    /// Fields a later build added to `Hello`. Protobuf concatenation means a
    /// current `Hello`'s bytes followed by these bytes is exactly what that
    /// later build would send.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct HelloAdditions {
        #[prost(string, tag = "13")]
        pub region: String,
        #[prost(message, optional, tag = "14")]
        pub future: Option<Telemetry>,
    }

    /// A `Deliver` using a delivery kind (9) that this build does not define.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Deliver {
        #[prost(int32, tag = "1")]
        pub kind: i32,
        #[prost(bytes = "vec", tag = "2")]
        pub input_utf8: Vec<u8>,
    }

    /// A `Record` carrying a body variant (30) that this build does not define.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Record {
        #[prost(uint64, tag = "1")]
        pub sequence: u64,
        #[prost(message, optional, tag = "2")]
        pub observed: Option<v1::Stamp>,
        #[prost(message, optional, tag = "3")]
        pub run: Option<v1::Run>,
        #[prost(oneof = "record::Body", tags = "11, 30")]
        pub body: Option<record::Body>,
    }

    pub mod record {
        use boss_protocol_spoke::v1;

        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Body {
            #[prost(message, tag = "11")]
            Event(v1::WorkerEvent),
            #[prost(message, tag = "30")]
            Telemetry(super::Telemetry),
        }
    }
}

fn required_features() -> Vec<v1::Feature> {
    REQUIRED_FEATURES
        .iter()
        .map(|name| v1::Feature {
            name: (*name).into(),
            revision: 1,
        })
        .collect()
}

fn current_hello() -> v1::Hello {
    v1::Hello {
        host_id: "spoke-1".into(),
        journal_id: "journal-1".into(),
        boot_id: "boot-1".into(),
        min_epoch: 1,
        max_epoch: 1,
        features: required_features(),
        instance_nonce: vec![9; 16],
        ..Default::default()
    }
}

/// A `Hello` as a later build sends it: every current field plus additions.
fn later_hello_bytes(hello: &v1::Hello) -> Vec<u8> {
    let additions = new_peer::HelloAdditions {
        region: "us-west".into(),
        future: Some(new_peer::Telemetry {
            note: "from the future".into(),
        }),
    };
    let mut bytes = hello.encode_to_vec();
    bytes.extend(additions.encode_to_vec());
    bytes
}

fn stamp() -> v1::Stamp {
    v1::Stamp {
        boot_id: "boot-1".into(),
        wall_unix_ms: 1_700_000_000_000,
        awake_ns: 5,
        elapsed_ns: 6,
    }
}

fn run() -> v1::Run {
    v1::Run {
        execution_id: "e".into(),
        run_id: "r".into(),
        assignment_generation: 1,
        spawn_token: "t".into(),
    }
}

#[test]
fn a_hello_with_fields_from_a_later_build_decodes_and_negotiates_unchanged() {
    let bytes = later_hello_bytes(&current_hello());
    let hello = v1::Hello::decode(bytes.as_slice()).expect("unknown fields 13 and 14 are skipped");

    assert_eq!(
        (hello.host_id.as_str(), hello.min_epoch, hello.max_epoch),
        ("spoke-1", 1, 1)
    );
    assert_eq!(hello.instance_nonce, vec![9; 16]);
    hello.validate().expect("the known fields are enough");

    let selection = Contracts::first_release().select(&hello).expect("compatible");
    assert!(selection.has(feature::EXECUTION));
}

#[test]
fn raw_bytes_kept_by_the_journal_still_decode_for_the_newer_reader() {
    // prost is not assumed to carry unknown fields through a decode and
    // re-encode, so the journal keeps the original bytes. Those bytes remain
    // fully readable by the build that understands them.
    let original = later_hello_bytes(&current_hello());
    let _ = v1::Hello::decode(original.as_slice()).expect("this build decodes it");
    let later = new_peer::HelloAdditions::decode(original.as_slice()).expect("the newer build decodes it");
    assert_eq!(later.region, "us-west");
    assert_eq!(later.future.map(|f| f.note), Some("from the future".to_owned()));
}

#[test]
fn absent_optional_fields_take_their_documented_defaults() {
    // A Welcome that never advertises bounds leaves local policy in force.
    let welcome = v1::Welcome::decode(b"".as_slice()).expect("empty decodes");
    assert_eq!((welcome.max_frame_bytes, welcome.max_inflight_bytes), (0, 0));
    assert_eq!(
        Limits::default().negotiated(welcome.max_frame_bytes, welcome.max_inflight_bytes),
        Limits::default()
    );

    // A first-ever connection has no hub command cursor and no inventory yet.
    let hello = v1::Hello::decode(current_hello().encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(
        (hello.received_commands.is_none(), hello.inventory.is_none()),
        (true, true)
    );
    hello.validate().expect("optional fields may be absent");

    // A capture request that leaves cadence unset asks for the defaults.
    let capture = v1::Capture::decode(b"\x0a\x01s".as_slice()).expect("decodes");
    assert_eq!((capture.interval_ms, capture.ttl_seconds, capture.max_bytes), (0, 0, 0));
    capture.validate().expect("zero means default");
}

#[test]
fn exit_status_presence_survives_the_wire() {
    let exited = |exit_code| v1::PaneState {
        life: v1::PaneLife::Exited as i32,
        exit_code,
        ..Default::default()
    };
    for exit_code in [Some(0), Some(137), Some(-1), None] {
        let decoded = v1::PaneState::decode(exited(exit_code).encode_to_vec().as_slice()).expect("decodes");
        assert_eq!(
            decoded.exit_code, exit_code,
            "exited with status 0 must stay distinct from no status"
        );
    }
}

#[test]
fn an_unknown_enum_number_survives_decoding_and_is_never_success() {
    let from_newer = new_peer::Deliver {
        kind: 9,
        input_utf8: b"hello".to_vec(),
    };
    let deliver = v1::Deliver::decode(from_newer.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(deliver.kind, 9, "the number is preserved, not collapsed to a default");
    assert!(v1::DeliveryKind::try_from(deliver.kind).is_err());
    let err = deliver.validate().expect_err("unknown delivery kind");
    assert_eq!(
        err,
        ValidationError::UnknownEnum {
            field: "deliver.kind",
            value: 9
        }
    );
    assert!(err.is_unsupported(), "a newer peer, not a malformed one");

    // Likewise for results: an unknown code is not OK.
    let result = v1::CommandResult {
        command_id: "c".into(),
        code: 99,
        ..Default::default()
    };
    let decoded = v1::CommandResult::decode(result.encode_to_vec().as_slice()).expect("decodes");
    assert_ne!(decoded.code, v1::ResultCode::Ok as i32);
    assert!(decoded.validate().expect_err("unknown code").is_unsupported());
}

#[test]
fn an_unknown_record_body_is_quarantined_not_applied() {
    let newer = new_peer::Record {
        sequence: 41,
        observed: Some(stamp()),
        run: Some(run()),
        body: Some(new_peer::record::Body::Telemetry(new_peer::Telemetry {
            note: "x".into(),
        })),
    };
    let bytes = newer.encode_to_vec();

    let record = v1::Record::decode(bytes.as_slice()).expect("an unknown variant is skipped, not fatal");
    assert_eq!(
        record.sequence, 41,
        "the envelope is still readable, so the journal can order and receipt it"
    );
    assert!(record.body.is_none());
    let err = record.validate().expect_err("no body this build understands");
    assert!(err.is_unsupported(), "{err}");

    // The retained bytes still carry the body for a build that understands it.
    let later = new_peer::Record::decode(bytes.as_slice()).expect("decodes");
    assert!(matches!(later.body, Some(new_peer::record::Body::Telemetry(_))));
}

#[test]
fn a_known_body_from_a_newer_peer_still_validates() {
    let event = v1::WorkerEvent {
        session_id: "s".into(),
        event: Some(v1::worker_event::Event::Notification("n".into())),
    };
    let newer = new_peer::Record {
        sequence: 1,
        observed: Some(stamp()),
        run: Some(run()),
        body: Some(new_peer::record::Body::Event(event)),
    };
    let record = v1::Record::decode(newer.encode_to_vec().as_slice()).expect("decodes");
    record.validate().expect("the shared subset is unchanged");
}

#[test]
fn negotiation_across_builds() {
    let local = Contracts::first_release();
    let hello_with = |features: Vec<v1::Feature>, min_epoch, max_epoch| {
        let hello = v1::Hello {
            features,
            min_epoch,
            max_epoch,
            ..current_hello()
        };
        v1::Hello::decode(later_hello_bytes(&hello).as_slice()).expect("decodes")
    };

    // Optional features absent: execution continues, the viewer and upgrade
    // messages are simply never sent.
    let selection = local
        .select(&hello_with(required_features(), 1, 1))
        .expect("compatible");
    assert!(selection.has(feature::EXECUTION));
    assert!(!selection.has(feature::PANE_CAPTURE) && !selection.has(feature::UPGRADE));

    // Features this build has never heard of are ignored.
    let mut with_unknown = required_features();
    with_unknown.push(v1::Feature {
        name: "telepathy".into(),
        revision: 3,
    });
    assert!(local.select(&hello_with(with_unknown, 1, 4)).is_ok());

    // A required feature missing is a rejection naming it.
    let mut missing = required_features();
    missing.retain(|f| f.name != feature::WORKSPACE);
    let rejection = local.select(&hello_with(missing, 1, 1)).expect_err("incompatible");
    assert_eq!(
        rejection,
        Rejection::MissingRequired {
            features: vec!["workspace".to_owned()]
        }
    );

    // No common epoch: the Reject is readable by a peer that knows only the
    // handshake subset, and it tells that peer what this build supports.
    let rejection = local
        .select(&hello_with(required_features(), 2, 3))
        .expect_err("future epoch");
    assert_eq!(rejection.code(), reject_code::NO_COMMON_EPOCH);
    let wire = rejection.to_reject(&local).encode_to_vec();
    let old = old_peer::Reject::decode(wire.as_slice()).expect("the handshake stays readable");
    assert_eq!(old.code, reject_code::NO_COMMON_EPOCH);
    assert_eq!((old.supported_min_epoch, old.supported_max_epoch), (1, 1));
    assert!(old.diagnostic.contains("1..=1"), "{}", old.diagnostic);
}

/// Pins the wire encoding of the handshake building blocks. A change to a
/// field number, a wire type or a default-omission rule changes these bytes;
/// the descriptor check would also flag it, but this fails even when the
/// baseline was edited to match.
#[test]
fn handshake_encoding_is_pinned() {
    let cursor = v1::Cursor {
        journal_id: "j".into(),
        through: 7,
    };
    assert_eq!(hex(&cursor.encode_to_vec()), "0a016a1007");

    let feature = v1::Feature {
        name: "journal".into(),
        revision: 1,
    };
    assert_eq!(hex(&feature.encode_to_vec()), "0a076a6f75726e616c1001");

    let reject = v1::Reject {
        code: "c".into(),
        diagnostic: "d".into(),
        supported_min_epoch: 1,
        supported_max_epoch: 2,
    };
    assert_eq!(hex(&reject.encode_to_vec()), "0a016312016418012002");

    let hello_frame = v1::SpokeFrame {
        body: Some(v1::spoke_frame::Body::Hello(v1::Hello {
            host_id: "h".into(),
            min_epoch: 1,
            max_epoch: 1,
            ..Default::default()
        })),
    };
    assert_eq!(hex(&hello_frame.encode_to_vec()), "0a070a016820012801");

    // Defaults are omitted from the wire, which is what lets a new optional
    // field be added without changing any existing message's bytes.
    assert!(v1::Hello::default().encode_to_vec().is_empty());
    assert!(v1::Welcome::default().encode_to_vec().is_empty());
    assert_eq!(Limits::default().max_frame_bytes, MAX_FRAME_BYTES);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
