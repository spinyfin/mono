use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};
use walkdir::WalkDir;

use crate::input::SourceTree;
use crate::path::validate_relative_path;

pub struct LocalSourceTree {
    root: PathBuf,
}

impl LocalSourceTree {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
        if !root.is_dir() {
            bail!("source tree root is not a directory: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve_checked_path(&self, relative_path: &Path) -> Result<PathBuf> {
        validate_relative_path(relative_path)?;

        let mut current = self.root.clone();
        for component in relative_path.components() {
            if let Component::Normal(part) = component {
                current.push(part);

                if let Ok(metadata) = fs::symlink_metadata(&current) {
                    if metadata.file_type().is_symlink() {
                        let resolved = current.canonicalize().with_context(|| {
                            format!("failed to resolve symlink {}", current.display())
                        })?;
                        if !resolved.starts_with(&self.root) {
                            bail!(
                                "symlink escapes source tree root: {} -> {}",
                                current.display(),
                                resolved.display()
                            );
                        }
                    }
                }
            }
        }

        if let Ok(canonical) = current.canonicalize() {
            if !canonical.starts_with(&self.root) {
                bail!(
                    "resolved path escapes source tree root: {} -> {}",
                    relative_path.display(),
                    canonical.display()
                );
            }
        }

        Ok(current)
    }

    fn path_relative_to_root(&self, path: &Path) -> Result<PathBuf> {
        path.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .with_context(|| format!("path is not under source tree root: {}", path.display()))
    }
}

impl SourceTree for LocalSourceTree {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let path = self.resolve_checked_path(path)?;
        fs::read(&path).with_context(|| format!("failed to read file {}", path.display()))
    }

    fn exists(&self, path: &Path) -> bool {
        let Ok(path) = self.resolve_checked_path(path) else {
            return false;
        };
        fs::metadata(path).is_ok()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let directory_path = self.resolve_checked_path(path)?;
        let entries = fs::read_dir(&directory_path)
            .with_context(|| format!("failed to read directory {}", directory_path.display()))?;

        let mut output = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read directory entry under {}",
                    directory_path.display()
                )
            })?;
            output.push(self.path_relative_to_root(&entry.path())?);
        }

        output.sort();
        Ok(output)
    }

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let candidate = Path::new(pattern);
        if candidate.is_absolute() || pattern.contains("..") {
            bail!("invalid glob pattern: {pattern}");
        }

        let mut glob_builder = GlobSetBuilder::new();
        glob_builder
            .add(Glob::new(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?);
        let glob_set = glob_builder.build().context("failed to build glob set")?;

        let mut matches = Vec::new();
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry.with_context(|| {
                format!(
                    "failed to walk source tree rooted at {}",
                    self.root.display()
                )
            })?;

            if entry.file_type().is_dir() {
                continue;
            }

            let relative_path = self.path_relative_to_root(entry.path())?;
            if glob_set.is_match(&relative_path) {
                matches.push(relative_path);
            }
        }

        matches.sort();
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::LocalSourceTree;
    use crate::input::SourceTree;

    #[test]
    fn read_file_within_root_succeeds() {
        let temp = tempdir().expect("create temp dir");
        let file_path = temp.path().join("foo.txt");
        fs::write(&file_path, b"hello").expect("write file");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let contents = tree.read_file(Path::new("foo.txt")).expect("read file");
        assert_eq!(contents, b"hello");
    }

    #[test]
    fn read_file_rejects_escape_attempts() {
        let temp = tempdir().expect("create temp dir");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        let parent_escape = tree.read_file(Path::new("../outside.txt"));
        assert!(parent_escape.is_err());

        let absolute_escape = tree.read_file(Path::new("/tmp/outside.txt"));
        assert!(absolute_escape.is_err());
    }

    #[test]
    fn missing_paths_are_handled() {
        let temp = tempdir().expect("create temp dir");
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        assert!(!tree.exists(Path::new("missing.txt")));
        assert!(tree.read_file(Path::new("missing.txt")).is_err());
    }

    #[test]
    fn glob_matches_expected_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("src/nested")).expect("create dirs");
        fs::write(temp.path().join("src/lib.rs"), "pub fn x() {}\n").expect("write file");
        fs::write(temp.path().join("src/nested/mod.rs"), "pub mod nested;\n").expect("write file");
        fs::write(temp.path().join("README.md"), "docs\n").expect("write file");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let matches = tree.glob("src/**/*.rs").expect("glob files");

        assert_eq!(
            matches,
            vec![
                Path::new("src/lib.rs").to_path_buf(),
                Path::new("src/nested/mod.rs").to_path_buf()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_root_is_rejected() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("create temp dir");
        let outside = tempdir().expect("create outside dir");

        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, b"secret").expect("write outside file");

        let link_path = temp.path().join("link.txt");
        unix_fs::symlink(&outside_file, &link_path).expect("create symlink");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = tree.read_file(Path::new("link.txt"));
        assert!(result.is_err());
        assert!(!tree.exists(Path::new("link.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_exists_and_is_readable() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("create temp dir");
        let target = temp.path().join("target.txt");
        fs::write(&target, b"safe").expect("write target file");

        let link_path = temp.path().join("link.txt");
        unix_fs::symlink(&target, &link_path).expect("create symlink");

        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        assert!(tree.exists(Path::new("link.txt")));
        let content = tree
            .read_file(Path::new("link.txt"))
            .expect("read through symlink");
        assert_eq!(content, b"safe");
    }
}
