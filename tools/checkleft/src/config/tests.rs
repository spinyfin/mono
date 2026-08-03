use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::tempdir;

use crate::external::ExternalCheckImplementationRef;
use crate::output::Severity;

use super::{
    CheckScope, ConfigResolver, StaleExclusionMode, diagnose_unknown_check_fields, levenshtein_distance,
    suggest_check_field_correction, unknown_check_field_severity_for_version,
};

mod yaml;

#[test]
fn stale_exclusion_severity_defaults_to_warn_and_inherits() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
stale_exclusion_severity = "error"

[[checks]]
id = "rust/giant-structs"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    // The root setting is inherited by descendant directories.
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");
    assert_eq!(checks.stale_exclusion_mode(), StaleExclusionMode::Error);

    // A repo with no setting defaults to Warn.
    let bare = tempdir().expect("create temp dir");
    fs::write(
        bare.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\n",
    )
    .expect("write config");
    let bare_resolver = ConfigResolver::new(bare.path()).expect("create resolver");
    assert_eq!(
        bare_resolver
            .resolve_for_file(Path::new("a.rs"))
            .expect("resolve")
            .stale_exclusion_mode(),
        StaleExclusionMode::Warn
    );
}

#[test]
fn per_check_stale_exclusion_severity_override_is_parsed() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "rust/giant-structs"

[checks.policy]
stale_exclusion_severity = "off"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let check = checks.get("rust/giant-structs").expect("check present");
    assert_eq!(check.policy.stale_exclusion_mode, Some(StaleExclusionMode::Off));
}

#[test]
fn invalid_stale_exclusion_severity_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[settings]
stale_exclusion_severity = "loud"

[[checks]]
id = "rust/giant-structs"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("stale_exclusion_severity")),
        "expected a diagnostic about the invalid severity, got {diagnostics:?}"
    );
}

#[test]
fn scope_defaults_to_files() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let check = checks.get("rust/giant-structs").expect("check present");
    assert_eq!(check.scope, CheckScope::Files);
}

#[test]
fn scope_changeset_is_parsed() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"boss/no-boss-isms\"\nscope = \"changeset\"\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let check = checks.get("boss/no-boss-isms").expect("check present");
    assert_eq!(check.scope, CheckScope::Changeset);
}

#[test]
fn scope_changeset_in_subdirectory_config_produces_diagnostic_and_is_not_scheduled() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("subdir")).expect("create dir");
    fs::write(
        temp.path().join("subdir/CHECKS.toml"),
        "[[checks]]\nid = \"boss/no-boss-isms\"\nscope = \"changeset\"\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("subdir/a.rs"))
        .expect("resolve checks");

    // The check must not be silently scheduled: it's absent from the
    // resolved set, and a diagnostic explains why.
    assert!(
        checks.get("boss/no-boss-isms").is_none(),
        "a subdirectory-declared changeset-scope check must never be scheduled"
    );
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("scope = changeset") && diagnostic.message.contains("boss/no-boss-isms")
        }),
        "expected a diagnostic naming the misplaced changeset-scope check, got {diagnostics:?}"
    );
}

#[test]
fn subdirectory_override_without_scope_inherits_root_changeset_scope() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"boss/no-boss-isms\"\nscope = \"changeset\"\n",
    )
    .expect("write root config");
    fs::create_dir_all(temp.path().join("subdir")).expect("create dir");
    fs::write(
        temp.path().join("subdir/CHECKS.toml"),
        "[[checks]]\nid = \"boss/no-boss-isms\"\n\n[checks.policy]\nseverity = \"warning\"\n",
    )
    .expect("write subdirectory config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("subdir/a.rs"))
        .expect("resolve checks");

    // The override (no `scope` key) must inherit the root's `Changeset` scope,
    // not silently reset to `Files` — otherwise the check gets scheduled twice
    // (once as changeset at the root, once as files here).
    let check = checks.get("boss/no-boss-isms").expect("check present");
    assert_eq!(check.scope, CheckScope::Changeset);

    // No misplacement diagnostic: this is an inherited scope, not an explicit
    // (and invalid) `scope = changeset` declared in a subdirectory.
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("scope = changeset")),
        "inheriting scope from the root should not produce a misplacement diagnostic, got {diagnostics:?}"
    );
}

