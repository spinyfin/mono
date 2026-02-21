use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Instance, Memory, Module, Store};

use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding};

use super::{
    ExternalCheckArtifactPackage, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalSourcePackageBuilder, JavaScriptComponentSourcePackageBuilder,
    EXTERNAL_CHECK_RUNTIME_V1,
};

const CORE_ENTRYPOINT_EXPORT: &str = "checkleft_run";
const COMPONENT_ENTRYPOINT_EXPORT: &str = "run";
const MEMORY_EXPORT: &str = "memory";
const INPUT_OFFSET: usize = 0;
const WASM_PAGE_SIZE_BYTES: usize = 65_536;
const EXECUTION_FUEL_LIMIT: u64 = 10_000_000;

#[derive(Debug)]
enum CoreArtifactExecutionError {
    ArtifactMismatch(anyhow::Error),
    Execution(anyhow::Error),
}

impl CoreArtifactExecutionError {
    fn mismatch(err: anyhow::Error) -> Self {
        Self::ArtifactMismatch(err)
    }

    fn execution(err: anyhow::Error) -> Self {
        Self::Execution(err)
    }
}

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
    source_package_builder: Arc<dyn ExternalSourcePackageBuilder>,
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
        config.wasm_component_model(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).context("failed to initialize Wasmtime engine")?;

        let source_package_builder =
            Arc::new(JavaScriptComponentSourcePackageBuilder::new(root.clone()));
        Ok(Self {
            root,
            engine,
            source_package_builder,
        })
    }

    #[cfg(test)]
    fn with_source_package_builder(
        root: impl Into<PathBuf>,
        source_package_builder: Arc<dyn ExternalSourcePackageBuilder>,
    ) -> Result<Self> {
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
        config.wasm_component_model(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).context("failed to initialize Wasmtime engine")?;

        Ok(Self {
            root,
            engine,
            source_package_builder,
        })
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

        match self.execute_core_artifact(package, &module_bytes, changeset, config) {
            Ok(result) => Ok(result),
            Err(CoreArtifactExecutionError::ArtifactMismatch(core_error)) => self
                .execute_component_artifact(package, &module_bytes, changeset, config)
                .with_context(|| {
                    format!(
                        "failed to execute package `{}` as component after core mismatch: {core_error:#}",
                        package.id
                    )
                }),
            Err(CoreArtifactExecutionError::Execution(error)) => Err(error),
        }
    }

    fn execute_core_artifact(
        &self,
        package: &ExternalCheckPackage,
        module_bytes: &[u8],
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> std::result::Result<CheckResult, CoreArtifactExecutionError> {
        let module = Module::new(&self.engine, module_bytes)
            .with_context(|| format!("failed to compile core wasm module for `{}`", package.id))
            .map_err(CoreArtifactExecutionError::mismatch)?;
        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(EXECUTION_FUEL_LIMIT)
            .context("failed to configure runtime fuel limit")
            .map_err(CoreArtifactExecutionError::execution)?;

        let instance = Instance::new(&mut store, &module, &[])
            .with_context(|| format!("failed to instantiate wasm module for `{}`", package.id))
            .map_err(CoreArtifactExecutionError::mismatch)?;

        let memory = instance
            .get_memory(&mut store, MEMORY_EXPORT)
            .context("wasm module must export `memory`")
            .map_err(CoreArtifactExecutionError::mismatch)?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, CORE_ENTRYPOINT_EXPORT)
            .with_context(|| {
                format!(
                    "core wasm module must export `{CORE_ENTRYPOINT_EXPORT}` with signature (i32, i32) -> i64"
                )
            })
            .map_err(CoreArtifactExecutionError::mismatch)?;

        let input = ExternalCheckRuntimeInput { changeset, config };
        let input_bytes = serde_json::to_vec(&input)
            .context("failed to encode runtime input payload as JSON")
            .map_err(CoreArtifactExecutionError::execution)?;

        ensure_memory_capacity(&memory, &mut store, INPUT_OFFSET, input_bytes.len())
            .map_err(CoreArtifactExecutionError::execution)?;
        memory
            .write(&mut store, INPUT_OFFSET, &input_bytes)
            .context("failed to write runtime input into wasm memory")
            .map_err(CoreArtifactExecutionError::execution)?;

        let input_offset =
            i32::try_from(INPUT_OFFSET).context("input offset does not fit in i32");
        let input_offset = input_offset.map_err(CoreArtifactExecutionError::execution)?;
        let input_len = i32::try_from(input_bytes.len()).context("runtime input length exceeds i32");
        let input_len = input_len.map_err(CoreArtifactExecutionError::execution)?;
        let output_range_encoded = run
            .call(&mut store, (input_offset, input_len))
            .context("external wasm check execution failed")
            .map_err(CoreArtifactExecutionError::execution)?;
        let (output_offset, output_len) =
            decode_output_range(output_range_encoded).map_err(CoreArtifactExecutionError::execution)?;

        ensure_memory_capacity(&memory, &mut store, output_offset, output_len)
            .map_err(CoreArtifactExecutionError::execution)?;
        let mut output_bytes = vec![0_u8; output_len];
        memory
            .read(&mut store, output_offset, &mut output_bytes)
            .context("failed to read runtime output from wasm memory")
            .map_err(CoreArtifactExecutionError::execution)?;

        let output: ExternalCheckRuntimeOutput = serde_json::from_slice(&output_bytes)
            .context("runtime output was not valid JSON CheckResult payload")
            .map_err(CoreArtifactExecutionError::execution)?;

        Ok(CheckResult {
            check_id: package.id.clone(),
            findings: output.findings,
        })
    }

    fn execute_component_artifact(
        &self,
        package: &ExternalCheckPackage,
        component_bytes: &[u8],
        changeset: &ChangeSet,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let component = Component::new(&self.engine, component_bytes)
            .with_context(|| format!("failed to compile component for `{}`", package.id))?;
        let linker = Linker::<()>::new(&self.engine);
        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(EXECUTION_FUEL_LIMIT)
            .context("failed to configure runtime fuel limit")?;
        let instance = linker
            .instantiate(&mut store, &component)
            .with_context(|| format!("failed to instantiate component for `{}`", package.id))?;
        let run = instance
            .get_typed_func::<(String,), (String,)>(&mut store, COMPONENT_ENTRYPOINT_EXPORT)
            .with_context(|| {
                format!(
                    "component must export `{COMPONENT_ENTRYPOINT_EXPORT}` with signature (string) -> (string)"
                )
            })?;

        let input = ExternalCheckRuntimeInput { changeset, config };
        let input_json =
            serde_json::to_string(&input).context("failed to encode component runtime input")?;
        let (output_json,) = run
            .call(&mut store, (input_json,))
            .context("external component check execution failed")?;
        let output: ExternalCheckRuntimeOutput =
            serde_json::from_str(&output_json).context("component output was not valid JSON")?;

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
            ExternalCheckPackageImplementation::Source(source) => {
                let built_artifact = self
                    .source_package_builder
                    .build_source_package(package, source)?;
                self.execute_artifact(package, &built_artifact, changeset, config)
            }
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
mod tests;
