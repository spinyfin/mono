//! Real shared-store jj fixtures. Missing declared tooling fails the test.
//!
//! Building the layout through `jj` costs eight process launches (init, two
//! config writes, the baseline description, the `main` bookmark, the trunk
//! alias and two `workspace add`s), and a test binary builds it once per test.
//! Each layout is therefore built once per process into a template directory
//! and copied for every caller. The only location-dependent state a jj
//! workspace holds is the `.jj/repo` pointer file naming the shared store
//! (and, with an origin, the remote URL in the store's git config), so a copy
//! is made usable by rewriting those paths.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub struct JjRepo {
    pub repo: PathBuf,
    pub worker: PathBuf,
    pub replacement: PathBuf,
}

/// Which prebuilt layout a caller copies.
#[derive(Clone, Copy)]
enum Template {
    /// `shared` with one baseline commit on `main` (also `trunk()`), plus the
    /// `worker` and `replacement` workspaces on that commit.
    Plain,
    /// [`Template::Plain`] with an `origin` remote that is the store's own
    /// git repository, and `main` exported to it.
    Origin,
    /// Cube's layout: jj over a colocated `.git` directory.
    Colocated,
}

impl Template {
    fn cell(self) -> &'static OnceLock<PathBuf> {
        static PLAIN: OnceLock<PathBuf> = OnceLock::new();
        static ORIGIN: OnceLock<PathBuf> = OnceLock::new();
        static COLOCATED: OnceLock<PathBuf> = OnceLock::new();
        match self {
            Template::Plain => &PLAIN,
            Template::Origin => &ORIGIN,
            Template::Colocated => &COLOCATED,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Template::Plain => "plain",
            Template::Origin => "origin",
            Template::Colocated => "colocated",
        }
    }
}

impl JjRepo {
    /// A store at `root/shared` whose `main` bookmark (and `trunk()`) is one
    /// baseline commit, with `worker` and `replacement` workspaces on it.
    pub fn new(root: &Path) -> Self {
        Self::from_template(Template::Plain, root)
    }

    /// A shared store backed by a colocated Git directory, as cube creates.
    pub fn colocated(root: &Path) -> Self {
        Self::from_template(Template::Colocated, root)
    }

    /// Install colocated repository metadata while preserving existing fixture files.
    pub fn colocated_into(workspace: &Path) {
        assert!(!workspace.join(".jj").exists());
        assert!(!workspace.join(".git").exists());
        let source = Template::Colocated
            .cell()
            .get_or_init(|| Self::build_template(Template::Colocated));
        for name in [".jj", ".git"] {
            copy_dir(&source.join("shared").join(name), &workspace.join(name));
        }
        Self::rewrite_git_target(workspace);
    }

    fn rewrite_git_target(repo: &Path) {
        let pointer = repo.join(".jj/repo/store/git_target");
        let target = std::fs::read_to_string(&pointer).unwrap();
        if Path::new(target.trim()).is_absolute() {
            std::fs::write(
                pointer,
                std::fs::canonicalize(repo.join(".git"))
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
            )
            .unwrap();
        }
    }

    /// [`JjRepo::new`] plus an `origin` remote that is the store's own git
    /// repository, with `main` exported to it. Fetching from it afterwards
    /// tracks `main@origin`.
    pub fn with_origin(root: &Path) -> Self {
        Self::from_template(Template::Origin, root)
    }

    /// Snapshot a workspace's pending file edits into its working-copy
    /// commit. The cheapest command that does so: `status` also renders the
    /// diff summary.
    pub fn snapshot(workspace: &Path) {
        Self::run(workspace, &["debug", "snapshot"]);
    }

