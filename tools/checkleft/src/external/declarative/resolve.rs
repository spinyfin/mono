//! Framework-owned binary resolution.
//!
//! A declared binary requirement is resolved to a concrete command by the
//! *framework*, not by any guest code. Three resolvers are supported:
//!
//! - [`BinaryBinding::Path`] — a direct path or PATH name, located once before the
//!   child environment is cleared. Valid as a default binding or a CHECKS-config
//!   override; never accepted as a fallback.
//! - [`BinaryBinding::Bazel`] — a Bazel label, built then resolved to its
//!   executable. **Environment-conditional**: it requires a Bazel workspace, so it
//!   works in-repo but not in standalone checkleft. It reuses the *same* resolver
//!   the built-in buildifier check uses ([`resolve_bazel_target_executable`]),
//!   which proves the framework can own what the built-in hand-rolled.
//! - [`BinaryBinding::Npm`] — a version-pinned npm package, resolved to
//!   `npx --yes <package>@<version>` using the SHA-verified Node runtime in
//!   checkleft’s cache. The version pin precedes the check’s own args as a
//!   [`ResolvedBinary::prefix_args`] entry, so npx runs exactly the pinned release.
//!
//! A CHECKS-config override may substitute a different binding per declared name,
//! keeping CHECKS.yaml thin (enable + repo-specific overrides only) while the
//! definition lives in the package manifest. An `npm` override may set only the
//! field it wants to change (e.g. just `version`); omitted fields inherit from the
//! default `npm` binding.
//!
//! Resolution fails closed instead of falling back to a host PATH binary.
//! Alternate Bazel/npm bindings may still declare a pinned fallback.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{BinaryBinding, BinaryRequirement};
use crate::external::timeout::{CheckDeadline, SubprocessTimeout, output_with_timeout};

/// A declared binary resolved to a concrete command: the program to spawn plus any
/// prefix args that must precede the invocation's own templated args.
///
/// `path`/`bazel` bindings resolve to a bare program (`prefix_args` empty). The
/// `npm` binding resolves to `npx` with `["--yes", "<package>@<version>"]` as
/// prefix args, so the version pin precedes the check's own arguments.
///
/// `display_invocation` is the human-readable form used in remediation templates
/// via `{{needs.<name>.invocation}}`. It must be runnable on any host, so npm
/// bindings render a host-independent `npx --yes '<package>@<version>'` rather
/// than machine-local cache paths; path/bazel bindings use the program string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinary {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
    pub display_invocation: String,
}

impl ResolvedBinary {
    /// A bare program with no prefix args (the common `path`/`bazel` case).
    fn bare(program: PathBuf) -> Self {
        let display_invocation = program.to_string_lossy().into_owned();
        Self {
            program,
            prefix_args: Vec::new(),
            display_invocation,
        }
    }
}

/// Resolve every declared binary to a concrete command, honoring CHECKS-config
/// overrides. Returns a map keyed by the declared name.
pub fn resolve_all(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
    check_id: &str,
    timeout_ms: u64,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    resolve_all_with_deadline(repo_root, needs, config, check_id, CheckDeadline::new(timeout_ms))
}

enum NpmRuntime {
    Provision,
    #[cfg(test)]
    Injected(Option<PathBuf>),
}

/// Resolve every declared binary under a check-wide execution deadline.
pub(crate) fn resolve_all_with_deadline(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    resolve_all_with_runtime(repo_root, needs, config, &NpmRuntime::Provision, check_id, deadline)
}

/// Inner resolver with the located `npx` (or `None`) injected. Split out so tests
/// can drive both the resolved-pin and missing-runtime error paths deterministically
/// without depending on the host PATH.
#[cfg(test)]
pub(crate) fn resolve_all_with_npx(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
    npx: Option<&Path>,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    resolve_all_with_runtime(
        repo_root,
        needs,
        config,
        &NpmRuntime::Injected(npx.map(Path::to_owned)),
        "binary resolution",
        CheckDeadline::new(crate::external::timeout::resolve_timeout_ms(
            None,
            0,
            crate::external::timeout::DECLARATIVE_HOST_CEILING_TIMEOUT_MS,
        )),
    )
}

