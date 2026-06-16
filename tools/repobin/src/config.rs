use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::app::RepobinError;

pub const CONFIG_FILE_NAME: &str = "REPOBIN.toml";
const SUPPORTED_VERSION: u32 = 1;
const RESERVED_TOOL_NAME: &str = "repobin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub repo_root: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolConfig>,
    /// Tools pinned to a specific upstream version tag. repobin builds these
    /// from source at the pinned tag rather than at the current checkout's HEAD.
    /// Pinned tools may live in a different repo than the consumer (e.g.
    /// `checkleft` in `spinyfin/mono`, consumed by `flunge`).
    #[serde(default)]
    pub pins: BTreeMap<String, PinConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ToolConfig {
    pub target: String,
}

/// A tool pinned to an upstream git tag. The bazel `target` is NOT declared
/// here — it is read from the pinned checkout's own `REPOBIN.toml` after the
/// tag is resolved and checked out, so renaming the target upstream needs no
/// change in the consuming repo.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PinConfig {
    /// Git URL of the upstream repo that owns the tool's source.
    pub repo: String,
    /// Version tag in the upstream repo (e.g. `checkleft-v0.1.0-alpha.5`).
    pub tag: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), RepobinError> {
        if self.version != SUPPORTED_VERSION {
            return Err(RepobinError::UnsupportedConfigVersion { version: self.version });
        }

        if self.tools.is_empty() && self.pins.is_empty() {
            return Err(RepobinError::InvalidConfig(
                "REPOBIN.toml must define at least one tool or pin".to_string(),
            ));
        }

        for (name, tool) in &self.tools {
            validate_tool_name(name)?;
            if tool.target.trim().is_empty() {
                return Err(RepobinError::InvalidConfig(format!(
                    "tool `{name}` must declare a non-empty Bazel target"
                )));
            }
        }

        for (name, pin) in &self.pins {
            validate_tool_name(name)?;
            if pin.repo.trim().is_empty() {
                return Err(RepobinError::InvalidConfig(format!(
                    "pin `{name}` must declare a non-empty repo URL"
                )));
            }
            if pin.tag.trim().is_empty() {
                return Err(RepobinError::InvalidConfig(format!(
                    "pin `{name}` must declare a non-empty tag"
                )));
            }
        }

        for name in self.tools.keys() {
            if self.pins.contains_key(name) {
                return Err(RepobinError::InvalidConfig(format!(
                    "`{name}` is declared as both a tool and a pin; a name may be one or the other, not both"
                )));
            }
        }

        Ok(())
    }
}

pub fn load_repo_config(start_dir: &Path) -> Result<RepoConfig, RepobinError> {
    let config_path = find_config_path(start_dir).ok_or_else(|| RepobinError::ConfigNotFound {
        start_dir: start_dir.to_path_buf(),
    })?;
    let repo_root = config_path.parent().map(PathBuf::from).ok_or_else(|| {
        RepobinError::InvalidConfig(format!(
            "config path `{}` has no parent directory",
            config_path.display()
        ))
    })?;
    let raw = std::fs::read_to_string(&config_path).map_err(|source| RepobinError::ReadConfig {
        path: config_path.clone(),
        source,
    })?;
    let config: Config = toml::from_str(&raw).map_err(|source| RepobinError::ParseConfig {
        path: config_path.clone(),
        source,
    })?;
    config.validate()?;
    Ok(RepoConfig {
        repo_root,
        config_path,
        config,
    })
}

pub fn find_config_path(start_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    while let Some(path) = current {
        let candidate = path.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = path.parent();
    }
    None
}

