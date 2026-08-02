#[derive(Clone)]
struct StaticFindingCheck {
    id: String,
    severity: Severity,
    remediation: Option<String>,
}

#[async_trait]
impl Check for StaticFindingCheck {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "emits one static finding"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for StaticFindingCheck {
    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        _on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let path = changeset
            .changed_files
            .first()
            .map(|changed| changed.path.clone())
            .unwrap_or_else(|| Path::new("unknown").to_path_buf());

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: vec![Finding {
                fixable: false,
                severity: self.severity,
                message: "synthetic policy finding".to_owned(),
                location: Some(Location {
                    path,
                    line: Some(1),
                    column: Some(1),
                }),
                surface: None,
                remediations: self.remediation.iter().cloned().collect(),
                suggested_fix: None,
            }],
        })
    }
}

#[tokio::test]
async fn runner_defaults_to_error_severity_when_no_policy_specified() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Warning,
            remediation: None,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
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
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
}

#[tokio::test]
async fn runner_applies_policy_severity_override() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
severity = "warning"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: None,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
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
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
}

#[tokio::test]
async fn runner_applies_policy_bypass_when_directive_exists() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some("BYPASS_POLICY_CHECK=Legitimate exception.".to_owned())),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Warning);
    assert!(results[0].findings[0].message.contains("BYPASS_POLICY_CHECK"));
}

#[tokio::test]
async fn runner_appends_unreachable_pr_description_guidance_when_pr_description_unresolved() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    // ChangeSet::new leaves pr_description as None, the same shape checkleft
    // builds on a GitHub merge-queue build once branch-based PR-number
    // recovery has genuinely found nothing to fetch. The generic
    // "never use bypasses for convenience" guidance must NOT fire here — it
    // wrongly implies the PR description was checked and found empty.
    let results = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
    assert!(
        results[0].findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("Could not resolve a PR description")),
        "remediations: {:?}",
        results[0].findings[0].remediations
    );
}

#[tokio::test]
async fn runner_appends_standard_bypass_guidance_when_pr_description_resolved_but_missing_directive() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    // pr_description IS resolved here (unlike the test above) — it just
    // doesn't contain the directive. The generic guidance is correct in this
    // case since both surfaces were genuinely checked.
    let results = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_pr_description(Some("no bypass directive in here".to_owned())),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 1);
    assert!(
        results[0].findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("never use bypasses for convenience"))
    );
    assert!(
        results[0].findings[0]
            .remediations
            .iter()
            .all(|r| !r.contains("Could not resolve a PR description"))
    );
}

#[tokio::test]
async fn runner_ignores_legacy_config_policy_fields() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "policy-check"
check = "static-finding"

[checks.config]
severity = "info"
allow_bypass = true
bypass_name = "BYPASS_LEGACY_POLICY_CHECK"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(StaticFindingCheck {
            id: "static-finding".to_owned(),
            severity: Severity::Error,
            remediation: Some("fix me".to_owned()),
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results_without_bypass = runner
        .run_changeset(&ChangeSet::new(vec![ChangedFile {
            path: Path::new("docs/file.md").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }]))
        .await
        .expect("run checks");
    assert_eq!(results_without_bypass[0].findings[0].severity, Severity::Error);
    assert_eq!(
        results_without_bypass[0].findings[0].remediations,
        vec!["fix me".to_owned()]
    );

    let results_with_bypass = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some("BYPASS_LEGACY_POLICY_CHECK=Legacy fallback path.".to_owned())),
        )
        .await
        .expect("run checks");
    assert_eq!(results_with_bypass[0].findings[0].severity, Severity::Error);
    assert_eq!(results_with_bypass[0].findings[0].message, "synthetic policy finding");
}

#[tokio::test]
async fn runner_does_not_apply_bypass_to_runner_generated_errors() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
    fs::write(temp.path().join("docs/file.md"), "hello\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "missing-check"
check = "not-registered"

[checks.policy]
allow_bypass = true
"#,
    )
    .expect("write config");

    let runner = Runner::new(
        Arc::new(CheckRegistry::new()),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }])
            .with_commit_description(Some(
                "BYPASS_MISSING_CHECK=This should not bypass runner-generated errors.".to_owned(),
            )),
        )
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings[0].severity, Severity::Error);
    assert!(results[0].findings[0].message.contains("unknown implementation"));
}