    fn from_template(template: Template, root: &Path) -> Self {
        let source = template.cell().get_or_init(|| Self::build_template(template));
        for name in ["shared", "worker", "replacement"] {
            copy_dir(&source.join(name), &root.join(name));
        }
        let layout = Self::layout(root);
        if matches!(template, Template::Colocated) {
            Self::rewrite_git_target(&layout.repo);
        }
        let store = std::fs::canonicalize(layout.repo.join(".jj/repo")).expect("copied store exists");
        for workspace in [&layout.worker, &layout.replacement] {
            std::fs::write(workspace.join(".jj/repo"), store.as_os_str().as_encoded_bytes()).unwrap();
        }
        if matches!(template, Template::Origin) {
            let config = layout.repo.join(".jj/repo/store/git/config");
            let text = std::fs::read_to_string(&config).unwrap();
            let from = Self::origin_url(source);
            let to = Self::origin_url(root);
            assert!(text.contains(&from), "template origin URL missing from {text}");
            std::fs::write(&config, text.replace(&from, &to)).unwrap();
        }
        layout
    }

    fn layout(root: &Path) -> Self {
        Self {
            repo: root.join("shared"),
            worker: root.join("worker"),
            replacement: root.join("replacement"),
        }
    }

    /// The store's own git repository, as a remote URL. Canonical, because jj
    /// canonicalizes the workspace paths it records and the two must agree
    /// when a test compares store locations.
    fn origin_url(root: &Path) -> String {
        std::fs::canonicalize(root.join("shared/.jj/repo/store/git"))
            .expect("store exists")
            .to_str()
            .unwrap()
            .to_owned()
    }

