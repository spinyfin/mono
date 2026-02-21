use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::path::validate_relative_path;

use super::{ExternalCheckArtifactPackage, ExternalCheckPackage, ExternalCheckSourcePackage};

const SOURCE_MODE_CACHE_ROOT: &str = ".checkleft-cache/external-checks/source-mode";
const JS_COMPONENTIZER_TOOLCHAIN_DIR: &str = "tools/checks_js_componentizer";
const JS_COMPONENTIZER_LOCKFILE: &str = "pnpm-lock.yaml";
const JS_COMPONENTIZER_BUILD_SCRIPT: &str = "scripts/build_check.mjs";
const JS_COMPONENTIZER_BOOTSTRAP_ROOT: &str = ".checkleft-cache/js-componentizer/bootstrap";
const SOURCE_BUILD_ABI_VERSION: &str = "source-build-v1";

pub trait ExternalSourcePackageBuilder: Send + Sync {
    fn build_source_package(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage>;
}

pub struct JavaScriptComponentSourcePackageBuilder {
    root: PathBuf,
    command_runner: Arc<dyn CommandRunner>,
}

impl JavaScriptComponentSourcePackageBuilder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            command_runner: Arc::new(ProcessCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_command_runner(
        root: impl Into<PathBuf>,
        command_runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            root: root.into(),
            command_runner,
        }
    }

    fn build_javascript_component(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        let toolchain_dir = self.root.join(JS_COMPONENTIZER_TOOLCHAIN_DIR);
        let lock_path = toolchain_dir.join(JS_COMPONENTIZER_LOCKFILE);
        let lock_contents = fs::read(&lock_path).with_context(|| {
            format!("missing JS componentizer lockfile {}", lock_path.display())
        })?;
        let lock_hash = sha256_hex(&lock_contents);
        self.ensure_toolchain_bootstrapped(&toolchain_dir, &lock_hash)?;

        let source_inputs = self.collect_source_inputs(source)?;
        let cache_key = self.compute_cache_key(package, source, &lock_hash, &source_inputs);
        let artifact_dir = self.root.join(SOURCE_MODE_CACHE_ROOT).join(cache_key);
        let artifact_path = artifact_dir.join("check.wasm");

        if !artifact_path.exists() {
            fs::create_dir_all(&artifact_dir).with_context(|| {
                format!(
                    "failed to create source-mode cache directory {}",
                    artifact_dir.display()
                )
            })?;

            let entry_path = self.resolve_relative_path(&source.entry).with_context(|| {
                format!(
                    "invalid source entry path `{}` for package `{}`",
                    source.entry, package.id
                )
            })?;
            let build_script = toolchain_dir.join(JS_COMPONENTIZER_BUILD_SCRIPT);

            self.command_runner.run(
                &toolchain_dir,
                "node",
                &[
                    build_script.to_string_lossy().into_owned(),
                    "--repo-root".to_owned(),
                    self.root.to_string_lossy().into_owned(),
                    "--entry".to_owned(),
                    entry_path.to_string_lossy().into_owned(),
                    "--out".to_owned(),
                    artifact_path.to_string_lossy().into_owned(),
                ],
            )?;
        }

        let artifact_bytes = fs::read(&artifact_path).with_context(|| {
            format!(
                "JS source adapter did not produce wasm artifact {}",
                artifact_path.display()
            )
        })?;
        let artifact_sha256 = sha256_hex(&artifact_bytes);
        let artifact_rel_path = relative_to_root(&self.root, &artifact_path)?;

        Ok(ExternalCheckArtifactPackage {
            artifact_path: artifact_rel_path,
            artifact_sha256,
            provenance: None,
        })
    }

    fn ensure_toolchain_bootstrapped(&self, toolchain_dir: &Path, lock_hash: &str) -> Result<()> {
        fs::create_dir_all(toolchain_dir).with_context(|| {
            format!(
                "failed to create JS componentizer toolchain directory {}",
                toolchain_dir.display()
            )
        })?;

        let stamp = self
            .root
            .join(JS_COMPONENTIZER_BOOTSTRAP_ROOT)
            .join(format!("{lock_hash}.ok"));
        if stamp.exists() {
            return Ok(());
        }

        self.command_runner
            .run(toolchain_dir, "node", &["--version".to_owned()])?;
        self.command_runner
            .run(toolchain_dir, "corepack", &["--version".to_owned()])?;
        self.command_runner.run(
            toolchain_dir,
            "corepack",
            &[
                "pnpm".to_owned(),
                "install".to_owned(),
                "--frozen-lockfile".to_owned(),
            ],
        )?;

        let stamp_parent = stamp.parent().context("bootstrap stamp has no parent")?;
        fs::create_dir_all(stamp_parent).with_context(|| {
            format!(
                "failed to create JS componentizer stamp directory {}",
                stamp_parent.display()
            )
        })?;
        fs::write(&stamp, lock_hash).with_context(|| {
            format!(
                "failed to write JS componentizer bootstrap stamp {}",
                stamp.display()
            )
        })?;
        Ok(())
    }

