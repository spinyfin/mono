use super::preserved_restore::{select_tip, variant_archive_name};
use super::preserved_tests::fixture;
use super::*;
use boss_engine_test_git::jj::JjRepo;

struct DivergentViews(std::sync::atomic::AtomicUsize);

#[async_trait]
impl Jj for DivergentViews {
    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
    async fn run(&self, _: &Path, args: &[&str]) -> Result<String> {
        if args.iter().any(|arg| arg.contains("unsafe/*")) {
            return Ok(String::new());
        }
        if args.contains(&"--at-op") {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok("a\nb\n".into());
        }
        if args.contains(&"op") {
            return Ok((0..2_000).map(|n| format!("{n}.000000000 op{n}\n")).collect());
        }
        if args.contains(&"evolog") || args.iter().any(|arg| arg.contains("::base")) {
            return Ok(String::new());
        }
        if args.contains(&"change_id ++ \"\\n\"") {
            return Ok("same\nsame\n".into());
        }
        Ok("a\nb\n".into())
    }
}

#[tokio::test]
async fn divergent_selection_exhausts_with_integrity_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_divergent_cap", "local")
        .await
        .unwrap();
    let jj = DivergentViews(Default::default());
    let error = select_tip(&jj, &record, "base", None).await.unwrap_err();
    assert!(is_pointer_integrity_error(&error), "{error}");
    assert!(error.to_string().contains("exhausted"), "{error}");
    assert_eq!(
        jj.0.load(std::sync::atomic::Ordering::SeqCst),
        preserved::OP_VIEW_LOAD_LIMIT
    );
}

#[tokio::test]
async fn ancestor_selection_discloses_unrelated_variant_across_retries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_ancestor_variant", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work"), "unpublished").unwrap();
    let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    let pointer = preserved_name(&record.execution_id);
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &pointer, "-r", "main", "--allow-backwards"],
    );
    // Concurrently rewrite the original unpublished target. Merging this
    // operation with the move to main produces two different change identities.
    JjRepo::run(
        &repo.repo,
        &["--at-op", &op, "describe", &saved.commit_id, "-m", "Variant work"],
    );
    let identities = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &pointer, "-T", "change_id ++ \"\\n\""],
    );
    assert_eq!(identities.lines().collect::<std::collections::HashSet<_>>().len(), 2);
    JjRepo::run(&repo.replacement, &["new", "main"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, None)
        .await
        .unwrap()
        .unwrap();
    assert!(report.unrestaged_unpublished.contains("variants/"), "{report:?}");
    let retry = restore_preserved(&LocalJj, &record, &repo.replacement, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.unrestaged_unpublished, retry.unrestaged_unpublished);
}

#[tokio::test]
async fn retry_reports_real_archive_name_after_shared_ancestor_rebase() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_archive_retry", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("root"), "shared unpublished root").unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Selected"]);
    std::fs::write(repo.worker.join("selected"), "selected").unwrap();
    let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    let operation = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for title in ["Selected variant", "Archived variant"] {
        JjRepo::run(
            &repo.repo,
            &["--at-op", &operation, "describe", &saved.commit_id, "-m", title],
        );
    }
    let pointer = preserved_name(&record.execution_id);
    let variants = JjRepo::run(
        &repo.repo,
        &[
            "log",
            "--no-graph",
            "-r",
            &pointer,
            "-T",
            "commit_id ++ \" \" ++ description.first_line() ++ \"\\n\"",
        ],
    );
    let selected = variants
        .lines()
        .find_map(|line| line.strip_suffix(" Selected variant"))
        .unwrap();
    let old = variants
        .lines()
        .find_map(|line| line.strip_suffix(" Archived variant"))
        .unwrap();
    assert_ne!(selected, old);
    let identities = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &pointer, "-T", "change_id ++ \"\\n\""],
    );
    assert_eq!(identities.lines().collect::<std::collections::HashSet<_>>().len(), 1);
    let archive = variant_archive_name(&record.execution_id, old.trim());
    JjRepo::run(&repo.repo, &["bookmark", "set", &archive, "-r", old.trim()]);
    // A retry sees a single-target pointer after the alternatives were archived.
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &pointer, "-r", selected, "--allow-backwards"],
    );
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Legacy integrity archive"]);
    std::fs::write(repo.repo.join("integrity"), "unrelated unpublished work").unwrap();
    let legacy = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let integrity = preserved_archive_name(&record.execution_id, legacy.trim());
    JjRepo::run(&repo.repo, &["bookmark", "set", &integrity, "-r", legacy.trim()]);
    let published = variant_archive_name(&record.execution_id, "published");
    JjRepo::run(&repo.repo, &["bookmark", "set", &published, "-r", "main"]);
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "New base"]);
    std::fs::write(repo.replacement.join("base"), "new base").unwrap();
    JjRepo::run(&repo.replacement, &["new"]);
    let first = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    let current = JjRepo::run(
        &repo.repo,
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &archive,
            "-T",
            "commit_id",
        ],
    );
    assert_ne!(current.trim(), old.trim());
    assert!(
        first
            .unrestaged_unpublished
            .contains(&format!("{} ({archive})", current.trim()))
    );
    let retry = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(retry.unrestaged_unpublished.contains(&archive));
    assert!(!retry.unrestaged_unpublished.contains(&published));
    assert!(!retry.unrestaged_unpublished.contains(&integrity));
}