#[test]
fn changeset_scope_with_applies_to_override_produces_diagnostic_and_is_not_scheduled() {
    // A `scope = changeset` check resolves once at repo root with no changed file
    // to filter (Runner::schedule_changeset_scope_runs), so a `config.applies_to`
    // override on it is meaningless and must be rejected, not silently accepted.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
scope = "changeset"

[checks.config]
applies_to = ["**/*.rs"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");

    assert!(
        checks.get("boss/no-boss-isms").is_none(),
        "a changeset-scope check with a meaningless applies_to override must not be scheduled"
    );
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("scope: changeset") && diagnostic.message.contains("applies_to")
        }),
        "expected a diagnostic naming the scope+applies_to combination, got {diagnostics:?}"
    );
}

#[test]
fn changeset_scope_without_applies_to_override_is_unaffected() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
scope = "changeset"

[checks.config]
some_other_key = true
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");

    let check = checks.get("boss/no-boss-isms").expect("check present");
    assert_eq!(check.scope, CheckScope::Changeset);
    assert!(
        checks.diagnostics().next().is_none(),
        "unrelated config keys must not trip this"
    );
}

#[test]
fn invalid_scope_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nscope = \"directory\"\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("scope")),
        "expected a diagnostic about the invalid scope, got {diagnostics:?}"
    );
    // The malformed check is skipped, not upserted with a bogus scope.
    assert!(checks.get("rust/giant-structs").is_none());
}

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
    assert_eq!(checks.get("file-size").expect("file-size present").check, "file-size");
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
fn caches_ancestor_config_resolution_across_sibling_directories() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create src dir");
    fs::create_dir_all(temp.path().join("backend/tests")).expect("create tests dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let initial = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve backend/src checks");
    let initial_enabled: Vec<_> = initial.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(initial_enabled, vec!["file-size", "spelling-typos"]);

    fs::remove_file(temp.path().join("CHECKS.toml")).expect("remove root config");
    fs::remove_file(temp.path().join("backend/CHECKS.toml")).expect("remove backend config");

    let checks = resolver
        .resolve_for_file(Path::new("backend/tests/lib.rs"))
        .expect("resolve backend/tests checks");

    let enabled: Vec<_> = checks.enabled().map(|check| check.id.as_str()).collect();
    assert_eq!(enabled, vec!["file-size", "spelling-typos"]);
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

    let enabled_map: BTreeMap<_, _> = checks.iter().map(|check| (check.id.as_str(), check.enabled)).collect();
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
fn parses_explicit_generated_implementation_reference() {
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
    assert_eq!(check.policy.severity, None);
    assert_eq!(check.policy.allow_bypass, None);
    assert_eq!(check.policy.bypass_name, None);
}

// ── bundled resolution (new shape: id/check name only, no implementation: needed) ──

#[test]
fn bare_id_matching_bundled_name_resolves_to_bundled() {
    // The simplest consumer shape: just an id. No implementation:, no check_definitions.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/bazel"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("format/bazel").expect("check exists");
    assert_eq!(check.check, "format/bazel");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("format/bazel".to_owned()))
    );
}

#[test]
fn namespaced_id_resolves_to_bundled() {
    // A namespaced id (format/bazel, lint/rust, format/rust, etc.) resolves to its bundled def
    // — the id grammar allows lowercase segments separated by single slashes.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "lint/rust"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("lint/rust").expect("check exists");
    assert_eq!(check.check, "lint/rust");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("lint/rust".to_owned()))
    );
}

#[test]
fn custom_id_with_bundled_check_name_resolves_to_bundled() {
    // Custom instance id + check: pointing at a bundled name.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-format-bazel"
check = "format/bazel"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("my-format-bazel").expect("check exists");
    assert_eq!(check.check, "format/bazel");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("format/bazel".to_owned()))
    );
}

#[test]
fn unknown_name_without_exec_paths_leaves_implementation_none() {
    // A name that is neither bundled nor in exec_paths stays as None (routes to built-in).
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
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("file-size").expect("check exists");
    assert_eq!(check.implementation, None);
}

