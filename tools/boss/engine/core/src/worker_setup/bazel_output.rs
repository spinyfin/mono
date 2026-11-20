//! Bazel output-root grant for worker permission settings.
//!
//! A Bazel workspace exposes build outputs through `bazel-bin`,
//! `bazel-out` and `bazel-testlogs` symlinks that resolve into Bazel's
//! output base, outside the workspace. Claude Code resolves symlinks
//! before checking its allowed directories, so any Bash command that
//! reads or copies `bazel-bin/...` stops on an interactive permission
//! prompt no human is watching. Granting the output root through
//! `permissions.additionalDirectories` puts those paths inside the
//! allowed set (read and write: workers copy built binaries out).

use std::path::{Path, PathBuf};

/// Bounds `import` / `try-import` recursion in `.bazelrc`.
const MAX_IMPORT_DEPTH: usize = 8;

/// Directories a worker in `workspace` needs access to so paths reached
/// through the Bazel convenience symlinks stay inside the allowed set.
///
/// Empty when `workspace` is not a Bazel workspace. Otherwise exactly one
/// directory, chosen in precedence order:
///
/// 1. `startup --output_base=<dir>` from the workspace `.bazelrc`;
/// 2. `startup --output_user_root=<dir>` from the workspace `.bazelrc`;
/// 3. the output user root the workspace's existing `bazel-out` symlink
///    already points into;
/// 4. Bazel's platform default (`~/Library/Caches/bazel/_bazel_$USER` on
///    macOS, `~/.cache/bazel/_bazel_$USER` elsewhere).
pub(crate) fn bazel_output_directories(workspace: &Path) -> Vec<PathBuf> {
    bazel_output_directories_with(workspace, std::env::var_os("HOME"), std::env::var_os("USER"))
}

fn bazel_output_directories_with(
    workspace: &Path,
    home: Option<std::ffi::OsString>,
    user: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    if !is_bazel_workspace(workspace) {
        return Vec::new();
    }
    let rc = read_startup_options(&workspace.join(".bazelrc"), workspace, 0);
    if let Some(base) = rc.output_base {
        return vec![base];
    }
    if let Some(root) = rc.output_user_root {
        return vec![root];
    }
    if let Some(root) = user_root_from_symlink(workspace) {
        return vec![root];
    }
    default_user_root(home.map(PathBuf::from), user.and_then(|u| u.into_string().ok()))
        .into_iter()
        .collect()
}

fn is_bazel_workspace(workspace: &Path) -> bool {
    ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel", ".bazelrc"]
        .iter()
        .any(|marker| workspace.join(marker).exists())
}

fn default_user_root(home: Option<PathBuf>, user: Option<String>) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute())?;
    let user = user.filter(|u| !u.is_empty())?;
    let dir = format!("_bazel_{user}");
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Caches/bazel").join(dir)
    } else {
        home.join(".cache/bazel").join(dir)
    })
}

/// The output user root behind the workspace's `bazel-out` symlink:
/// `<root>/<hash>/execroot/<ws>/bazel-out` → `<root>`.
fn user_root_from_symlink(workspace: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(workspace.join("bazel-out")).ok()?;
    let execroot = target
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "execroot"))?;
    Some(execroot.parent()?.parent()?.to_path_buf())
}

#[derive(Default)]
struct StartupOptions {
    output_base: Option<PathBuf>,
    output_user_root: Option<PathBuf>,
}

fn read_startup_options(rc: &Path, workspace: &Path, depth: usize) -> StartupOptions {
    let mut out = StartupOptions::default();
    let Ok(text) = std::fs::read_to_string(rc) else {
        return out;
    };
    for line in text.lines() {
        let mut tokens = line.split('#').next().unwrap_or("").split_whitespace();
        match tokens.next() {
            Some("import" | "try-import") if depth < MAX_IMPORT_DEPTH => {
                if let Some(path) = tokens.next().and_then(|p| resolve_rc_path(p, workspace)) {
                    let nested = read_startup_options(&path, workspace, depth + 1);
                    out.output_base = nested.output_base.or(out.output_base);
                    out.output_user_root = nested.output_user_root.or(out.output_user_root);
                }
            }
            Some("startup") => {
                for token in tokens {
                    if let Some(v) = token.strip_prefix("--output_base=") {
                        out.output_base = resolve_rc_path(v, workspace).or(out.output_base);
                    } else if let Some(v) = token.strip_prefix("--output_user_root=") {
                        out.output_user_root = resolve_rc_path(v, workspace).or(out.output_user_root);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Resolve an rc path: `%workspace%` and `~` expand; relative paths are
/// ignored (Bazel itself requires absolute output roots).
fn resolve_rc_path(raw: &str, workspace: &Path) -> Option<PathBuf> {
    let raw = raw.trim_matches(|c| c == '"' || c == '\'');
    let path = if let Some(rest) = raw.strip_prefix("%workspace%") {
        workspace.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(raw)
    };
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_user(workspace: &Path) -> Vec<PathBuf> {
        bazel_output_directories_with(workspace, Some("/home/u".into()), Some("u".into()))
    }

    #[test]
    fn non_bazel_workspace_gets_no_grant() {
        let ws = tempfile::tempdir().unwrap();
        assert!(with_user(ws.path()).is_empty());
    }

    #[test]
    fn default_root_is_per_user_and_platform_specific() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
        let expected = if cfg!(target_os = "macos") {
            "/home/u/Library/Caches/bazel/_bazel_u"
        } else {
            "/home/u/.cache/bazel/_bazel_u"
        };
        assert_eq!(with_user(ws.path()), vec![PathBuf::from(expected)]);
    }

    #[test]
    fn bazelrc_output_user_root_and_output_base_are_honoured() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
        std::fs::write(
            ws.path().join(".bazelrc"),
            "build --foo\nstartup --output_user_root=/data/bz # c\n",
        )
        .unwrap();
        assert_eq!(with_user(ws.path()), vec![PathBuf::from("/data/bz")]);
        std::fs::write(
            ws.path().join(".bazelrc"),
            "startup --output_user_root=/data/bz\nstartup --output_base=/data/base\n",
        )
        .unwrap();
        assert_eq!(with_user(ws.path()), vec![PathBuf::from("/data/base")]);
    }

    #[test]
    fn imported_bazelrc_is_followed() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(".bazelrc"), "try-import %workspace%/ci.bazelrc\n").unwrap();
        std::fs::write(ws.path().join("ci.bazelrc"), "startup --output_user_root=/ci/root\n").unwrap();
        assert_eq!(with_user(ws.path()), vec![PathBuf::from("/ci/root")]);
    }

    #[test]
    fn existing_bazel_out_symlink_names_the_root() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("MODULE.bazel"), "").unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let out = root_path.join("hash/execroot/_main/bazel-out");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&out, ws.path().join("bazel-out")).unwrap();
        assert_eq!(with_user(ws.path()), vec![root_path]);
    }
}
