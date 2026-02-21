use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine, Instance, Memory, Module, Store};

use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding};

use super::{
    EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckArtifactPackage, ExternalCheckPackage,
    ExternalCheckPackageImplementation,
};

const ENTRYPOINT_EXPORT: &str = "checkleft_run";
const MEMORY_EXPORT: &str = "memory";
const INPUT_OFFSET: usize = 0;
const WASM_PAGE_SIZE_BYTES: usize = 65_536;
const EXECUTION_FUEL_LIMIT: u64 = 10_000_000;

pub trait ExternalCheckExecutor: Send + Sync {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult>;
}

#[derive(Debug, Default)]
pub struct NoopExternalCheckExecutor;

impl ExternalCheckExecutor for NoopExternalCheckExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        _changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        _config: &toml::Value,
    ) -> Result<CheckResult> {
        bail!(
            "external check package `{}` resolved successfully but sandbox runtime execution is not implemented yet",
            package.id
        )
    }
}

pub struct WasmExternalCheckExecutor {
    root: PathBuf,
    engine: Engine,
}

impl WasmExternalCheckExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize check runtime root {}",
                root.display()
            )
        })?;
        if !root.is_dir() {
            bail!("check runtime root is not a directory: {}", root.display());
        }

        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).context("failed to initialize Wasmtime engine")?;

        Ok(Self { root, engine })
    }

    fn execute_artifact(
        &self,
        package: &ExternalCheckPackage,
        artifact: &ExternalCheckArtifactPackage,
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let artifact_path = self.resolve_artifact_path(&artifact.artifact_path)?;
        let module_bytes = fs::read(&artifact_path)
            .with_context(|| format!("failed to read wasm artifact {}", artifact_path.display()))?;
        validate_artifact_sha256(package, artifact, &module_bytes)?;
        let module = Module::new(&self.engine, &module_bytes).with_context(|| {
            format!(
                "failed to compile wasm artifact for package `{}`",
                package.id
            )
        })?;

        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(EXECUTION_FUEL_LIMIT)
            .context("failed to configure runtime fuel limit")?;

        let instance = Instance::new(&mut store, &module, &[])
            .with_context(|| format!("failed to instantiate wasm module for `{}`", package.id))?;

        let memory = instance
            .get_memory(&mut store, MEMORY_EXPORT)
            .context("wasm module must export `memory`")?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, ENTRYPOINT_EXPORT)
            .with_context(|| {
                format!(
                    "wasm module must export `{ENTRYPOINT_EXPORT}` with signature (i32, i32) -> i64"
                )
            })?;

        let input = ExternalCheckRuntimeInput { changeset, config };
        let input_bytes =
            serde_json::to_vec(&input).context("failed to encode runtime input payload as JSON")?;

        ensure_memory_capacity(&memory, &mut store, INPUT_OFFSET, input_bytes.len())?;
        memory
            .write(&mut store, INPUT_OFFSET, &input_bytes)
            .context("failed to write runtime input into wasm memory")?;

        let output_range_encoded = run
            .call(
                &mut store,
                (
                    i32::try_from(INPUT_OFFSET).context("input offset does not fit in i32")?,
                    i32::try_from(input_bytes.len()).context("runtime input length exceeds i32")?,
                ),
            )
            .context("external wasm check execution failed")?;
        let (output_offset, output_len) = decode_output_range(output_range_encoded)?;

        ensure_memory_capacity(&memory, &mut store, output_offset, output_len)?;
        let mut output_bytes = vec![0_u8; output_len];
        memory
            .read(&mut store, output_offset, &mut output_bytes)
            .context("failed to read runtime output from wasm memory")?;

        let output: ExternalCheckRuntimeOutput = serde_json::from_slice(&output_bytes)
            .context("runtime output was not valid JSON CheckResult payload")?;

        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: output.findings,
        })
    }

    fn resolve_artifact_path(&self, artifact_path: &str) -> Result<PathBuf> {
        let path = Path::new(artifact_path);
        if path.is_absolute() {
            bail!("artifact path must be relative to repository root");
        }
        Ok(self.root.join(path))
    }
}

