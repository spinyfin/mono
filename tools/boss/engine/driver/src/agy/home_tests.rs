use super::*;
use crate::AgentDriver;
use crate::scoped_home::trust_path_variants;
use crate::test_support::home_override;
use tempfile::TempDir;

#[test]
fn provisioning_refuses_aliases_before_writing_host_files() {
    for alias in [
        "process-home",
        "process-home/.gemini",
        "process-home/.gemini/antigravity-cli",
        "control",
        "rules-root",
    ] {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let target = tmp.path().join("host-state");
        fs::create_dir_all(target.join("cache")).unwrap();
        fs::write(target.join("settings.json"), b"host settings").unwrap();
        fs::write(target.join("cache/onboarding.json"), b"host onboarding").unwrap();
        let _guard = env_for_provision(&homes);
        let link = homes.join("run-alias").join(alias);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = provision_agy_home(&workspace, "prompt", "run-alias").unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs::read(target.join("settings.json")).unwrap(), b"host settings");
        assert_eq!(
            fs::read(target.join("cache/onboarding.json")).unwrap(),
            b"host onboarding"
        );
        assert!(!target.join("config/hooks.json").exists());
    }
}

#[test]
fn spawn_posture_rejects_intermediate_alias_after_provision() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-swap").unwrap();
    let private = runtime.process_home.join(".gemini/antigravity-cli");
    let target = tmp.path().join("shared");
    fs::rename(&private, &target).unwrap();
    std::os::unix::fs::symlink(&target, &private).unwrap();
    assert!(assert_agy_posture(&runtime.process_home, tmp.path()).is_err());
}

use super::super::test_environment::{env_for_provision, env_for_provision_with_home_override, transcript_store_for};

#[test]
fn process_home_for_run_is_under_homes_root() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let home = process_home_for_run("run-abc").unwrap();
    assert!(home.starts_with(tmp.path()));
    assert!(home.ends_with("process-home"));
}

#[test]
fn onboarding_json_skips_the_screen_without_answering_consent() {
    let parsed: serde_json::Value = serde_json::from_str(render_onboarding_json()).unwrap();
    assert_eq!(parsed["onboardingComplete"], true);
    assert_eq!(parsed["consumerOnboardingComplete"], true);
    assert!(parsed.get("enableTelemetry").is_none());
}

