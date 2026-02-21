use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::tempdir;

use crate::external::ExternalCheckImplementationRef;

use super::ConfigResolver;

#[test]
fn resolves_single_config_file() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 500

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write config file");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "spelling-typos"]);
    assert_eq!(
        checks.get("file-size").expect("file-size present").check,
        "file-size"
    );
    assert_eq!(
        checks
            .get("file-size")
            .expect("file-size present")
            .config
            .as_table()
            .expect("file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(500)
    );
}

#[test]
fn merges_hierarchy_and_child_overrides_parent() {
    let temp = tempdir().expect("create temp dir");

    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 500

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
max_lines = 200

[[checks]]
id = "rust-naming"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "rust-naming", "spelling-typos"]);
    assert_eq!(
        checks
            .get("file-size")
            .expect("file-size present")
            .config
            .as_table()
            .expect("file-size config table")
            .get("max_lines")
            .expect("max_lines")
            .as_integer(),
        Some(200)
    );
}

#[test]
fn child_can_disable_inherited_check() {
    let temp = tempdir().expect("create temp dir");

    fs::create_dir_all(temp.path().join("backend/generated")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/generated/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
enabled = false
"#,
    )
    .expect("write generated config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/generated/output.rs"))
        .expect("resolve checks");

    let enabled_map: BTreeMap<_, _> = checks
        .iter()
        .map(|check| (check.id.as_str(), check.enabled))
        .collect();
    assert_eq!(enabled_map.get("file-size"), Some(&false));
    assert_eq!(checks.enabled().count(), 0);
}

#[test]
fn supports_instance_id_with_check_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typos"
check = "typo"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");

    let check = checks.get("domain-typos").expect("check exists");
    assert_eq!(check.id, "domain-typos");
    assert_eq!(check.check, "typo");
    assert_eq!(check.implementation, None);
}

#[test]
fn parses_external_check_implementation_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");

    let check = checks.get("domain-typo").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Generated(
            "domain-typo-check".to_owned()
        ))
    );
}

#[test]
fn rejects_invalid_external_check_implementation_reference() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "../escape/check.toml"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let error = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect_err("must fail");
    assert!(error.to_string().contains("invalid `implementation`"));
}

#[test]
fn ignores_invalid_external_check_implementation_for_disabled_checks() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
enabled = false
implementation = "../escape/check.toml"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("domain-typo").expect("check exists");

    assert!(!check.enabled);
    assert_eq!(check.implementation, None);
}

#[test]
fn excludes_config_files_by_default() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("CHECKS.toml"))
        .expect("resolve checks");

    assert!(!checks.include_config_files());
}

#[test]
fn allows_opt_in_to_include_config_files() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
include_config_files = true

[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("CHECKS.toml"))
        .expect("resolve checks");

    assert!(checks.include_config_files());
}

#[test]
fn child_config_can_override_include_config_files_setting() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
include_config_files = true
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[settings]
include_config_files = false
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/CHECKS.toml"))
        .expect("resolve checks");

    assert!(!checks.include_config_files());
}
