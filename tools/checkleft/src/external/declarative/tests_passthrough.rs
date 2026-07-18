//! Passthrough transform tests — the folded `exec` tier. A custom binary emits a
//! checkleft findings document on stdout and the declarative runtime passes it
//! through unchanged: the manifest accepts the transform, rejects a stray
//! `select`/`finding`, returns findings directly, surfaces invalid JSON, and
//! runs a real binary end-to-end.

use crate::external::{ExternalCheckPackageImplementation, parse_external_check_package_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Severity;

/// A minimal TOML declarative manifest with a single passthrough invocation. The
/// `tool` binding is filled in by `.replace("TOOL_PATH", …)`. `applies_to = ["**"]`
/// mirrors exactly what the `local_check` bazel rule generates for a folded
/// `exec` binary, so this fixture also guards that codegen's glob choice.
const PASSTHROUGH_MANIFEST: &str = r#"
id = "passthrough-check"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**"]

[needs.tool.default]
path = "TOOL_PATH"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "findings", default = "error" }

[invocations.transform]
kind = "passthrough"
"#;

#[test]
fn manifest_accepts_passthrough_transform() {
    let package = parse_external_check_package_manifest(&PASSTHROUGH_MANIFEST.replace("TOOL_PATH", "emit_findings"))
        .expect("passthrough manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => {
            assert_eq!(declarative.invocations.len(), 1);
            assert_eq!(
                declarative.invocations[0].transform,
                super::transform::Transform::Passthrough
            );
        }
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

#[test]
fn passthrough_transform_rejects_select_and_finding() {
    let manifest = PASSTHROUGH_MANIFEST
        .replace("TOOL_PATH", "emit_findings")
        .replace("kind = \"passthrough\"", "kind = \"passthrough\"\nselect = \".x\"");
    let err = parse_external_check_package_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("must not set `select`"),
        "unexpected: {err:#}"
    );
}

#[test]
fn passthrough_transform_returns_findings_directly() {
    let stdout = br#"{"findings":[
        {"severity":"warning","message":"hello","location":null,"remediations":["fix it"],"suggested_fix":null}
    ]}"#;
    let findings = super::transform::Transform::Passthrough
        .apply(stdout, Some(0), None, None)
        .expect("passthrough parses findings");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert_eq!(findings[0].message, "hello");
    assert_eq!(findings[0].remediations, vec!["fix it".to_owned()]);
}

#[test]
fn passthrough_transform_surfaces_invalid_json() {
    let err = super::transform::Transform::Passthrough
        .apply(b"not json", Some(0), None, None)
        .expect_err("invalid findings JSON must error");
    assert!(
        format!("{err:#}").contains("checkleft findings document"),
        "unexpected: {err:#}"
    );
}

/// End-to-end fold of the old `exec` case: a custom binary emits a checkleft
/// findings document on stdout, and the declarative runtime runs it + passes its
/// output through unchanged. Also exercises the relative→null stdin contract
/// (`Command::output` closes stdin, so a `cat`-ing binary sees EOF).
#[test]
#[cfg(unix)]
fn passthrough_runs_binary_end_to_end() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("emit_findings.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"passthrough-ran\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut permissions = std::fs::metadata(&script_path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("chmod");

    // Run via /bin/sh to avoid ETXTBSY (Text file busy) on Linux: directly
    // exec-ing a script that was just written to disk can fail when other test
    // threads hold a write fd open at the same time. Passing the script as an
    // argument to the interpreter avoids the exec-on-open-write race entirely.
    let package = parse_external_check_package_manifest(&format!(
        r#"id = "passthrough-check"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**"]

[needs.tool.default]
path = "/bin/sh"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{script}", "{{{{files}}}}"]
exit = {{ "0" = "findings", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#,
        script = script_path.display()
    ))
    .expect("passthrough manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: "docs/file.md".into(),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let result = super::run_declarative_check(
        temp.path(),
        "passthrough-check",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("declarative passthrough runs");

    assert_eq!(result.check_id, "passthrough-check");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Warning);
    assert_eq!(result.findings[0].message, "passthrough-ran");
}
