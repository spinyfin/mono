//! Claude Code's scratch tree for a workspace.
//!
//! A Claude Code session keeps its scratchpad, background-task output and
//! tool temp files under `<tmp>/claude-<uid>/<slug>/<session>/`, where
//! `<slug>` is the directory the session started in with every character
//! outside `[A-Za-z0-9]` replaced by `-`. A tool the session backgrounds from
//! there (a `nohup` soak script, a private tmux server) has its working
//! directory in that tree rather than in the workspace, so a sweep of the
//! workspace alone never sees it once it has re-parented.
//!
//! The tree is keyed by the workspace path alone and is shared by every
//! session ever started there, so it is only safe to sweep when the workspace
//! itself is being swept: on release and removal, never while a holder is
//! still working.

use std::path::{Path, PathBuf};

/// Where Claude Code roots its per-user temp directory. It honours no
/// `TMPDIR`; `/tmp` resolves to `/private/tmp` on macOS.
const CLAUDE_TMP: &str = "/tmp";

/// The scratch trees of the Claude Code sessions started in `workspace` by
/// this user. Empty when the path cannot name one (relative, not UTF-8, or
/// with no alphanumeric character to tell it from another directory).
pub fn claude_scratchpad_roots(workspace: &Path) -> Vec<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    claude_scratchpad_roots_in(Path::new(CLAUDE_TMP), uid, workspace)
}

/// [`claude_scratchpad_roots`] under an explicit temp directory and user, for
/// tests that must not touch the shared one.
pub fn claude_scratchpad_roots_in(tmp: &Path, uid: u32, workspace: &Path) -> Vec<PathBuf> {
    let base = crate::canonical(tmp).join(format!("claude-{uid}"));
    let mut roots = Vec::new();
    // A session records the directory it was started in as the kernel
    // reports it, which is the resolved path; the caller may hold either.
    for spelling in [workspace.to_path_buf(), crate::canonical(workspace)] {
        // A scratch tree has no scratch tree of its own.
        if spelling.starts_with(&base) {
            continue;
        }
        if let Some(slug) = slug(&spelling) {
            let root = base.join(slug);
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots
}

fn slug(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    // One `-` per UTF-16 code unit, as the JavaScript replacement produces.
    let slug: String = path
        .to_str()?
        .encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect();
    slug.contains(|c: char| c.is_ascii_alphanumeric()).then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_replaces_every_separator_dot_and_non_ascii_unit() {
        assert_eq!(
            slug(Path::new("/Users/dev/.local/share/cube/workspaces/mono-agent-822")).unwrap(),
            "-Users-dev--local-share-cube-workspaces-mono-agent-822"
        );
        assert_eq!(slug(Path::new("/pool/a_b c")).unwrap(), "-pool-a-b-c");
        // U+00E9 is one UTF-16 unit, U+1F600 is two.
        assert_eq!(slug(Path::new("/p/é😀x")).unwrap(), "-p----x");
    }

    #[test]
    fn paths_that_cannot_name_one_workspace_have_no_scratch_tree() {
        assert_eq!(slug(Path::new("relative/ws")), None);
        assert_eq!(slug(Path::new("/")), None);
        assert_eq!(slug(Path::new("/./..")), None);
        assert!(claude_scratchpad_roots(Path::new("ws")).is_empty());
    }

    /// A resolved, empty directory private to one test.
    fn fresh_dir(name: &str) -> PathBuf {
        let base = std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("scratchpad-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn roots_sit_under_the_users_claude_directory_in_the_resolved_temp_dir() {
        let tmp = fresh_dir("tmp");
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&tmp, &link).unwrap();
        let roots = claude_scratchpad_roots_in(&link, 501, Path::new("/pool/mono-agent-007"));
        assert_eq!(roots, vec![tmp.join("claude-501/-pool-mono-agent-007")]);
    }

    #[test]
    fn a_scratch_tree_has_no_scratch_tree_of_its_own() {
        let tmp = fresh_dir("nested");
        let root = claude_scratchpad_roots_in(&tmp, 501, Path::new("/pool/mono-agent-007")).remove(0);
        assert!(claude_scratchpad_roots_in(&tmp, 501, &root).is_empty());
        assert!(claude_scratchpad_roots_in(&tmp, 501, &root.join("session/scratchpad")).is_empty());
    }

    #[test]
    fn a_symlinked_workspace_is_matched_under_both_spellings() {
        let dir = fresh_dir("ws");
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let roots = claude_scratchpad_roots_in(Path::new("/tmp"), 7, &link);
        assert_eq!(roots.len(), 2);
        assert!(roots[0].ends_with(slug(&link).unwrap()));
        assert!(roots[1].ends_with(slug(&real).unwrap()));
    }
}