    fn collect_source_inputs(&self, source: &ExternalCheckSourcePackage) -> Result<Vec<PathBuf>> {
        let mut relative_paths = BTreeSet::new();
        relative_paths.insert(PathBuf::from(&source.entry));
        for source_path in &source.sources {
            relative_paths.insert(PathBuf::from(source_path));
        }

        let mut resolved_paths = Vec::with_capacity(relative_paths.len());
        for path in relative_paths {
            let absolute = self
                .resolve_relative_path(path.to_string_lossy().as_ref())
                .with_context(|| format!("invalid source path `{}`", path.display()))?;
            resolved_paths.push(absolute);
        }
        Ok(resolved_paths)
    }

    fn resolve_relative_path(&self, raw: &str) -> Result<PathBuf> {
        let path = Path::new(raw);
        validate_relative_path(path)?;
        let resolved = self.root.join(path);
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("source path does not exist: {}", resolved.display()))?;
        let root = self.root.canonicalize().with_context(|| {
            format!("failed to canonicalize source root {}", self.root.display())
        })?;
        if !canonical.starts_with(&root) {
            bail!(
                "source path escapes repository root: {}",
                resolved.display()
            );
        }
        Ok(canonical)
    }

    fn compute_cache_key(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
        lock_hash: &str,
        source_inputs: &[PathBuf],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(SOURCE_BUILD_ABI_VERSION);
        hasher.update(package.id.as_bytes());
        hasher.update(package.runtime.as_bytes());
        hasher.update(package.api_version.as_bytes());
        hasher.update(source.language.as_bytes());
        hasher.update(source.entry.as_bytes());
        hasher.update(source.build_adapter.as_bytes());
        hasher.update(lock_hash.as_bytes());

        for source_path in source_inputs {
            hasher.update(source_path.to_string_lossy().as_bytes());
            if let Ok(bytes) = fs::read(source_path) {
                hasher.update(bytes);
            }
        }

        format!("{:x}", hasher.finalize())
    }
}

impl ExternalSourcePackageBuilder for JavaScriptComponentSourcePackageBuilder {
    fn build_source_package(
        &self,
        package: &ExternalCheckPackage,
        source: &ExternalCheckSourcePackage,
    ) -> Result<ExternalCheckArtifactPackage> {
        let language = source.language.trim();
        let build_adapter = source.build_adapter.trim();
        if !matches!(language, "javascript" | "typescript") {
            bail!(
                "unsupported source language `{language}` for package `{}`",
                package.id
            );
        }
        if build_adapter != "javascript-component" {
            bail!(
                "unsupported source build adapter `{build_adapter}` for package `{}`",
                package.id
            );
        }

        self.build_javascript_component(package, source)
    }
}

trait CommandRunner: Send + Sync {
    fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()>;
}

struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()> {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .with_context(|| format!("failed to run `{program}` in {}", cwd.display()))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let rendered_args = args.join(" ");
        bail!(
            "command `{program} {rendered_args}` failed in {} (status {}): stderr=`{stderr}` stdout=`{stdout}`",
            cwd.display(),
            output.status
        );
    }
}

