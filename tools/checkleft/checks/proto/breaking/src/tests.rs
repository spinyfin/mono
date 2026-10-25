//! Fixture-pair tests: every rule has an `old.proto`/`new.proto` pair in
//! [`crate::fixtures`] that must produce the expected violation, and the
//! `compatible-*` pairs must produce none. The check entry point is covered
//! through `CheckInput` for the base-availability cases.

use checkleft_check_sdk::{BaseFile, ChangeKind, ChangeSet, ChangedFile, CheckInput, Finding, Severity};

use crate::compare::Violation;
use crate::{check_file, compare, proto_breaking_check, schema};

use crate::fixtures::{BREAKING, COMPATIBLE, UNPARSEABLE, pair};

/// `(old.proto, new.proto)` for `name`.
fn fixture(name: &str) -> (String, String) {
    pair(name)
}

fn violations(name: &str) -> Vec<Violation> {
    let (old, new) = fixture(name);
    let old = schema::parse("old.proto", &old).expect("old fixture parses");
    let new = schema::parse("new.proto", &new).expect("new fixture parses");
    compare(&old, &new)
}

/// The 1-based line of the first line in `new.proto` containing `needle`.
fn line_of(name: &str, needle: &str) -> u32 {
    let (_, new) = fixture(name);
    let index = new
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` not found in {name}/new.proto"));
    u32::try_from(index + 1).unwrap()
}

/// Assert `name` yields exactly one violation, whose message contains every
/// `fragment` and which sits on the first line of `new.proto` containing
/// `anchor`.
fn assert_single(name: &str, anchor: &str, fragments: &[&str]) {
    let found = violations(name);
    assert_eq!(found.len(), 1, "{name}: expected one violation, got {found:#?}");
    assert_violation(name, &found[0], anchor, fragments);
}

fn assert_violation(name: &str, violation: &Violation, anchor: &str, fragments: &[&str]) {
    for fragment in fragments {
        assert!(
            violation.message.contains(fragment),
            "{name}: `{fragment}` missing from {:?}",
            violation.message
        );
    }
    assert_eq!(
        violation.line,
        line_of(name, anchor),
        "{name}: wrong line for {:?}",
        violation.message
    );
    assert!(!violation.remediation.is_empty(), "{name}: remediation is empty");
}

// ── compatible changes pass ──────────────────────────────────────────────────

#[test]
fn compatible_fixtures_produce_no_violations() {
    for name in COMPATIBLE {
        let found = violations(name);
        assert!(found.is_empty(), "{name}: expected no violations, got {found:#?}");
    }
}

#[test]
fn every_breaking_fixture_produces_at_least_one_violation() {
    for name in BREAKING {
        assert!(!violations(name).is_empty(), "{name}: expected a violation");
    }
}

// ── fields ───────────────────────────────────────────────────────────────────

#[test]
fn field_type_change() {
    assert_single(
        "field-type-change",
        "string sequence = 2;",
        &["field `sequence` (2)", "changed type from `int64` to `string`"],
    );
}

#[test]
fn field_renamed() {
    assert_single(
        "field-renamed",
        "bytes body_bytes = 3;",
        &["field 3 of message `Record`", "renamed from `payload` to `body_bytes`"],
    );
}

#[test]
fn field_removed_without_any_reservation() {
    assert_single(
        "field-removed-unreserved",
        "message Record {",
        &[
            "field `payload` (3)",
            "removed without reserving its number and its name",
        ],
    );
}

#[test]
fn field_removed_with_only_number_reserved() {
    assert_single(
        "field-removed-name-not-reserved",
        "message Record {",
        &["field `payload` (3)", "removed without reserving its name"],
    );
}

#[test]
fn field_removed_with_only_name_reserved() {
    assert_single(
        "field-removed-number-not-reserved",
        "message Record {",
        &["field `payload` (3)", "removed without reserving its number"],
    );
}

#[test]
fn field_number_reuse_is_reported_at_the_new_field_and_the_lost_reservation() {
    let found = violations("field-number-reuse");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "field-number-reuse",
        &found[0],
        "message Record {",
        &["message `Record` no longer reserves number 5"],
    );
    assert_violation(
        "field-number-reuse",
        &found[1],
        "uint32 attempt = 5;",
        &["field `attempt` (5)", "reuses reserved number 5"],
    );
}

#[test]
fn field_name_reuse() {
    let found = violations("field-name-reuse");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "field-name-reuse",
        &found[0],
        "message Record {",
        &["message `Record` no longer reserves name `legacy`"],
    );
    assert_violation(
        "field-name-reuse",
        &found[1],
        "string legacy = 7;",
        &["field `legacy` (7)", "reuses reserved name `legacy`"],
    );
}

#[test]
fn field_cardinality_change() {
    assert_single(
        "field-cardinality-change",
        "repeated int64 sequence = 2;",
        &[
            "field `sequence` (2)",
            "changed from singular (implicit presence) to repeated",
        ],
    );
}

#[test]
fn field_presence_change() {
    assert_single(
        "field-presence-change",
        "optional int64 sequence = 2;",
        &[
            "field `sequence` (2)",
            "changed from singular (implicit presence) to optional",
        ],
    );
}

#[test]
fn field_moved_into_oneof() {
    assert_single(
        "field-oneof-move",
        "    bytes payload = 3;",
        &["field `payload` (3)", "moved from outside any oneof into oneof `body`"],
    );
}

#[test]
fn field_json_name_change() {
    assert_single(
        "field-json-name-change",
        "json_name = \"seq\"",
        &["field `sequence` (2)", "changed json_name from `(default)` to `seq`"],
    );
}

// ── enums ────────────────────────────────────────────────────────────────────

#[test]
fn enum_value_renumbering_is_reported_as_renames_on_both_values() {
    let found = violations("enum-value-renumbered");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "enum-value-renumbered",
        &found[0],
        "KIND_RECORD = 2;",
        &["enum value 2 of `Kind`", "renamed from `KIND_COMMAND` to `KIND_RECORD`"],
    );
    assert_violation(
        "enum-value-renumbered",
        &found[1],
        "KIND_COMMAND = 1;",
        &["enum value 1 of `Kind`", "renamed from `KIND_RECORD` to `KIND_COMMAND`"],
    );
}

#[test]
fn enum_value_renamed() {
    assert_single(
        "enum-value-renamed",
        "KIND_INSTRUCTION = 2;",
        &[
            "enum value 2 of `Kind`",
            "renamed from `KIND_COMMAND` to `KIND_INSTRUCTION`",
        ],
    );
}

#[test]
fn enum_value_removed_without_reservation() {
    assert_single(
        "enum-value-removed-unreserved",
        "enum Kind {",
        &[
            "enum value `KIND_COMMAND` (2)",
            "removed without reserving its number and its name",
        ],
    );
}

#[test]
fn enum_value_removed_with_only_number_reserved() {
    assert_single(
        "enum-value-removed-name-not-reserved",
        "enum Kind {",
        &["enum value `KIND_COMMAND` (2)", "removed without reserving its name"],
    );
}

#[test]
fn enum_value_reuses_reserved_number() {
    let found = violations("enum-value-reuses-reserved-number");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "enum-value-reuses-reserved-number",
        &found[0],
        "enum Kind {",
        &["enum `Kind` no longer reserves number 3"],
    );
    assert_violation(
        "enum-value-reuses-reserved-number",
        &found[1],
        "KIND_RETRY = 3;",
        &["enum value `KIND_RETRY` (3)", "reuses reserved number 3"],
    );
}

#[test]
fn enum_value_reuses_reserved_name() {
    let found = violations("enum-value-reuses-reserved-name");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "enum-value-reuses-reserved-name",
        &found[0],
        "enum Kind {",
        &["enum `Kind` no longer reserves name `KIND_RETIRED`"],
    );
    assert_violation(
        "enum-value-reuses-reserved-name",
        &found[1],
        "KIND_RETIRED = 4;",
        &["enum value `KIND_RETIRED` (4)", "reuses reserved name `KIND_RETIRED`"],
    );
}

// ── messages, enums, services ────────────────────────────────────────────────

#[test]
fn message_removed_is_reported_once_at_the_package_line() {
    // The fixture also drops the oneof member and the RPC response that used
    // the message; those are violations of their own.
    let found = violations("message-removed");
    assert_eq!(found.len(), 3, "{found:#?}");
    assert_violation(
        "message-removed",
        &found[0],
        "package fixture.v1;",
        &["message `Goodbye` was removed or renamed"],
    );
    assert_violation(
        "message-removed",
        &found[1],
        "message Record {",
        &["field `goodbye` (11)", "removed without reserving"],
    );
    assert_violation(
        "message-removed",
        &found[2],
        "rpc Ping(Hello) returns (Hello);",
        &["rpc `Ping`", "changed its response type from `Goodbye` to `Hello`"],
    );
}

#[test]
fn message_renamed_is_reported_as_removed() {
    assert_single(
        "message-renamed",
        "package fixture.v1;",
        &["message `Goodbye` was removed or renamed"],
    );
}

#[test]
fn nested_message_removed_is_reported_at_its_parent() {
    let found = violations("nested-message-removed");
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_violation(
        "nested-message-removed",
        &found[0],
        "message Outer {",
        &["message `Outer.Inner` was removed or renamed"],
    );
    assert_violation(
        "nested-message-removed",
        &found[1],
        "string inner = 1;",
        &[
            "field `inner` (1) of message `Outer`",
            "changed type from `Inner` to `string`",
        ],
    );
}

#[test]
fn nested_field_type_change() {
    assert_single(
        "nested-field-type-change",
        "bytes id = 1;",
        &[
            "field `id` (1) of message `Outer.Inner`",
            "changed type from `string` to `bytes`",
        ],
    );
}

#[test]
fn map_value_type_change_is_a_field_type_change_on_the_entry() {
    let found = violations("map-value-type-change");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0].message.contains("`Env.VarsEntry`") && found[0].message.contains("from `string` to `bytes`"),
        "{found:#?}"
    );
}

#[test]
fn enum_removed() {
    assert_single(
        "enum-removed",
        "package fixture.v1;",
        &["enum `Kind` was removed or renamed"],
    );
}

#[test]
fn service_removed() {
    assert_single(
        "service-removed",
        "package fixture.v1;",
        &["service `Hub` was removed or renamed"],
    );
}

#[test]
fn rpc_removed() {
    assert_single(
        "rpc-removed",
        "service Hub {",
        &["rpc `Ping` of service `Hub` was removed or renamed"],
    );
}

#[test]
fn rpc_renamed_is_reported_as_removed() {
    assert_single(
        "rpc-renamed",
        "service Hub {",
        &["rpc `Ping` of service `Hub` was removed or renamed"],
    );
}

#[test]
fn rpc_request_type_change() {
    assert_single(
        "rpc-request-type-change",
        "rpc Ping(Record)",
        &["rpc `Ping`", "changed its request type from `Hello` to `Record`"],
    );
}

#[test]
fn rpc_response_type_change() {
    assert_single(
        "rpc-response-type-change",
        "rpc Ping(Hello) returns (Hello);",
        &["rpc `Ping`", "changed its response type from `Goodbye` to `Hello`"],
    );
}

#[test]
fn rpc_streaming_change() {
    assert_single(
        "rpc-streaming-change",
        "rpc Connect(Record)",
        &[
            "rpc `Connect`",
            "changed streaming from bidirectional streaming to server streaming",
        ],
    );
}

// ── file level ───────────────────────────────────────────────────────────────

#[test]
fn package_change() {
    assert_single(
        "package-change",
        "package fixture.v2;",
        &["package changed from `fixture.v1` to `fixture.v2`"],
    );
}

#[test]
fn syntax_change() {
    // proto2 `optional` is explicit presence where proto3 was implicit, so the
    // syntax change also surfaces as a cardinality change on each singular
    // field; the syntax violation itself is the one on the syntax line.
    let found = violations("syntax-change");
    let syntax_line = line_of("syntax-change", "syntax = \"proto2\";");
    let on_syntax: Vec<_> = found.iter().filter(|v| v.line == syntax_line).collect();
    assert_eq!(on_syntax.len(), 1, "{found:#?}");
    assert!(
        on_syntax[0]
            .message
            .contains("syntax changed from `proto3` to `proto2`")
    );
}

#[test]
fn reserved_number_dropped() {
    assert_single(
        "reserved-number-dropped",
        "message Record {",
        &["message `Record` no longer reserves number 5"],
    );
}

#[test]
fn reserved_name_dropped() {
    assert_single(
        "reserved-name-dropped",
        "enum Kind {",
        &["enum `Kind` no longer reserves name `KIND_RETIRED`"],
    );
}

// ── parse failures and the check entry point ─────────────────────────────────

#[test]
fn unparseable_fixtures_do_not_parse_while_their_bases_do() {
    for name in UNPARSEABLE {
        let (old, new) = fixture(name);
        assert!(schema::parse("old.proto", &old).is_ok(), "{name}: old must parse");
        assert!(schema::parse("new.proto", &new).is_err(), "{name}: new must not parse");
    }
}

fn changed(path: &str, kind: ChangeKind, old_path: Option<&str>) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        kind,
        old_path: old_path.map(str::to_owned),
    }
}

fn input(files: Vec<ChangedFile>, base_files: Vec<(&str, &str)>, whole_repo: bool) -> CheckInput {
    CheckInput::__from_parts(
        ChangeSet {
            changed_files: files,
            file_diffs: vec![],
            commit_description: None,
            pr_description: None,
            pr_description_unavailable_reason: None,
            change_id: None,
            repository: None,
            base_files: base_files
                .into_iter()
                .map(|(path, content)| BaseFile {
                    path: path.to_owned(),
                    content: content.to_owned(),
                })
                .collect(),
            whole_repo,
        },
        "{}".to_owned(),
    )
}

#[test]
fn unparseable_new_file_is_one_error_finding_at_the_error_line() {
    let (old, new) = fixture("new-file-unparseable");
    let file = changed("api/x.proto", ChangeKind::Modified, None);
    let findings = check_file(&file, &old, &new);
    assert_eq!(findings.len(), 1, "{findings:#?}");
    let finding = &findings[0];
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("`api/x.proto` does not parse"), "{finding:#?}");
    let location = finding.location.as_ref().expect("located");
    assert_eq!(location.path, "api/x.proto");
    // `message Hello` without a brace: the parser trips on the next token, the
    // field declaration on the following line.
    assert_eq!(
        location.line,
        Some(line_of("new-file-unparseable", "string host_id = 1;"))
    );
}

#[test]
fn unparseable_base_is_one_error_finding_on_line_one() {
    let (_, broken) = fixture("new-file-unparseable");
    let (_, fine) = fixture("compatible-identical");
    let file = changed("api/x.proto", ChangeKind::Modified, None);
    let findings = check_file(&file, &broken, &fine);
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(
        findings[0]
            .message
            .contains("base revision of `api/x.proto` does not parse")
    );
    assert_eq!(findings[0].location.as_ref().and_then(|l| l.line), Some(1));
}

#[test]
fn check_file_findings_carry_the_new_path_and_a_remediation() {
    let (old, new) = fixture("field-type-change");
    let file = changed("api/x.proto", ChangeKind::Modified, None);
    let findings = check_file(&file, &old, &new);
    assert_eq!(findings.len(), 1);
    let finding: &Finding = &findings[0];
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.starts_with("backward-incompatible schema change: "));
    assert_eq!(finding.location.as_ref().map(|l| l.path.as_str()), Some("api/x.proto"));
    assert_eq!(finding.remediations.len(), 1);
}

#[test]
fn whole_repo_mode_is_a_no_op() {
    let findings = proto_breaking_check(input(
        vec![changed("api/x.proto", ChangeKind::Modified, None)],
        vec![],
        true,
    ));
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn added_files_pass_without_a_base() {
    let findings = proto_breaking_check(input(
        vec![changed("api/new.proto", ChangeKind::Added, None)],
        vec![],
        false,
    ));
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn modified_file_without_base_content_is_an_error_not_a_pass() {
    let findings = proto_breaking_check(input(
        vec![changed("api/x.proto", ChangeKind::Modified, None)],
        vec![],
        false,
    ));
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert_eq!(findings[0].severity, Severity::Error);
    assert!(
        findings[0]
            .message
            .contains("base-revision content of `api/x.proto` is unavailable"),
        "{findings:#?}"
    );
    assert_eq!(findings[0].location.as_ref().and_then(|l| l.line), Some(1));
}

#[test]
fn renamed_file_without_base_content_names_its_old_path() {
    let findings = proto_breaking_check(input(
        vec![changed("api/v1/x.proto", ChangeKind::Renamed, Some("api/x.proto"))],
        vec![],
        false,
    ));
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(
        findings[0]
            .message
            .contains("base-revision content of `api/x.proto` is unavailable"),
        "{findings:#?}"
    );
    assert_eq!(
        findings[0].location.as_ref().map(|l| l.path.as_str()),
        Some("api/v1/x.proto")
    );
}

#[test]
fn renamed_file_is_compared_with_the_content_of_its_old_path() {
    let (old, new) = fixture("field-type-change");
    let file = changed("api/v1/x.proto", ChangeKind::Renamed, Some("api/x.proto"));
    let changeset = input(vec![file.clone()], vec![("api/x.proto", &old)], false).changeset;
    let base = changeset
        .base_content_for(&file)
        .expect("base found under the old path");
    let findings = check_file(&file, base, &new);
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(
        findings[0].message.contains("changed type from `int64` to `string`"),
        "{findings:#?}"
    );
    assert_eq!(
        findings[0].location.as_ref().map(|l| l.path.as_str()),
        Some("api/v1/x.proto")
    );
}