#[test]
fn provision_creates_private_tree_and_keychain_bridge() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let real_home = tmp.path().join("real-home");
    let keychain_dir = real_home.join("Library/Keychains");
    fs::create_dir_all(&keychain_dir).unwrap();
    let keychain_file = keychain_dir.join("login.keychain-db");
    fs::write(&keychain_file, b"dummy-keychain").unwrap();
    fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
    fs::write(
        real_home.join(".gemini/antigravity-cli/settings.json"),
        "{\"enableTelemetry\": true}\n",
    )
    .unwrap();
    let _home = home_override(&real_home);
    let _guard = env_for_provision_with_home_override(&homes);

    let runtime = provision_agy_home(&workspace, "hello prompt", "run-prov-1").unwrap();
    assert!(runtime.process_home.starts_with(&homes));
    assert_ne!(runtime.process_home, real_home);

    let gemini = runtime.process_home.join(".gemini");
    assert!(gemini.join("antigravity-cli/cache/onboarding.json").is_file());
    assert!(gemini.join("antigravity-cli/settings.json").is_file());
    assert!(gemini.join("config/hooks.json").is_file());
    let rules_dir = homes.join("run-prov-1/rules-root");
    assert!(rules_dir.is_dir());
    assert_eq!(
        agent_rules_path_for_run("run-prov-1").unwrap(),
        rules_dir.join("AGENTS.md"),
        "an always-on rule needs no frontmatter only at the root of the added directory"
    );
    assert_eq!(
        agent_rules_path_for_run("run-prov-1").unwrap().parent(),
        Some(rules_dir.as_path())
    );
    assert_eq!(
        super::super::AgyDriver::default()
            .agent_rules_destination(&workspace, "run-prov-1")
            .parent(),
        Some(rules_dir.as_path())
    );
    assert!(!gemini.join("AGENTS.md").exists());
    assert!(!gemini.join("settings.json").exists());
    assert!(!gemini.join("trustedFolders.json").exists());
    assert!(runtime.control_dir.join("initial-prompt.txt").is_file());
    assert_eq!(
        fs::read_to_string(runtime.control_dir.join("initial-prompt.txt")).unwrap(),
        "hello prompt"
    );
    assert_eq!(
        read_workspace_path_stamp(&runtime.control_dir).unwrap(),
        fs::canonicalize(&workspace).unwrap()
    );

    let bridged = runtime.process_home.join(LOGIN_KEYCHAIN_LEAF);
    let meta = fs::symlink_metadata(&bridged).expect("login keychain bridge");
    assert!(meta.file_type().is_symlink());
    assert_eq!(fs::read_link(&bridged).unwrap(), keychain_file);

    // Host ~/.gemini must not be the run's .gemini.
    let host_gemini = fs::canonicalize(real_home.join(".gemini")).unwrap();
    let run_gemini = fs::canonicalize(&gemini).unwrap();
    assert_ne!(host_gemini, run_gemini);

    let settings = fs::read_to_string(gemini.join("antigravity-cli/settings.json")).unwrap();
    assert!(!settings.contains("enableTelemetry"));
    assert!(settings.contains("trustedWorkspaces"));
    let private: serde_json::Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(private["queuedMessages"], "queue");
    let session: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&runtime.session_settings_path).unwrap()).unwrap();
    assert_eq!(session["queuedMessages"], "queue");

    assert_agy_posture(&runtime.process_home, &workspace).unwrap();
}

#[test]
fn posture_refuses_unpinned_or_immediate_input() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-queue-mode").unwrap();
    let path = runtime.process_home.join(".gemini/antigravity-cli/settings.json");
    let original: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for mode in [None, Some("send-immediately")] {
        let mut settings = original.clone();
        settings.as_object_mut().unwrap().remove("queuedMessages");
        if let Some(mode) = mode {
            settings["queuedMessages"] = mode.into();
        }
        fs::write(&path, settings.to_string()).unwrap();
        let error = assert_agy_posture(&runtime.process_home, tmp.path()).unwrap_err();
        assert!(error.to_string().contains("queuedMessages"), "{error:#}");
        // The refusal says what the file holds, so it can be diagnosed from
        // the spawn failure alone once the home has been reclaimed.
        let found = mode.map_or("found no queuedMessages key".to_owned(), |mode| {
            format!("found \"{mode}\"")
        });
        assert!(error.to_string().contains(&found), "{error:#}");
    }
}

#[test]
fn settings_key_changes_names_what_a_cli_start_did_to_the_file() {
    let before = r#"{"queuedMessages": "queue", "trustedWorkspaces": ["/ws"], "toolPermission": "strict"}"#;
    let after = r#"{"trustedWorkspaces": ["/ws"], "toolPermission": "request-review", "colorScheme": "dark"}"#;
    assert_eq!(
        settings_key_changes(before, after),
        (
            vec!["queuedMessages".to_owned()],
            vec!["toolPermission".to_owned()],
            vec!["colorScheme".to_owned()]
        )
    );
    // A file the CLI left unreadable has lost every key.
    let (dropped, changed, added) = settings_key_changes(before, "{broken");
    assert_eq!(dropped.len(), 3);
    assert!(changed.is_empty() && added.is_empty());
}

