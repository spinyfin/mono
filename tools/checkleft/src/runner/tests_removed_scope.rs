// Exercise scheduling and the downstream scope filter without a current file.
struct RemovedSchemaTree;

impl SourceTree for RemovedSchemaTree {
    fn read_file(&self, _path: &Path) -> Result<Vec<u8>> {
        anyhow::bail!("no current files")
    }

    fn read_file_versioned(&self, path: &Path, version: crate::input::TreeVersion) -> Result<Vec<u8>> {
        if matches!(version, crate::input::TreeVersion::Base) && path == Path::new("api/schema.proto") {
            Ok(
                b"syntax = \"proto3\"; package p; message Gone {} service Hub { rpc Call(Gone) returns (Gone); }"
                    .to_vec(),
            )
        } else {
            anyhow::bail!("no base at this path")
        }
    }

    fn exists(&self, _path: &Path) -> bool {
        false
    }
    fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![])
    }
    fn glob(&self, _pattern: &str) -> Result<Vec<PathBuf>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn removed_schema_findings_survive_the_real_wasm_host() {
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("CHECKS.yaml"),
        "checks:\n  - id: proto/breaking\n    scope: files-including-removed\n    include: [api/**]\n",
    )
    .unwrap();
    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).unwrap()),
        Arc::new(RemovedSchemaTree),
        Arc::new(crate::external::BundledExternalCheckPackageProvider),
        shared_builder_audit_executor(),
    );
    for file in [
        ChangedFile {
            path: "api/schema.proto".into(),
            kind: ChangeKind::Deleted,
            old_path: None,
        },
        ChangedFile {
            path: "outside/schema.proto".into(),
            kind: ChangeKind::Renamed,
            old_path: Some("api/schema.proto".into()),
        },
    ] {
        let results = runner.run_changeset(&ChangeSet::new(vec![file])).await.unwrap();
        let findings: Vec<_> = results.iter().flat_map(|r| &r.findings).collect();
        for definition in ["message `Gone` was removed", "service `Hub` was removed"] {
            assert!(findings.iter().any(|f| f.message.contains(definition)), "{findings:?}");
        }
        assert!(findings.iter().all(|f| {
            f.location
                .as_ref()
                .is_some_and(|l| l.path == Path::new("api/schema.proto") && l.line == Some(1))
        }));
    }
}

#[test]
fn removal_scope_schedules_deletions_and_renames_outside_contract() {
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("CHECKS.yaml"),
        "checks:\n  - id: proto/breaking\n    scope: files-including-removed\n    include: [api/**]\n",
    )
    .unwrap();
    let runner = Runner::with_external_package_provider(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).unwrap()),
        Arc::new(LocalSourceTree::new(temp.path()).unwrap()),
        Arc::new(crate::external::BundledExternalCheckPackageProvider),
    );
    for file in [
        ChangedFile {
            path: "api/schema.proto".into(),
            kind: ChangeKind::Deleted,
            old_path: None,
        },
        ChangedFile {
            path: "outside/schema.proto".into(),
            kind: ChangeKind::Renamed,
            old_path: Some("api/schema.proto".into()),
        },
    ] {
        let changeset = ChangeSet::new(vec![file]);
        assert!(
            runner
                .list_configured_checks(&changeset)
                .unwrap()
                .contains(&"proto/breaking".to_owned())
        );
        let scheduled = runner.schedule_runs(&changeset).unwrap();
        let files: Vec<_> = scheduled
            .runs
            .iter()
            .flat_map(|run| run.path_scope.filter_changeset(&run.changeset).changed_files)
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, Path::new("api/schema.proto"));
        assert_eq!(files[0].kind, ChangeKind::Deleted);
    }
}

#[test]
fn rename_inside_contract_is_compared_without_synthetic_deletion() {
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("CHECKS.yaml"),
        "checks:\n  - id: proto/breaking\n    scope: files-including-removed\n    include: [api/**]\n",
    )
    .unwrap();
    let runner = Runner::with_external_package_provider(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).unwrap()),
        Arc::new(LocalSourceTree::new(temp.path()).unwrap()),
        Arc::new(crate::external::BundledExternalCheckPackageProvider),
    );
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: "api/renamed.proto".into(),
        kind: ChangeKind::Renamed,
        old_path: Some("api/schema.proto".into()),
    }]);
    let scheduled = runner.schedule_runs(&changeset).unwrap();
    let files: Vec<_> = scheduled
        .runs
        .iter()
        .flat_map(|run| run.path_scope.filter_changeset(&run.changeset).changed_files)
        .collect();
    assert_eq!(files, changeset.changed_files);
}

#[test]
fn default_file_scope_still_skips_deleted_files() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("CHECKS.yaml"), "checks:\n  - id: file/size\n").unwrap();
    let runner = Runner::with_external_package_provider(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).unwrap()),
        Arc::new(LocalSourceTree::new(temp.path()).unwrap()),
        Arc::new(crate::external::BundledExternalCheckPackageProvider),
    );
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: "api/schema.proto".into(),
        kind: ChangeKind::Deleted,
        old_path: None,
    }]);
    assert!(runner.schedule_runs(&changeset).unwrap().runs.is_empty());
    assert!(runner.list_configured_checks(&changeset).unwrap().is_empty());
}

#[test]
fn rename_into_destination_override_preserves_old_contract_removal() {
    for config in [
        "checks:\n  - id: proto/breaking\n    enabled: false\n",
        "checks:\n  - id: proto/breaking\n    include: [other/**]\n",
        "exclude: [schema.proto]\n",
        "checks:\n  - id: proto/breaking\n    scope: files\n",
    ] {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join("api/sub")).unwrap();
        fs::write(
            temp.path().join("CHECKS.yaml"),
            "checks:\n  - id: proto/breaking\n    scope: files-including-removed\n    include: [api/**]\n",
        )
        .unwrap();
        fs::write(temp.path().join("api/sub/CHECKS.yaml"), config).unwrap();
        let runner = Runner::with_external_package_provider(
            Arc::new(CheckRegistry::new()),
            Arc::new(ConfigResolver::new(temp.path()).unwrap()),
            Arc::new(LocalSourceTree::new(temp.path()).unwrap()),
            Arc::new(crate::external::BundledExternalCheckPackageProvider),
        );
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "api/sub/schema.proto".into(),
            kind: ChangeKind::Renamed,
            old_path: Some("api/schema.proto".into()),
        }]);
        assert_eq!(
            runner.list_configured_checks(&changeset).unwrap(),
            vec!["proto/breaking"]
        );
        let scheduled = runner.schedule_runs(&changeset).unwrap();
        let files: Vec<_> = scheduled
            .runs
            .iter()
            .flat_map(|run| run.path_scope.filter_changeset(&run.changeset).changed_files)
            .collect();
        assert!(
            files
                .iter()
                .any(|file| file.path == Path::new("api/schema.proto") && file.kind == ChangeKind::Deleted),
            "{config}: {files:?}"
        );
    }
}