fn validate_tool_name(name: &str) -> Result<(), RepobinError> {
    if name.is_empty() {
        return Err(RepobinError::InvalidConfig("tool names must not be empty".to_string()));
    }
    if name == "." || name == ".." {
        return Err(RepobinError::InvalidConfig(format!(
            "tool name `{name}` is not allowed"
        )));
    }
    if name == RESERVED_TOOL_NAME {
        return Err(RepobinError::InvalidConfig(format!(
            "tool name `{RESERVED_TOOL_NAME}` is reserved"
        )));
    }
    if name.contains('/') {
        return Err(RepobinError::InvalidConfig(format!(
            "tool name `{name}` must not contain path separators"
        )));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RepobinError::InvalidConfig(format!(
            "tool name `{name}` may only contain ASCII letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{CONFIG_FILE_NAME, find_config_path, load_repo_config};

    #[test]
    fn find_config_path_prefers_nearest_ancestor() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let nested_repo = root.join("nested");
        let deep = nested_repo.join("a/b/c");
        fs::create_dir_all(&deep).expect("create deep path");
        fs::write(
            root.join(CONFIG_FILE_NAME),
            "version = 1\n[tools.one]\ntarget = \"//:one\"\n",
        )
        .expect("write root config");
        fs::write(
            nested_repo.join(CONFIG_FILE_NAME),
            "version = 1\n[tools.two]\ntarget = \"//:two\"\n",
        )
        .expect("write nested config");

        let found = find_config_path(&deep).expect("config path");
        assert_eq!(found, nested_repo.join(CONFIG_FILE_NAME));
    }

    #[test]
    fn load_repo_config_rejects_reserved_tool_name() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(&path, "version = 1\n[tools.repobin]\ntarget = \"//:tool\"\n").expect("write config");

        let error = load_repo_config(temp.path()).expect_err("reserved name should fail");
        assert!(error.to_string().contains("tool name `repobin` is reserved"));
    }

    #[test]
    fn load_repo_config_parses_pins() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "version = 1\n\
             [tools.boss]\ntarget = \"//tools/boss/cli:boss\"\n\
             [pins.checkleft]\nrepo = \"git@github.com:spinyfin/mono.git\"\ntag = \"checkleft-v0.1.0-alpha.5\"\n",
        )
        .expect("write config");

        let loaded = load_repo_config(temp.path()).expect("config loads");
        assert_eq!(loaded.config.tools.len(), 1);
        let pin = loaded.config.pins.get("checkleft").expect("checkleft pin");
        assert_eq!(pin.repo, "git@github.com:spinyfin/mono.git");
        assert_eq!(pin.tag, "checkleft-v0.1.0-alpha.5");
    }

    #[test]
    fn pin_only_config_is_valid() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "version = 1\n[pins.checkleft]\nrepo = \"https://x.git\"\ntag = \"v1\"\n",
        )
        .expect("write config");
        assert!(load_repo_config(temp.path()).is_ok());
    }

    #[test]
    fn pin_missing_tag_fails_clearly() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "version = 1\n[pins.checkleft]\nrepo = \"https://x.git\"\ntag = \"\"\n",
        )
        .expect("write config");
        let err = load_repo_config(temp.path()).expect_err("empty tag should fail");
        assert!(err.to_string().contains("must declare a non-empty tag"), "got: {err}");
    }

    #[test]
    fn pin_empty_repo_fails_clearly() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(&path, "version = 1\n[pins.checkleft]\nrepo = \"\"\ntag = \"v1\"\n").expect("write config");
        let err = load_repo_config(temp.path()).expect_err("empty repo should fail");
        assert!(
            err.to_string().contains("must declare a non-empty repo URL"),
            "got: {err}"
        );
    }

    #[test]
    fn name_declared_as_both_tool_and_pin_fails() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "version = 1\n\
             [tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n\
             [pins.checkleft]\nrepo = \"https://x.git\"\ntag = \"v1\"\n",
        )
        .expect("write config");
        let err = load_repo_config(temp.path()).expect_err("name collision should fail");
        assert!(err.to_string().contains("both a tool and a pin"), "got: {err}");
    }

    #[test]
    fn empty_config_still_rejected() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(CONFIG_FILE_NAME);
        fs::write(&path, "version = 1\n").expect("write config");
        let err = load_repo_config(temp.path()).expect_err("empty config should fail");
        assert!(err.to_string().contains("at least one tool or pin"), "got: {err}");
    }
}
