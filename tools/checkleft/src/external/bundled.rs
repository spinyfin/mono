//! First-party check definitions embedded directly in the checkleft binary.
//!
//! A target repo that has *no* checkleft definition files on disk can still run
//! these checks: the manifests are compiled into the binary via `include_str!`,
//! so there is zero install. A check opts in with a `bundled:<name>` reference,
//! or via the `check_def_source = "bundled"` directive (see [`crate::config`]).
//!
//! ## Adding a bundled definition
//!
//! 1. Add the manifest at `tools/checkleft/checks/<name>/check.yaml`.
//! 2. Add a row to [`BUNDLED_CHECK_DEFS`] below (one `include_str!`).
//! 3. Add the file to `checkleft_lib`'s `compile_data` in `BUILD.bazel` so the
//!    bazel build can read it at compile time.
//!
//! We embed each file explicitly (rather than `include_dir!`) because the bazel
//! build does not run `build.rs`, and every embedded file must be declared as
//! `compile_data` anyway — so an explicit, reviewable table is both hermetic
//! under bazel and clearer about exactly what ships in the binary.

use anyhow::{Context, Result};

use super::{
    ExternalCheckImplementationRef, ExternalCheckPackage, ExternalCheckPackageProvider,
    parse_external_check_manifest,
};

/// A first-party definition compiled into the binary.
struct BundledCheckDef {
    /// Bundle key — the directory name under `tools/checkleft/checks/`. This is
    /// what a `bundled:<name>` reference names.
    name: &'static str,
    /// File extension of the embedded manifest, selecting the parser
    /// (`yaml`/`yml` → declarative schema, otherwise the TOML schema).
    extension: &'static str,
    /// The raw manifest contents, embedded at compile time.
    contents: &'static str,
}

/// The embedded first-party definitions. To add one, see the module docs.
static BUNDLED_CHECK_DEFS: &[BundledCheckDef] = &[BundledCheckDef {
    name: "buildifier",
    extension: "yaml",
    contents: include_str!("../../checks/buildifier/check.yaml"),
}];

/// Names of all bundled definitions (for diagnostics / `--list`-style output).
pub fn bundled_check_names() -> impl Iterator<Item = &'static str> {
    BUNDLED_CHECK_DEFS.iter().map(|def| def.name)
}

/// Resolves [`ExternalCheckImplementationRef::Bundled`] references against the
/// definitions embedded in the binary. Always available — needs no on-disk
/// files, env vars, or network — which is the whole point of the bundle.
#[derive(Debug, Default)]
pub struct BundledExternalCheckPackageProvider;

impl ExternalCheckPackageProvider for BundledExternalCheckPackageProvider {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>> {
        let ExternalCheckImplementationRef::Bundled(name) = implementation_ref else {
            return Ok(None);
        };

        let Some(def) = BUNDLED_CHECK_DEFS.iter().find(|def| def.name == name) else {
            return Ok(None);
        };

        parse_external_check_manifest(def.contents, def.extension)
            .with_context(|| format!("invalid bundled check definition `{name}`"))
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bundled_buildifier_definition() {
        let provider = BundledExternalCheckPackageProvider;
        let package = provider
            .resolve(&ExternalCheckImplementationRef::Bundled("buildifier".to_owned()))
            .expect("resolve")
            .expect("package");
        assert_eq!(package.id, "buildifier-declarative");
    }

    #[test]
    fn every_bundled_definition_parses() {
        // Guards against a stale `include_str!` row: each embedded manifest must
        // parse cleanly so a target repo never hits a broken bundled def.
        let provider = BundledExternalCheckPackageProvider;
        for name in bundled_check_names() {
            provider
                .resolve(&ExternalCheckImplementationRef::Bundled(name.to_owned()))
                .unwrap_or_else(|err| panic!("bundled def `{name}` failed to parse: {err:#}"))
                .unwrap_or_else(|| panic!("bundled def `{name}` did not resolve"));
        }
    }

    #[test]
    fn returns_none_for_unknown_bundled_name() {
        let provider = BundledExternalCheckPackageProvider;
        let resolved = provider
            .resolve(&ExternalCheckImplementationRef::Bundled(
                "does-not-exist".to_owned(),
            ))
            .expect("resolve");
        assert!(resolved.is_none());
    }

    #[test]
    fn ignores_non_bundled_refs() {
        let provider = BundledExternalCheckPackageProvider;
        let resolved = provider
            .resolve(&ExternalCheckImplementationRef::Generated(
                "buildifier".to_owned(),
            ))
            .expect("resolve");
        assert!(resolved.is_none());
    }
}