#[test]
fn posture_fails_when_private_state_is_symlinked_out() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let real_home = tmp.path().join("real-home");
    fs::create_dir_all(&real_home).unwrap();
    let _home = home_override(&real_home);
    let _guard = env_for_provision_with_home_override(&homes);

    let runtime = provision_agy_home(&workspace, "p", "run-leak-1").unwrap();
    let dest = runtime
        .process_home
        .join(".gemini/antigravity-cli/conversation_summaries.db");
    let outside = tmp.path().join("shared-db");
    fs::write(&outside, b"shared").unwrap();
    let _ = fs::remove_file(&dest);
    std::os::unix::fs::symlink(&outside, &dest).unwrap();

    let err = assert_agy_posture(&runtime.process_home, &workspace)
        .unwrap_err()
        .to_string();
    assert!(err.contains("symlink"), "{err}");
    assert!(err.contains("conversation_summaries.db"), "{err}");
}

#[test]
fn two_runs_do_not_share_private_state_paths() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let a = process_home_for_run_with(&homes, "run-a");
    let b = process_home_for_run_with(&homes, "run-b");
    assert_ne!(a, b);
    assert_ne!(
        a.join(".gemini/antigravity-cli/settings.json"),
        b.join(".gemini/antigravity-cli/settings.json")
    );
}

fn process_home_for_run_with(homes: &Path, run_id: &str) -> PathBuf {
    let _guard = env_for_provision(homes);
    process_home_for_run(run_id).unwrap()
}

#[test]
fn reclaim_keeps_control_and_legacy_transcripts_and_removes_process_settings() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);
    let process_home = process_home_for_run("run-retain-1").unwrap();
    let container = process_home.parent().unwrap();
    let cli = process_home.join(".gemini/antigravity-cli");
    let conversations = cli.join("conversations/session.json");
    let brain = cli.join("brain/cid/.system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(conversations.parent().unwrap()).unwrap();
    fs::create_dir_all(brain.parent().unwrap()).unwrap();
    fs::write(&conversations, b"keep-conversations").unwrap();
    fs::write(&brain, b"keep-brain").unwrap();
    fs::write(cli.join("settings.json"), b"drop-settings").unwrap();
    fs::write(cli.join("log"), b"drop-log").unwrap();
    let rules_root = container.join("rules-root");
    fs::create_dir_all(&rules_root).unwrap();
    fs::write(rules_root.join("AGENTS.md"), b"drop-rules").unwrap();
    let control = container.join("control");
    fs::create_dir_all(&control).unwrap();
    fs::write(control.join("session-settings.json"), b"keep-control").unwrap();
    fs::write(control.join("agy.log"), b"startup failure").unwrap();

    reclaim_agy_run(&process_home).unwrap();

    assert_eq!(fs::read_to_string(&conversations).unwrap(), "keep-conversations");
    assert_eq!(fs::read_to_string(&brain).unwrap(), "keep-brain");
    assert!(!cli.join("settings.json").exists());
    assert!(!cli.join("log").exists());
    assert!(!container.join("rules-root").exists());
    assert_eq!(
        fs::read_to_string(control.join("session-settings.json")).unwrap(),
        "keep-control"
    );
    assert_eq!(fs::read_to_string(control.join("agy.log")).unwrap(), "startup failure");
}

