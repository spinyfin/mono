use super::*;
use std::io::Write;
use std::process::{Command, Stdio};

fn git_dir(s: &Store) -> PathBuf {
    let marker = s.repo.repo.join(".jj/repo/store/git_target");
    if marker.is_file() {
        marker
            .parent()
            .unwrap()
            .join(std::fs::read_to_string(&marker).unwrap().trim())
    } else {
        marker
    }
}

fn git(s: &Store, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir(s))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn a_colocated_cube_store_removes_git_refs_and_preserves_unpublished_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::colocated(dir.path());
    let s = Store { _dir: dir, repo };
    assert!(s.repo.repo.join(".git").is_dir());
    let work = s.work("main", "colocated.txt");
    let main = s.commit_of("main");
    s.bookmark(&["boss-base/exec_colocated"], &main);
    s.bookmark(&["boss-recovery/exec_colocated"], &work);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    assert_eq!(report.archived[0].commit_id, work);
    s.run(&["git", "import"]);
    assert!(!s.has("boss-base/exec_colocated") && !s.has("boss-recovery/exec_colocated"));
    assert!(s.has(&format!("preserved-archive/exec_colocated/legacy/{work}")));
    let refs = git(&s, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        !refs.contains("boss-base/") && !refs.contains("boss-recovery/"),
        "{refs}"
    );
}

#[tokio::test]
async fn distinct_published_heads_shrink_view_and_git_refs_and_stay_gone_on_import() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::colocated(dir.path());
    let s = Store { _dir: dir, repo };
    let backing_git = git_dir(&s);
    let main = s.commit_of("main");
    // Fast-import builds the many-distinct-heads shape in one fixture write,
    // without thousands of setup operations copying the growing jj view.
    let mut stream = String::new();
    for n in 0..6000 {
        let family = ["boss-base", "boss-recovery", "boss", "pr"][n % 4];
        let name = if family == "pr" {
            format!("pr/{n}")
        } else {
            format!("{family}/exec_distinct_published_head_fixture_{n}")
        };
        stream.push_str(&format!("commit refs/heads/{name}\ncommitter Fixture <fixture@example.invalid> 1 +0000\ndata 5\nwork\nfrom {main}\nM 100644 inline work.txt\ndata {}\n{n}\n\n", n.to_string().len()));
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&backing_git)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stream.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    git(&s, &["pack-refs", "--all"]);
    s.run(&["git", "import"]);
    let heads: Vec<_> = s
        .run(&[
            "log",
            "--no-graph",
            "-r",
            "bookmarks() ~ main",
            "-T",
            "commit_id ++ \"\\n\"",
        ])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(heads.len(), 6000);
    let packed = backing_git.join("packed-refs");
    let before_view = s.view_bytes();
    let before_refs = std::fs::metadata(&packed).unwrap().len();
    assert!(
        before_view > 100_000 && before_refs > 500_000,
        "fixture: view={before_view}, refs={before_refs}"
    );

    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &heads).await.unwrap();
    assert_eq!(report.forgotten, 6000, "{report:?}");
    assert!(report.archived.is_empty() && report.kept.is_empty());
    assert!(
        s.view_bytes() < 100_000,
        "cleanup itself must meet the view target before another import"
    );
    s.run(&["git", "import"]);
    assert_eq!(
        s.names(),
        vec!["main".to_owned()],
        "import must not resurrect retired names"
    );
    let refs = git(&s, &["for-each-ref", "--format=%(refname)"]);
    for prefix in ["boss-base/", "boss-recovery/", "boss/exec_", "pr/"] {
        assert!(!refs.contains(prefix), "git refs survive: {refs}");
    }
    let after_view = s.view_bytes();
    let after_refs = std::fs::metadata(&packed).map_or(0, |m| m.len());
    assert!(
        after_view < 100_000 && after_refs < 500_000,
        "view {before_view} -> {after_view}, packed-refs {before_refs} -> {after_refs}"
    );
    assert_eq!(s.commit_of("heads(all()) ~ ::working_copies() ~ ::main"), "");
    println!(
        "6000 distinct heads: view {before_view} -> {after_view} bytes; packed-refs {before_refs} -> {after_refs} bytes; {} operations",
        report.operations
    );
}

struct MovesBackingRef<'a> {
    store: &'a Store,
    target: &'a str,
}

#[async_trait]
impl Jj for MovesBackingRef<'_> {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"util") {
            git(
                self.store,
                &["update-ref", "refs/heads/boss-base/exec_race", self.target],
            );
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn a_concurrent_backing_ref_change_aborts_the_whole_deletion_batch() {
    let s = Store::new();
    let main = s.commit_of("main");
    let work = s.work("main", "concurrent-ref.txt");
    s.bookmark(&["boss-base/exec_race", "boss-base/exec_other"], &main);
    s.run(&["git", "export"]);
    let jj = MovesBackingRef {
        store: &s,
        target: &work,
    };
    let err = cleanup_legacy_store(&jj, &s.repo.repo, &[]).await.unwrap_err();
    assert!(format!("{err:#}").contains("removing exported legacy Git refs atomically"));
    assert_eq!(git(&s, &["rev-parse", "refs/heads/boss-base/exec_race"]).trim(), work);
    assert_eq!(git(&s, &["rev-parse", "refs/heads/boss-base/exec_other"]).trim(), main);
    assert!(s.has("boss-base/exec_race") && s.has("boss-base/exec_other"));
    s.run(&["git", "import"]);
    assert_eq!(s.commit_of("bookmarks(exact:\"boss-base/exec_race\")"), work);
}