impl ExternalCheckExecutor for WasmExternalCheckExecutor {
    fn execute(
        &self,
        package: &ExternalCheckPackage,
        changeset: &ChangeSet,
        _source_tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        if package.runtime != EXTERNAL_CHECK_RUNTIME_V1 {
            bail!(
                "unsupported external runtime `{}` for package `{}`",
                package.runtime,
                package.id
            );
        }

        match &package.implementation {
            ExternalCheckPackageImplementation::Artifact(artifact) => {
                self.execute_artifact(package, artifact, changeset, config)
            }
            ExternalCheckPackageImplementation::Source(_) => bail!(
                "source-mode external package `{}` requires build adapters (Phase 3)",
                package.id
            ),
        }
    }
}

#[derive(Serialize)]
struct ExternalCheckRuntimeInput<'a> {
    changeset: &'a ChangeSet,
    config: &'a toml::Value,
}

#[derive(Deserialize)]
struct ExternalCheckRuntimeOutput {
    findings: Vec<Finding>,
}

fn validate_artifact_sha256(
    package: &ExternalCheckPackage,
    artifact: &ExternalCheckArtifactPackage,
    bytes: &[u8],
) -> Result<()> {
    let actual_sha256 = sha256_hex(bytes);
    if actual_sha256 == artifact.artifact_sha256 {
        return Ok(());
    }

    bail!(
        "artifact sha256 mismatch for package `{}` (path `{}`): expected `{}`, got `{}`",
        package.id,
        artifact.artifact_path,
        artifact.artifact_sha256,
        actual_sha256
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn ensure_memory_capacity(
    memory: &Memory,
    store: &mut Store<()>,
    offset: usize,
    len: usize,
) -> Result<()> {
    let required_size = offset
        .checked_add(len)
        .context("requested wasm memory range overflows usize")?;
    let current_size = memory.data_size(&mut *store);
    if required_size <= current_size {
        return Ok(());
    }

    let needed_bytes = required_size - current_size;
    let additional_pages = needed_bytes.div_ceil(WASM_PAGE_SIZE_BYTES);
    memory
        .grow(
            &mut *store,
            u64::try_from(additional_pages).context("page count does not fit in u64")?,
        )
        .context("failed to grow wasm memory")?;
    Ok(())
}

fn decode_output_range(encoded: i64) -> Result<(usize, usize)> {
    let encoded = u64::try_from(encoded).context("runtime returned negative output range")?;
    let offset = usize::try_from((encoded >> 32) as u32).context("output offset does not fit")?;
    let len = usize::try_from((encoded & 0xffff_ffff) as u32).context("output len does not fit")?;
    Ok((offset, len))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::external::{
        EXTERNAL_CHECK_API_V1, ExternalCheckArtifactPackage, ExternalCheckCapabilities,
        ExternalCheckPackage, ExternalCheckPackageImplementation,
    };
    use crate::input::ChangeSet;
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    use super::{ExternalCheckExecutor, WasmExternalCheckExecutor, sha256_hex};

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
        let artifact_sha256 =
            sha256_hex(&fs::read(temp.path().join("check.wasm")).expect("read wasm"));

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

    #[test]
    fn source_mode_is_rejected_until_phase_three() {
        let temp = tempdir().expect("temp dir");
        let executor = WasmExternalCheckExecutor::new(temp.path()).expect("create executor");
        let package = ExternalCheckPackage {
            id: "example-check".to_owned(),
            runtime: "sandbox-v1".to_owned(),
            api_version: EXTERNAL_CHECK_API_V1.to_owned(),
            capabilities: ExternalCheckCapabilities::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                crate::external::ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "./check.ts".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: vec!["./check.ts".to_owned()],
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
            .expect_err("must fail");
        assert!(error.to_string().contains("Phase 3"));
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
                    artifact_sha256:
                        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
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
}