#[test]
fn safe_to_delete_refuses_host_home() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let err = assert_agy_home_safe_to_delete(Path::new("/Users/someone/.gemini"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside"), "{err}");
}

#[test]
fn models_probe_initialization_cannot_contaminate_session_log() {
    use super::super::permissions::{SettingsLogCheck, SettingsPlan, render_settings, verify_settings_log_files};
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let binary = tmp.path().join("fake-agy");
    fs::write(
        &binary,
        r#"#!/bin/sh
test "$1" = "--log-file" || exit 1
printf '%s\n' 'CLI settings initialized: permissions=&{Allow:[] Deny:[] Ask:[]}, toolPermission=request-review' >> "$2"
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let log = tmp.path().join("agy.log");
    let settings_path = tmp.path().join("session-settings.json");
    let environment =
        AgyProcessEnvironment::resolve(&tmp.path().join("process-home"), &log, &settings_path, binary).unwrap();
    probe_agy_models(&environment).unwrap();
    assert!(!log.exists());
    let probe_log = fs::read_to_string(tmp.path().join("agy-models-probe.log")).unwrap();
    let settings = render_settings(&SettingsPlan {
        worker_kind: crate::WorkerKind::Triage,
        workspace: Path::new("/workspace"),
        boss_data_dir: None,
        hooks_dir: &tmp.path().join("hooks"),
        control_dir: &tmp.path().join("control"),
        rules_root: &tmp.path().join("rules-root"),
    })
    .unwrap();
    fs::write(&settings_path, settings.to_json().unwrap()).unwrap();
    assert!(super::super::permissions::verify_settings_log(&settings.to_json().unwrap(), &probe_log).is_err());
    fs::write(
        &log,
        format!(
            "CLI settings initialized: permissions=&{{Allow:[] Deny:[{}] Ask:[]}}, toolPermission=request-review\n{}\n",
            settings.deny_rules().join(" "),
            super::super::permissions::HOOKS_LOADED_LINE,
        ),
    )
    .unwrap();
    assert_eq!(
        verify_settings_log_files(&settings_path, &log).unwrap(),
        SettingsLogCheck::ConsistentSoFar
    );
}

// ── Durable transcript store ────────────────────────────────────────────────

const TRANSCRIPT_LEAF: &str = ".system_generated/logs/transcript_full.jsonl";

fn write_conversation(process_home: &Path, cid: &str, body: &str) -> PathBuf {
    let transcript = process_home
        .join(".gemini/antigravity-cli/brain")
        .join(cid)
        .join(TRANSCRIPT_LEAF);
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, body).unwrap();
    transcript
}

fn durable_dir(homes: &Path, run_id: &str) -> PathBuf {
    crate::transcript_store::durable_sessions_dir(&transcript_store_for(homes), "agy", run_id).unwrap()
}

#[test]
fn provisioning_links_brain_into_the_durable_transcript_store() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-durable").unwrap();

    let brain = runtime.process_home.join(".gemini/antigravity-cli/brain");
    assert!(fs::symlink_metadata(&brain).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::canonicalize(&brain).unwrap(),
        fs::canonicalize(durable_dir(&homes, "run-durable")).unwrap()
    );
    // The CLI creates `brain/<cid>/…` itself; it lands in the store.
    write_conversation(&runtime.process_home, "cid-1", "row\n");
    assert_eq!(
        fs::read_to_string(durable_dir(&homes, "run-durable").join("cid-1").join(TRANSCRIPT_LEAF)).unwrap(),
        "row\n"
    );
    // The private tree is still private: the link is the only symlink in it.
    assert_agy_posture(&runtime.process_home, tmp.path()).unwrap();
}

#[test]
fn reprovisioning_a_durable_home_keeps_the_verified_link() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let first = provision_agy_home(tmp.path(), "prompt", "run-again").unwrap();
    let transcript = write_conversation(&first.process_home, "cid-1", "kept\n");
    let second = provision_agy_home(tmp.path(), "prompt", "run-again").unwrap();
    assert_eq!(second.process_home, first.process_home);
    assert_eq!(fs::read_to_string(transcript).unwrap(), "kept\n");
}

#[test]
fn provisioning_refuses_to_hide_a_legacy_transcript_behind_the_link() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let process_home = process_home_for_run("run-legacy-brain").unwrap();
    write_conversation(&process_home, "cid-1", "the only copy\n");

    let error = provision_agy_home(tmp.path(), "prompt", "run-legacy-brain").unwrap_err();
    assert!(format!("{error:#}").contains("non-empty"), "{error:#}");
}

