use std::path::PathBuf;
use std::time::Duration;

use super::*;

const WS: &str = "/ws/mono-agent-001";

fn ws_process(pid: i32, ppid: i32, command: &str) -> LiveProcess {
    fake_process(pid, ppid, command, Some(WS))
}

/// The incident shape: a pane shell (100) → agent (200) → detached tool
/// shell (300, own group, since exited) → `( while … ) &` subshell (400)
/// re-parented to launchd, a `/bin/sh -c :` child of it (401), an engine
/// test binary stranded in the bazel sandbox (500), the workspace's bazel
/// server (600) with a live action child (601), a human shell `cd`'d into
/// the workspace (700), and the caller (900) whose parent (901) also sits
/// in the workspace.
fn incident_table() -> StaticProcessTable {
    let mut bazel = ws_process(600, 1, "java");
    bazel.argv = vec![
        "bazel(mono-agent-001)".into(),
        "--output_base=/cache/_bazel_me/abc".into(),
        format!("--workspace_directory={WS}"),
    ];
    let mut human = ws_process(700, 1, "zsh");
    human.has_controlling_terminal = true;
    let mut pane = ws_process(100, 50, "zsh");
    pane.has_controlling_terminal = true;
    let mut agent = ws_process(200, 100, "claude");
    agent.has_controlling_terminal = true;
    let processes = vec![
        pane,
        agent,
        ws_process(400, 1, "zsh"),
        ws_process(401, 400, "sh"),
        fake_process(
            500,
            1,
            "engine_lib_test",
            Some("/cache/_bazel_me/abc/sandbox/7/execroot/_main"),
        ),
        bazel,
        fake_process(
            601,
            600,
            "sandbox-exec",
            Some("/cache/_bazel_me/abc/sandbox/8/execroot/_main"),
        ),
        human,
        ws_process(900, 901, "cube"),
        ws_process(901, 1, "zsh"),
        fake_process(800, 1, "unrelated", Some("/elsewhere")),
    ];
    StaticProcessTable::new(processes, 900)
}

fn pids(owned: &[OwnedProcess]) -> Vec<i32> {
    owned.iter().map(|o| o.process.pid).collect()
}

#[test]
fn scan_owns_orphans_by_working_directory_and_descendants_by_ancestry() {
    let table = incident_table();
    let scope = Scope::for_workspace(WS).with_root(100);
    let scan = scan(&table, &scope).unwrap();
    assert_eq!(pids(&scan.owned), vec![100, 200, 400, 401, 500]);
    let by_pid = |pid: i32| {
        scan.owned
            .iter()
            .find(|o| o.process.pid == pid)
            .unwrap()
            .ownership
            .clone()
    };
    assert_eq!(by_pid(100), Ownership::Descendant { root: 100 });
    assert_eq!(by_pid(200), Ownership::Descendant { root: 100 });
    assert_eq!(
        by_pid(400),
        Ownership::WorkingDirectory {
            inside: PathBuf::from(WS)
        }
    );
    // The stranded test binary is owned through the bazel server's output base.
    assert_eq!(
        by_pid(500),
        Ownership::WorkingDirectory {
            inside: PathBuf::from("/cache/_bazel_me/abc")
        }
    );
    assert!(scan.paths.contains(&PathBuf::from("/cache/_bazel_me/abc")));
}

#[test]
fn scan_spares_the_bazel_server_its_children_and_interactive_shells() {
    let table = incident_table();
    let scan = scan(&table, &Scope::for_workspace(WS)).unwrap();
    let spared: Vec<(i32, SpareReason)> = scan.spared.iter().map(|s| (s.process.pid, s.reason.clone())).collect();
    // Without a root, the pane shell and agent — which hold the pane's pty —
    // are indistinguishable from a human's shell and are spared too; the
    // engine passes the pane pid as a root precisely so they are not.
    assert_eq!(
        spared,
        vec![
            (100, SpareReason::Interactive),
            (200, SpareReason::Interactive),
            (600, SpareReason::BazelServer),
            (601, SpareReason::BazelServerDescendant { server: 600 }),
            (700, SpareReason::Interactive),
        ]
    );
    assert_eq!(pids(&scan.owned), vec![400, 401, 500]);
}

