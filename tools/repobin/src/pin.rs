use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use crate::app::{RepobinError, args_request_json};
use crate::bazel::BazelAdapter;
use crate::cache::{RepoCache, resolve_tag};
use crate::config::{PinConfig, load_repo_config};
use crate::dispatch::{DispatchPlan, plan_from_target};
use crate::lock::{load_lock, update_lock};

/// Build and plan a tool pinned to an upstream version tag.
///
/// Flow (the "build-from-source-at-tag" path):
/// 1. Resolve the tag to a commit SHA — lock-first for reproducibility, falling
///    back to `git ls-remote` and recording the result in `REPOBIN.lock`.
/// 2. Full-clone the upstream repo into a per-SHA cache slot and check out the
///    resolved commit.
/// 3. Read that checkout's own `REPOBIN.toml` for the tool's bazel target.
/// 4. Build it the usual repobin way (bazel build + resolve executable),
///    keyed in the dispatch cache by the SHA-specific checkout path.
///
/// `cache_root` is the repobin cache root (used for both the repo clone and the
/// dispatch cache). `lock_path` is the consuming repo's `REPOBIN.lock`.
#[allow(clippy::too_many_arguments)]
pub fn prepare_pinned_plan<B: BazelAdapter>(
    bazel: &B,
    cache_root: &Path,
    lock_path: &Path,
    tool_name: &str,
    pin: &PinConfig,
    cwd: &Path,
    forwarded_args: &[OsString],
    verbose: bool,
) -> Result<DispatchPlan, RepobinError> {
    let sha = resolve_pinned_sha(lock_path, tool_name, pin)?;

    let cache = RepoCache::for_url(cache_root, &pin.repo);
    let lock = cache.lock()?;
    let outcome = lock.ensure_at_commit(&sha, tool_name, lock_path)?;
    let checkout = lock.cache().commit_checkout_dir(&sha);

    if verbose && !args_request_json(forwarded_args) {
        let head = outcome.head();
        let short = if head.len() >= 7 { &head[..7] } else { head };
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "repobin: running `{tool_name}` from {} at tag {} (resolved {short}; pinned build from source)",
            pin.repo, pin.tag
        );
    }

    // The pinned checkout's REPOBIN.toml is the source of truth for the tool's
    // bazel target, so renaming the target upstream needs no consumer change.
    let upstream = load_repo_config(&checkout)?;
    let tool = upstream
        .config
        .tools
        .get(tool_name)
        .ok_or_else(|| RepobinError::PinnedToolNotInUpstream {
            tool: tool_name.to_string(),
            repo: pin.repo.clone(),
            tag: pin.tag.clone(),
            config_path: upstream.config_path.clone(),
        })?;

    plan_from_target(
        bazel,
        Some(cache_root),
        &checkout,
        tool_name,
        &tool.target,
        cwd,
        forwarded_args,
    )
}

