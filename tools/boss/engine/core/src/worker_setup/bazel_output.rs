//! Resolve a narrow Bazel output grant for local autonomous workers.
use bazel_support::{cache_root, is_output_base_dir_name, logical_bazelrc_lines, shell_split};
use std::path::{Component, Path, PathBuf};

const MAX_IMPORT_DEPTH: usize = 8;

/// Return at most one grant: explicit base, explicit user root, existing
/// output user root (or custom base), then the environment-aware per-user default.
pub(crate) fn bazel_output_directory(workspace: &Path) -> Option<PathBuf> {
    bazel_output_directory_with(
        workspace,
        std::env::var("HOME").ok().as_deref(),
        std::env::var("USER").ok().as_deref(),
        std::env::var("TEST_TMPDIR").ok().as_deref(),
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
    )
}

fn bazel_output_directory_with(
    workspace: &Path,
    home: Option<&str>,
    user: Option<&str>,
    test_tmpdir: Option<&str>,
    xdg: Option<&str>,
) -> Option<PathBuf> {
    if !["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel", ".bazelrc"]
        .iter()
        .any(|m| workspace.join(m).exists())
    {
        return None;
    }
    let rc = read_startup_options(&workspace.join(".bazelrc"), workspace, home, 0);
    let root = rc
        .output_base
        .or(rc.output_user_root)
        .or_else(|| output_base_from_symlink(workspace))
        .or_else(|| {
            let user = user.filter(|u| !u.is_empty() && !u.contains('/') && *u != "." && *u != "..")?;
            Some(cache_root(test_tmpdir, home, xdg, cfg!(target_os = "macos"))?.join(format!("_bazel_{user}")))
        })?;
    let root = normalize(&root)?;
    let home = home.and_then(|h| normalize(Path::new(h)));
    (root.parent().is_some() && !home.as_ref().is_some_and(|h| h.starts_with(&root))).then_some(root)
}

/// Resolve existing ancestors too, so a missing leaf below a symlink cannot
/// disguise a whole-home or filesystem-root grant.
fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
            _ => {}
        }
        if let Ok(canonical) = std::fs::canonicalize(&normalized) {
            normalized = canonical;
        }
    }
    Some(normalized)
}

fn output_base_from_symlink(workspace: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(workspace.join("bazel-out")).ok()?;
    output_base_from_target(&target)
}

fn output_base_from_target(target: &Path) -> Option<PathBuf> {
    let execroot = target
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "execroot"))?;
    let base = execroot.parent()?;
    let is_hash = base
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_output_base_dir_name);
    if is_hash
        && let Some(parent) = base.parent()
        && parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("_bazel_"))
    {
        return Some(parent.to_path_buf());
    }
    Some(base.to_path_buf())
}

#[derive(Default)]
struct StartupOptions {
    output_base: Option<PathBuf>,
    output_user_root: Option<PathBuf>,
}

fn read_startup_options(rc: &Path, workspace: &Path, home: Option<&str>, depth: usize) -> StartupOptions {
    let mut out = StartupOptions::default();
    let Ok(text) = std::fs::read_to_string(rc) else {
        return out;
    };
    let Ok(lines) = logical_bazelrc_lines(&text) else {
        return out;
    };
    for (_, line) in lines {
        let Ok(tokens) = shell_split(&line) else {
            continue;
        };
        let mut tokens = tokens.iter().map(String::as_str);
        match tokens.next() {
            Some("import" | "try-import") if depth < MAX_IMPORT_DEPTH => {
                if let Some(path) = tokens.next().and_then(|p| resolve_rc_path(p, workspace, home)) {
                    let nested = read_startup_options(&path, workspace, home, depth + 1);
                    out.output_base = nested.output_base.or(out.output_base);
                    out.output_user_root = nested.output_user_root.or(out.output_user_root);
                }
            }
            Some("startup") => {
                while let Some(token) = tokens.next() {
                    let (flag, value) = match token.split_once('=') {
                        Some(pair) => pair,
                        None if matches!(token, "--output_base" | "--output_user_root") => {
                            let Some(value) = tokens.next() else {
                                break;
                            };
                            (token, value)
                        }
                        None => continue,
                    };
                    match flag {
                        "--output_base" => {
                            out.output_base = resolve_rc_path(value, workspace, home).or(out.output_base)
                        }
                        "--output_user_root" => {
                            out.output_user_root = resolve_rc_path(value, workspace, home).or(out.output_user_root)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn resolve_rc_path(raw: &str, workspace: &Path, home: Option<&str>) -> Option<PathBuf> {
    let path = if let Some(rest) = raw.strip_prefix("%workspace%") {
        workspace.join(rest.trim_start_matches('/'))
    } else if raw == "~" {
        PathBuf::from(home?)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(home?).join(rest)
    } else {
        PathBuf::from(raw)
    };
    path.is_absolute().then_some(path)
}

#[cfg(test)]
#[path = "bazel_output_tests.rs"]
mod tests;
