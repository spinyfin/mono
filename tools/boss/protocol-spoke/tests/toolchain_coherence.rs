//! The code generator and the runtime it generates for must be one coherent
//! set. Both come from the single Cargo.lock behind `@mono_crates`: the
//! `protoc-gen-prost` / `protoc-gen-tonic` plugins (build-dependencies of this
//! crate) are built from `prost-build` / `tonic-build`, and the generated code
//! calls into `prost` / `tonic`. A generator one minor behind (or ahead of) its
//! runtime emits code that may not compile, or compiles and misbehaves, so a
//! lockfile bump that splits them must fail here rather than at a distance.

use std::collections::BTreeMap;

/// `name -> versions` for every package in Cargo.lock.
fn locked_packages() -> BTreeMap<String, Vec<String>> {
    let path = std::env::var("CARGO_LOCK").expect("CARGO_LOCK names the repository Cargo.lock");
    let lock = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path}: {err}"));
    let mut packages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut name = None;
    for line in lock.lines() {
        if let Some(rest) = line.strip_prefix("name = \"") {
            name = rest.strip_suffix('"').map(str::to_owned);
        } else if let (Some(rest), Some(name)) = (line.strip_prefix("version = \""), name.take()) {
            packages
                .entry(name)
                .or_default()
                .push(rest.trim_end_matches('"').to_owned());
        }
    }
    packages
}

fn only_version(packages: &BTreeMap<String, Vec<String>>, name: &str) -> String {
    let versions = packages
        .get(name)
        .unwrap_or_else(|| panic!("{name} is not in Cargo.lock"));
    assert_eq!(
        versions.len(),
        1,
        "{name} must resolve to exactly one version, found {versions:?}"
    );
    versions[0].clone()
}

fn minor(version: &str) -> (String, String) {
    let mut parts = version.split('.');
    (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    )
}

#[test]
fn prost_generator_and_runtime_are_the_same_release() {
    let packages = locked_packages();
    let runtime = only_version(&packages, "prost");
    for companion in ["prost-build", "prost-derive", "prost-types"] {
        assert_eq!(
            only_version(&packages, companion),
            runtime,
            "{companion} must match the prost runtime ({runtime}); the protoc-gen-prost plugin and the generated code have to agree"
        );
    }
}

#[test]
fn tonic_generator_and_runtime_share_a_minor_release() {
    let packages = locked_packages();
    let runtime = only_version(&packages, "tonic");
    let generator = only_version(&packages, "tonic-build");
    assert_eq!(
        minor(&generator),
        minor(&runtime),
        "tonic-build {generator} (inside protoc-gen-tonic) must match the tonic runtime {runtime} at major.minor"
    );
}

#[test]
fn the_codegen_plugins_are_present_so_bazel_can_build_them() {
    let packages = locked_packages();
    only_version(&packages, "protoc-gen-prost");
    only_version(&packages, "protoc-gen-tonic");
}
