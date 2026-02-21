#[tokio::test]
async fn runner_reports_missing_external_package() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(
        results[0].findings[0]
            .message
            .contains("was not found in configured providers")
    );
}

#[tokio::test]
async fn runner_reports_external_package_id_mismatch() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "different-id".to_owned(),
            runtime: "sandbox-v1".to_owned(),
            api_version: "v1".to_owned(),
            capabilities: Default::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "./check.ts".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: Vec::new(),
                },
            ),
        }),
    };

    let runner = Runner::with_external_package_provider(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("id mismatch"));
}

#[tokio::test]
async fn runner_executes_external_package_via_executor() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "sandbox-v1".to_owned(),
            api_version: "v1".to_owned(),
            capabilities: Default::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "./check.ts".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: Vec::new(),
                },
            ),
        }),
    };
    let seen_packages = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticExternalExecutor {
        result: Some(CheckResult {
            check_id: "domain-typo-check".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "external ran".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            }],
        }),
        error_message: None,
        seen_packages: Arc::clone(&seen_packages),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
    assert_eq!(results[0].findings[0].message, "external ran");

    let seen_packages = seen_packages.lock().expect("lock seen packages").clone();
    assert_eq!(seen_packages, vec!["domain-typo-check".to_owned()]);
}

#[tokio::test]
async fn runner_maps_external_executor_failures_to_findings() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let provider = StaticExternalProvider {
        package: Some(ExternalCheckPackage {
            id: "domain-typo-check".to_owned(),
            runtime: "sandbox-v1".to_owned(),
            api_version: "v1".to_owned(),
            capabilities: Default::default(),
            implementation: ExternalCheckPackageImplementation::Source(
                ExternalCheckSourcePackage {
                    language: "javascript".to_owned(),
                    entry: "./check.ts".to_owned(),
                    build_adapter: "javascript-component".to_owned(),
                    sources: Vec::new(),
                },
            ),
        }),
    };
    let executor = StaticExternalExecutor {
        result: None,
        error_message: Some("sandbox runtime failed".to_owned()),
        seen_packages: Arc::new(Mutex::new(Vec::new())),
    };

    let runner = Runner::with_external(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(provider),
        Arc::new(executor),
    );

    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].check_id, "domain-typo");
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(
        results[0].findings[0]
            .message
            .contains("sandbox runtime failed")
    );
}

#[test]
fn list_configured_checks_reports_external_resolution_errors() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "value\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "domain-typo"
check = "domain-typo-check"
implementation = "generated:domain-typo-check"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let error = runner
        .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .expect_err("must fail");

    assert!(
        error
            .to_string()
            .contains("failed to resolve external check packages")
    );
}

#[test]
fn list_configured_checks_deduplicates() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "file-size"

[[checks]]
id = "spelling-typos"
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let checks = runner
        .list_configured_checks(&ChangeSet::new(vec![
            ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: Path::new("backend/src/b.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ]))
        .expect("list checks");

    let check_map: BTreeMap<_, _> = checks
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect();
    assert_eq!(check_map.len(), 2);
    assert!(check_map.contains_key("file-size"));
    assert!(check_map.contains_key("spelling-typos"));
}
