use super::*;

use std::path::PathBuf;

fn environment(root: &Path) -> AgyProcessEnvironment {
    AgyProcessEnvironment::resolve(
        &root.join("run one/process-home"),
        &root.join("run one/control/agy.log"),
        &root.join("run one/control/session-settings.json"),
        root.join("bin/agy"),
    )
    .unwrap()
}

#[test]
fn model_id_rung_is_the_suffix_only() {
    assert_eq!(model_id_rung("gemini-4-hydrogen-xhigh"), Some("xhigh"));
    assert_eq!(model_id_rung("gemini-3.8-flash-low"), Some("low"));
    assert_eq!(model_id_rung("gemini-4-carbon"), None);
    assert_eq!(model_id_rung("gemini-4-hydrogen-max"), None);
    assert_eq!(model_id_rung("low"), None);
}

#[test]
fn effort_never_contradicts_the_model_id_rung() {
    // Agreeing with the model id: passed through.
    assert_eq!(effort_flag("gemini-4-hydrogen-xhigh", Some("xhigh")), Some("xhigh"));
    assert_eq!(effort_flag("gemini-4-carbon-high", Some("high")), Some("high"));
    // Disagreeing: the model id wins and no flag is passed.
    assert_eq!(effort_flag("gemini-4-hydrogen-xhigh", Some("low")), None);
    assert_eq!(effort_flag("gemini-4-hydrogen-xhigh", Some("max")), None);
    assert_eq!(effort_flag("gemini-4-carbon-high", Some("xhigh")), None);
    // No rung in the id: the menu's value is the only source.
    assert_eq!(effort_flag("gemini-4-carbon", Some("medium")), Some("medium"));
    // Absent or unrecognised: nothing, rather than a flag the CLI rejects.
    assert_eq!(effort_flag("gemini-4-carbon", None), None);
    assert_eq!(effort_flag("gemini-4-carbon", Some("extreme")), None);
}

#[test]
fn command_runs_the_launcher_over_the_owned_binary_with_quoted_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let environment = environment(tmp.path());
    let control = tmp.path().join("run one/control");
    let command = build_agy_pane_command(&environment, &control, "gemini-4-hydrogen-xhigh", Some("xhigh"));

    let launcher = format!("python3 -I '{}'/initial-prompt.py ", control.display());
    assert!(command.starts_with(&launcher), "{command}");
    assert!(command.ends_with('\n'), "{command:?}");
    let agy = format!("'{}'", tmp.path().join("bin/agy").display());
    assert!(command[launcher.len()..].starts_with(&format!("{agy} ")), "{command}");
    for expected in [
        format!(
            "'--log-file' '{}'",
            tmp.path().join("run one/control/agy.log").display()
        ),
        format!("'--add-dir' '{}'", tmp.path().join("run one/rules-root").display()),
        "--model 'gemini-4-hydrogen-xhigh'".to_owned(),
        "--effort 'xhigh'".to_owned(),
    ] {
        assert!(command.contains(&expected), "missing {expected}: {command}");
    }
}

#[test]
fn command_never_puts_the_brief_or_interactive_prompt_flag_in_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let environment = environment(tmp.path());
    let command = build_agy_pane_command(&environment, &tmp.path().join("c"), "gemini-4-carbon-high", None);
    for forbidden in [
        " -i ",
        "--interactive",
        "$(cat",
        "--dangerously",
        " --mode ",
        "--sandbox",
        "--settings",
    ] {
        assert!(!command.contains(forbidden), "{forbidden:?} in {command}");
    }
    assert!(!command.contains("--effort"), "{command}");
    // The permission-flag merge keys off these tokens; the command must not
    // contain either, or the settings flag would land mid-argument.
    assert!(!command.contains(" -m "), "{command}");
    assert!(!command.contains(" < "), "{command}");
}

#[test]
fn a_contradicting_effort_is_dropped_from_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let environment = environment(tmp.path());
    let command = build_agy_pane_command(
        &environment,
        &PathBuf::from("/c"),
        "gemini-4-hydrogen-low",
        Some("high"),
    );
    assert!(command.contains("--model 'gemini-4-hydrogen-low'"), "{command}");
    assert!(!command.contains("--effort"), "{command}");
}

#[test]
fn every_selected_gemini_family_launches_with_matching_thinking_level() {
    let tmp = tempfile::tempdir().unwrap();
    let environment = environment(tmp.path());
    let menu = &super::super::AGY_DESCRIPTOR.model_menu;
    for (family, expected_rung) in [
        ("gemini-4-hydrogen", "xhigh"),
        ("gemini-4-neon", "xhigh"),
        ("gemini-4-carbon", "high"),
    ] {
        let (model, effort) = menu.resolve_model_effort.unwrap()(family, Some("xhigh"));
        let command = build_agy_pane_command(&environment, &tmp.path().join("c"), &model, effort);
        assert!(
            command.contains(&format!(
                "--model '{family}-{expected_rung}' --effort '{expected_rung}'"
            )),
            "{command}"
        );
    }
}
