use std::path::{Component, Path};

use anyhow::{Result, bail};

pub fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("absolute paths are not allowed: {}", path.display());
    }

    for component in path.components() {
        match component {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir => {
                bail!("path traversal is not allowed: {}", path.display());
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths are not allowed: {}", path.display());
            }
        }
    }

    Ok(())
}