#[tokio::test]
async fn runner_change_file_count_fails_above_max_and_bypasses_with_directive() {
    use crate::external::BundledExternalCheckPackageProvider;
    use crate::external::test_support::executor_with_precompiled_cache;

    let temp = tempdir().expect("create temp dir");
    for i in 0..4 {
        let path = temp.path().join(format!("f{i}.txt"));
        fs::write(&path, format!("content {i}\n")).expect("write file");
    }
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "change/file-count"

[checks.config]
max_files = 3

[checks.policy]
severity = "error"
allow_bypass = true
"#,
    )
    .expect("write config");

    // change/file-count is a preinstalled wasm check, not a legacy built-in.
    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");
    let runner = Runner::with_external(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(BundledExternalCheckPackageProvider),
        Arc::new(executor_with_precompiled_cache(temp.path())),
    );

    let oversized = ChangeSet::new(
        (0..4)
            .map(|i| ChangedFile {
                path: Path::new(&format!("f{i}.txt")).to_path_buf(),
                kind: ChangeKind::Added,
                old_path: None,
            })
            .collect(),
    );

    let fail_results = runner.run_changeset(&oversized).await.expect("run without bypass");
    let file_count_result = fail_results
        .iter()
        .find(|r| r.check_id == "change/file-count")
        .expect("change/file-count ran");
    assert_eq!(file_count_result.findings.len(), 1);
    assert_eq!(file_count_result.findings[0].severity, Severity::Error);
    assert!(
        file_count_result.findings[0].message.contains("max_files=3"),
        "message was: {}",
        file_count_result.findings[0].message
    );
    assert!(
        file_count_result.findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("BYPASS_CHANGE_FILE_COUNT")),
        "failure remediations should mention bypass guidance; got {:?}",
        file_count_result.findings[0].remediations
    );

    let bypass_results = runner
        .run_changeset(
            &oversized.with_commit_description(Some(
                "BYPASS_CHANGE_FILE_COUNT=Coordinated rename of the protocol surface; splitting would leave the tree unbuildable mid-stack.".to_owned(),
            )),
        )
        .await
        .expect("run with bypass");
    let bypassed = bypass_results
        .iter()
        .find(|r| r.check_id == "change/file-count")
        .expect("change/file-count ran with bypass");
    assert_eq!(bypassed.findings.len(), 1);
    assert_eq!(bypassed.findings[0].severity, Severity::Warning);
    assert!(
        bypassed.findings[0].message.contains("BYPASS_CHANGE_FILE_COUNT"),
        "message was: {}",
        bypassed.findings[0].message
    );
    assert!(
        bypassed.findings[0]
            .remediations
            .iter()
            .any(|r| r.contains("Coordinated rename")),
        "bypass reason should be recorded; got {:?}",
        bypassed.findings[0].remediations
    );
}

