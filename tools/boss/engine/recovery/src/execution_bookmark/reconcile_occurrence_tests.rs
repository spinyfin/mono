use super::*;

#[derive(Default)]
struct LargeHistory(std::sync::atomic::AtomicUsize);

#[async_trait]
impl Jj for LargeHistory {
    async fn run(&self, _repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"interdiff") {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok("different patch".into());
        }
        if args.last() == Some(&"commit_id") {
            return Ok(String::new());
        }
        if args.last().unwrap().contains("description") {
            return Ok("preserved work Unpublished work\n".into());
        }
        Ok((0..200).map(|i| format!("change{i} commit{i}\n")).collect())
    }

    async fn shared_repo(&self, _workspace: &Path) -> Result<PathBuf> {
        unreachable!("containment only reads history")
    }
}

#[tokio::test]
async fn large_patch_search_fails_closed_after_bounded_comparisons() {
    let jj = LargeHistory::default();
    let result = reconcile::containment(&jj, Path::new("/unused"), Some("base"), "head", "published", "main")
        .await
        .unwrap();
    assert!(!result.is_contained());
    assert_eq!(jj.0.load(std::sync::atomic::Ordering::Relaxed), 128);
}

/// A published add/revert pair cannot vouch for an unpublished re-add.
#[tokio::test]
async fn repeated_patch_requires_a_distinct_published_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_repeated_patch", "local")
        .await
        .unwrap();
    let base = one(&repo.repo, &revision(&record.base()));
    std::fs::write(repo.worker.join("work.txt"), "work\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Add work"]);
    let add = one(&repo.worker, "@");
    JjRepo::run(&repo.worker, &["new", "-m", "Remove work"]);
    std::fs::remove_file(repo.worker.join("work.txt")).unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let remove = one(&repo.worker, "@");
    let readd = reimport(&repo.worker, &add, "work.txt", "Re-add unpublished work");
    bookmark(&repo.repo, &record.head(), &readd);

    JjRepo::run(&repo.repo, &["new", &base]);
    reimport(&repo.repo, &add, "work.txt", "Published add");
    let published = reimport(&repo.repo, &remove, "work.txt", "Published removal");
    bookmark(&repo.repo, "pr/7", &published);
    bookmark(&repo.repo, &record.publication(), &published);
    let before = snapshot_pointers(&LocalJj, &record).await.unwrap();
    let error = reconcile(&LocalJj, &record, Some("pr/7"), "main").await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
    assert_eq!(snapshot_pointers(&LocalJj, &record).await.unwrap(), before);
}

#[tokio::test]
async fn reversed_patch_occurrences_are_not_contained() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let base = one(&repo.repo, "@");
    JjRepo::run(&repo.repo, &["new", "-m", "First patch"]);
    std::fs::write(repo.repo.join("first.txt"), "first\n").unwrap();
    JjRepo::run(&repo.repo, &["status"]);
    let first = one(&repo.repo, "@");
    JjRepo::run(&repo.repo, &["new", "-m", "Second patch"]);
    std::fs::write(repo.repo.join("second.txt"), "second\n").unwrap();
    JjRepo::run(&repo.repo, &["status"]);
    let second = one(&repo.repo, "@");
    JjRepo::run(&repo.repo, &["new", &base]);
    reimport(&repo.repo, &second, "second.txt", "Second patch first");
    let reversed = reimport(&repo.repo, &first, "first.txt", "First patch second");
    let result = reconcile::containment(&LocalJj, &repo.repo, Some(&base), &second, &reversed, "main")
        .await
        .unwrap();
    assert!(!result.is_contained());
}