#[test]
fn exec_paths_resolves_check_from_on_disk_dir() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a fake check def at checks/my-check/check.yaml.
    let defs_dir = temp.path().join("checks/my-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    // The file just needs to exist; content irrelevant for config resolution.
    fs::write(defs_dir.join("check.yaml"), "id: my-check\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "my-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-check/check.yaml").to_path_buf()
        ))
    );
}

#[test]
fn allow_override_bundled_makes_exec_path_win_over_bundled() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a local copy of the bundled format/bazel def using the flat layout.
    let defs_dir = temp.path().join("tools/checkleft/checks/format");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("bazel.yaml"), "id: format/bazel\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["tools/checkleft/checks"]
allow_override_bundled = true

[[checks]]
id = "format/bazel"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("format/bazel").expect("check exists");
    // The exec-path copy wins over the bundled def.
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("tools/checkleft/checks/format/bazel.yaml").to_path_buf()
        ))
    );
}

#[test]
fn bundled_wins_over_exec_path_by_default() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a local copy of the bundled format/bazel def using the flat layout.
    let defs_dir = temp.path().join("checks/format");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("bazel.yaml"), "id: format/bazel\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "format/bazel"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("format/bazel").expect("check exists");
    // Bundled wins (allow_override_bundled defaults to false).
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("format/bazel".to_owned()))
    );
}

#[test]
fn check_definitions_is_inherited_by_child_configs() {
    let temp = tempdir().expect("create temp dir");
    let defs_dir = temp.path().join("checks/my-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("check.yaml"), "id: my-check\n").expect("write def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]
"#,
    )
    .expect("write root config");

    fs::create_dir_all(temp.path().join("sub")).expect("create child dir");
    fs::write(
        temp.path().join("sub/CHECKS.toml"),
        r#"
[[checks]]
id = "my-check"
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("sub/file.rs"))
        .expect("resolve checks");

    let check = checks.get("my-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-check/check.yaml").to_path_buf()
        ))
    );
}

#[test]
fn explicit_bundled_ref_still_works() {
    // Explicit `implementation: bundled:<name>` still resolves correctly.
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-format-bazel"
check = "format/bazel"
implementation = "bundled:format/bazel"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");

    let check = checks.get("my-format-bazel").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Bundled("format/bazel".to_owned()))
    );
}

#[test]
fn explicit_generated_ref_still_works() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "my-custom"
implementation = "generated:my-custom"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-custom").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::Generated("my-custom".to_owned()))
    );
}

#[test]
fn rejects_invalid_exec_path() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["../escape"]
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("BUILD.bazel"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0]
            .message
            .contains("invalid `check_definitions.exec_paths`")
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
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert!(checks.get("domain-typo").is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "domain-typo");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(diagnostics[0].message.contains("invalid `implementation`"));
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
fn parses_policy_config_for_enabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_FILE_SIZE_LIMIT"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");

    assert_eq!(check.policy.severity, Some(Severity::Error));
    assert_eq!(check.policy.allow_bypass, Some(true));
    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_FILE_SIZE_LIMIT"));
}

#[test]
fn normalizes_policy_bypass_name_from_non_prefixed_value() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"

[checks.policy]
bypass_name = "domain-typo"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("domain-typo").expect("check exists");

    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_DOMAIN_TYPO"));
}

#[test]
fn child_config_overrides_policy_values() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "warning"
allow_bypass = false
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_CUSTOM_CHILD"
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");

    assert_eq!(check.policy.severity, Some(Severity::Error));
    assert_eq!(check.policy.allow_bypass, Some(true));
    assert_eq!(check.policy.bypass_name.as_deref(), Some("BYPASS_CUSTOM_CHILD"));
}

#[test]
fn rejects_invalid_policy_severity_for_enabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.policy]
severity = "fatal"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert!(checks.get("file-size").is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "file-size");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(diagnostics[0].message.contains("invalid `policy.severity`"));
}