fn relative_to_root(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("path {} is not under {}", path.display(), root.display()))?;
    validate_relative_path(relative)?;

    let rendered = relative
        .components()
        .map(|part| part.as_os_str())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join("/");
    Ok(rendered)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::external::{
        EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckCapabilities,
        ExternalCheckPackage, ExternalCheckPackageImplementation,
    };

    use super::{
        CommandRunner, ExternalSourcePackageBuilder, JavaScriptComponentSourcePackageBuilder,
    };

    #[derive(Default)]
    struct MockCommandRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for MockCommandRunner {
        fn run(&self, cwd: &Path, program: &str, args: &[String]) -> Result<()> {
            self.calls
                .lock()
                .expect("lock calls")
                .push((program.to_owned(), args.to_vec()));

            if program == "node" && args.first().is_some_and(|arg| arg.ends_with(".mjs")) {
                let out_index = args
                    .iter()
                    .position(|arg| arg == "--out")
                    .expect("out flag")
                    + 1;
                let output_path = Path::new(&args[out_index]);
                let wasm = wat::parse_str(
                    r#"(module
  (memory (export "memory") 1)
  (data (i32.const 16) "{\"findings\":[]}")
  (func (export "checkleft_run") (param i32 i32) (result i64)
    i64.const 68719476748
  )
)"#,
                )
                .expect("valid wat");
                std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir");
                std::fs::write(output_path, wasm).expect("write wasm");
            }

            assert!(cwd.exists(), "cwd must exist");
            Ok(())
        }
    }

    fn make_source_package(root: &Path) -> ExternalCheckPackage {
        std::fs::create_dir_all(root.join("checks/js")).expect("mkdir");
        std::fs::create_dir_all(root.join("tools/checks_js_componentizer/scripts"))
            .expect("mkdir scripts");
        std::fs::write(
            root.join("tools/checks_js_componentizer/pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .expect("lock");
        std::fs::write(
            root.join("tools/checks_js_componentizer/scripts/build_check.mjs"),
            "// test stub\n",
        )
        .expect("script");
        std::fs::write(
            root.join("checks/js/check.js"),
            "export function run(input) { return input; }\n",
        )
        .expect("source");

        ExternalCheckPackage {
            id: "js-check".to_owned(),
            runtime: EXTERNAL_CHECK_RUNTIME_V1.to_owned(),
            api_version: EXTERNAL_CHECK_API_V1.to_owned(),
            capabilities: ExternalCheckCapabilities::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                crate::external::ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "checks/js/check.js".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: vec!["checks/js/check.js".to_owned()],
                },
            ),
        }
    }

    #[test]
    fn source_build_uses_cache_between_runs() {
        let temp = tempdir().expect("temp dir");
        let package = make_source_package(temp.path());
        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder = JavaScriptComponentSourcePackageBuilder::with_command_runner(
            temp.path(),
            runner.clone(),
        );

        let first = builder
            .build_source_package(&package, source)
            .expect("first build");
        let second = builder
            .build_source_package(&package, source)
            .expect("second build");

        assert_eq!(first.artifact_path, second.artifact_path);
        assert_eq!(first.artifact_sha256, second.artifact_sha256);

        let calls = runner.calls.lock().expect("calls").clone();
        let compile_calls = calls
            .iter()
            .filter(|(program, args)| {
                program == "node" && args.first().is_some_and(|arg| arg.ends_with(".mjs"))
            })
            .count();
        assert_eq!(compile_calls, 1, "compile should be cached");
    }

    #[test]
    fn source_build_rebuilds_when_sources_change() {
        let temp = tempdir().expect("temp dir");
        let package = make_source_package(temp.path());
        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder = JavaScriptComponentSourcePackageBuilder::with_command_runner(
            temp.path(),
            runner.clone(),
        );

        let first = builder
            .build_source_package(&package, source)
            .expect("first build");
        std::fs::write(
            temp.path().join("checks/js/check.js"),
            "export function run(input) { return input + 'x'; }\n",
        )
        .expect("rewrite source");
        let second = builder
            .build_source_package(&package, source)
            .expect("second build");

        assert_ne!(
            first.artifact_path, second.artifact_path,
            "cache key should include source bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_build_rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let outside = tempdir().expect("outside temp dir");
        std::fs::write(
            outside.path().join("check.js"),
            "export function run(input){return input;}",
        )
        .expect("write outside source");

        let package = make_source_package(temp.path());
        std::fs::remove_file(temp.path().join("checks/js/check.js")).expect("remove source file");
        symlink(
            outside.path().join("check.js"),
            temp.path().join("checks/js/check.js"),
        )
        .expect("create symlink");

        let source = match &package.implementation {
            ExternalCheckPackageImplementation::Source(source) => source,
            _ => panic!("expected source implementation"),
        };
        let runner = Arc::new(MockCommandRunner::default());
        let builder =
            JavaScriptComponentSourcePackageBuilder::with_command_runner(temp.path(), runner);

        let error = builder
            .build_source_package(&package, source)
            .expect_err("must reject escaping source path");
        let message = error.to_string();
        assert!(
            message.contains("escapes repository root") || message.contains("invalid source path")
        );
    }
}
