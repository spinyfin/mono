use super::*;
use boss_engine_test_git::jj::JjRepo;
use std::sync::Mutex;

#[path = "legacy_cleanup_acceptance_tests.rs"]
mod acceptance;

struct Store {
    _dir: tempfile::TempDir,
    repo: JjRepo,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::new(dir.path());
        Self { _dir: dir, repo }
    }

    /// Published history: `main`, fetched from the store's own `origin`.
    fn with_origin() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::with_origin(dir.path());
        JjRepo::run(&repo.repo, &["git", "fetch"]);
        Self { _dir: dir, repo }
    }

    fn run(&self, args: &[&str]) -> String {
        JjRepo::run(&self.repo.repo, args)
    }

    fn commit_of(&self, revset: &str) -> String {
        self.run(&["log", "--no-graph", "-r", revset, "-T", "commit_id"])
            .trim()
            .to_owned()
    }

    /// A non-empty commit on `parent`; every call is independent work.
    fn work(&self, parent: &str, file: &str) -> String {
        JjRepo::run(&self.repo.worker, &["new", parent, "-m", file]);
        std::fs::write(self.repo.worker.join(file), file).unwrap();
        JjRepo::snapshot(&self.repo.worker);
        JjRepo::run(&self.repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .trim()
            .to_owned()
    }

    fn bookmark(&self, names: &[&str], commit: &str) {
        let mut args = vec!["bookmark", "create"];
        args.extend(names);
        args.extend(["-r", commit]);
        self.run(&args);
    }

    fn names(&self) -> Vec<String> {
        self.run(&["bookmark", "list", "-T", "name ++ \"\\n\""])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn operations(&self) -> usize {
        self.run(&["op", "log", "--no-graph", "-T", "id ++ \"\\n\""])
            .lines()
            .count()
    }

    fn has(&self, name: &str) -> bool {
        self.names().iter().any(|n| n == name)
    }

    /// Size of the view of the newest operation: what every jj command in
    /// every workspace of the store loads.
    fn view_bytes(&self) -> u64 {
        let views = self.repo.repo.join(".jj/repo/op_store/views");
        std::fs::read_dir(views)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .max_by_key(|meta| meta.modified().unwrap())
            .unwrap()
            .len()
    }
}

fn names_in(names: &[String], prefix: &str) -> Vec<String> {
    names.iter().filter(|n| n.starts_with(prefix)).cloned().collect()
}

#[tokio::test]
async fn forgets_what_holds_nothing_unpublished_and_archives_what_does() {
    let s = Store::new();
    let main = s.commit_of("main");
    let pushed = s.work("main", "pushed.txt");
    let lost = s.work("main", "lost.txt");
    s.bookmark(&["boss-base/exec_a", "pr/7"], &main);
    s.bookmark(&["boss-recovery/exec_a", "boss/exec_a"], &pushed);
    s.bookmark(&["boss-base/exec_b"], &main);
    s.bookmark(&["boss-recovery/exec_b", "boss/exec_b", "pr/8"], &lost);
    // Names this cleanup does not own, including near misses.
    s.bookmark(
        &[
            "keeper",
            "preserved/exec_a",
            "boss/feature",
            "boss-base/not-an-exec",
            "pr/abc",
        ],
        &lost,
    );
    let before = s.operations();

    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, std::slice::from_ref(&pushed))
        .await
        .unwrap();

    let archive = format!("preserved-archive/exec_b/legacy/{lost}");
    assert_eq!(report.found, 8);
    assert_eq!(report.forgotten, 6, "{report:?}");
    assert_eq!(
        report.archived,
        vec![LegacyArchive {
            execution_id: "exec_b".into(),
            source: "boss-recovery/exec_b".into(),
            archive: archive.clone(),
            commit_id: lost.clone(),
        }]
    );
    assert_eq!(report.kept, vec!["boss/exec_b".to_owned(), "pr/8".to_owned()]);
    assert_eq!(
        report.operations, 4,
        "two forgets, one archive and one published head hidden: {report:?}"
    );
    assert_eq!(s.operations(), before + 4);
    let names = s.names();
    for gone in [
        "boss-base/exec_a",
        "boss-recovery/exec_a",
        "boss/exec_a",
        "pr/7",
        "boss-base/exec_b",
        "boss-recovery/exec_b",
    ] {
        assert!(!names.contains(&gone.to_owned()), "{gone} forgotten: {names:?}");
    }
    for kept in [
        "boss/exec_b",
        "pr/8",
        "keeper",
        "preserved/exec_a",
        "boss/feature",
        "boss-base/not-an-exec",
        "pr/abc",
        "main",
        archive.as_str(),
    ] {
        assert!(names.contains(&kept.to_owned()), "{kept} survives: {names:?}");
    }
    assert_eq!(s.commit_of(&format!("bookmarks(exact:\"{archive}\")")), lost);
}

