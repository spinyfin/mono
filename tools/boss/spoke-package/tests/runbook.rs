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

#[test]
fn every_installer_subcommand_it_runs_exists() {
    let text = runbook();
    let known: BTreeSet<&str> = [
        "keygen",
        "build",
        "sign",
        "fingerprint",
        "verify",
        "install",
        "enroll",
        "preflight",
        "apply-update",
    ]
    .into();
    let mut used = BTreeSet::new();
    for line in text.lines() {
        for tool in [
            "/opt/boss-spoke/current/bin/boss-spoke-package",
            "boss-spoke-package-<platform>",
        ] {
            if let Some(rest) = line
                .trim_start()
                .strip_prefix(&format!("./{tool} "))
                .or_else(|| line.trim_start().strip_prefix(&format!("{tool} ")))
            {
                used.insert(rest.split_whitespace().next().unwrap_or_default().to_owned());
            }
        }
    }
    assert!(
        used.contains("install") && used.contains("enroll") && used.contains("preflight"),
        "{used:?}"
    );
    for subcommand in &used {
        assert!(
            known.contains(subcommand.as_str()),
            "the runbook runs an unknown subcommand {subcommand}"
        );
    }
}

#[test]
fn every_bossctl_subcommand_it_runs_is_one_the_hub_operator_has() {
    let text = runbook();
    let known = ["token", "list", "revoke"];
    let mut used = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("bossctl enrollment ") {
            used.insert(rest.split_whitespace().next().unwrap_or_default().to_owned());
        }
    }
    for want in known {
        assert!(
            used.contains(want),
            "the runbook never shows `bossctl enrollment {want}`"
        );
    }
    for subcommand in &used {
        assert!(
            known.contains(&subcommand.as_str()),
            "unknown bossctl subcommand {subcommand}"
        );
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