    /// Build a template once per process. It lives for the rest of the process
    /// under the test's private temporary directory, which the test runner
    /// removes.
    fn build_template(template: Template) -> PathBuf {
        let root = std::env::temp_dir().join(format!("boss-jj-fixture-{}-{}", template.name(), std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        Self::build(&root, matches!(template, Template::Colocated));
        if matches!(template, Template::Origin) {
            let shared = root.join("shared");
            Self::run(&shared, &["git", "remote", "add", "origin", &Self::origin_url(&root)]);
            Self::run(&shared, &["git", "export"]);
        }
        root
    }

    /// Build the layout through `jj`, one process per step.
    fn build(root: &Path, colocated: bool) -> Self {
        let layout = Self::layout(root);
        let repo = &layout.repo;
        let mut init = Command::new(Self::binary());
        // jj 0.34 colocates by default; say which layout this fixture is.
        init.args(if colocated {
            ["git", "init", "--colocate"].as_slice()
        } else {
            ["--config", "git.colocate=false", "git", "init"].as_slice()
        });
        // jj 0.38 reads, and migrates, the configuration of the repository
        // around its working directory. Under Bazel that is the checkout.
        let output = init
            .arg(repo)
            .current_dir(root)
            .output()
            .expect("Bazel must provide jj");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Self::run(repo, &["config", "set", "--repo", "user.name", "Recovery test"]);
        Self::run(
            repo,
            &["config", "set", "--repo", "user.email", "recovery@example.invalid"],
        );
        std::fs::write(repo.join("base.txt"), "baseline\n").unwrap();
        Self::run(repo, &["describe", "-m", "Baseline"]);
        // Workers branch from trunk, so the baseline commit is the trunk commit.
        Self::run(repo, &["bookmark", "create", "main", "-r", "@"]);
        Self::run(repo, &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "main"]);
        for (name, path) in [("worker", &layout.worker), ("replacement", &layout.replacement)] {
            Self::run(
                repo,
                &["workspace", "add", path.to_str().unwrap(), "--name", name, "-r", "@"],
            );
        }
        layout
    }

    /// The declared jj, as an absolute path: commands run inside their
    /// repository, where a runfiles-relative path would not resolve.
    pub fn binary() -> std::ffi::OsString {
        std::fs::canonicalize(std::env::var_os("BOSS_JJ_BIN").expect("Bazel must declare BOSS_JJ_BIN"))
            .expect("the declared jj binary must exist")
            .into_os_string()
    }

    /// Run jj in `repo`. `args` are written for jj 0.26, as in production,
    /// and respelled for the declared jj the same way production does it.
    pub fn run(repo: &Path, args: &[&str]) -> String {
        let binary = Self::binary();
        let version = git_utils::jj_compat::detected_version(&binary).expect("the declared jj must be supported");
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let output = Command::new(binary)
            .args(["--no-pager", "-R"])
            .arg(repo)
            .current_dir(repo)
            .args(git_utils::jj_compat::arguments(version, &args))
            .output()
            .expect("Bazel must provide jj");
        assert!(
            output.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

/// Copy a directory tree, including symlinks as symlinks. A jj store is a few
/// hundred small files, so this is far cheaper than one `jj` launch.
fn copy_dir(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JjRepo;

    fn commit(repo: &std::path::Path, revset: &str) -> String {
        JjRepo::run(repo, &["log", "--no-graph", "-r", revset, "-T", "commit_id"])
    }

    #[test]
    fn copies_point_their_workspaces_at_their_own_store() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = JjRepo::new(first.path());
        let b = JjRepo::new(second.path());
        let store_a = std::fs::canonicalize(a.repo.join(".jj/repo")).unwrap();
        let store_b = std::fs::canonicalize(b.repo.join(".jj/repo")).unwrap();
        assert_ne!(store_a, store_b);
        for (layout, store) in [(&a, &store_a), (&b, &store_b)] {
            for workspace in [&layout.worker, &layout.replacement] {
                let pointer = std::fs::read_to_string(workspace.join(".jj/repo")).unwrap();
                assert_eq!(std::path::Path::new(pointer.trim()), store.as_path());
                // A command in the workspace runs against this store only.
                assert_eq!(commit(workspace, "trunk()"), commit(&layout.repo, "main"));
            }
        }
        // Writing in one copy is invisible to the other.
        std::fs::write(a.worker.join("only-a.txt"), "a").unwrap();
        JjRepo::snapshot(&a.worker);
        assert!(JjRepo::run(&a.repo, &["file", "list", "-r", "worker@"]).contains("only-a.txt"));
        assert!(!JjRepo::run(&b.repo, &["file", "list", "-r", "worker@"]).contains("only-a.txt"));
    }

    #[test]
    fn colocated_copy_preserves_existing_files_and_uses_its_own_git_store() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("existing.txt"), "fixture contents").unwrap();
        JjRepo::colocated_into(first.path());
        JjRepo::colocated_into(second.path());
        assert_eq!(
            std::fs::read_to_string(first.path().join("existing.txt")).unwrap(),
            "fixture contents"
        );
        for workspace in [first.path(), second.path()] {
            let pointer = std::fs::read_to_string(workspace.join(".jj/repo/store/git_target")).unwrap();
            let target = workspace.join(".jj/repo/store").join(pointer.trim());
            assert_eq!(
                target.canonicalize().unwrap(),
                workspace.join(".git").canonicalize().unwrap()
            );
        }
        JjRepo::snapshot(first.path());
        assert!(JjRepo::run(first.path(), &["file", "list"]).contains("existing.txt"));
        assert!(!JjRepo::run(second.path(), &["file", "list"]).contains("existing.txt"));
    }

    #[test]
    fn origin_copies_fetch_from_their_own_store() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::with_origin(dir.path());
        let config = std::fs::read_to_string(repo.repo.join(".jj/repo/store/git/config")).unwrap();
        let own_store = std::fs::canonicalize(repo.repo.join(".jj/repo/store/git")).unwrap();
        assert!(config.contains(own_store.to_str().unwrap()), "{config}");
        assert!(!config.contains("boss-jj-fixture-"), "{config}");
        // `main` was exported before the copy; fetching tracks it as main@origin.
        JjRepo::run(&repo.repo, &["git", "fetch"]);
        assert_eq!(
            commit(&repo.repo, "remote_bookmarks(exact:\"main\", exact:\"origin\")"),
            commit(&repo.repo, "main")
        );
    }
}