#[test]
fn ignores_invalid_policy_severity_for_disabled_check() {
    let temp = tempdir().expect("create temp dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
enabled = false

[checks.policy]
severity = "fatal"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let check = checks.get("file-size").expect("check exists");
    assert!(!check.enabled);
    assert_eq!(check.policy.severity, None);
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

#[test]
fn malformed_toml_reports_diagnostic_instead_of_failing() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"
config = { max_lines = [1, 2 }
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("docs/file.md"))
        .expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();

    assert_eq!(checks.enabled().count(), 0);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].check_id, "checks-config");
    assert_eq!(diagnostics[0].location.path, Path::new("CHECKS.toml"));
    assert!(diagnostics[0].message.contains("failed to parse checks config"));
}

#[test]
fn coexisting_yaml_and_toml_produces_violation() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("CHECKS.yaml"), "checks:\n  - id: file-size\n").expect("write CHECKS.yaml");
    fs::write(temp.path().join("CHECKS.toml"), "[[checks]]\nid = \"file-size\"\n").expect("write CHECKS.toml");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert_eq!(diagnostics.len(), 1, "expected exactly one coexistence diagnostic");
    assert_eq!(diagnostics[0].check_id, "checks-config");
    assert!(
        diagnostics[0].message.contains("CHECKS.yaml") && diagnostics[0].message.contains("CHECKS.toml"),
        "diagnostic message should name both files: {}",
        diagnostics[0].message
    );
    assert!(
        diagnostics[0].message.contains("keep exactly one"),
        "diagnostic message should instruct the user to keep one: {}",
        diagnostics[0].message
    );
}

#[test]
fn single_config_file_produces_no_coexistence_violation() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("CHECKS.toml"), "[[checks]]\nid = \"file-size\"\n").expect("write CHECKS.toml");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics for a single config file, got: {diagnostics:?}"
    );
}

#[test]
fn exec_paths_resolves_component_check_from_toml_manifest() {
    let temp = tempdir().expect("create temp dir");
    // Lay down a component-mode check.toml (no yaml present for this check).
    let defs_dir = temp.path().join("checks/my-component-check");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(
        defs_dir.join("check.toml"),
        r#"
id = "my-component-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/my_component_check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write component manifest");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "my-component-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("my-component-check").expect("check exists");
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/my-component-check/check.toml").to_path_buf()
        ))
    );
}

#[test]
fn exec_paths_yaml_wins_over_toml_when_both_present() {
    // Flat .yaml takes precedence over flat .toml in the same exec_path.
    let temp = tempdir().expect("create temp dir");
    let defs_dir = temp.path().join("checks");
    fs::create_dir_all(&defs_dir).expect("create def dir");
    fs::write(defs_dir.join("dual-format-check.yaml"), "id: dual-format-check\n").expect("write yaml def");
    fs::write(
        defs_dir.join("dual-format-check.toml"),
        r#"
id = "dual-format-check"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "checks/dual.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
    )
    .expect("write toml def");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[check_definitions]
exec_paths = ["checks"]

[[checks]]
id = "dual-format-check"
"#,
    )
    .expect("write root config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    let check = checks.get("dual-format-check").expect("check exists");
    // flat .yaml wins (checked first in find_in_exec_paths)
    assert_eq!(
        check.implementation,
        Some(ExternalCheckImplementationRef::File(
            Path::new("checks/dual-format-check.yaml").to_path_buf()
        ))
    );
}

// ── exclusion matcher: global and per-check excludes ──────────────────────────

#[test]
fn root_global_excludes_are_stored_in_resolved_checks() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["mobile/ios/vendor/**", "**/*.generated.*"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("src/lib.rs"))
        .expect("resolve checks");

    assert_eq!(
        checks.global_exclude_patterns(),
        &["mobile/ios/vendor/**", "**/*.generated.*"]
    );
}

#[test]
fn global_excludes_accumulate_down_hierarchy() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        // Authored relative to backend/: "generated/**" means "backend/generated/**"
        // after repo-root normalization.
        r#"
exclude = ["generated/**"]

