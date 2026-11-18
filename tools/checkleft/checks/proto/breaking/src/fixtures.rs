//! Old/new `.proto` pairs for the tests, one per rule plus the compatible
//! changes that must pass.
//!
//! Every pair is a textual edit of [`BASE`], so each case states exactly which
//! change it makes. Kept as Rust strings rather than files so the pairs do not
//! inflate the file count of a change that touches them.

/// The schema every case starts from.
pub const BASE: &str = r#"syntax = "proto3";

package fixture.v1;

message Record {
  string id = 1;
  int64 sequence = 2;
  bytes payload = 3;
  repeated string tags = 4;
  oneof body {
    Hello hello = 10;
    Goodbye goodbye = 11;
  }
  reserved 5, 6;
  reserved "legacy";
}

message Hello {
  string host_id = 1;
}

message Goodbye {
  string reason = 1;
}

enum Kind {
  KIND_UNSPECIFIED = 0;
  KIND_RECORD = 1;
  KIND_COMMAND = 2;
  reserved 3;
  reserved "KIND_RETIRED";
}

service Hub {
  rpc Connect(stream Record) returns (stream Record);
  rpc Ping(Hello) returns (Goodbye);
}
"#;

const HEADER: &str = "syntax = \"proto3\";\n\npackage fixture.v1;\n\n";
const NESTED_OLD: &str = "message Outer {\n  message Inner {\n    string id = 1;\n  }\n  Inner inner = 1;\n}\n";

/// Cases whose `new.proto` breaks compatibility with its `old.proto`.
pub const BREAKING: &[&str] = &[
    "field-type-change",
    "field-renamed",
    "field-removed-unreserved",
    "field-removed-name-not-reserved",
    "field-removed-number-not-reserved",
    "field-number-reuse",
    "field-name-reuse",
    "field-cardinality-change",
    "field-presence-change",
    "field-oneof-move",
    "field-json-name-change",
    "enum-value-renumbered",
    "enum-value-renamed",
    "enum-value-removed-unreserved",
    "enum-value-removed-name-not-reserved",
    "enum-value-reuses-reserved-number",
    "enum-value-reuses-reserved-name",
    "message-removed",
    "message-renamed",
    "nested-message-removed",
    "nested-field-type-change",
    "map-value-type-change",
    "enum-removed",
    "service-removed",
    "rpc-removed",
    "rpc-renamed",
    "rpc-request-type-change",
    "rpc-response-type-change",
    "rpc-streaming-change",
    "package-change",
    "syntax-change",
    "reserved-number-dropped",
    "reserved-name-dropped",
];

/// Cases whose `new.proto` is a compatible evolution of its `old.proto`.
pub const COMPATIBLE: &[&str] = &[
    "compatible-identical",
    "compatible-add-field",
    "compatible-add-message-enum-value-rpc",
    "compatible-remove-field-with-reservation",
    "compatible-remove-enum-value-with-reservation",
    "compatible-comments-reorder-options",
    "compatible-new-oneof-with-new-fields",
    "compatible-widen-reservations",
];

/// Cases whose `new.proto` does not parse.
pub const UNPARSEABLE: &[&str] = &["new-file-unparseable"];

/// `(old.proto, new.proto)` for `case`.
pub fn pair(case: &str) -> (String, String) {
    match case {
        "nested-message-removed" => (
            format!("{HEADER}{NESTED_OLD}"),
            format!("{HEADER}message Outer {{\n  string inner = 1;\n}}\n"),
        ),
        "nested-field-type-change" => (
            format!("{HEADER}{NESTED_OLD}"),
            format!("{HEADER}{}", r(NESTED_OLD, "string id = 1;", "bytes id = 1;")),
        ),
        "map-value-type-change" => (
            format!("{HEADER}message Env {{\n  map<string, string> vars = 1;\n}}\n"),
            format!("{HEADER}message Env {{\n  map<string, bytes> vars = 1;\n}}\n"),
        ),
        _ => (BASE.to_owned(), new_from_base(case)),
    }
}

/// Replace `from` (which must occur) with `to` in `src`.
fn r(src: &str, from: &str, to: &str) -> String {
    assert!(src.contains(from), "fixture edit target `{from}` not found");
    src.replacen(from, to, 1)
}

