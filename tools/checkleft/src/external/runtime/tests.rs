use std::fs;
use std::sync::Arc;

use anyhow::Result;
use tempfile::tempdir;

use crate::external::{
    ExternalCheckArtifactPackage, ExternalCheckCapabilities, ExternalCheckPackage,
    ExternalCheckPackageImplementation, ExternalSourcePackageBuilder, EXTERNAL_CHECK_API_V1,
};
use crate::input::ChangeSet;
use crate::output::Severity;
use crate::source_tree::LocalSourceTree;

use super::{sha256_hex, ExternalCheckExecutor, WasmExternalCheckExecutor};

#[test]
fn executes_artifact_module_and_parses_findings() {
    let temp = tempdir().expect("temp dir");
    let output_json = r#"{"findings":[{"severity":"info","message":"hello","location":null,"remediation":null,"suggested_fix":null}]}"#;
    let output_offset = 1024_u64;
    let output_len = output_json.len() as u64;
    let encoded = (output_offset << 32) | output_len;
    let wat = format!(
        r#"(module
  (memory (export "memory") 1)
  (data (i32.const {offset}) {output:?})
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const {encoded}
  )
)"#,
        offset = output_offset,
        output = output_json,
        encoded = encoded,
    );
    let wasm_bytes = wat::parse_str(&wat).expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read wasm"));

    let executor = WasmExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256,
                provenance: None,
            },
        ),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");

    assert_eq!(result.check_id, "example-check");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Info);
    assert_eq!(result.findings[0].message, "hello");
}

struct StaticSourcePackageBuilder {
    artifact: ExternalCheckArtifactPackage,
}

impl ExternalSourcePackageBuilder for StaticSourcePackageBuilder {
    fn build_source_package(
        &self,
        _package: &ExternalCheckPackage,
        _source: &crate::external::ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        Ok(self.artifact.clone())
    }
}

#[test]
fn source_mode_executes_with_built_artifact() {
    let temp = tempdir().expect("temp dir");
    let output_json = r#"{"findings":[{"severity":"warning","message":"from-source","location":null,"remediation":null,"suggested_fix":null}]}"#;
    let output_offset = 2048_u64;
    let output_len = output_json.len() as u64;
    let encoded = (output_offset << 32) | output_len;
    let wat = format!(
        r#"(module
  (memory (export "memory") 1)
  (data (i32.const {offset}) {output:?})
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const {encoded}
  )
)"#,
        offset = output_offset,
        output = output_json,
        encoded = encoded,
    );
    let wasm_bytes = wat::parse_str(&wat).expect("parse wat");
    fs::write(temp.path().join("built.wasm"), wasm_bytes).expect("write built artifact");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("built.wasm")).expect("read"));
    let source_builder = Arc::new(StaticSourcePackageBuilder {
        artifact: ExternalCheckArtifactPackage {
            artifact_path: "built.wasm".to_owned(),
            artifact_sha256,
            provenance: None,
        },
    });
    let executor =
        WasmExternalCheckExecutor::with_source_package_builder(temp.path(), source_builder)
            .expect("create executor");
    let package = ExternalCheckPackage {
        id: "source-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Source(
            crate::external::ExternalCheckSourcePackage {
                language: "javascript".to_owned(),
                entry: "./check.js".to_owned(),
                build_adapter: "javascript-component".to_owned(),
                sources: vec!["./check.js".to_owned()],
            },
        ),
    };

    let result = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect("execute");
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].severity, Severity::Warning);
    assert_eq!(result.findings[0].message, "from-source");
}

#[test]
fn artifact_digest_mismatch_is_rejected() {
    let temp = tempdir().expect("temp dir");
    let wasm_bytes = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "checkleft_run") (param i32 i32) (result i64)
i64.const 0
  )
)"#,
    )
    .expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");

    let executor = WasmExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .to_owned(),
                provenance: None,
            },
        ),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("must reject digest mismatch");
    assert!(error.to_string().contains("artifact sha256 mismatch"));
}

#[test]
fn core_runtime_trap_does_not_fall_back_to_component_mode() {
    let temp = tempdir().expect("temp dir");
    let wasm_bytes = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "checkleft_run") (param i32 i32) (result i64)
    unreachable
  )
)"#,
    )
    .expect("parse wat");
    fs::write(temp.path().join("check.wasm"), wasm_bytes).expect("write wasm");
    let artifact_sha256 = sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read wasm"));

    let executor = WasmExternalCheckExecutor::new(temp.path()).expect("create executor");
    let package = ExternalCheckPackage {
        id: "example-check".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities::default(),
        implementation: ExternalCheckPackageImplementation::Artifact(
            ExternalCheckArtifactPackage {
                artifact_path: "check.wasm".to_owned(),
                artifact_sha256,
                provenance: None,
            },
        ),
    };

    let error = executor
        .execute(
            &package,
            &ChangeSet::default(),
            &LocalSourceTree::new(temp.path()).expect("tree"),
            &toml::Value::Table(Default::default()),
        )
        .expect_err("core trap must surface as runtime execution failure");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("external wasm check execution failed"));
    assert!(!rendered.contains("failed to compile component"));
}