#[test]
fn a_retargeted_brain_link_is_refused_everywhere_it_is_read() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-retarget").unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let brain = runtime.process_home.join(".gemini/antigravity-cli/brain");
    fs::remove_file(&brain).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &brain).unwrap();

    for error in [
        assert_agy_posture(&runtime.process_home, tmp.path()).unwrap_err(),
        reclaim_agy_run(&runtime.process_home).unwrap_err(),
        transcript_containment_root("run-retarget").unwrap_err(),
    ] {
        assert!(format!("{error:#}").contains("targets"), "{error:#}");
    }
    assert!(elsewhere.exists(), "refusal must not touch the link's target");
}

#[test]
fn only_the_brain_link_is_exempt_from_the_private_tree_symlink_ban() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-ban").unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    for planted in [
        ".gemini/antigravity-cli/conversations",
        ".gemini/antigravity-cli/knowledge",
        ".gemini/planted",
    ] {
        let link = runtime.process_home.join(planted);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let error = assert_agy_posture(&runtime.process_home, tmp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{planted}: {error:#}");
        fs::remove_file(&link).unwrap();
    }
    assert_agy_posture(&runtime.process_home, tmp.path()).unwrap();
}

#[test]
fn a_durable_home_retains_control_until_the_sweep_while_its_transcript_remains() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-reclaim").unwrap();
    let container = runtime.process_home.parent().unwrap().to_path_buf();
    write_conversation(&runtime.process_home, "cid-1", "durable\n");
    let conversations = runtime
        .process_home
        .join(".gemini/antigravity-cli/conversations/cid-1.db");
    fs::create_dir_all(conversations.parent().unwrap()).unwrap();
    fs::write(&conversations, b"cli state, not Boss evidence").unwrap();

    assert!(
        !contains_agy_transcripts(&container),
        "retention must not hold a container whose transcript is already durable"
    );
    reclaim_agy_run(&runtime.process_home).unwrap();

    assert_eq!(
        fs::read_dir(&container).unwrap().count(),
        1,
        "only the control directory is retained for the bounded sweep"
    );
    assert!(runtime.control_dir.is_dir());
    assert!(!runtime.process_home.exists());
    assert_eq!(
        fs::read_to_string(durable_dir(&homes, "run-reclaim").join("cid-1").join(TRANSCRIPT_LEAF)).unwrap(),
        "durable\n"
    );
}

#[test]
fn a_legacy_home_is_still_protected_until_its_only_copy_is_gone() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let process_home = process_home_for_run("run-legacy-protect").unwrap();
    let container = process_home.parent().unwrap().to_path_buf();
    assert!(
        !contains_agy_transcripts(&container),
        "an empty container holds nothing"
    );
    write_conversation(&process_home, "cid-1", "only copy\n");
    assert!(contains_agy_transcripts(&container));
}

#[test]
fn containment_root_follows_the_durable_store_for_live_reclaimed_and_legacy_homes() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);

    let runtime = provision_agy_home(tmp.path(), "prompt", "run-contain").unwrap();
    let expected = fs::canonicalize(durable_dir(&homes, "run-contain")).unwrap();
    assert_eq!(
        transcript_containment_root("run-contain").unwrap(),
        Some(expected.clone())
    );
    reclaim_agy_run(&runtime.process_home).unwrap();
    assert_eq!(
        transcript_containment_root("run-contain").unwrap(),
        Some(expected),
        "the durable directory is the root after the home is gone"
    );

    let legacy = process_home_for_run("run-contain-legacy").unwrap();
    write_conversation(&legacy, "cid-1", "x\n");
    assert_eq!(transcript_containment_root("run-contain-legacy").unwrap(), None);
    assert_eq!(transcript_containment_root("run-never-provisioned").unwrap(), None);
}