#[tokio::test]
async fn exported_unpublished_work_survives_ref_removal_and_import_in_its_archive() {
    let s = Store::new();
    let work = s.work("main", "unpublished.txt");
    s.bookmark(&["boss-recovery/exec_exported"], &work);
    s.run(&["git", "export"]);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(
        report.archived.len(),
        1,
        "a local Git ref is not publication evidence: {report:?}"
    );
    let archive = format!("preserved-archive/exec_exported/legacy/{work}");
    s.run(&["git", "import"]);
    assert!(!s.has("boss-recovery/exec_exported"));
    assert!(s.has(&archive));
    assert_eq!(s.commit_of(&format!("bookmarks(exact:\"{archive}\")")), work);
    assert!(s.run(&["file", "list", "-r", &work]).contains("unpublished.txt"));
    assert!(!git(&s, &["for-each-ref", "--format=%(refname)"]).contains("boss-recovery/exec_exported"));
}

#[tokio::test]
async fn evolution_predecessor_is_published_but_a_successor_edit_is_not() {
    let s = Store::new();
    let original = s.work("main", "original.txt");
    JjRepo::run(&s.repo.worker, &["describe", "-m", "Content-preserving rewrite"]);
    let published = s.commit_of("worker@");
    assert_ne!(original, published);
    // Bring back the old variant as legacy stores do after concurrent ops.
    s.bookmark(&["boss-recovery/exec_old"], &original);
    std::fs::write(s.repo.worker.join("later.txt"), "later edit").unwrap();
    JjRepo::snapshot(&s.repo.worker);
    let later = s.commit_of("worker@");
    s.bookmark(&["boss-recovery/exec_later"], &later);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[published])
        .await
        .unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    assert_eq!(report.archived[0].commit_id, later);
    assert!(!s.has(&format!("preserved-archive/exec_old/legacy/{original}")));
    assert!(s.has(&format!("preserved-archive/exec_later/legacy/{later}")));
}

#[tokio::test]
async fn a_published_duplicate_vouches_for_its_predecessor_across_change_identities() {
    let s = Store::new();
    let original = s.work("main", "original.txt");
    s.run(&["duplicate", &original]);
    let published = s.commit_of("heads(all()) ~ ::working_copies()");
    assert_ne!(
        s.run(&["log", "--no-graph", "-r", &original, "-T", "change_id"]),
        s.run(&["log", "--no-graph", "-r", &published, "-T", "change_id"])
    );
    assert!(
        s.run(&["evolog", "--no-graph", "-r", &published, "-T", "commit_id ++ \"\\n\""])
            .contains(&original)
    );
    std::fs::write(s.repo.worker.join("later.txt"), "unpublished later edit").unwrap();
    JjRepo::snapshot(&s.repo.worker);
    let later = s.commit_of("worker@");
    // Restore the old variant after the edit; a bookmark created before it
    // would follow the rewrite to `later`, which is deliberately unpublished.
    s.bookmark(&["boss-recovery/exec_original"], &original);
    s.bookmark(&["boss-recovery/exec_later"], &later);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[published])
        .await
        .unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    assert_eq!(report.archived[0].commit_id, later);
    assert!(!s.has(&format!("preserved-archive/exec_original/legacy/{original}")));
}

#[tokio::test]
async fn many_work_holding_names_are_archived_in_batches_after_the_view_shrinks() {
    let s = Store::new();
    let lost = s.work("main", "lost.txt");
    let main = s.commit_of("main");
    let dead: Vec<_> = (0..1200).map(|n| format!("boss-base/exec_dead{n}")).collect();
    s.bookmark(&dead.iter().map(String::as_str).collect::<Vec<_>>(), &main);
    let work: Vec<_> = (0..800).map(|n| format!("boss-recovery/exec_work{n}")).collect();
    s.bookmark(&work.iter().map(String::as_str).collect::<Vec<_>>(), &lost);
    let before = s.operations();
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.archived.len(), 800);
    assert_eq!(report.forgotten, 2000);
    assert_eq!(
        report.operations, 6,
        "three dead-name forgets, one archive import, two source forgets"
    );
    assert_eq!(s.operations(), before + 6);
    assert_eq!(names_in(&s.names(), "preserved-archive/").len(), 800);
}