#[tokio::test]
async fn work_published_to_a_remote_needs_no_archive() {
    let s = Store::with_origin();
    let pushed = s.work("main", "pushed.txt");
    s.bookmark(&["boss/exec_open"], &pushed);
    JjRepo::run(&s.repo.worker, &["git", "export"]);
    s.run(&["git", "fetch"]);
    assert!(
        s.run(&["bookmark", "list", "--all-remotes", "boss/exec_open"])
            .contains("@origin"),
        "the branch has a remote twin"
    );
    s.bookmark(&["boss-recovery/exec_open", "boss-base/exec_open", "pr/9"], &pushed);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert!(report.archived.is_empty(), "{report:?}");
    assert_eq!(report.forgotten, 3, "{report:?}");
    assert_eq!(report.kept, vec!["boss/exec_open".to_owned()]);
    assert!(
        s.has("boss/exec_open"),
        "a branch that exists on the remote is a live PR"
    );
    assert!(!s.has("boss-recovery/exec_open"));
}

#[tokio::test]
async fn a_conflicted_bookmark_with_one_unpublished_side_is_archived_whole() {
    let s = Store::new();
    let published = s.work("main", "published.txt");
    let lost = s.work("main", "lost.txt");
    let base = s.run(&["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for commit in [&published, &lost] {
        s.run(&[
            "--at-op",
            base.trim(),
            "bookmark",
            "create",
            "boss-recovery/exec_c",
            "-r",
            commit,
        ]);
    }
    let conflicted = s.run(&[
        "bookmark",
        "list",
        "boss-recovery/exec_c",
        "-T",
        "if(conflict, \"conflict\", \"ok\")",
    ]);
    assert_eq!(conflicted.trim(), "conflict", "fixture must conflict");

    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, std::slice::from_ref(&published))
        .await
        .unwrap();

    assert_eq!(report.forgotten, 1);
    let mut archived: Vec<_> = report.archived.iter().map(|a| a.commit_id.clone()).collect();
    archived.sort();
    let mut expected = vec![published.clone(), lost.clone()];
    expected.sort();
    assert_eq!(archived, expected, "every side is kept");
    for commit in [&published, &lost] {
        assert!(s.has(&format!("preserved-archive/exec_c/legacy/{commit}")));
    }
    assert!(!s.has("boss-recovery/exec_c"));
}

#[tokio::test]
async fn only_exact_commit_ids_vouch_for_work() {
    let s = Store::new();
    let lost = s.work("main", "lost.txt");
    s.bookmark(&["boss-recovery/exec_d"], &lost);
    // A revset fragment, an abbreviation and a hash from another store all
    // vouch for nothing, and the fragment must not be evaluated as a revset.
    let vouchers = vec![
        "main) | all(".to_owned(),
        lost[..12].to_owned(),
        "f".repeat(40),
        lost.to_uppercase(),
    ];
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &vouchers).await.unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    assert_eq!(report.forgotten, 1);
    assert!(s.has(&format!("preserved-archive/exec_d/legacy/{lost}")));
}

#[tokio::test]
async fn thousands_of_names_take_a_handful_of_operations() {
    let s = Store::new();
    let main = s.commit_of("main");
    let pushed = s.work("main", "pushed.txt");
    let names: Vec<String> = (0..450)
        .flat_map(|n| {
            [
                format!("boss-base/exec_{n}"),
                format!("boss-recovery/exec_{n}"),
                format!("boss/exec_{n}"),
            ]
        })
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    s.bookmark(&refs[..refs.len() / 2], &main);
    s.bookmark(&refs[refs.len() / 2..], &pushed);
    let before = s.operations();
    let view_before = s.view_bytes();

    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, std::slice::from_ref(&pushed))
        .await
        .unwrap();

    assert_eq!(report.found, 1350);
    assert_eq!(report.forgotten, 1350, "{report:?}");
    assert!(report.archived.is_empty() && report.kept.is_empty());
    assert_eq!(report.operations, 4, "1350 names in batches of 400");
    assert_eq!(s.operations(), before + 4);
    assert_eq!(names_in(&s.names(), "boss"), Vec::<String>::new());
    let view_after = s.view_bytes();
    assert!(
        view_after < 10_000 && view_after * 10 < view_before,
        "the view shrinks from {view_before} to {view_after} bytes"
    );
}

#[tokio::test]
async fn a_second_run_changes_nothing() {
    let s = Store::new();
    let lost = s.work("main", "lost.txt");
    s.bookmark(&["boss-recovery/exec_e", "boss/exec_e"], &lost);
    cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    let before = s.operations();
    let names = s.names();
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.forgotten, 0, "{report:?}");
    assert_eq!(
        report.archived.len(),
        1,
        "existing archives are reported for crash-safe inspection filing"
    );
    assert_eq!(report.archived[0].commit_id, lost);
    assert_eq!(report.kept, vec!["boss/exec_e".to_owned()]);
    assert_eq!(report.operations, 0);
    assert_eq!(s.operations(), before);
    assert_eq!(s.names(), names);
}