#[test]
fn a_root_below_a_bazel_server_still_owns_its_descendants() {
    // Under `bazel test` the test process itself descends from a live
    // server; a sleeper it spawns is the run's, not the server's.
    let mut server = fake_process(600, 1, "java", Some("/elsewhere"));
    server.argv = vec!["bazel(mono)".into()];
    let table = StaticProcessTable::new(
        vec![
            server,
            fake_process(610, 600, "sandbox-exec", Some("/elsewhere")),
            fake_process(620, 610, "test_binary", Some("/elsewhere")),
            fake_process(630, 620, "sleep", Some("/elsewhere")),
        ],
        1,
    );
    let scope = Scope {
        roots: vec![620],
        paths: vec![],
        include_bazel_output_bases: false,
    };
    let scan = scan(&table, &scope).unwrap();
    assert_eq!(pids(&scan.owned), vec![620, 630]);
    assert!(scan.spared.is_empty());
}

#[test]
fn scan_never_touches_the_caller_or_its_ancestors_or_unrelated_processes() {
    let table = incident_table();
    let scan = scan(&table, &Scope::for_workspace(WS).with_root(100)).unwrap();
    for pid in [900, 901, 800, 1] {
        assert!(!scan.owned.iter().any(|o| o.process.pid == pid), "pid {pid} owned");
        assert!(!scan.spared.iter().any(|s| s.process.pid == pid), "pid {pid} spared");
    }
}

#[test]
fn scan_without_bazel_output_bases_stays_inside_the_workspace() {
    let table = incident_table();
    let scope = Scope {
        roots: vec![],
        paths: vec![PathBuf::from(WS)],
        include_bazel_output_bases: false,
    };
    let scan = scan(&table, &scope).unwrap();
    assert_eq!(pids(&scan.owned), vec![400, 401]);
    assert_eq!(scan.paths, vec![PathBuf::from(WS)]);
}

#[test]
fn scan_with_nothing_inside_is_clean() {
    let table = incident_table();
    let scan = scan(&table, &Scope::for_workspace("/ws/mono-agent-999")).unwrap();
    assert!(scan.is_clean());
    assert!(scan.spared.is_empty());
}

#[test]
fn reap_terms_then_kills_what_ignores_term_and_reports_survivors() {
    let table = incident_table();
    // The fork loop shrugs off SIGTERM; the test binary cannot be signalled
    // at all (models EPERM / a process that will not die).
    table.ignore_term(400);
    let report = reap(
        &table,
        &Scope::for_workspace(WS).with_root(100),
        Duration::from_millis(10),
    )
    .unwrap();
    let sent: Vec<(i32, Signal)> = report.signalled.iter().map(|s| (s.pid, s.signal)).collect();
    assert_eq!(
        sent,
        vec![
            (100, Signal::Term),
            (200, Signal::Term),
            (400, Signal::Term),
            (401, Signal::Term),
            (500, Signal::Term),
            (400, Signal::Kill),
        ]
    );
    assert_eq!(report.killed_pids(), vec![100, 200, 400, 401, 500]);
    assert!(report.is_clean());
    assert_eq!(report.passes, 3);
    assert!(!table.is_alive(400));
    assert!(table.is_alive(600), "bazel server spared");
    assert!(table.is_alive(700), "interactive shell spared");
    assert_eq!(report.spared.len(), 3);
    let loop_line = report
        .signalled
        .iter()
        .find(|s| s.pid == 400 && s.signal == Signal::Kill)
        .unwrap();
    assert_eq!(loop_line.age_secs, 1_000_000);
    assert!(loop_line.describe().contains("SIGKILL"), "{}", loop_line.describe());
}

#[test]
fn reap_is_a_single_pass_when_nothing_is_owned() {
    let table = incident_table();
    let report = reap(&table, &Scope::for_workspace("/ws/other"), Duration::from_secs(5)).unwrap();
    assert_eq!(report.passes, 1);
    assert!(report.signalled.is_empty());
    assert!(table.signals().is_empty());
}

#[test]
fn signal_owned_skips_a_recycled_pid() {
    let table = incident_table();
    let scan = scan(&table, &Scope::for_workspace(WS)).unwrap();
    let mut stale = scan.owned.clone();
    for target in &mut stale {
        target.process.start_micros += 1;
    }
    assert!(signal_owned(&table, &stale, Signal::Kill).is_empty());
    assert!(table.signals().is_empty());
}

#[test]
fn survivor_summary_names_pids_and_commands() {
    let report = ReapReport {
        survivors: vec![OwnedProcess {
            process: ws_process(400, 1, "zsh"),
            ownership: Ownership::WorkingDirectory { inside: WS.into() },
        }],
        ..ReapReport::default()
    };
    assert_eq!(report.survivor_summary(), "400 (zsh)");
    assert!(!report.is_clean());
}
