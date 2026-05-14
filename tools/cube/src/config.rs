use std::path::PathBuf;

use serde::Deserialize;

use crate::app::CubeError;

const DEFAULT_CLONE_COMMAND: &str = "mint";
const DEFAULT_ORG: &str = "linkedin-multiproduct";

pub fn config_dir() -> Result<PathBuf, CubeError> {
    if let Some(path) = std::env::var_os("CUBE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("cube"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        CubeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set",
        ))
    })?;
    Ok(PathBuf::from(home).join(".config").join("cube"))
}

pub fn config_file_path() -> Result<PathBuf, CubeError> {
    Ok(config_dir()?.join("cube.toml"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MultiproductConfig {
    /// Enable mint-based cloning for multiproduct repos. Default: false.
    pub enabled: bool,
    /// The clone command to invoke (e.g. "mint"). Default: "mint".
    pub clone_command: String,
    /// The org slug in the git remote that identifies a multiproduct repo.
    /// Default: "linkedin-multiproduct".
    pub org: String,
}

impl Default for MultiproductConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            clone_command: DEFAULT_CLONE_COMMAND.to_string(),
            org: DEFAULT_ORG.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CubeConfig {
    pub multiproduct: MultiproductConfig,
}

/// Load cube user config from the standard config file path.
/// Returns a default (all-off) config if the file does not exist or the home
/// directory cannot be determined.
pub fn load_config() -> Result<CubeConfig, CubeError> {
    let path = match config_file_path() {
        Ok(p) => p,
        // If we can't determine where config lives (e.g. HOME unset), treat it
        // as absent and return defaults rather than propagating a hard error.
        Err(_) => return Ok(CubeConfig::default()),
    };
    if !path.exists() {
        return Ok(CubeConfig::default());
    }
    let content = std::fs::read_to_string(&path)?;
    toml::from_str(&content).map_err(|e| {
        CubeError::InvalidArgument(format!(
            "failed to parse cube config at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_multiproduct_disabled() {
        let cfg = CubeConfig::default();
        assert!(!cfg.multiproduct.enabled);
        assert_eq!(cfg.multiproduct.clone_command, "mint");
        assert_eq!(cfg.multiproduct.org, "linkedin-multiproduct");
    }

    #[test]
    fn parse_minimal_multiproduct_config() {
        let toml = "[multiproduct]\nenabled = true\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert!(cfg.multiproduct.enabled);
        assert_eq!(cfg.multiproduct.clone_command, "mint");
        assert_eq!(cfg.multiproduct.org, "linkedin-multiproduct");
    }

    #[test]
    fn parse_full_multiproduct_config() {
        let toml = "[multiproduct]\nenabled = true\nclone_command = \"corp-mint\"\norg = \"my-multiproduct\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert!(cfg.multiproduct.enabled);
        assert_eq!(cfg.multiproduct.clone_command, "corp-mint");
        assert_eq!(cfg.multiproduct.org, "my-multiproduct");
    }

    #[test]
    fn load_config_returns_default_when_file_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // CUBE_CONFIG_DIR points to a dir that exists but has no cube.toml
        // SAFETY: test-only; no other threads read this env var concurrently.
        unsafe { std::env::set_var("CUBE_CONFIG_DIR", tmp.path()) };
        let cfg = load_config().expect("load");
        unsafe { std::env::remove_var("CUBE_CONFIG_DIR") };
        assert!(!cfg.multiproduct.enabled);
    }
}