fn new_from_base(case: &str) -> String {
    match case {
        // ── fields ──────────────────────────────────────────────────────────
        "field-type-change" => r(BASE, "int64 sequence = 2;", "string sequence = 2;"),
        "field-renamed" => r(BASE, "bytes payload = 3;", "bytes body_bytes = 3;"),
        "field-removed-unreserved" => r(BASE, "  bytes payload = 3;\n", ""),
        "field-removed-name-not-reserved" => r(
            &r(BASE, "  bytes payload = 3;\n", ""),
            "reserved 5, 6;",
            "reserved 3, 5, 6;",
        ),
        "field-removed-number-not-reserved" => r(
            &r(BASE, "  bytes payload = 3;\n", ""),
            "reserved \"legacy\";",
            "reserved \"legacy\", \"payload\";",
        ),
        "field-number-reuse" => r(BASE, "  reserved 5, 6;\n", "  reserved 6;\n  uint32 attempt = 5;\n"),
        "field-name-reuse" => r(BASE, "  reserved \"legacy\";\n", "  string legacy = 7;\n"),
        "field-cardinality-change" => r(BASE, "int64 sequence = 2;", "repeated int64 sequence = 2;"),
        "field-presence-change" => r(BASE, "int64 sequence = 2;", "optional int64 sequence = 2;"),
        "field-oneof-move" => r(
            &r(BASE, "  bytes payload = 3;\n", ""),
            "    Goodbye goodbye = 11;\n",
            "    Goodbye goodbye = 11;\n    bytes payload = 3;\n",
        ),
        "field-json-name-change" => r(BASE, "int64 sequence = 2;", "int64 sequence = 2 [json_name = \"seq\"];"),
        // ── enums ───────────────────────────────────────────────────────────
        "enum-value-renumbered" => r(
            BASE,
            "  KIND_RECORD = 1;\n  KIND_COMMAND = 2;\n",
            "  KIND_RECORD = 2;\n  KIND_COMMAND = 1;\n",
        ),
        "enum-value-renamed" => r(BASE, "KIND_COMMAND = 2;", "KIND_INSTRUCTION = 2;"),
        "enum-value-removed-unreserved" => r(BASE, "  KIND_COMMAND = 2;\n", ""),
        "enum-value-removed-name-not-reserved" => {
            r(&r(BASE, "  KIND_COMMAND = 2;\n", ""), "reserved 3;", "reserved 2, 3;")
        }
        "enum-value-reuses-reserved-number" => r(BASE, "  reserved 3;\n", "  KIND_RETRY = 3;\n"),
        "enum-value-reuses-reserved-name" => r(BASE, "  reserved \"KIND_RETIRED\";\n", "  KIND_RETIRED = 4;\n"),
        // ── messages, enums, services ───────────────────────────────────────
        "message-removed" => r(
            &r(
                &r(BASE, "message Goodbye {\n  string reason = 1;\n}\n\n", ""),
                "    Goodbye goodbye = 11;\n",
                "",
            ),
            "returns (Goodbye)",
            "returns (Hello)",
        ),
        "message-renamed" => BASE.replace("Goodbye", "Farewell"),
        "enum-removed" => r(
            BASE,
            "enum Kind {\n  KIND_UNSPECIFIED = 0;\n  KIND_RECORD = 1;\n  KIND_COMMAND = 2;\n  reserved 3;\n  reserved \"KIND_RETIRED\";\n}\n\n",
            "",
        ),
        "service-removed" => r(
            BASE,
            "service Hub {\n  rpc Connect(stream Record) returns (stream Record);\n  rpc Ping(Hello) returns (Goodbye);\n}\n",
            "",
        ),
        "rpc-removed" => r(BASE, "  rpc Ping(Hello) returns (Goodbye);\n", ""),
        "rpc-renamed" => r(BASE, "rpc Ping(Hello)", "rpc Probe(Hello)"),
        "rpc-request-type-change" => r(
            BASE,
            "rpc Ping(Hello) returns (Goodbye);",
            "rpc Ping(Record) returns (Goodbye);",
        ),
        "rpc-response-type-change" => r(
            BASE,
            "rpc Ping(Hello) returns (Goodbye);",
            "rpc Ping(Hello) returns (Hello);",
        ),
        "rpc-streaming-change" => r(
            BASE,
            "rpc Connect(stream Record) returns (stream Record);",
            "rpc Connect(Record) returns (stream Record);",
        ),
        // ── file level ──────────────────────────────────────────────────────
        "package-change" => r(BASE, "package fixture.v1;", "package fixture.v2;"),
        "syntax-change" => r(BASE, "syntax = \"proto3\";", "syntax = \"proto2\";")
            .replace("string id = 1;", "optional string id = 1;")
            .replace("int64 sequence = 2;", "optional int64 sequence = 2;")
            .replace("bytes payload = 3;", "optional bytes payload = 3;")
            .replace("string host_id = 1;", "optional string host_id = 1;")
            .replace("string reason = 1;", "optional string reason = 1;"),
        "reserved-number-dropped" => r(BASE, "  reserved 5, 6;\n", "  reserved 6;\n"),
        "reserved-name-dropped" => r(BASE, "  reserved \"KIND_RETIRED\";\n", ""),
        "new-file-unparseable" => r(BASE, "message Hello {", "message Hello"),
        // ── compatible ──────────────────────────────────────────────────────
        "compatible-identical" => BASE.to_owned(),
        "compatible-add-field" => r(
            BASE,
            "  repeated string tags = 4;\n",
            "  repeated string tags = 4;\n  // Set by newer peers; absent means unknown.\n  uint32 attempt = 7;\n  optional string note = 8;\n",
        ),
        "compatible-add-message-enum-value-rpc" => r(
            &r(
                &r(
                    BASE,
                    "  rpc Ping(Hello) returns (Goodbye);\n",
                    "  rpc Ping(Hello) returns (Goodbye);\n  rpc Status(Hello) returns (StatusReply);\n",
                ),
                "message Goodbye {",
                "message StatusReply {\n  Kind kind = 1;\n}\n\nmessage Goodbye {",
            ),
            "  KIND_COMMAND = 2;\n",
            "  KIND_COMMAND = 2;\n  KIND_STATUS = 4;\n",
        ),
        "compatible-remove-field-with-reservation" => r(
            &r(
                &r(BASE, "  bytes payload = 3;\n", ""),
                "reserved 5, 6;",
                "reserved 3, 5, 6;",
            ),
            "reserved \"legacy\";",
            "reserved \"legacy\", \"payload\";",
        ),
        "compatible-remove-enum-value-with-reservation" => r(
            &r(&r(BASE, "  KIND_COMMAND = 2;\n", ""), "reserved 3;", "reserved 2 to 3;"),
            "reserved \"KIND_RETIRED\";",
            "reserved \"KIND_RETIRED\", \"KIND_COMMAND\";",
        ),
        "compatible-comments-reorder-options" => r(
            &r(
                &r(
                    BASE,
                    "message Hello {\n  string host_id = 1;\n}\n\nmessage Goodbye {\n  string reason = 1;\n}\n",
                    "// Sent once per connection.\nmessage Goodbye {\n  string reason = 1 [deprecated = true];\n}\n\nmessage Hello {\n  // The enrolled host id.\n  string host_id = 1;\n}\n",
                ),
                "  string id = 1;\n  int64 sequence = 2;\n",
                "  int64 sequence = 2;\n  string id = 1;\n",
            ),
            "  reserved 5, 6;\n",
            "  reserved 5 to 6;\n",
        ),
        "compatible-new-oneof-with-new-fields" => r(
            BASE,
            "  repeated string tags = 4;\n",
            "  repeated string tags = 4;\n  oneof origin {\n    string host = 20;\n    string pane = 21;\n  }\n",
        ),
        "compatible-widen-reservations" => r(
            &r(BASE, "  reserved 5, 6;\n", "  reserved 5 to 9, 1000 to max;\n"),
            "  reserved \"legacy\";\n",
            "  reserved \"legacy\", \"ancient\";\n",
        ),
        other => panic!("unknown fixture case `{other}`"),
    }
}
