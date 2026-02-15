use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::path::validate_relative_path;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CHECKS_FILE_NAME: &str = "CHECKS.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConfig {
    pub check: String,
    pub id: String,
    pub enabled: bool,
    pub config: toml::Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedChecks {
    checks_by_id: BTreeMap<String, CheckConfig>,
    include_config_files: bool,
}

impl ResolvedChecks {
    pub fn iter(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values()
    }

    pub fn enabled(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values().filter(|check| check.enabled)
    }

    pub fn get(&self, id: &str) -> Option<&CheckConfig> {
        self.checks_by_id.get(id)
    }

    pub fn include_config_files(&self) -> bool {
        self.include_config_files
    }

    fn upsert(&mut self, check: CheckConfig) {
        self.checks_by_id.insert(check.id.clone(), check);
    }
}

#[derive(Debug)]
pub struct ConfigResolver {
    root: PathBuf,
}

impl ConfigResolver {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
        if !root.is_dir() {
            bail!(
                "config resolver root is not a directory: {}",
                root.display()
            );
        }

        Ok(Self { root })
    }

    pub fn resolve_for_file(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;

        let parent_dir = file_path.parent().unwrap_or(Path::new(""));
        let search_dirs = root_to_leaf_dirs(parent_dir)?;

        let mut resolved = ResolvedChecks::default();
        for relative_dir in search_dirs {
            let config_path = self.root.join(relative_dir).join(CHECKS_FILE_NAME);
            if !config_path.exists() {
                continue;
            }

            let checks_file = parse_checks_file(&config_path)?;
            if let Some(include_config_files) = checks_file.settings.include_config_files {
                resolved.include_config_files = include_config_files;
            }
            for check in checks_file.checks {
                let configured_id = check.id;
                resolved.upsert(CheckConfig {
                    check: check.check.unwrap_or_else(|| configured_id.clone()),
                    id: configured_id,
                    enabled: check.enabled,
                    config: check.config,
                });
            }
        }

        Ok(resolved)
    }
}

#[derive(Debug, Deserialize)]
struct ParsedChecksFile {
    #[serde(default)]
    settings: ParsedSettings,
    #[serde(default)]
    checks: Vec<ParsedCheckConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ParsedSettings {
    #[serde(default)]
    include_config_files: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ParsedCheckConfig {
    id: String,
    #[serde(default)]
    check: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default = "empty_toml_table")]
    config: toml::Value,
}

fn parse_checks_file(path: &Path) -> Result<ParsedChecksFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;

    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn enabled_default() -> bool {
    true
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn root_to_leaf_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    validate_relative_path(path)?;

    let mut output = vec![PathBuf::new()];
    let mut current = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            output.push(current.clone());
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

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
}
