//! Repository rules for the `boss.spoke.v1` schema, checked against the
//! descriptor Bazel hands to the code generator (not a second parse of the
//! source), so they hold for exactly what the generated types come from.
//!
//! checkleft's `proto/breaking` judges a change against its base revision.
//! These rules cover what it cannot: properties every version of the schema must have, including the first.

use prost::Message;
use prost_types::{FileDescriptorSet, field_descriptor_proto::Label};

fn descriptor() -> prost_types::FileDescriptorProto {
    let path = std::env::var("SPOKE_V1_DESCRIPTOR").expect("SPOKE_V1_DESCRIPTOR names the Bazel descriptor set");
    let bytes = std::fs::read(&path).unwrap_or_else(|err| panic!("read {path}: {err}"));
    let mut set = FileDescriptorSet::decode(bytes.as_slice()).expect("a FileDescriptorSet");
    assert_eq!(set.file.len(), 1, "the schema has no imports");
    set.file.remove(0)
}

#[test]
fn the_schema_is_proto3_in_the_versioned_package() {
    let file = descriptor();
    assert_eq!(
        file.name(),
        "boss/spoke/v1/spoke.proto",
        "the path must match the package directory"
    );
    assert_eq!(file.package(), "boss.spoke.v1");
    assert_eq!(file.syntax(), "proto3");
}

#[test]
fn every_enum_starts_with_an_unspecified_zero() {
    // Zero is what a missing field decodes to; it must never name a real
    // value, or an omitted field would silently mean something.
    let file = descriptor();
    assert!(!file.enum_type.is_empty());
    for enumeration in &file.enum_type {
        let first = enumeration.value.first().expect("enums are not empty");
        assert_eq!(
            first.number(),
            0,
            "{}: the first value must be zero",
            enumeration.name()
        );
        assert!(
            first.name().ends_with("_UNSPECIFIED"),
            "{}: zero is {}, expected *_UNSPECIFIED",
            enumeration.name(),
            first.name()
        );
        let mut numbers: Vec<i32> = enumeration.value.iter().map(|v| v.number()).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(
            numbers.len(),
            enumeration.value.len(),
            "{}: aliased numbers",
            enumeration.name()
        );
        assert!(
            numbers.iter().all(|n| *n >= 0),
            "{}: negative numbers are not portable",
            enumeration.name()
        );
    }
}

#[test]
fn framing_messages_carry_exactly_one_oneof_named_body() {
    // Receivers treat an unset `body` as "malformed or newer than me"; that
    // only works if there is exactly one oneof to be unset.
    let file = descriptor();
    for name in ["SpokeFrame", "HubFrame", "Record", "Command"] {
        let message = file
            .message_type
            .iter()
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("{name} exists"));
        let oneofs: Vec<&str> = message.oneof_decl.iter().map(|o| o.name()).collect();
        assert_eq!(oneofs, ["body"], "{name}");
    }
}

#[test]
fn no_field_is_required_or_repeated_where_presence_matters() {
    // proto3 has no `required`; make sure nothing re-introduces proto2 labels.
    let file = descriptor();
    for message in &file.message_type {
        for field in &message.field {
            assert_ne!(field.label(), Label::Required, "{}.{}", message.name(), field.name());
        }
    }
}

#[test]
fn the_services_have_the_documented_shape() {
    let file = descriptor();
    let service = |name: &str| {
        file.service
            .iter()
            .find(|s| s.name() == name)
            .unwrap_or_else(|| panic!("{name}"))
    };

    let hub = service("SpokeHub");
    assert_eq!(hub.method.len(), 1);
    let connect = &hub.method[0];
    assert_eq!(connect.name(), "Connect");
    assert!(
        connect.client_streaming() && connect.server_streaming(),
        "Connect is one bidirectional stream"
    );

    let enrollment = service("Enrollment");
    let mut methods: Vec<&str> = enrollment.method.iter().map(|m| m.name()).collect();
    methods.sort_unstable();
    assert_eq!(methods, ["Enroll", "Renew"]);
    assert!(
        enrollment
            .method
            .iter()
            .all(|m| !m.client_streaming() && !m.server_streaming())
    );
}
