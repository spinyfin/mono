use crate::{compare, schema};

fn compare_sources(old: &str, new: &str) -> Vec<crate::Violation> {
    compare(
        &schema::parse("old.proto", old).unwrap(),
        &schema::parse("new.proto", new).unwrap(),
    )
}

#[test]
fn nested_shadowing_changes_field_identity() {
    let old = "syntax = \"proto3\"; package p; message Payload { string text = 1; } message Envelope { Payload payload = 1; }";
    let new = old.replace("Payload payload", "message Payload { int32 text = 1; } Payload payload");
    let found = compare_sources(old, &new);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("from `.p.Payload` to `.p.Envelope.Payload`"));
}

#[test]
fn relative_package_and_absolute_references_have_the_same_identity() {
    let old = "syntax = \"proto3\"; package p; message Payload {} message Envelope { Payload payload = 1; } service Hub { rpc Call(Payload) returns (Payload); }";
    for name in ["p.Payload", ".p.Payload"] {
        let new = old
            .replace("Payload payload", &format!("{name} payload"))
            .replace("(Payload)", &format!("({name})"));
        assert!(compare_sources(old, &new).is_empty());
    }
}

#[test]
fn unresolved_types_fail_explicitly() {
    for source in [
        "syntax = \"proto3\"; message Envelope { Missing payload = 1; }",
        "syntax = \"proto3\"; service Hub { rpc Call(Missing) returns (Missing); }",
        "syntax = \"proto3\"; import \"other.proto\"; message Envelope { other.Payload payload = 1; }",
        "syntax = \"proto3\"; message Outer { message Payload {} message Inner { message Outer {} Outer.Payload payload = 1; } }",
    ] {
        let error = schema::parse("new.proto", source).unwrap_err();
        assert!(error.message.contains("cannot resolve type"));
        assert_eq!(error.line, Some(1));
    }
}

#[test]
fn reserved_map_removal_is_compatible() {
    let old = "syntax = \"proto3\"; message Env { map<string, string> vars = 1; }";
    let new = "syntax = \"proto3\"; message Env { reserved 1; reserved \"vars\"; }";
    assert!(compare_sources(old, new).is_empty());
    let unreserved = "syntax = \"proto3\"; message Env {}";
    let found = compare_sources(old, unreserved);
    assert_eq!(found.len(), 1);
    assert!(found[0].message.contains("without reserving"));
}

#[test]
fn removed_definitions_anchor_to_moved_new_containers() {
    let old = "syntax = \"proto3\";\npackage p;\nmessage Removed {}\nmessage Outer {\n message Removed {}\n enum RemovedEnum { ZERO = 0; }\n}\nenum RemovedEnum { ZERO = 0; }";
    let new =
        "// header\n// header\n// header\n// header\n// header\nsyntax = \"proto3\";\npackage p;\nmessage Outer {}";
    let found = compare_sources(old, new);
    assert_eq!(found.len(), 4, "{found:?}");
    for finding in found {
        assert_eq!(finding.line, if finding.message.contains("Outer.") { 8 } else { 7 });
    }
}

#[test]
fn resolution_findings_explain_same_file_limit_for_both_revisions() {
    let valid = "syntax = \"proto3\"; message Envelope {}";
    let unresolved = "syntax = \"proto3\";\nimport \"google/protobuf/timestamp.proto\";\nmessage Envelope { google.protobuf.Timestamp time = 1; }";
    let file = checkleft_check_sdk::ChangedFile {
        path: "new.proto".into(),
        old_path: Some("old.proto".into()),
        kind: checkleft_check_sdk::ChangeKind::Renamed,
    };
    for base in [false, true] {
        let findings = if base {
            crate::check_file(&file, unresolved, valid)
        } else {
            crate::check_file(&file, valid, unresolved)
        };
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert!(finding.message.contains("not declared in this file"));
        assert!(finding.message.contains("compares only same-file declarations"));
        assert!(finding.message.contains(if base {
            "base revision of `old.proto`"
        } else {
            "`new.proto`"
        }));
        assert!(!finding.message.contains("does not parse"));
        assert!(finding.remediations[0].contains("Declare the type in the contracted file or keep imported types out"));
        let location = finding.location.as_ref().unwrap();
        assert_eq!(location.path, "new.proto");
        assert_eq!(location.line, Some(if base { 1 } else { 3 }));
    }
}