/// Moves a legacy bookmark after the first evaluation, as a still-running
/// worker of an earlier engine would.
struct MovesBookmark {
    repo: PathBuf,
    target: String,
    listings: Mutex<usize>,
    on_forget: bool,
    second_target: Option<String>,
}

#[async_trait]
impl Jj for MovesBookmark {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"set")
            && let Some(target) = &self.second_target
        {
            LocalJj
                .run(&self.repo, &["bookmark", "set", "boss-recovery/exec_f", "-r", target])
                .await?;
        }
        if args.contains(&"--all-remotes") || (self.on_forget && args.contains(&"forget")) {
            let listing = {
                let mut listings = self.listings.lock().unwrap();
                *listings += 1;
                *listings
            };
            if (!self.on_forget && listing == 2) || (self.on_forget && args.contains(&"forget")) {
                LocalJj
                    .run(
                        &self.repo,
                        &[
                            "bookmark",
                            "set",
                            "boss-recovery/exec_f",
                            "-r",
                            &self.target,
                            "--allow-backwards",
                        ],
                    )
                    .await?;
            }
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn a_name_that_moved_during_the_cleanup_is_left_in_place() {
    let s = Store::new();
    let main = s.commit_of("main");
    let new_work = s.work("main", "new.txt");
    s.bookmark(&["boss-recovery/exec_f"], &main);
    let jj = MovesBookmark {
        repo: s.repo.repo.clone(),
        target: new_work.clone(),
        listings: Mutex::new(0),
        on_forget: false,
        second_target: None,
    };
    let report = cleanup_legacy_store(&jj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.forgotten, 0, "{report:?}");
    assert_eq!(report.kept, vec!["boss-recovery/exec_f".to_owned()]);
    assert_eq!(s.commit_of("bookmarks(exact:\"boss-recovery/exec_f\")"), new_work);
}

#[tokio::test]
async fn a_move_after_the_last_recheck_survives_the_forget_operation_merge() {
    let s = Store::new();
    let main = s.commit_of("main");
    let work = s.work("main", "concurrent.txt");
    s.bookmark(&["boss-recovery/exec_f"], &main);
    let jj = MovesBookmark {
        repo: s.repo.repo.clone(),
        target: work.clone(),
        listings: Mutex::new(0),
        on_forget: true,
        second_target: None,
    };
    let report = cleanup_legacy_store(&jj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.forgotten, 0, "{report:?}");
    assert_eq!(report.kept, vec!["boss-recovery/exec_f".to_owned()]);
    assert_eq!(s.commit_of("bookmarks(exact:\"boss-recovery/exec_f\")"), work);
    assert_eq!(
        s.run(&[
            "log",
            "--no-graph",
            "-r",
            "bookmarks(exact:\"boss-recovery/exec_f\")",
            "-T",
            "if(conflict, \"conflict\", \"clean\")"
        ]),
        "clean",
        "the merge of a forget and a concurrent move must not leave a conflicted name"
    );
    let listing = s.run(&["bookmark", "list", "exec_f", "boss-recovery/exec_f"]);
    assert!(!listing.contains("conflicted"), "{listing}");
}

#[tokio::test]
async fn an_archive_the_restore_path_made_is_not_reported_as_legacy_work() {
    let s = Store::new();
    let lost = s.work("main", "lost.txt");
    // Created by dispatch's normal restore, not by this cleanup.
    s.bookmark(&[&format!("preserved-archive/exec_normal/{lost}")], &lost);
    s.bookmark(&["boss-recovery/exec_legacy"], &lost);
    let report = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(report.archived.len(), 1, "{report:?}");
    assert_eq!(report.archived[0].execution_id, "exec_legacy");
    // A retry reports only the legacy archive, once.
    let again = cleanup_legacy_store(&LocalJj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(again.archived.len(), 1, "{again:?}");
    assert_eq!(
        again.archived[0].archive,
        format!("preserved-archive/exec_legacy/legacy/{lost}")
    );
    assert!(s.has(&format!("preserved-archive/exec_normal/{lost}")));
}

#[tokio::test]
async fn a_second_move_immediately_before_conflict_repair_is_not_rolled_back() {
    let s = Store::new();
    let main = s.commit_of("main");
    let first = s.work("main", "first.txt");
    let latest = s.work(&first, "latest.txt");
    s.bookmark(&["boss-recovery/exec_f"], &main);
    let jj = MovesBookmark {
        repo: s.repo.repo.clone(),
        target: first,
        listings: Mutex::new(0),
        on_forget: true,
        second_target: Some(latest.clone()),
    };
    cleanup_legacy_store(&jj, &s.repo.repo, &[]).await.unwrap();
    assert_eq!(s.commit_of("bookmarks(exact:\"boss-recovery/exec_f\")"), latest);
    let listing = s.run(&["bookmark", "list", "boss-recovery/exec_f"]);
    assert!(!listing.contains("conflicted"), "{listing}");
}