[[checks]]
id = "lint-rust"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("backend/src/lib.rs"))
        .expect("resolve checks");

    // Both root and child global excludes should be present (union).
    // "generated/**" in backend/ normalizes to "backend/generated/**".
    let patterns = checks.global_exclude_patterns();
    assert!(
        patterns.contains(&"vendor/**".to_owned()),
        "root exclude must be present; got {patterns:?}"
    );
    assert!(
        patterns.contains(&"backend/generated/**".to_owned()),
        "child exclude must be present; got {patterns:?}"
    );
}

#[test]
fn global_excludes_from_child_do_not_appear_at_root_level() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend")).expect("create backend dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("backend/CHECKS.toml"),
        r#"
exclude = ["generated/**"]

[[checks]]
id = "lint-rust"
"#,
    )
    .expect("write backend config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");

    // File at root level: should only see the root global excludes.
    let root_checks = resolver
        .resolve_for_file(Path::new("Cargo.toml"))
        .expect("resolve root checks");
    assert_eq!(root_checks.global_exclude_patterns(), &["vendor/**"]);
}

#[test]
fn per_check_excludes_are_stored_on_check_config() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["frontend/testdata/report-*.reference.html"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("frontend/src/app.ts"))
        .expect("resolve");

    let check = checks.get("format/oxc").expect("check exists");
    assert_eq!(
        check.exclude_patterns,
        vec!["frontend/testdata/report-*.reference.html".to_owned()]
    );
}

#[test]
fn per_check_excludes_are_replaced_on_upsert() {
    // Per-check excludes follow the upsert-replace rule, not union.
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("sub")).expect("create sub dir");

    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["parent-only/**"]