fn resolve_all_with_runtime(
    repo_root: &Path,
    needs: &BTreeMap<String, BinaryRequirement>,
    config: &toml::Value,
    npx: &NpmRuntime,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<BTreeMap<String, ResolvedBinary>> {
    let mut resolved = BTreeMap::new();
    for (name, requirement) in needs {
        let override_binding = override_binding(name, requirement, config)
            .transpose()
            .with_context(|| format!("invalid config override for binary `{name}`"))?;

        let binary = if let Some(binding) = override_binding {
            resolve_binding(repo_root, &binding, npx, check_id, deadline)
                .with_context(|| format!("failed to resolve declared binary `{name}`"))?
        } else {
            resolve_requirement(repo_root, name, requirement, npx, check_id, deadline)?
        };

        resolved.insert(name.clone(), binary);
    }
    Ok(resolved)
}

/// Resolve using the default binding, falling back if declared and the primary
/// (bazel/npm) binding fails to resolve. A `path` default always resolves, so it
/// never reaches the fallback branch.
fn resolve_requirement(
    repo_root: &Path,
    name: &str,
    requirement: &BinaryRequirement,
    npx: &NpmRuntime,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<ResolvedBinary> {
    match resolve_binding(repo_root, &requirement.default, npx, check_id, deadline) {
        Ok(binary) => Ok(binary),
        Err(err) => {
            let Some(fallback) = &requirement.fallback else {
                return Err(err).with_context(|| {
                    format!("failed to resolve declared binary `{name}`; provision its declared toolchain")
                });
            };
            if matches!(fallback, BinaryBinding::Path(_)) {
                return Err(err).with_context(|| {
                    format!(
                        "failed to resolve `{name}`; host PATH fallbacks are disabled; provision the declared toolchain"
                    )
                });
            }
            let fallback_binary = resolve_binding(repo_root, fallback, npx, check_id, deadline).with_context(|| {
                format!("primary resolution of `{name}` failed AND fallback resolution also failed")
            })?;
            let version = match binary_version_string_with_timeout(
                &fallback_binary.program,
                check_id,
                &format!(
                    "version probe for fallback binary `{}`",
                    fallback_binary.program.display()
                ),
                deadline.remaining_ms(),
            ) {
                Ok(version) => version,
                Err(err) if err.downcast_ref::<SubprocessTimeout>().is_some() => return Err(err),
                Err(_) => String::new(),
            };
            eprintln!(
                "warning: checkleft: `{name}`: hermetic toolchain unresolved ({err:#}); \
                 using alternate pinned binary `{}` ({})",
                fallback_binary.program.display(),
                if version.is_empty() {
                    "version unknown".to_owned()
                } else {
                    version
                }
            );
            Ok(fallback_binary)
        }
    }
}

fn resolve_binding(
    repo_root: &Path,
    binding: &BinaryBinding,
    npx: &NpmRuntime,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<ResolvedBinary> {
    match binding {
        BinaryBinding::Path(path) => Ok(ResolvedBinary::bare(resolve_path_binding(repo_root, path))),
        BinaryBinding::Bazel(target) => Ok(ResolvedBinary::bare(resolve_bazel_target_executable(
            repo_root, target, check_id, deadline,
        )?)),
        BinaryBinding::Npm { package, version } => resolve_npm_binding(package, version, npx, check_id, deadline),
    }
}

/// Resolve a package using the provisioned pinned Node distribution.
fn resolve_npm_binding(
    package: &str,
    version: &str,
    npx: &NpmRuntime,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<ResolvedBinary> {
    let npx = match npx {
        NpmRuntime::Provision => super::node::provision(check_id, deadline)?,
        #[cfg(test)]
        NpmRuntime::Injected(npx) => npx.clone().ok_or_else(|| {
            anyhow::anyhow!("pinned npx unavailable; cannot provision npm package `{package}@{version}`")
        })?,
    };
    // Remediation text is read on other machines (e.g. from CI logs), so it must
    // not embed this host's cache paths; `checkleft fix` provisions the pinned Node.
    let display_invocation = format!(
        "npx --yes {}",
        crate::install::shell_single_quote(&format!("{package}@{version}")),
    );
    Ok(ResolvedBinary {
        program: npx,
        // `--yes` auto-confirms fetching the pinned package non-interactively (CI);
        // the `<package>@<version>` spec forces the exact release.
        prefix_args: vec!["--yes".to_owned(), format!("{package}@{version}")],
        display_invocation,
    })
}

/// Resolve a `path` binding to a concrete program path.
///
/// - A **bare name** (no path separator) is located once before the child
///   environment is cleared; the resolved tool directory becomes part of PATH.
/// - An **absolute path** is used as-is.
/// - A **relative path with a separator** (e.g. a `bazel-bin/…/launcher` produced
///   by the `local_check` bazel rule, which is how the folded `exec` tier ships a
///   binary) is joined to `repo_root`. `Command` program resolution combined with
///   `current_dir` is platform-specific for relative program paths, so we anchor
///   it to the repo root to make the spawn unambiguous — matching the old exec
///   runtime, which resolved repo-relative executables against the root.
fn resolve_path_binding(repo_root: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let has_separator = path.contains('/') || path.contains('\\');
    if candidate.is_absolute() || !has_separator {
        crate::external::environment::locate(candidate)
    } else {
        repo_root.join(candidate)
    }
}

/// Returns the trimmed first line of `binary --version`, or an empty string
/// when the probe cannot run or emits non-UTF-8 output.
///
/// This preserves the original best-effort public API. Runtime resolution uses
/// [`binary_version_string_with_timeout`] so it can distinguish timeouts.
pub fn binary_version_string(binary: &Path) -> String {
    binary_version_string_with_timeout(
        binary,
        "binary version probe",
        &format!("version probe for binary `{}`", binary.display()),
        crate::external::timeout::resolve_timeout_ms(
            None,
            0,
            crate::external::timeout::DECLARATIVE_HOST_CEILING_TIMEOUT_MS,
        ),
    )
    .unwrap_or_default()
}

/// Returns the trimmed first line of `binary --version` under the supplied
/// timeout, preserving errors for callers that must fail closed on deadlines.
fn binary_version_string_with_timeout(
    binary: &Path,
    check_id: &str,
    subprocess: &str,
    timeout_ms: u64,
) -> Result<String> {
    let mut command = Command::new(binary);
    command.arg("--version");
    let output = output_with_timeout(&mut command, check_id, subprocess, timeout_ms)?;
    Ok(String::from_utf8(output.stdout)
        .ok()
        .and_then(|version| version.lines().next().map(|line| line.trim().to_owned()))
        .unwrap_or_default())
}

/// Read an optional `applies_to` glob list override from the per-check `config` blob.
///
/// When `applies_to` is present in `config`, its value REPLACES the definition's
/// `applies_to` list entirely. The override uses the same glob vocabulary as the
/// check definition — a list of one or more glob strings. An empty list is rejected:
/// to disable a check entirely, use `enabled: false` instead.
///
/// Returns `None` when no override is present (caller uses the definition's list).
/// Returns `Some(Err(...))` on a malformed override.
/// Returns `Some(Ok(globs))` with the replacement list when the override is valid.
pub fn override_applies_to(config: &toml::Value) -> Option<Result<Vec<String>>> {
    let value = config.get("applies_to")?;
    let array = match value.as_array() {
        Some(arr) => arr,
        None => {
            return Some(Err(anyhow::anyhow!(
                "`applies_to` config override must be a list of glob strings, not a scalar"
            )));
        }
    };
    if array.is_empty() {
        return Some(Err(anyhow::anyhow!(
            "`applies_to` config override must not be empty; \
              use `enabled: false` to disable the check instead"
        )));
    }
    let mut globs = Vec::with_capacity(array.len());
    for (i, entry) in array.iter().enumerate() {
        match entry.as_str() {
            Some(s) if !s.trim().is_empty() => {
                if let Some(reason) = crate::glob_scope::structurally_empty_reason(s, false) {
                    return Some(Err(anyhow::anyhow!(
                        "`applies_to[{i}]` config override pattern `{s}` can never match any \
                         changeset path: {reason}"
                    )));
                }
                globs.push(s.to_owned())
            }
            Some(_) => {
                return Some(Err(anyhow::anyhow!(
                    "`applies_to[{i}]` config override entry must not be empty"
                )));
            }
            None => {
                return Some(Err(anyhow::anyhow!(
                    "`applies_to[{i}]` config override entry must be a string"
                )));
            }
        }
    }
    Some(Ok(globs))
}

/// Read an optional binding override from `config` at `needs.<name>.{path|bazel|npm}`.
///
/// An `npm` override may set only the field it wants to change; any omitted
/// `package`/`version` is inherited from the requirement's default `npm` binding,
/// so a repo can re-pin the version with just `needs.<name>.npm.version = "<X>"`.
fn override_binding(
    name: &str,
    requirement: &BinaryRequirement,
    config: &toml::Value,
) -> Option<Result<BinaryBinding>> {
    let entry = config.get("needs")?.get(name)?;
    let bazel = entry.get("bazel").and_then(toml::Value::as_str);
    let path = entry.get("path").and_then(toml::Value::as_str);
    let npm = entry.get("npm");

    let set_count = bazel.is_some() as u8 + path.is_some() as u8 + npm.is_some() as u8;
    if set_count == 0 {
        return Some(Err(anyhow::anyhow!(
            "override for `{name}` must set `bazel`, `path`, or `npm`"
        )));
    }
    if set_count > 1 {
        return Some(Err(anyhow::anyhow!(
            "override for `{name}` sets more than one of `bazel`/`path`/`npm`; set exactly one"
        )));
    }

    let result = if let Some(bazel) = bazel {
        Ok(BinaryBinding::Bazel(bazel.to_owned()))
    } else if let Some(path) = path {
        Ok(BinaryBinding::Path(path.to_owned()))
    } else {
        override_npm_binding(name, requirement, npm.expect("npm set when others unset"))
    };

    Some(result.and_then(|binding| match &binding {
        BinaryBinding::Path(value) | BinaryBinding::Bazel(value) if value.trim().is_empty() => {
            bail!("override for `{name}` must not be empty")
        }
        BinaryBinding::Npm { package, version } if package.trim().is_empty() || version.trim().is_empty() => {
            bail!("`npm` override for `{name}` must not have an empty `package` or `version`")
        }
        _ => Ok(binding),
    }))
}

/// Build an `npm` override binding, inheriting `package`/`version` from the
/// requirement's default `npm` binding for any field the override omits.
fn override_npm_binding(name: &str, requirement: &BinaryRequirement, npm: &toml::Value) -> Result<BinaryBinding> {
    let (default_package, default_version) = match &requirement.default {
        BinaryBinding::Npm { package, version } => (Some(package.as_str()), Some(version.as_str())),
        _ => (None, None),
    };
    let package = npm.get("package").and_then(toml::Value::as_str).or(default_package);
    let version = npm.get("version").and_then(toml::Value::as_str).or(default_version);
    match (package, version) {
        (Some(package), Some(version)) => Ok(BinaryBinding::Npm {
            package: package.to_owned(),
            version: version.to_owned(),
        }),
        _ => bail!(
            "`npm` override for `{name}` must set `package` and `version` \
             (or omit them to inherit from a default `npm` binding)"
        ),
    }
}

/// Construct provisioning commands with canonical startup flags and environment.
pub(super) fn bazel_resolve_command(repo_root: &Path, launcher: &Path, target: &str, build: bool) -> Command {
    bazel_resolve_command_with_ci_flags(repo_root, launcher, target, build, &super::ci_bazel_flags())
}

/// As [`bazel_resolve_command`], with the CI per-command flags passed in so they can be pinned in tests.
pub(super) fn bazel_resolve_command_with_ci_flags(
    repo_root: &Path,
    launcher: &Path,
    target: &str,
    build: bool,
    ci_flags: &[String],
) -> Command {
    let mut command = Command::new(crate::external::environment::locate(launcher));
    command
        .args(super::ci_bazel_startup_flags())
        .arg(if build { "build" } else { "cquery" })
        .args(ci_flags)
        .args(["--color=no", "--curses=no", "--noshow_progress"]);
    if build {
        command
            .args(["--show_result=0", "--ui_event_filters=-info"])
            .arg(target);
    } else {
        command
            .arg(target)
            .arg("--output=starlark")
            .arg("--starlark:expr=target.files_to_run.executable.path if target.files_to_run.executable else ''");
    }
    command.current_dir(repo_root);
    command
}

/// Builds `target` and resolves its executable path via `bazel cquery --output=starlark`.
/// Returns an absolute path to the built binary.
pub(crate) fn resolve_bazel_target_executable(
    repo_root: &Path,
    target: &str,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<PathBuf> {
    let mut build = bazel_resolve_command(repo_root, Path::new("bazel"), target, true);
    let build_output = output_with_timeout(
        &mut build,
        check_id,
        &format!("Bazel build while resolving `{target}`"),
        deadline.remaining_ms(),
    )?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        bail!(
            "`bazel build {target}` failed (exit {}): {}",
            build_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let mut cquery = bazel_resolve_command(repo_root, Path::new("bazel"), target, false);
    let cquery_output = output_with_timeout(
        &mut cquery,
        check_id,
        &format!("Bazel cquery while resolving `{target}`"),
        deadline.remaining_ms(),
    )?;

    if !cquery_output.status.success() {
        let stderr = String::from_utf8_lossy(&cquery_output.stderr);
        bail!(
            "`bazel cquery {target}` failed (exit {}): {}",
            cquery_output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let raw = String::from_utf8_lossy(&cquery_output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();

    if raw.is_empty() {
        bail!(
            "bazel target `{target}` does not produce an executable \
             (cquery returned an empty path)"
        );
    }

    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_root.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_host_fallback_fails_closed() {
        let needs = BTreeMap::from([(
            "tool".into(),
            BinaryRequirement {
                default: BinaryBinding::Npm {
                    package: "tool".into(),
                    version: "1.0.0".into(),
                },
                fallback: Some(BinaryBinding::Path("/bin/true".into())),
            },
        )]);
        let error = resolve_all_with_npx(
            Path::new("/repo"),
            &needs,
            &toml::Value::Table(Default::default()),
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("host PATH fallbacks are disabled"));
    }
}