#[tokio::test]
async fn runner_change_file_count_whole_repo_is_noop() {
    use crate::external::BundledExternalCheckPackageProvider;
    use crate::external::test_support::executor_with_precompiled_cache;

    let temp = tempdir().expect("create temp dir");
    for i in 0..40 {
        let path = temp.path().join(format!("tracked{i}.txt"));
        fs::write(&path, format!("content {i}\n")).expect("write file");
    }
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "change/file-count"

[checks.config]
max_files = 5

[checks.policy]
severity = "error"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");
    let runner = Runner::with_external(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        Arc::new(BundledExternalCheckPackageProvider),
        Arc::new(executor_with_precompiled_cache(temp.path())),
    );

    let whole_repo = ChangeSet::new(
        (0..40)
            .map(|i| ChangedFile {
                path: Path::new(&format!("tracked{i}.txt")).to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    )
    .with_whole_repo(true);

    let results = runner.run_changeset(&whole_repo).await.expect("run --all-style");
    let file_count_result = results
        .iter()
        .find(|r| r.check_id == "change/file-count")
        .expect("change/file-count ran");
    assert!(
        file_count_result.findings.is_empty(),
        "whole_repo / --all must no-op change/file-count; findings: {:?}",
        file_count_result.findings
    );
}

// ── `changed_lines_only` line-level finding filter ──────────────────────────
//
// `super::scope_findings_to_changed_lines` is exercised directly here (same
// pattern as `scope_filter_drops_findings_outside_changeset` further below in
// this module for the file-level filter), since it is a private free
// function and these unit tests want precise control over `Finding` shapes
// (locationless, lineless, in-range, out-of-range) without standing up a real
// per-file check. `runner_changed_lines_only_wires_policy_end_to_end` below
// covers the config -> `EffectiveCheckPolicy` -> filter plumbing.

fn changed_lines_finding_at_line(path: &str, line: Option<u32>) -> Finding {
    Finding {
        fixable: false,
        severity: Severity::Error,
        message: format!("finding at {path}:{line:?}"),
        location: Some(Location {
            path: Path::new(path).to_path_buf(),
            line,
            column: line.map(|_| 1),
        }),
        surface: None,
        remediations: vec![],
        suggested_fix: None,
    }
}

fn changeset_with_one_added_line_range(path: &str, start: u32, end: u32) -> ChangeSet {
    use crate::input::{DiffHunk, FileDiff};

    ChangeSet::new(vec![ChangedFile {
        path: Path::new(path).to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }])
    .with_file_diff(
        Path::new(path).to_path_buf(),
        FileDiff {
            hunks: vec![DiffHunk {
                old_start: start as usize,
                old_lines: (end - start + 1) as usize,
                new_start: start as usize,
                new_lines: (end - start + 1) as usize,
                added_lines: (end - start + 1) as usize,
                removed_lines: 0,
            }],
            added_line_ranges: vec![(start, end)],
        },
    )
}

#[test]
fn changed_lines_filter_keeps_finding_inside_changed_range_and_drops_outside() {
    let changeset = changeset_with_one_added_line_range("src/lib.rs", 2, 2);
    let mut result = CheckResult {
        check_id: "some-check".to_owned(),
        findings: vec![
            changed_lines_finding_at_line("src/lib.rs", Some(2)),
            changed_lines_finding_at_line("src/lib.rs", Some(1)),
        ],
    };

    super::scope_findings_to_changed_lines(&mut result, &changeset);

    assert_eq!(result.findings.len(), 1, "got: {:?}", result.findings);
    assert_eq!(result.findings[0].location.as_ref().unwrap().line, Some(2));
}

#[test]
fn changed_lines_filter_keeps_locationless_and_lineless_findings() {
    let changeset = changeset_with_one_added_line_range("src/lib.rs", 2, 2);
    let mut result = CheckResult {
        check_id: "some-check".to_owned(),
        findings: vec![
            changed_lines_finding_at_line("src/lib.rs", None), // whole-file finding
            Finding {
                fixable: false,
                severity: Severity::Error,
                message: "check-level finding".to_owned(),
                location: None,
                surface: None,
                remediations: vec![],
                suggested_fix: None,
            },
            changed_lines_finding_at_line("src/lib.rs", Some(1)), // on an untouched line: dropped
        ],
    };

    super::scope_findings_to_changed_lines(&mut result, &changeset);

    assert_eq!(result.findings.len(), 2, "got: {:?}", result.findings);
    assert!(result.findings.iter().any(|f| f.location.is_none()));
    assert!(
        result
            .findings
            .iter()
            .any(|f| f.location.as_ref().is_some_and(|l| l.line.is_none()))
    );
}

/// Mirrors `--all` mode: the changeset carries no `file_diffs` entry at all for
/// the file (no hunks were ever computed), so the filter must not restrict
/// anything for it — there is no changed-line data to filter against.
#[test]
fn changed_lines_filter_is_noop_when_changeset_has_no_diff_data() {
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: Path::new("src/lib.rs").to_path_buf(),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let mut result = CheckResult {
        check_id: "some-check".to_owned(),
        findings: vec![
            changed_lines_finding_at_line("src/lib.rs", Some(1)),
            changed_lines_finding_at_line("src/lib.rs", Some(99)),
        ],
    };

    super::scope_findings_to_changed_lines(&mut result, &changeset);

    assert_eq!(result.findings.len(), 2);
}

/// A file present in the changeset with an empty (but present) added-line
/// range set — e.g. a content-identical rename — legitimately drops every
/// line-anchored finding for that file.
#[test]
fn changed_lines_filter_drops_all_line_findings_for_file_with_zero_added_lines() {
    use crate::input::FileDiff;

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: Path::new("src/renamed.rs").to_path_buf(),
        kind: ChangeKind::Renamed,
        old_path: Some(Path::new("src/old_name.rs").to_path_buf()),
    }])
    .with_file_diff(Path::new("src/renamed.rs").to_path_buf(), FileDiff::default());
    let mut result = CheckResult {
        check_id: "some-check".to_owned(),
        findings: vec![changed_lines_finding_at_line("src/renamed.rs", Some(1))],
    };

    super::scope_findings_to_changed_lines(&mut result, &changeset);

    assert!(result.findings.is_empty());
}

