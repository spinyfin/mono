use super::super::home::{provision_agy_home, session_settings_path_for_run};
use super::super::test_environment::env_for_provision;
use super::*;
use serde_json::Value;
use std::collections::BTreeSet;
use tempfile::TempDir;

const ALL_KINDS: [WorkerKind; 6] = [
    WorkerKind::Standard,
    WorkerKind::Reviewer,
    WorkerKind::Triage,
    WorkerKind::AnswerAgent,
    WorkerKind::ReviewGuide,
    WorkerKind::Progression,
];

/// Paths that do not exist, so rendering is a pure function of the spelling.
const WORKSPACE: &str = "/boss-test-absent/workspaces/ws-1";
const WORKSPACES_ROOT: &str = "/boss-test-absent/workspaces";
const DATA_DIR: &str = "/boss-test-absent/Library/Application Support/Boss";
const HOOKS_DIR: &str = "/boss-test-absent/agy-homes/run-1/process-home/.gemini/config";
const CLI_SETTINGS: &str = "/boss-test-absent/agy-homes/run-1/process-home/.gemini/antigravity-cli/settings.json";
const RULES_ROOT: &str = "/boss-test-absent/agy-homes/run-1/rules-root";
const CONTROL_DIR: &str = "/boss-test-absent/agy-homes/run-1/control";

fn plan(worker_kind: WorkerKind, boss_data_dir: Option<&'static str>) -> SettingsPlan<'static> {
    SettingsPlan {
        worker_kind,
        workspace: Path::new(WORKSPACE),
        boss_data_dir: boss_data_dir.map(Path::new),
        hooks_dir: Path::new(HOOKS_DIR),
        control_dir: Path::new(CONTROL_DIR),
        rules_root: Path::new(RULES_ROOT),
    }
}

fn rendered(worker_kind: WorkerKind) -> SessionSettings {
    render_settings(&plan(worker_kind, Some(DATA_DIR))).unwrap()
}

fn json_of(settings: &SessionSettings) -> Value {
    serde_json::from_str(&settings.to_json().unwrap()).unwrap()
}

fn is_deny_by_default(worker_kind: WorkerKind) -> bool {
    matches!(
        worker_kind,
        WorkerKind::AnswerAgent | WorkerKind::ReviewGuide | WorkerKind::Progression
    )
}

// ---------------------------------------------------------------------------
// Posture
// ---------------------------------------------------------------------------

#[test]
fn every_kind_renders_the_pinned_posture_and_nothing_else() {
    for kind in ALL_KINDS {
        let json = json_of(&rendered(kind));
        let keys: BTreeSet<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from(["agentMode", "permissions", "toolPermission", "trustedWorkspaces"]),
            "{kind:?}"
        );
        // Pinned explicitly: an absent agentMode starts in Auto on 1.5.7+.
        assert_eq!(json["agentMode"], "", "{kind:?}");
        assert_eq!(json["toolPermission"], "request-review", "{kind:?}");
        assert_eq!(json["trustedWorkspaces"], serde_json::json!([WORKSPACE]), "{kind:?}");

        let permission_keys: BTreeSet<&str> = json["permissions"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            permission_keys.is_subset(&BTreeSet::from(["allow", "deny"])),
            "{kind:?}"
        );
        assert!(permission_keys.contains("deny"), "{kind:?}");
        // Ask rules override a hook `auto_approve` and raise a dialog.
        assert!(json["permissions"].get("ask").is_none(), "{kind:?}");
        assert_eq!(
            json["permissions"].get("allow").is_some(),
            is_deny_by_default(kind),
            "only deny-by-default kinds carry an allowlist: {kind:?}"
        );
    }
}

#[test]
fn rejected_postures_are_never_rendered() {
    for kind in ALL_KINDS {
        let text = rendered(kind).to_json().unwrap();
        for forbidden in [
            "strict",
            "always-proceed",
            "proceed-in-sandbox",
            "\"plan\"",
            "accept-edits",
            "enableTerminalSandbox",
            "allowNonWorkspaceAccess",
            "dangerously",
        ] {
            assert!(!text.contains(forbidden), "{kind:?} rendered {forbidden:?}: {text}");
        }
    }
}