#[tokio::test]
async fn head_cleanup_keeps_unpublished_ancestry_names_and_working_copies() {
    let s = Store::new();
    let published = s.work("main", "published.txt");
    let unreferenced = s.work("main", "unpublished.txt");
    // An empty connector still holds its unpublished ancestor.
    JjRepo::run(&s.repo.worker, &["new", &unreferenced, "-m", "Empty connector"]);
    let connector = s.commit_of("worker@");
    JjRepo::run(&s.repo.worker, &["new", "main", "-m", "Active working copy"]);
    let active = s.commit_of("worker@");
    s.bookmark(&["keeper"], &published);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, std::slice::from_ref(&published))
        .await
        .unwrap();
    assert_eq!(report.operations, 0, "every head is protected: {report:?}");
    for commit in [&published, &unreferenced, &connector, &active] {
        assert_eq!(s.commit_of(&format!("all() & {commit}")), *commit);
    }
}

fn fast_import(s: &Store, stream: &str) {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir(s))
        .args(["fast-import", "--quiet", "--force"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stream.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn commit_stream(reference: &str, parent: &str, n: usize) -> String {
    format!(
        "commit {reference}\nmark :{}\ncommitter Fixture <fixture@example.invalid> {} +0000\ndata 5\nwork\nfrom {parent}\nM 100644 inline work.txt\ndata {}\n{n}\n\n",
        n + 1,
        n + 1,
        n.to_string().len()
    )
}

#[tokio::test]
async fn hundreds_of_distinct_unpublished_targets_take_a_constant_number_of_operations() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::colocated(dir.path());
    let s = Store { _dir: dir, repo };
    let main = s.commit_of("main");
    let stream: String = (0..300)
        .map(|n| commit_stream(&format!("refs/heads/boss-recovery/exec_distinct{n}"), &main, n))
        .collect();
    fast_import(&s, &stream);
    s.run(&["git", "import"]);
    let before = s.operations();
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.archived.len(), 300, "{report:?}");
    assert_eq!(report.forgotten, 300, "{report:?}");
    assert!(report.operations <= 4, "constant, not one per target: {report:?}");
    assert!(s.operations() - before <= 4);
    assert_eq!(names_in(&s.names(), "preserved-archive/").len(), 300);
    assert!(names_in(&s.names(), "boss-recovery/").is_empty());
    for archive in &report.archived {
        assert_eq!(
            s.commit_of(&format!("bookmarks(exact:\"{}\")", archive.archive)),
            archive.commit_id
        );
    }
}

/// Counts the evolution logs read.
struct CountsEvologs(std::sync::atomic::AtomicUsize);

#[async_trait]
impl Jj for CountsEvologs {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"evolog") {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn deep_published_history_with_unrelated_unpublished_work_reads_a_bounded_number_of_evologs() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    let s = Store { _dir: dir, repo };
    s.run(&["git", "fetch"]);
    let mut stream = String::new();
    for n in 0..400 {
        let parent = if n == 0 { s.commit_of("main") } else { format!(":{n}") };
        stream.push_str(&commit_stream("refs/heads/main", &parent, n));
    }
    fast_import(&s, &stream);
    // The fixture's trunk is the local `main`, which import moves.
    s.run(&["git", "import"]);
    let depth = s
        .run(&["log", "--no-graph", "-r", "::trunk()", "-T", "commit_id ++ \"\\n\""])
        .lines()
        .count();
    assert!(
        depth > 400,
        "trunk depth {depth}: {} {}",
        git(&s, &["rev-list", "--count", "refs/heads/main"]),
        s.run(&["bookmark", "list", "--all-remotes"])
    );
    // Unrelated unpublished work, as an active worker edit would be, keeps the
    // candidate set from ever emptying.
    let busy = s.work("main", "busy.txt");
    s.bookmark(&["boss-recovery/exec_busy"], &busy);
    let jj = CountsEvologs(Default::default());
    let report = cleanup_legacy_store(&jj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    let evologs = jj.0.load(std::sync::atomic::Ordering::SeqCst);
    assert!(evologs <= 5, "trunk history is not walked commit by commit: {evologs}");
}

/// Creates a child of the commit about to be abandoned, as a worker starting
/// on it concurrently would.
struct AddsChild<'a> {
    store: &'a Store,
    parent: &'a str,
}

#[async_trait]
impl Jj for AddsChild<'_> {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"abandon") {
            JjRepo::run(&self.store.repo.worker, &["new", self.parent, "-m", "concurrent child"]);
            std::fs::write(self.store.repo.worker.join("child.txt"), "child").unwrap();
            JjRepo::snapshot(&self.store.repo.worker);
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn a_commit_that_gains_a_child_before_the_abandon_is_not_abandoned() {
    let s = Store::new();
    let published = s.work("main", "published.txt");
    s.bookmark(&["boss/exec_gone"], &published);
    JjRepo::run(&s.repo.worker, &["new", "main", "-m", "elsewhere"]);
    let jj = AddsChild {
        store: &s,
        parent: &published,
    };
    cleanup_legacy_store(&jj, &s.repo.repo, std::slice::from_ref(&published))
        .await
        .unwrap();
    assert_eq!(
        s.commit_of(&format!("all() & {published}")),
        published,
        "parent survives"
    );
    let child = s.commit_of("worker@");
    assert_eq!(s.commit_of(&format!("{child}-")), published, "child was not rebased");
    assert!(s.run(&["file", "list", "-r", &child]).contains("published.txt"));
}
