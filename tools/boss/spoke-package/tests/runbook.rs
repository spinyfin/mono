//! The manual host enrollment runbook names commands, files, service labels and
//! failure codes. This keeps them equal to the ones the package ships, so a
//! rename in the code fails here instead of misleading the next person on a
//! fresh host.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use boss_spoke_package::bootstrap::Failure;
use boss_spoke_package::install::Layout;
use boss_spoke_package::manifest::Platform;
use boss_spoke_package::service;

fn runbook() -> String {
    fs::read_to_string(std::env::var("RUNBOOK").expect("RUNBOOK")).expect("the runbook is readable")
}

fn layout(platform: Platform) -> Layout {
    Layout {
        platform,
        user: "boss-worker".into(),
        root: None,
    }
}

#[test]
fn it_documents_every_failure_code_with_its_exit_status() {
    let text = runbook();
    let failures = [
        Failure::NotInstalled(String::new()),
        Failure::TokenInvalid(String::new()),
        Failure::AlreadyEnrolled,
        Failure::HubUnreachable(String::new()),
        Failure::EnrollmentRefused(String::new()),
        Failure::CredentialsUnwritable(String::new()),
        Failure::ServiceNotRunning(String::new()),
        Failure::PreflightFailed {
            failed: vec![],
            output: String::new(),
        },
        Failure::Unexpected(String::new()),
    ];
    for failure in &failures {
        let row = format!("| `{}`", failure.code());
        let line = text
            .lines()
            .find(|line| line.starts_with(&row))
            .unwrap_or_else(|| panic!("no row for {}", failure.code()));
        let columns: Vec<_> = line.split('|').map(str::trim).collect();
        assert_eq!(
            columns[2],
            failure.exit_code().to_string(),
            "exit status of {} in the runbook",
            failure.code()
        );
    }
}

#[test]
fn it_names_the_paths_and_service_labels_of_both_platforms() {
    let text = runbook();
    for platform in [
        Platform::parse("macos-arm64").unwrap(),
        Platform::parse("linux-amd64").unwrap(),
    ] {
        let layout = layout(platform);
        for path in [
            layout.prefix(),
            layout.definition(),
            layout.helper_definition(),
            layout.config_path(),
            layout.current().join("bin/boss-spoke-package"),
        ] {
            assert!(text.contains(&path.display().to_string()), "{}", path.display());
        }
    }
    let layout = layout(Platform::parse("linux-amd64").unwrap());
    for path in [layout.status_path(), layout.pinned_key_path()] {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            text.contains(&name) || text.contains(&path.display().to_string()),
            "{name}"
        );
    }
    for label in [
        service::LAUNCHD_LABEL,
        service::LAUNCHD_UPDATE_LABEL,
        service::SYSTEMD_UNIT,
        service::SYSTEMD_UPDATE_PATH,
        service::SYSTEMD_UPDATE_SERVICE,
    ] {
        assert!(text.contains(label), "{label}");
    }
}

fn parse_installer_invocation(text: &str) -> Option<(String, BTreeSet<String>)> {
    let logical = text.trim();
    for tool in [
        "/opt/boss-spoke/current/bin/boss-spoke-package",
        "boss-spoke-package-<platform>",
    ] {
        let Some(rest) = logical
            .strip_prefix(&format!("./{tool} "))
            .or_else(|| logical.strip_prefix(&format!("{tool} ")))
        else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let name = words.next().unwrap_or_default().to_owned();
        let flags = words
            .filter_map(|word| word.strip_prefix("--"))
            .map(str::to_owned)
            .collect();
        return Some((name, flags));
    }
    None
}

fn inline_code_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        if rest.starts_with("``") {
            // A fenced block; skip through the closing fence.
            rest = rest.trim_start_matches('`');
            if let Some(end) = rest.find("```") {
                rest = &rest[end + 3..];
            } else {
                break;
            }
            continue;
        }
        let Some(end) = rest.find('`') else {
            break;
        };
        spans.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    spans
}

/// Every installer command in the runbook, as `(subcommand, flags)`. A command
/// continued over several lines with a trailing backslash is one command.
/// Inline code spans are parsed the same way, so flags documented only in
/// prose still get checked.
fn installer_commands(text: &str) -> Vec<(String, BTreeSet<String>)> {
    let mut commands = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let mut logical = line.trim().to_owned();
        while logical.ends_with('\\') {
            logical.pop();
            logical.push(' ');
            match lines.next() {
                Some(next) => logical.push_str(next.trim()),
                None => break,
            }
        }
        if let Some(command) = parse_installer_invocation(&logical) {
            commands.push(command);
        }
    }
    for span in inline_code_spans(text) {
        if let Some(command) = parse_installer_invocation(span) {
            commands.push(command);
        } else if let Some(rest) = span.trim().strip_prefix("enroll ") {
            let flags = rest
                .split_whitespace()
                .filter_map(|word| word.strip_prefix("--"))
                .map(str::to_owned)
                .collect();
            commands.push(("enroll".into(), flags));
        }
    }
    commands
}

/// True when `help` documents `--flag` as its own long option, not as a prefix
/// of a longer one (`--token` must not match `--token-file`).
fn help_has_flag(help: &str, flag: &str) -> bool {
    let needle = format!("--{flag}");
    help.match_indices(&needle).any(|(index, _)| {
        help.as_bytes()
            .get(index + needle.len())
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'-')
    })
}

#[test]
fn every_installer_subcommand_and_flag_it_runs_exists() {
    let text = runbook();
    let commands = installer_commands(&text);
    let used: BTreeSet<&str> = commands.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        used.contains("install") && used.contains("enroll") && used.contains("preflight"),
        "{used:?}"
    );
    let flags: BTreeSet<&str> = commands
        .iter()
        .flat_map(|(_, flags)| flags.iter().map(String::as_str))
        .collect();
    for required in ["replace-identity", "token-stdin"] {
        assert!(
            flags.contains(required),
            "the runbook never shows --{required}; collected {flags:?}"
        );
    }
    let installer = std::env::var("INSTALLER_BIN").expect("INSTALLER_BIN");
    for (name, flags) in &commands {
        let help = std::process::Command::new(&installer)
            .args([name.as_str(), "--help"])
            .output()
            .expect("the installer runs");
        assert!(
            help.status.success(),
            "the runbook runs an unknown subcommand {name}: {}",
            String::from_utf8_lossy(&help.stderr)
        );
        let help = String::from_utf8_lossy(&help.stdout);
        for flag in flags {
            assert!(
                help_has_flag(&help, flag),
                "`{name}` has no --{flag}; its help is:\n{help}"
            );
        }
    }
}

#[test]
fn it_does_not_offer_ssh_as_a_way_to_enroll() {
    let text = runbook();
    let offered: Vec<_> = text
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("ssh ") || line.starts_with("$ ssh ")
        })
        .collect();
    assert!(offered.is_empty(), "{offered:?}");
    assert!(Path::new(&std::env::var("RUNBOOK").unwrap()).is_file());
}