#[test]
fn restricted_kinds_mirror_the_engine_forced_permission_mode() {
    // Mirrors `boss_engine_execution_contracts::WorkerKind::forced_permission_mode`;
    // the engine-side cross-check lives in `boss-engine`'s worker_setup tests.
    for kind in ALL_KINDS {
        let expected = is_deny_by_default(kind).then_some("dontAsk");
        assert_eq!(permission_mode_for_worker_kind(kind), expected, "{kind:?}");
        let posture = verdict_posture_for_worker_kind(kind);
        assert_eq!(
            posture,
            if is_deny_by_default(kind) {
                VerdictPosture::GuardedAllowlist
            } else {
                VerdictPosture::GuardedApproval
            },
            "{kind:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Deny rules
// ---------------------------------------------------------------------------

#[test]
fn deny_rules_are_native_file_rules_only() {
    for kind in ALL_KINDS {
        let settings = rendered(kind);
        assert!(!settings.deny_rules().is_empty(), "{kind:?}");
        for rule in settings.deny_rules() {
            assert!(
                rule.starts_with("read_file(") || rule.starts_with("write_file("),
                "{kind:?}: command-level denies belong to the hook adapter, got {rule}"
            );
        }
        // The structural command denies are the adapter's: none may leak into
        // the settings, where a command deny beside a read_file deny stopped
        // working in the study.
        let text = settings.to_json().unwrap();
        for command in ["bossctl", "sudo", "rm -rf", "git push"] {
            assert!(!text.contains(command), "{kind:?} rendered {command}: {text}");
        }
    }
}

#[test]
fn boss_state_and_the_runs_own_wiring_are_fenced_for_every_kind() {
    for kind in ALL_KINDS {
        let settings = rendered(kind);
        for action in ["read_file", "write_file"] {
            let rule = format!("{action}({DATA_DIR})");
            assert!(settings.deny_rules().contains(&rule), "{kind:?} missing {rule}");
        }
        // The run may not rewrite its own hook wiring, settings or launcher...
        for dir in [HOOKS_DIR, CONTROL_DIR, RULES_ROOT] {
            let rule = format!("write_file({dir})");
            assert!(settings.deny_rules().contains(&rule), "{kind:?} missing {rule}");
        }
        // ...but only writes are fenced: the CLI keeps conversation artifacts
        // under the same HOME and reading them back is ordinary.
        for rule in settings.deny_rules() {
            if rule.starts_with("read_file(") {
                assert!(rule.contains(DATA_DIR), "{kind:?}: unexpected read fence {rule}");
            }
        }
    }
}

#[test]
fn remote_workers_have_no_boss_data_dir_fence() {
    for kind in ALL_KINDS {
        let settings = render_settings(&plan(kind, None)).unwrap();
        assert!(
            !settings
                .deny_rules()
                .iter()
                .any(|rule| rule.contains("Application Support")),
            "{kind:?}: {:?}",
            settings.deny_rules()
        );
        assert!(settings.deny_rules().contains(&format!("write_file({HOOKS_DIR})")));
        assert!(settings.deny_rules().contains(&format!("write_file({CONTROL_DIR})")));
    }
}

#[test]
fn reviewer_keeps_its_external_artifact_while_other_non_writers_get_a_blanket_fence() {
    let fence = format!("write_file({WORKSPACES_ROOT})");
    for kind in ALL_KINDS {
        let settings = rendered(kind);
        let fenced = settings.deny_rules().contains(&fence);
        assert_eq!(
            fenced,
            kind == WorkerKind::Reviewer,
            "{kind:?}: {:?}",
            settings.deny_rules()
        );
        assert_eq!(
            settings.deny_rules().contains(&"write_file(/)".to_owned()),
            !matches!(kind, WorkerKind::Standard | WorkerKind::Reviewer)
        );
        // No kind is denied reads of its own workspace.
        for rule in settings.deny_rules() {
            if let Some(target) = rule.strip_prefix("read_file(").and_then(|r| r.strip_suffix(')')) {
                assert!(!Path::new(WORKSPACE).starts_with(target), "{kind:?}: {rule}");
            }
        }
    }
    // A Standard worker writes its checkout: nothing may fence it.
    for rule in rendered(WorkerKind::Standard).deny_rules() {
        let target = rule.split_once('(').unwrap().1.trim_end_matches(')');
        assert!(!Path::new(WORKSPACE).starts_with(target), "{rule}");
    }
}

#[test]
fn a_layout_that_fences_the_workers_own_workspace_is_refused() {
    // A control directory that contains the checkout would deny a writing
    // worker its workspace.
    let overlapping = SettingsPlan {
        control_dir: Path::new(WORKSPACES_ROOT),
        ..plan(WorkerKind::Standard, None)
    };
    let error = format!("{:#}", render_settings(&overlapping).unwrap_err());
    assert!(error.contains("own workspace"), "{error}");
    // A kind that may not write is not harmed by a write fence over its checkout.
    let overlapping = SettingsPlan {
        control_dir: Path::new(WORKSPACES_ROOT),
        ..plan(WorkerKind::Reviewer, None)
    };
    render_settings(&overlapping).unwrap();
    // Reads are never fenced off from any kind: a data directory that contains
    // the checkout is refused for all of them.
    for kind in ALL_KINDS {
        let overlapping = SettingsPlan {
            boss_data_dir: Some(Path::new(WORKSPACES_ROOT)),
            ..plan(kind, None)
        };
        assert!(render_settings(&overlapping).is_err(), "{kind:?}");
    }
}

// ---------------------------------------------------------------------------
// Allowlists
// ---------------------------------------------------------------------------

#[test]
fn allow_by_default_kinds_have_no_allowlist() {
    for kind in [WorkerKind::Standard, WorkerKind::Reviewer, WorkerKind::Triage] {
        let settings = rendered(kind);
        assert!(settings.allow_rules().is_empty(), "{kind:?}");
        assert!(json_of(&settings)["permissions"].get("allow").is_none(), "{kind:?}");
    }
}

#[test]
fn every_shared_allowlist_entry_translates_to_a_valid_rule() {
    // Each table is translated entry by entry: a new entry shape that has no
    // `agy` equivalent fails here instead of silently changing a capability.
    for kind in [
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        let claude = claude_allowlist(kind);
        assert!(!claude.is_empty(), "{kind:?}");
        let allow = rendered(kind);
        assert!(!allow.allow_rules().is_empty(), "{kind:?}");
        for claude_rule in &claude {
            let translated = translate_claude_allow_rule(claude_rule, Path::new(WORKSPACE))
                .unwrap_or_else(|error| panic!("{kind:?}: {error:#}"));
            assert!(!translated.is_empty(), "{kind:?}: {claude_rule}");
            for rule in &translated {
                validate_rule(RuleList::Allow, rule).unwrap();
                assert!(allow.allow_rules().contains(rule), "{kind:?}: {rule}");
            }
        }
    }
}

#[test]
fn answer_agent_may_read_its_workspace_and_post_one_reply_only() {
    assert_eq!(
        rendered(WorkerKind::AnswerAgent).allow_rules(),
        [
            format!("read_file({WORKSPACE})"),
            "command(boss comment reply)".to_owned()
        ]
    );
}

#[test]
fn progression_allowlist_is_the_shared_table_with_no_vcs_or_cube() {
    let settings = rendered(WorkerKind::Progression);
    let allow = settings.allow_rules();
    for expected in [
        format!("read_file({WORKSPACE})"),
        "command(boss propose)".to_owned(),
        r#"command("$BOSS_BIN" propose)"#.to_owned(),
        "command(boss pr status)".to_owned(),
        "command(gh pr view)".to_owned(),
        "command(gh pr checks)".to_owned(),
    ] {
        assert!(allow.contains(&expected), "missing {expected} in {allow:?}");
    }
    for rule in allow {
        for forbidden in [
            "command(jj",
            "command(git",
            "command(cube",
            "command(gh api",
            "command(sh",
        ] {
            assert!(!rule.starts_with(forbidden), "{rule}");
        }
    }
}

#[test]
fn review_guide_allowlist_keeps_the_narrow_command_shapes() {
    let settings = rendered(WorkerKind::ReviewGuide);
    let allow = settings.allow_rules();
    for expected in ["command(git show)", "command(sed -n)", "command(wc -l)", "command(nl)"] {
        assert!(allow.contains(&expected.to_owned()), "missing {expected}");
    }
    // Broader prefixes of the same commands would admit mutating forms.
    for broad in ["command(git)", "command(sed)", "command(wc)"] {
        assert!(!allow.contains(&broad.to_owned()), "{broad} widens the table");
    }
}

#[test]
fn translation_refuses_entries_with_no_agy_equivalent() {
    for rule in ["Edit", "WebFetch", "Bash", "Bash(gh api)", "Bash(cat)", "mcp__x"] {
        let error = translate_claude_allow_rule(rule, Path::new(WORKSPACE)).unwrap_err();
        assert!(format!("{error:#}").contains("no agy equivalent"), "{rule}: {error:#}");
    }
}

// ---------------------------------------------------------------------------
// Rule validation
// ---------------------------------------------------------------------------

#[test]
fn malformed_rules_from_the_study_are_rejected_before_they_can_fail_open() {
    // The strings the CLI logged `ignoring invalid` for, plus the shapes it
    // accepted without a warning but whose effect was never measured.
    for rule in [
        "comand(printf)",
        "command(printf",
        "command (cat)",
        "write_file",
        "read_file()",
        "read_file(relative/path)",
        "write_file(/a/../b)",
        "write_file( /a)",
        "write_file(/a(b))",
        "write_file(/a\nb)",
        "unsandboxed(printf)",
        "mcp(server)",
        "read_url(https://example.com)",
        "execute_url(https://example.com)",
    ] {
        for list in [RuleList::Allow, RuleList::Deny] {
            assert!(validate_rule(list, rule).is_err(), "{list:?} accepted {rule:?}");
        }
    }
    assert!(validate_rule(RuleList::Allow, "command(regex:echo .*)").is_err());
}

#[test]
fn command_rules_are_never_deny_rules() {
    for rule in [
        "command(printf)",
        "command(git push)",
        "command(sudo)",
        "command(bossctl)",
    ] {
        let error = validate_rule(RuleList::Deny, rule).unwrap_err();
        assert!(format!("{error:#}").contains("native file rules only"), "{error:#}");
        // The same string is a fine allow rule: the restriction is the list.
        validate_rule(RuleList::Allow, rule).unwrap();
    }
}

#[test]
fn well_formed_rules_are_accepted() {
    for rule in [
        "command(git push)",
        r#"command("$BOSS_BIN" propose)"#,
        "read_file(/tmp/x)",
        "write_file(/Users/someone/Library/Application Support/Boss)",
    ] {
        validate_rule(RuleList::Allow, rule).unwrap();
    }
    for rule in ["read_file(/x)", "write_file(/x)"] {
        validate_rule(RuleList::Deny, rule).unwrap();
    }
}

#[test]
fn rendering_refuses_a_command_deny_duplicates_and_an_empty_deny_list() {
    let mut settings = rendered(WorkerKind::Standard);
    settings.permissions.deny.push("command(printf)".to_owned());
    assert!(settings.to_json().is_err());

    let mut settings = rendered(WorkerKind::Standard);
    let first = settings.permissions.deny[0].clone();
    settings.permissions.deny.push(first);
    assert!(format!("{:#}", settings.to_json().unwrap_err()).contains("duplicate"));

    let mut settings = rendered(WorkerKind::Standard);
    settings.permissions.deny.clear();
    assert!(settings.to_json().is_err());
}

#[test]
fn a_rule_the_cli_would_drop_fails_the_render_not_the_run() {
    // A directory name with a parenthesis cannot be written as a rule target.
    let odd = SettingsPlan {
        boss_data_dir: Some(Path::new("/boss-test-absent/Boss (dev)")),
        ..plan(WorkerKind::Standard, None)
    };
    let error = format!("{:#}", render_settings(&odd).unwrap_err());
    assert!(error.contains("parenthesis"), "{error}");
}

#[test]
fn parse_rendered_rejects_loosened_or_misspelled_settings() {
    let good = rendered(WorkerKind::Triage).to_json().unwrap();
    assert!(parse_rendered(&good).is_ok());
    type Edit = fn(&mut Value);
    let edits: [(&str, Edit); 8] = [
        ("unknown key", |v| {
            v["toolPermissions"] = "request-review".into();
        }),
        ("strict level", |v| v["toolPermission"] = "strict".into()),
        ("auto mode", |v| v["agentMode"] = "auto".into()),
        ("missing posture", |v| {
            v.as_object_mut().unwrap().remove("agentMode");
        }),
        ("ask rules", |v| {
            v["permissions"]["ask"] = serde_json::json!(["command(ls)"]);
        }),
        ("second trusted path", |v| {
            v["trustedWorkspaces"] = serde_json::json!([WORKSPACE, WORKSPACES_ROOT]);
        }),
        ("relative trust", |v| {
            v["trustedWorkspaces"] = serde_json::json!(["ws-1"]);
        }),
        ("command deny", |v| {
            v["permissions"]["deny"] = serde_json::json!(["command(sudo)"]);
        }),
    ];
    for (label, edit) in edits {
        let mut value: Value = serde_json::from_str(&good).unwrap();
        edit(&mut value);
        assert!(parse_rendered(&value.to_string()).is_err(), "{label} was accepted");
    }
}

// ---------------------------------------------------------------------------
// Writing the file and the artifacts
// ---------------------------------------------------------------------------

fn permission_input(
    kind: WorkerKind,
    workspace: &Path,
    data_dir: &Path,
    run_id: &str,
    remote: bool,
) -> PermissionInput {
    fs::create_dir_all(data_dir).unwrap();
    let forwarder = data_dir.join("boss-event");
    crate::hook_guards::write_executable(&forwarder, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    PermissionInput::builder()
        .worker_kind(kind)
        .workspace_path(workspace.to_path_buf())
        .events_socket_path(data_dir.join("events.sock"))
        .boss_event_path(forwarder)
        .run_id(run_id)
        .lease_id("lease")
        .execution_kind("task_implementation")
        .is_remote(remote)
        .build()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Exercise the settings renderer on its own. The driver's
/// `write_permission_config` renders these settings and then installs the
/// authorization adapter, which is covered by the driver-level tests.
fn write_config(input: &PermissionInput) -> anyhow::Result<PermissionArtifacts> {
    write_session_settings(input)
}

#[test]
fn write_permission_config_renders_settings_over_the_provisioned_trust_only_file() {
    for kind in ALL_KINDS {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("workspaces/ws-1");
        let data_dir = tmp.path().join("boss-data");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes);
        let run_id = format!("run-perm-{kind:?}");
        let runtime = provision_agy_home(&workspace, "prompt", &run_id).unwrap();
        let provisioned = read_json(&runtime.session_settings_path);
        assert!(
            provisioned.get("toolPermission").is_none(),
            "provisioning is trust-only"
        );

        let input = permission_input(kind, &workspace, &data_dir, &run_id, false);
        let artifacts = write_config(&input).unwrap();

        let settings_path = session_settings_path_for_run(&run_id).unwrap();
        assert_eq!(settings_path, runtime.session_settings_path);
        assert!(
            artifacts.config_files.contains(&settings_path),
            "config_files must include session settings: {:?}",
            artifacts.config_files
        );
        // `--settings` is the only per-run flag; no posture is carried by
        // `--mode`, `--sandbox` or the skip-permissions flag, and no env.
        assert_eq!(
            artifacts.extra_args,
            vec!["--settings".to_owned(), settings_path.display().to_string()],
            "{kind:?}"
        );
        assert!(artifacts.env.is_empty());

        let json = read_json(&settings_path);
        let resolved = fs::canonicalize(&workspace).unwrap();
        assert_eq!(json["agentMode"], "", "{kind:?}");
        assert_eq!(json["toolPermission"], "request-review", "{kind:?}");
        assert_eq!(
            json["trustedWorkspaces"],
            serde_json::json!([resolved.display().to_string()]),
            "exactly the resolved workspace; the control dir is not trusted: {kind:?}"
        );
        assert_eq!(json["permissions"].get("allow").is_some(), is_deny_by_default(kind));
        // The file passes the same validation the post-spawn check applies.
        parse_rendered(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        // Idempotent.
        let again = write_config(&input).unwrap();
        assert_eq!(again.extra_args, artifacts.extra_args);
        assert_eq!(read_json(&settings_path), json);
    }
}

#[test]
fn the_rendered_fences_name_the_engine_data_dir_and_resolved_paths() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("workspaces/ws-1");
    let data_dir = tmp.path().join("boss-data");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let _guard = env_for_provision(&homes);
    provision_agy_home(&workspace, "prompt", "run-fences").unwrap();

    let reviewer = permission_input(WorkerKind::Reviewer, &workspace, &data_dir, "run-fences", false);
    write_config(&reviewer).unwrap();
    let json = read_json(&session_settings_path_for_run("run-fences").unwrap());
    let deny: Vec<&str> = json["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let resolved_data = fs::canonicalize(&data_dir).unwrap();
    let resolved_root = fs::canonicalize(workspace.parent().unwrap()).unwrap();
    for expected in [
        format!("read_file({})", resolved_data.display()),
        format!("write_file({})", resolved_data.display()),
        format!("write_file({})", resolved_root.display()),
        format!("write_file({})", hooks_dir_for_run("run-fences").unwrap().display()),
        format!("write_file({})", control_dir_for_run("run-fences").unwrap().display()),
    ] {
        assert!(deny.contains(&expected.as_str()), "missing {expected} in {deny:?}");
    }
    // The rules fence is this run's `rules-root`, not the shared homes tree
    // above it, and the production renderer agrees with the live probe.
    let rules_root = super::super::home::agent_rules_path_for_run("run-fences")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(rules_root.ends_with("run-fences/rules-root"), "{rules_root:?}");
    assert!(
        deny.contains(&format!("write_file({})", rules_root.display()).as_str()),
        "missing rules-root fence in {deny:?}"
    );
    assert!(
        !deny.contains(&format!("write_file({})", super::super::home::agy_homes_root().display()).as_str()),
        "the shared homes dir must not be fenced: {deny:?}"
    );

    // Remote: the forwarded socket's directory is not a Boss data dir.
    let remote = permission_input(WorkerKind::Standard, &workspace, &data_dir, "run-fences", true);
    write_config(&remote).unwrap();
    let json = read_json(&session_settings_path_for_run("run-fences").unwrap());
    assert!(
        !json["permissions"]["deny"]
            .to_string()
            .contains(&resolved_data.display().to_string())
    );
}

#[test]
fn write_permission_config_requires_a_provisioned_run_and_a_real_workspace() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let _guard = env_for_provision(&homes);

    let data_dir = tmp.path().join("boss-data");
    let unprovisioned = permission_input(WorkerKind::Standard, &workspace, &data_dir, "run-never", false);
    let error = format!("{:#}", write_config(&unprovisioned).unwrap_err());
    assert!(error.contains("provision_workspace"), "{error}");
    assert!(!session_settings_path_for_run("run-never").unwrap().exists());

    provision_agy_home(&workspace, "prompt", "run-gone").unwrap();
    let missing = permission_input(
        WorkerKind::Standard,
        &tmp.path().join("deleted-workspace"),
        &data_dir,
        "run-gone",
        false,
    );
    let error = format!("{:#}", write_config(&missing).unwrap_err());
    assert!(error.contains("resolving agy workspace"), "{error}");
}

#[test]
fn write_permission_config_never_writes_through_a_planted_symlink() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let _guard = env_for_provision(&homes);
    provision_agy_home(&workspace, "prompt", "run-link").unwrap();
    let settings_path = session_settings_path_for_run("run-link").unwrap();
    let victim = tmp.path().join("host-settings.json");
    fs::write(&victim, b"host settings").unwrap();
    fs::remove_file(&settings_path).unwrap();
    std::os::unix::fs::symlink(&victim, &settings_path).unwrap();

    let data_dir = tmp.path().join("boss-data");
    let input = permission_input(WorkerKind::Standard, &workspace, &data_dir, "run-link", false);
    let error = format!("{:#}", write_config(&input).unwrap_err());
    assert!(error.contains("symlink"), "{error}");
    assert_eq!(fs::read(&victim).unwrap(), b"host settings");
}

// ---------------------------------------------------------------------------
// Post-spawn log check
// ---------------------------------------------------------------------------

/// A `CLI settings initialized` line in the shape the permission study
/// recorded, written out by hand so the check is not tested against its own
/// formatter.
fn init_line(allow: &str, deny: &str, level: &str) -> String {
    format!(
        "I1114 10:00:00.000000 4242 cli_setting_manager.go:98] CLI settings initialized: \
         permissions=&{{Allow:[{allow}] Deny:[{deny}] Ask:[]}}, toolPermission={level}"
    )
}

#[test]
fn log_check_matches_the_study_line_shape_exactly() {
    let settings = render_settings(&SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new(WORKSPACE),
        boss_data_dir: None,
        hooks_dir: Path::new(HOOKS_DIR),
        control_dir: Path::new(CONTROL_DIR),
        rules_root: Path::new(RULES_ROOT),
    })
    .unwrap();
    let file = settings.to_json().unwrap();
    let line = init_line(
        "",
        &format!(
            "write_file({HOOKS_DIR}) write_file({CONTROL_DIR}) write_file({RULES_ROOT}) write_file({CLI_SETTINGS})"
        ),
        "request-review",
    );
    assert_eq!(
        verify_settings_log(&file, &format!("{line}\n{HOOKS_LOADED_LINE}")).unwrap(),
        SettingsLogCheck::ConsistentSoFar
    );
}

#[test]
fn log_check_requires_the_hooks_file_to_have_loaded() {
    let settings = rendered(WorkerKind::Standard);
    let file = settings.to_json().unwrap();
    let init = init_line(
        &settings.allow_rules().join(" "),
        &settings.deny_rules().join(" "),
        "request-review",
    );
    // Settings alone are not admission: with no hooks the CLI still starts,
    // in review mode, with no authorization hook and no progress ingress.
    for log in [
        init.clone(),
        format!("{init}\nI1117 hooks_manager.go:54] loaded 0 named hooks from 0 hooks.json file(s)"),
        format!("{init}\nI1117 hooks_manager.go:54] loaded 0 named hooks from 1 hooks.json file(s)"),
    ] {
        assert_eq!(
            verify_settings_log(&file, &log).unwrap(),
            SettingsLogCheck::NotYetInitialized,
            "{log}"
        );
    }
    // A hooks file the CLI cannot parse is a failure, not a wait.
    let unparseable = format!(
        "{init}\nE1117 hooks_manager.go:34] failed to parse hooks.json at /h/hooks.json: invalid character\n\
         I1117 hooks_manager.go:54] loaded 0 named hooks from 1 hooks.json file(s)"
    );
    let error = format!("{:#}", verify_settings_log(&file, &unparseable).unwrap_err());
    assert!(error.contains("no authorization hook is armed"), "{error}");
}

#[test]
fn log_check_waits_for_initialisation_and_then_accepts_a_matching_line() {
    for kind in ALL_KINDS {
        let settings = rendered(kind);
        let file = settings.to_json().unwrap();
        assert_eq!(
            verify_settings_log(&file, "starting up\nloaded onboarding\n").unwrap(),
            SettingsLogCheck::NotYetInitialized,
            "{kind:?}"
        );
        let log = format!(
            "noise\n{}\n{HOOKS_LOADED_LINE}\nmore noise\n",
            init_line(
                &settings.allow_rules().join(" "),
                &settings.deny_rules().join(" "),
                "request-review"
            )
        );
        assert_eq!(
            verify_settings_log(&file, &log).unwrap(),
            SettingsLogCheck::ConsistentSoFar,
            "{kind:?}"
        );
    }
}

#[test]
fn log_check_fails_when_the_cli_initialised_a_different_posture() {
    let settings = rendered(WorkerKind::Progression);
    let file = settings.to_json().unwrap();
    let allow = settings.allow_rules().join(" ");
    let deny = settings.deny_rules().join(" ");
    let mut dropped_allow = settings.allow_rules().to_vec();
    dropped_allow.pop();
    let mut reversed = settings.deny_rules().to_vec();
    reversed.reverse();
    for (label, line) in [
        ("always-proceed", init_line(&allow, &deny, "always-proceed")),
        ("strict", init_line(&allow, &deny, "strict")),
        (
            "fewer allow rules",
            init_line(&dropped_allow.join(" "), &deny, "request-review"),
        ),
        ("no deny rules", init_line(&allow, "", "request-review")),
        (
            "reordered deny rules",
            init_line(&allow, &reversed.join(" "), "request-review"),
        ),
        (
            "an extra deny rule",
            init_line(&allow, &format!("{deny} read_file(/extra)"), "request-review"),
        ),
    ] {
        let error = format!("{:#}", verify_settings_log(&file, &line).unwrap_err());
        assert!(error.contains("differ from the rendered posture"), "{label}: {error}");
    }
    // One mismatching line among matching ones is still a failure.
    let log = format!(
        "{}\n{}\n",
        init_line(&allow, &deny, "request-review"),
        init_line(&allow, &deny, "always-proceed")
    );
    assert!(verify_settings_log(&file, &log).is_err());
}

#[test]
fn log_check_fails_on_any_rule_the_cli_dropped_as_invalid() {
    let settings = rendered(WorkerKind::Reviewer);
    let file = settings.to_json().unwrap();
    let good = init_line(
        &settings.allow_rules().join(" "),
        &settings.deny_rules().join(" "),
        "request-review",
    );
    // Even with an initialisation line that matches, a dropped deny fails open.
    for warning in [
        "permission_grant_store.go:571] ignoring invalid deny entry \"write_file(/x\": invalid grant string",
        "permission_grant_store.go:587] ignoring invalid allow entry \"command (cat)\": unknown action",
    ] {
        let log = format!("{good}\n{warning}\n");
        let error = format!("{:#}", verify_settings_log(&file, &log).unwrap_err());
        assert!(error.contains("ignoring invalid"), "{error}");
        // The warning arrives after initialisation in the study, but the check
        // does not depend on the order.
        let log = format!("{warning}\n{good}\n");
        assert!(verify_settings_log(&file, &log).is_err());
        // And it fails before any initialisation line exists.
        assert!(verify_settings_log(&file, warning).is_err());
    }
}

#[test]
fn log_check_re_validates_the_file_it_is_given() {
    let mut value: Value = serde_json::from_str(&rendered(WorkerKind::Standard).to_json().unwrap()).unwrap();
    value["agentMode"] = "auto".into();
    assert!(verify_settings_log(&value.to_string(), "").is_err());
    assert!(verify_settings_log("not json", "").is_err());
}

#[test]
fn log_check_reads_the_run_files_and_treats_a_missing_log_as_not_yet_initialised() {
    let tmp = TempDir::new().unwrap();
    let settings = rendered(WorkerKind::Triage);
    let settings_path = tmp.path().join("session-settings.json");
    fs::write(&settings_path, settings.to_json().unwrap()).unwrap();
    let log_path = tmp.path().join("agy.log");
    assert_eq!(
        verify_settings_log_files(&settings_path, &log_path).unwrap(),
        SettingsLogCheck::NotYetInitialized
    );
    fs::write(
        &log_path,
        format!(
            "{}\n{HOOKS_LOADED_LINE}\n",
            init_line(
                &settings.allow_rules().join(" "),
                &settings.deny_rules().join(" "),
                "request-review",
            )
        ),
    )
    .unwrap();
    assert_eq!(
        verify_settings_log_files(&settings_path, &log_path).unwrap(),
        SettingsLogCheck::ConsistentSoFar
    );
    assert!(verify_settings_log_files(&tmp.path().join("absent.json"), &log_path).is_err());
}

#[test]
fn allow_reads_cover_every_spelling_of_a_var_or_tmp_rooted_workspace() {
    for workspace in ["/var/folders/xx/T/ws-1", "/tmp/ws-1"] {
        let settings = render_settings(&SettingsPlan {
            worker_kind: WorkerKind::Progression,
            workspace: Path::new(workspace),
            boss_data_dir: Some(Path::new(DATA_DIR)),
            hooks_dir: Path::new(HOOKS_DIR),
            control_dir: Path::new(CONTROL_DIR),
            rules_root: Path::new(RULES_ROOT),
        })
        .unwrap();
        let allow = settings.allow_rules();
        let spellings = trust_path_variants(Path::new(workspace));
        assert!(spellings.len() >= 2, "{workspace}: {spellings:?}");
        for spelling in &spellings {
            assert!(
                allow.contains(&format!("read_file({spelling})")),
                "{workspace}: {allow:?}"
            );
        }
        let unique: BTreeSet<&String> = allow.iter().collect();
        assert_eq!(unique.len(), allow.len(), "allow rules must be deduplicated: {allow:?}");
    }
}

#[test]
fn private_cli_settings_file_is_write_fenced_for_every_kind() {
    let fence = "write_file(/boss-test-absent/agy-homes/run-1/process-home/.gemini/antigravity-cli/settings.json)";
    for kind in ALL_KINDS {
        assert!(rendered(kind).deny_rules().iter().any(|rule| rule == fence), "{kind:?}");
    }
}