#[test]
fn the_durable_directory_is_filed_under_the_sanitized_run_id() {
    // The container is named after the sanitized id; the store, the layout
    // check and the stamped-path rewrite all derive the same name from it.
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);
    let runtime = provision_agy_home(tmp.path(), "prompt", "run/odd id").unwrap();
    assert!(runtime.process_home.ends_with("run_odd_id/process-home"));
    assert_eq!(
        fs::canonicalize(runtime.process_home.join(".gemini/antigravity-cli/brain")).unwrap(),
        fs::canonicalize(durable_dir(&homes, "run_odd_id")).unwrap()
    );
    assert_agy_posture(&runtime.process_home, tmp.path()).unwrap();
    assert!(transcript_containment_root("run/odd id").unwrap().is_some());
}

fn trusted_paths(json: &serde_json::Value) -> Vec<String> {
    json["trustedWorkspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn assert_home_pre_trusts(runtime: &AgyRuntimeState, workspace: &Path) {
    let required = trust_path_variants(workspace);
    assert!(
        !required.is_empty(),
        "working directory {} has no trust spellings",
        workspace.display()
    );
    for path in [
        runtime.process_home.join(".gemini/antigravity-cli/settings.json"),
        runtime.session_settings_path.clone(),
    ] {
        let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let trusted = trusted_paths(&json);
        for spelling in &required {
            assert!(
                trusted.contains(spelling),
                "{} missing {spelling:?} in {trusted:?}",
                path.display()
            );
        }
    }
    assert_agy_posture(&runtime.process_home, workspace).unwrap();
    for spelling in &required {
        assert_agy_posture(&runtime.process_home, Path::new(spelling)).unwrap();
    }
}

#[test]
fn rendered_home_pre_trusts_a_cube_workspace_and_a_progression_scratch_dir() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let _guard = env_for_provision(&homes);

    let cube = tmp.path().join("workspaces/impl-ws");
    fs::create_dir_all(&cube).unwrap();
    let scratch = tmp.path().join("boss-progression-scratch/exec_folder_trust_1");
    fs::create_dir_all(&scratch).unwrap();

    let cube_home = provision_agy_home(&cube, "prompt", "run-cube").unwrap();
    assert_home_pre_trusts(&cube_home, &cube);

    let scratch_home = provision_agy_home(&scratch, "prompt", "run-progression").unwrap();
    assert_home_pre_trusts(&scratch_home, &scratch);
}

#[test]
fn posture_refuses_a_working_directory_that_is_not_fully_pre_trusted() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let workspace = tmp.path().join("boss-progression-scratch/exec_untrusted");
    fs::create_dir_all(&workspace).unwrap();
    let runtime = provision_agy_home(&workspace, "prompt", "run-partial-trust").unwrap();
    let required = trust_path_variants(&workspace);
    let private = runtime.process_home.join(".gemini/antigravity-cli/settings.json");

    let mut empty: serde_json::Value = serde_json::from_str(&fs::read_to_string(&private).unwrap()).unwrap();
    empty["trustedWorkspaces"] = serde_json::json!([]);
    fs::write(&private, empty.to_string()).unwrap();
    let error = assert_agy_posture(&runtime.process_home, &workspace)
        .unwrap_err()
        .to_string();
    assert!(error.contains("folder-trust dialog"), "{error}");
    assert!(error.contains("private CLI settings.json"), "{error}");

    // Restore the private file, then drop spellings only from `--settings`,
    // which is the document the pane CLI actually reads.
    super::write_private_cli_settings(&runtime.process_home, &workspace).unwrap();
    let mut session: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&runtime.session_settings_path).unwrap()).unwrap();
    session["trustedWorkspaces"] = if required.len() > 1 {
        serde_json::json!([fs::canonicalize(&workspace).unwrap().display().to_string()])
    } else {
        serde_json::json!([])
    };
    fs::write(&runtime.session_settings_path, session.to_string()).unwrap();
    let error = assert_agy_posture(&runtime.process_home, &workspace)
        .unwrap_err()
        .to_string();
    assert!(error.contains("folder-trust dialog"), "{error}");
    assert!(error.contains("session --settings file"), "{error}");
}