/// Determine the commit SHA a pinned tool should build at.
///
/// Lock-first: if `REPOBIN.lock` already records this tool at the configured
/// repo+tag, that recorded commit is authoritative (deterministic, offline) —
/// re-pinning happens only when the tag in `REPOBIN.toml` changes. Otherwise
/// resolve the tag against the remote and write the result back to the lock.
fn resolve_pinned_sha(lock_path: &Path, tool_name: &str, pin: &PinConfig) -> Result<String, RepobinError> {
    if let Some(lock) = load_lock(lock_path)?
        && let Some(locked) = lock.tools.get(tool_name)
        && locked.repo == pin.repo
        && locked.tag == pin.tag
        && !locked.resolved.trim().is_empty()
    {
        return Ok(locked.resolved.clone());
    }

    let resolved = resolve_tag(tool_name, &pin.repo, &pin.tag)?;
    update_lock(lock_path, tool_name, &pin.repo, &pin.tag, &resolved)?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    use crate::bazel::BazelAdapter;
    use crate::config::PinConfig;
    use crate::lock::{load_lock, lock_path};

    use super::prepare_pinned_plan;

    #[derive(Default)]
    struct FakeBazel {
        builds: RefCell<Vec<(PathBuf, String)>>,
        executable: PathBuf,
    }

    impl BazelAdapter for FakeBazel {
        fn build(&self, repo_root: &Path, target: &str) -> Result<(), crate::app::RepobinError> {
            self.builds
                .borrow_mut()
                .push((repo_root.to_path_buf(), target.to_string()));
            Ok(())
        }

        fn resolve_executable(&self, _repo_root: &Path, _target: &str) -> Result<PathBuf, crate::app::RepobinError> {
            Ok(self.executable.clone())
        }

        fn resolve_source_files(
            &self,
            _repo_root: &Path,
            _target: &str,
        ) -> Result<Vec<PathBuf>, crate::app::RepobinError> {
            Ok(vec![])
        }
    }

    fn git(args: &[&str], dir: &Path) {
        Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
    }

    /// Build a bare remote whose tree carries the given REPOBIN.toml, with a
    /// lightweight tag at HEAD. Returns (remote_url, tag, head_sha).
    fn remote_with_config_and_tag(temp: &TempDir, repobin_toml: &str, tag: &str) -> (String, String, String) {
        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        Command::new("git")
            .args(["clone"])
            .arg(&remote)
            .arg(&work)
            .output()
            .unwrap();
        std::fs::write(work.join("REPOBIN.toml"), repobin_toml).unwrap();
        git(&["add", "."], &work);
        git(&["commit", "-m", "release"], &work);
        let sha = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&work)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        Command::new("git")
            .args(["tag", tag])
            .current_dir(&work)
            .output()
            .unwrap();
        git(&["push", "origin", "HEAD:main"], &work);
        Command::new("git")
            .args(["push", "origin", &format!("refs/tags/{tag}")])
            .current_dir(&work)
            .output()
            .unwrap();
        (format!("file://{}", remote.display()), tag.to_string(), sha)
    }

    #[test]
    fn resolves_builds_tagged_version_and_writes_lock() {
        let temp = TempDir::new().unwrap();
        let (url, tag, sha) = remote_with_config_and_tag(
            &temp,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
            "checkleft-v1",
        );
        let cache_root = temp.path().join("cache");
        let lock_file = lock_path(&temp.path().join("consumer"));
        std::fs::create_dir_all(lock_file.parent().unwrap()).unwrap();

        let bazel = FakeBazel {
            executable: temp.path().join("fake-exe"),
            ..FakeBazel::default()
        };
        let pin = PinConfig {
            repo: url.clone(),
            tag: tag.clone(),
        };

        let plan = prepare_pinned_plan(
            &bazel,
            &cache_root,
            &lock_file,
            "checkleft",
            &pin,
            temp.path(),
            &[],
            false,
        )
        .expect("pinned plan");

        // Built the upstream target from the SHA-specific checkout.
        assert_eq!(plan.target, "//tools/checkleft:checkleft");
        assert!(plan.repo_root.to_string_lossy().contains(&sha));
        let builds = bazel.builds.borrow();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].1, "//tools/checkleft:checkleft");
        assert!(
            builds[0].0.to_string_lossy().contains(&sha),
            "must build at the tag's commit"
        );

        // Lock records the resolved version so it is reproducible/verifiable.
        let lock = load_lock(&lock_file).unwrap().unwrap();
        let entry = lock.tools.get("checkleft").expect("lock entry");
        assert_eq!(entry.tag, "checkleft-v1");
        assert_eq!(entry.resolved, sha);
        assert_eq!(entry.repo, url);
    }

    #[test]
    fn lock_is_authoritative_on_second_call() {
        let temp = TempDir::new().unwrap();
        let (url, tag, sha) = remote_with_config_and_tag(
            &temp,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
            "checkleft-v1",
        );
        let cache_root = temp.path().join("cache");
        let lock_file = lock_path(&temp.path().join("consumer"));
        std::fs::create_dir_all(lock_file.parent().unwrap()).unwrap();

        let bazel = FakeBazel {
            executable: temp.path().join("fake-exe"),
            ..FakeBazel::default()
        };
        let pin = PinConfig { repo: url, tag };

        prepare_pinned_plan(
            &bazel,
            &cache_root,
            &lock_file,
            "checkleft",
            &pin,
            temp.path(),
            &[],
            false,
        )
        .unwrap();
        let first = load_lock(&lock_file).unwrap().unwrap();

        prepare_pinned_plan(
            &bazel,
            &cache_root,
            &lock_file,
            "checkleft",
            &pin,
            temp.path(),
            &[],
            false,
        )
        .unwrap();
        let second = load_lock(&lock_file).unwrap().unwrap();

        assert_eq!(first, second, "lock must be stable across calls");
        assert_eq!(second.tools["checkleft"].resolved, sha);
    }

    #[test]
    fn bad_tag_fails_clearly() {
        let temp = TempDir::new().unwrap();
        let (url, _, _) = remote_with_config_and_tag(
            &temp,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
            "checkleft-v1",
        );
        let cache_root = temp.path().join("cache");
        let lock_file = lock_path(temp.path());

        let bazel = FakeBazel::default();
        let pin = PinConfig {
            repo: url,
            tag: "checkleft-v-does-not-exist".to_string(),
        };
        let err = prepare_pinned_plan(
            &bazel,
            &cache_root,
            &lock_file,
            "checkleft",
            &pin,
            temp.path(),
            &[],
            false,
        )
        .expect_err("missing tag must fail");
        match err {
            crate::app::RepobinError::TagNotFound { tool, tag, .. } => {
                assert_eq!(tool, "checkleft");
                assert_eq!(tag, "checkleft-v-does-not-exist");
            }
            other => panic!("expected TagNotFound, got {other:?}"),
        }
        assert!(bazel.builds.borrow().is_empty(), "no build should run for a bad tag");
    }

    #[test]
    fn upstream_missing_tool_fails_clearly() {
        let temp = TempDir::new().unwrap();
        let (url, tag, _) = remote_with_config_and_tag(
            &temp,
            "version = 1\n[tools.boss]\ntarget = \"//tools/boss/cli:boss\"\n",
            "checkleft-v1",
        );
        let cache_root = temp.path().join("cache");
        let lock_file = lock_path(temp.path());

        let bazel = FakeBazel::default();
        let pin = PinConfig { repo: url, tag };
        let err = prepare_pinned_plan(
            &bazel,
            &cache_root,
            &lock_file,
            "checkleft",
            &pin,
            temp.path(),
            &[],
            false,
        )
        .expect_err("upstream missing tool must fail");
        assert!(
            matches!(err, crate::app::RepobinError::PinnedToolNotInUpstream { .. }),
            "got: {err:?}"
        );
    }
}