#[derive(Clone)]
struct ConfigurableFindingsCheck {
    findings: Vec<Finding>,
}

#[async_trait]
impl Check for ConfigurableFindingsCheck {
    fn id(&self) -> &str {
        "configurable-findings"
    }

    fn description(&self) -> &str {
        "always emits a fixed set of findings, ignoring the changeset"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for ConfigurableFindingsCheck {
    async fn run_with_progress(
        &self,
        _changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        _on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: self.findings.clone(),
        })
    }
}

/// End-to-end through the full `Runner`: `policy.changed_lines_only: true` in
/// `CHECKS.toml` must reach `apply_policy_to_result` and filter findings,
/// proving the config -> `EffectiveCheckPolicy` -> filter wiring, not just the
/// filter function in isolation (covered above).
#[tokio::test]
async fn runner_changed_lines_only_wires_policy_end_to_end() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("create dirs");
    fs::write(temp.path().join("src/lib.rs"), "one\ntwo\nthree\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "line-scoped-check"
check = "configurable-findings"

[checks.policy]
changed_lines_only = true
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(ConfigurableFindingsCheck {
            findings: vec![
                changed_lines_finding_at_line("src/lib.rs", Some(2)),
                changed_lines_finding_at_line("src/lib.rs", Some(1)),
            ],
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&changeset_with_one_added_line_range("src/lib.rs", 2, 2))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].findings.len(),
        1,
        "only the changed-line finding should survive"
    );
    assert_eq!(results[0].findings[0].location.as_ref().unwrap().line, Some(2));
}

/// `changed_lines_only` defaults to `false`: a check that does not set it must
/// behave exactly as before this feature existed (file-level scoping only).
#[tokio::test]
async fn runner_changed_lines_only_default_off_preserves_existing_scoping() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("create dirs");
    fs::write(temp.path().join("src/lib.rs"), "one\ntwo\nthree\n").expect("write file");
    fs::write(
        temp.path().join("CHECKS.toml"),
        r#"
[[checks]]
id = "line-scoped-check"
check = "configurable-findings"
"#,
    )
    .expect("write config");

    let mut registry = CheckRegistry::new();
    registry
        .register(ConfigurableFindingsCheck {
            findings: vec![
                changed_lines_finding_at_line("src/lib.rs", Some(2)),
                changed_lines_finding_at_line("src/lib.rs", Some(1)),
            ],
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
        Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
    );

    let results = runner
        .run_changeset(&changeset_with_one_added_line_range("src/lib.rs", 2, 2))
        .await
        .expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].findings.len(),
        2,
        "changed_lines_only defaults to false; both findings on the changed file must survive"
    );
}