"#,
    )
    .expect("write root config");

    fs::write(
        temp.path().join("sub/CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["sub-only/**"]
"#,
    )
    .expect("write child config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("sub/file.ts")).expect("resolve");

    let check = checks.get("format/oxc").expect("check exists");
    // Only child's exclude should remain (parent's was replaced by upsert).
    assert_eq!(check.exclude_patterns, vec!["sub/sub-only/**".to_owned()]);
}

#[test]
fn per_check_excludes_from_subdirectory_are_normalized() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("frontend")).expect("create frontend dir");

    fs::write(
        temp.path().join("frontend/CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["testdata/**"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver
        .resolve_for_file(Path::new("frontend/src/app.ts"))
        .expect("resolve");

    let check = checks.get("format/oxc").expect("check exists");
    // Authored as "testdata/**" in frontend/CHECKS.toml → normalized to "frontend/testdata/**".
    assert_eq!(check.exclude_patterns, vec!["frontend/testdata/**".to_owned()]);
}

#[test]
fn legacy_config_exclude_files_is_merged_into_per_check_excludes() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file/size"

[checks.config]
max_lines = 500
exclude_files = ["**/*.md", "**/*.lock"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let check = checks.get("file/size").expect("check exists");
    assert_eq!(
        check.exclude_patterns,
        vec!["**/*.md".to_owned(), "**/*.lock".to_owned()]
    );
}

#[test]
fn framework_level_and_legacy_excludes_are_merged() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file/size"
exclude = ["**/*.generated.rs"]

[checks.config]
max_lines = 500
exclude_files = ["**/*.md"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let check = checks.get("file/size").expect("check exists");
    assert!(
        check.exclude_patterns.contains(&"**/*.generated.rs".to_owned()),
        "framework-level exclude must be present; got {:?}",
        check.exclude_patterns
    );
    assert!(
        check.exclude_patterns.contains(&"**/*.md".to_owned()),
        "legacy config exclude must be present; got {:?}",
        check.exclude_patterns
    );
}

#[test]
fn effective_matcher_for_combines_global_and_per_check() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/**"]

[[checks]]
id = "format/oxc"
exclude = ["testdata/**"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.ts")).expect("resolve");

    let check = checks.get("format/oxc").expect("check exists");
    let matcher = checks.effective_matcher_for(check).expect("build matcher");

    use std::path::Path;
    assert!(
        matcher.is_excluded(Path::new("vendor/dep/lib.ts")),
        "global exclude must apply"
    );
    assert!(
        matcher.is_excluded(Path::new("testdata/report.ts")),
        "per-check exclude must apply"
    );
    assert!(
        !matcher.is_excluded(Path::new("src/lib.ts")),
        "normal file must not be excluded"
    );
}

#[test]
fn empty_global_exclude_list_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = []

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("must not be an empty list")),
        "expected diagnostic about empty exclude list; got {diagnostics:?}"
    );
    // No patterns should have been added.
    assert!(checks.global_exclude_patterns().is_empty());
}

#[test]
fn empty_per_check_exclude_list_produces_diagnostic_and_skips_check() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = []
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/file.ts")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("must not be an empty list")),
        "expected diagnostic about empty per-check exclude list; got {diagnostics:?}"
    );
    // The check itself should be skipped (not added to resolved set).
    assert!(
        checks.get("format/oxc").is_none(),
        "check with invalid exclude should be absent"
    );
}

#[test]
fn exclude_files_alias_works_for_global_excludes() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude_files = ["Cargo.lock"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    assert_eq!(checks.global_exclude_patterns(), &["Cargo.lock".to_owned()]);
}

#[test]
fn exclude_globs_alias_works_for_global_excludes() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude_globs = ["**/*.generated.*"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    assert_eq!(checks.global_exclude_patterns(), &["**/*.generated.*".to_owned()]);
}

#[test]
fn exclude_files_alias_works_for_per_check_excludes() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude_files = ["testdata/**"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/file.ts")).expect("resolve");

    let check = checks.get("format/oxc").expect("check exists");
    assert_eq!(check.exclude_patterns, vec!["testdata/**".to_owned()]);
}

#[test]
fn misspelled_exclude_key_produces_diagnostic_with_correction() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nexcludes = [\"testdata/**\"]\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.iter().any(|d| d.message.contains("excludes")
            && d.message.contains("did you mean `exclude`?")
            && d.check_id == "rust/giant-structs"),
        "expected a `did you mean exclude` diagnostic, got {diagnostics:?}"
    );
}

#[test]
fn policy_field_written_as_sibling_produces_misplacement_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nseverity = \"warning\"\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("severity") && d.message.contains("belongs inside `policy:`")),
        "expected a misplacement diagnostic for `severity`, got {diagnostics:?}"
    );
}

#[test]
fn unrelated_unknown_key_produces_generic_diagnostic_listing_known_fields() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nfrobnicate = true\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("frobnicate") && d.message.contains("expected one of")),
        "expected a generic unknown-field diagnostic, got {diagnostics:?}"
    );
}

#[test]
fn unknown_check_entry_key_does_not_block_loading_during_grace_period() {
    // During the grace period (severity pinned to Warning explicitly, so this
    // integration test does not depend on the ambient build-time version), a
    // stray key is diagnosed but the check still loads with its recognised
    // fields applied.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nexcludes = [\"testdata/**\"]\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path())
        .expect("create resolver")
        .with_unknown_field_severity_for_test(Severity::Warning);
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(!diagnostics.is_empty(), "the stray key should be diagnosed");
    assert!(
        checks.get("rust/giant-structs").is_some(),
        "check should still load during the warning-only grace period"
    );
}

#[test]
fn unknown_check_entry_key_blocks_loading_once_escalated_to_error() {
    // Once the grace period ends (severity pinned to Error explicitly), a stray
    // key's check is skipped entirely rather than silently loaded with the
    // unknown key ignored.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        "[[checks]]\nid = \"rust/giant-structs\"\nexcludes = [\"testdata/**\"]\n",
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path())
        .expect("create resolver")
        .with_unknown_field_severity_for_test(Severity::Error);
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(!diagnostics.is_empty(), "the stray key should be diagnosed");
    assert!(
        diagnostics.iter().any(|d| d.severity == Severity::Error),
        "the diagnostic should be tagged Error, got {diagnostics:?}"
    );
    assert!(
        checks.get("rust/giant-structs").is_none(),
        "check should be skipped once the deprecation window has closed"
    );
}

#[test]
fn unknown_check_field_diagnostics_are_tagged_with_the_requested_severity() {
    // The apply-time `Error` branch (post-grace-period) skips loading the check —
    // exercised structurally by every other `push_diagnostic(...); continue;` config
    // error in this file (e.g. `invalid_scope_produces_diagnostic`). This test pins
    // the severity-tagging half of that contract directly, since the crate can't
    // fake being a different released version from within a test.
    let mut unknown_fields = BTreeMap::new();
    unknown_fields.insert("excludes".to_owned(), serde::de::IgnoredAny);
    let warning_diagnostics = diagnose_unknown_check_fields(
        "rust/giant-structs",
        &unknown_fields,
        Path::new("CHECKS.toml"),
        Severity::Warning,
    );
    assert_eq!(warning_diagnostics.len(), 1);
    assert_eq!(warning_diagnostics[0].severity, Severity::Warning);

    let error_diagnostics = diagnose_unknown_check_fields(
        "rust/giant-structs",
        &unknown_fields,
        Path::new("CHECKS.toml"),
        Severity::Error,
    );
    assert_eq!(error_diagnostics.len(), 1);
    assert_eq!(error_diagnostics[0].severity, Severity::Error);
}

#[test]
fn unknown_check_field_severity_has_one_released_grace_version() {
    assert_eq!(
        unknown_check_field_severity_for_version("0.1.0-alpha.8"),
        Severity::Warning
    );
    assert_eq!(
        unknown_check_field_severity_for_version("0.1.0-alpha.9"),
        Severity::Warning
    );
    assert_eq!(
        unknown_check_field_severity_for_version("0.1.0-alpha.10"),
        Severity::Error
    );
    assert_eq!(unknown_check_field_severity_for_version("0.1.0"), Severity::Error);
    assert_eq!(unknown_check_field_severity_for_version("0.0.0-dev"), Severity::Error);
}

#[test]
fn suggest_check_field_correction_covers_misspelling_misplacement_and_unrelated_keys() {
    assert_eq!(
        suggest_check_field_correction("excludes").as_deref(),
        Some("did you mean `exclude`?")
    );
    assert_eq!(
        suggest_check_field_correction("severity").as_deref(),
        Some("`severity` belongs inside `policy:`, not as a sibling of it")
    );
    assert_eq!(suggest_check_field_correction("frobnicate"), None);
}

#[test]
fn levenshtein_distance_matches_known_values() {
    assert_eq!(levenshtein_distance("exclude", "exclude"), 0);
    assert_eq!(levenshtein_distance("excludes", "exclude"), 1);
    assert_eq!(levenshtein_distance("scop", "scope"), 1);
    assert_eq!(levenshtein_distance("frobnicate", "exclude"), 9);
}

#[test]
fn unknown_key_inside_config_block_is_not_flagged() {
    // `config:` is guest configuration passed through verbatim to the check
    // implementation — its keys are never checked against the check-entry schema.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "rust/giant-structs"

[checks.config]
excludes = ["testdata/**"]
whatever_the_check_wants = 1
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("a.rs")).expect("resolve checks");
    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.is_empty(),
        "keys inside `config:` must not be checked against the check-entry schema, got {diagnostics:?}"
    );
}

#[test]
fn invalid_glob_in_global_exclude_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["[invalid-glob"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("invalid glob pattern") && d.message.contains("exclude")),
        "expected diagnostic about invalid glob in global exclude; got {diagnostics:?}"
    );
    // No patterns should have been added.
    assert!(checks.global_exclude_patterns().is_empty());
}

#[test]
fn invalid_glob_in_per_check_exclude_produces_diagnostic_and_skips_check() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["[invalid-glob"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/file.ts")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("invalid glob pattern") && d.message.contains("format/oxc")),
        "expected diagnostic about invalid glob for check; got {diagnostics:?}"
    );
    // The check itself should be skipped (not added to resolved set).
    assert!(
        checks.get("format/oxc").is_none(),
        "check with invalid exclude glob should be absent"
    );
}

// ── structurally-empty patterns on the framework `exclude` key ────────────────
//
// Same taxonomy as `applies_to` (see `external::declarative` tests): a leading
// `./`, a trailing path separator, or a `!` prefix can never match any
// changeset path, decided from the pattern text alone, on either the
// top-level global `exclude` or a per-check `exclude`.

#[test]
fn global_exclude_leading_dot_slash_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["./vendor/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("./vendor/**") && d.message.contains("exclude[0]")),
        "expected diagnostic naming the pattern and position; got {diagnostics:?}"
    );
    assert!(checks.global_exclude_patterns().is_empty());
}

#[test]
fn global_exclude_negation_prefix_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["!vendor/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics.iter().any(|d| d.message.contains("!vendor/**")),
        "expected diagnostic naming the pattern; got {diagnostics:?}"
    );
}

#[test]
fn global_exclude_trailing_separator_produces_diagnostic() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("vendor/") && d.message.contains("separator")),
        "expected diagnostic naming the pattern and reason; got {diagnostics:?}"
    );
}

#[test]
fn global_exclude_typo_and_wrong_case_are_not_rejected() {
    // Case (b): not statically decidable, must never error here.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendorr/**", "VENDOR/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    assert!(
        checks.diagnostics().next().is_none(),
        "typo'd/wrong-case exclude patterns must not produce a diagnostic"
    );
    assert_eq!(
        checks.global_exclude_patterns(),
        &["vendorr/**".to_owned(), "VENDOR/**".to_owned()]
    );
}

#[test]
fn per_check_exclude_structurally_empty_pattern_produces_diagnostic_and_skips_check() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"
exclude = ["./testdata/**"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/file.ts")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("./testdata/**") && d.message.contains("format/oxc")),
        "expected diagnostic naming the pattern and check; got {diagnostics:?}"
    );
    assert!(
        checks.get("format/oxc").is_none(),
        "check with invalid exclude should be absent"
    );
}

#[test]
fn global_exclude_one_bad_pattern_does_not_discard_the_others() {
    // A single structurally-empty pattern in a multi-entry `exclude` list must
    // not discard the file's other, valid global excludes — only the offending
    // entry is rejected (with a diagnostic), the rest still apply.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor/**", "./third_party/**", "generated/**"]

[[checks]]
id = "file-size"
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("./third_party/**") && d.message.contains("exclude[1]")),
        "expected diagnostic naming the offending pattern and its index; got {diagnostics:?}"
    );
    assert_eq!(
        checks.global_exclude_patterns(),
        &["vendor/**".to_owned(), "generated/**".to_owned()],
        "the other, valid patterns must still apply despite the one bad entry"
    );
}

#[test]
fn legacy_exclude_files_structurally_empty_pattern_produces_diagnostic() {
    // The legacy `config.exclude_files`/`config.exclude_globs` alias position is a
    // live, documented backwards-compatible exclude position — it must be validated
    // the same way as the framework-level `exclude` key, not silently accept a
    // structurally-empty pattern.
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "format/oxc"

[checks.config]
exclude_files = ["vendor/"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/file.ts")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("vendor/") && d.message.contains("exclude_files")),
        "expected diagnostic naming the pattern and legacy key; got {diagnostics:?}"
    );
    assert!(
        checks.get("format/oxc").is_none(),
        "check with an invalid legacy exclude should be absent"
    );
}

#[test]
fn per_repo_applies_to_override_structurally_empty_pattern_produces_diagnostic_at_resolution() {
    // The per-repo `config.applies_to` override position is what authors actually
    // type into CHECKS.yaml. It must produce a `ConfigDiagnostic` at config
    // resolution time (not only at check-execution time via `override_applies_to`),
    // so it's visible to `list_configured_checks` and any run that never schedules
    // this check (e.g. an empty changeset).
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[checks.config]
applies_to = ["./src/**/*.rs"]
"#,
    )
    .expect("write config");

    let resolver = ConfigResolver::new(temp.path()).expect("create resolver");
    let checks = resolver.resolve_for_file(Path::new("src/lib.rs")).expect("resolve");

    let diagnostics: Vec<_> = checks.diagnostics().collect();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("./src/**/*.rs") && d.message.contains("applies_to[0]")),
        "expected a config-resolution-time diagnostic naming the pattern and position; got {diagnostics:?}"
    );
}
