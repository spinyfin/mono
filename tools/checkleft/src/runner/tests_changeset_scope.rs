// ── scope = changeset scheduling ────────────────────────────────────────────

/// A check standing in for the eventual `boss/no-boss-isms` check: it ignores
/// the changed-file set entirely and instead flags a marker string in the PR
/// description, emitting a locationless finding — the only shape available for
/// a subject with no file to point at.
#[derive(Clone)]
struct ChangesetInspectingCheck {
    id: String,
    call_count: Arc<Mutex<usize>>,
}

#[async_trait]
impl Check for ChangesetInspectingCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "flags a marker string leaking into the PR description"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for ChangesetInspectingCheck {
    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        _on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        *self.call_count.lock().expect("lock call count") += 1;

        let mut findings = Vec::new();
        if let Some(desc) = changeset.pr_description.as_deref()
            && desc.contains("T3124")
        {
            findings.push(Finding {
                fixable: false,
                severity: Severity::Error,
                message: "PR description leaks a work-item id".to_owned(),
                location: None,
                remediations: vec![],
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

fn runner_with_changeset_check(temp: &tempfile::TempDir, call_count: Arc<Mutex<usize>>) -> Runner {
    let mut registry = CheckRegistry::new();
    registry
        .register(ChangesetInspectingCheck {
            id: "changeset-inspecting".to_owned(),
            call_count,
        })
        .expect("register check");

    Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    )
}

#[tokio::test]
async fn changeset_scope_check_runs_once_regardless_of_changed_files() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/a.md"), "hello\n").expect("write file");
    fs::write(temp.path().join("docs/b.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
check = "changeset-inspecting"
scope = "changeset"
"#,
    )
    .expect("write config");

    let call_count = Arc::new(Mutex::new(0));
    let runner = runner_with_changeset_check(&temp, Arc::clone(&call_count));

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: Path::new("docs/a.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("docs/b.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ])
    .with_pr_description(Some("Fixes T3124 by removing the leak.".to_owned()));

    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(
        *call_count.lock().expect("lock call count"),
        1,
        "a changeset-scope check must run exactly once per invocation, not once per changed file"
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
    assert!(
        results[0].findings[0].location.is_none(),
        "a changeset-scope finding has no file to point at"
    );
    assert_eq!(results[0].findings[0].message, "PR description leaks a work-item id");
}

#[tokio::test]
async fn changeset_scope_check_runs_with_no_changed_files_at_all() {
    let temp = tempdir().expect("create temp dir");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
check = "changeset-inspecting"
scope = "changeset"
"#,
    )
    .expect("write config");

    let call_count = Arc::new(Mutex::new(0));
    let runner = runner_with_changeset_check(&temp, Arc::clone(&call_count));

    let changeset = ChangeSet::new(vec![]).with_pr_description(Some("Fixes T3124.".to_owned()));
    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(
        *call_count.lock().expect("lock call count"),
        1,
        "a changeset-scope check must run even when the changeset has no changed files"
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
}

#[tokio::test]
async fn changeset_scope_check_still_runs_when_every_changed_file_is_excluded() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("vendor.rs"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
exclude = ["vendor.rs"]

[[checks]]
id = "boss/no-boss-isms"
check = "changeset-inspecting"
scope = "changeset"
"#,
    )
    .expect("write config");

    let call_count = Arc::new(Mutex::new(0));
    let runner = runner_with_changeset_check(&temp, Arc::clone(&call_count));

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: Path::new("vendor.rs").to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }])
    .with_pr_description(Some("Fixes T3124.".to_owned()));
    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(
        *call_count.lock().expect("lock call count"),
        1,
        "excluding the only changed file must not skip a changeset-scope check"
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
}

#[tokio::test]
async fn changeset_scope_check_with_no_match_produces_no_findings() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("a.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
check = "changeset-inspecting"
scope = "changeset"
"#,
    )
    .expect("write config");

    let call_count = Arc::new(Mutex::new(0));
    let runner = runner_with_changeset_check(&temp, Arc::clone(&call_count));

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: Path::new("a.md").to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }])
    .with_pr_description(Some("A perfectly ordinary description.".to_owned()));
    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(*call_count.lock().expect("lock call count"), 1);
    assert_eq!(results.len(), 1);
    assert!(results[0].findings.is_empty());
}

#[test]
fn list_configured_checks_includes_changeset_scope_check() {
    let temp = tempdir().expect("create temp dir");
    fs::write(temp.path().join("a.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "boss/no-boss-isms"
check = "changeset-inspecting"
scope = "changeset"
"#,
    )
    .expect("write config");

    let call_count = Arc::new(Mutex::new(0));
    let runner = runner_with_changeset_check(&temp, Arc::clone(&call_count));

    // The only changed file doesn't even resolve any per-file checks, so a
    // changeset-scope check would be missed by a naive per-file listing.
    let checks = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("a.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect("list checks");

    assert_eq!(checks, vec!["boss/no-boss-isms".to_owned()]);
}
