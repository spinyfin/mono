use std::env;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;

#[test]
fn credentials_and_host_tools_are_not_in_the_test_environment() {
    for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GITHUB_TOKEN", "GH_TOKEN"] {
        assert!(env::var_os(key).is_none(), "{key} reached repository-owned test code");
    }

    let test_tmpdir = env::var_os("TEST_TMPDIR").expect("Bazel must set TEST_TMPDIR");
    let expected_path = Path::new(&test_tmpdir).join("mono-test-bin");
    let path = env::var_os("PATH").expect("the hermetic wrapper must set PATH");
    assert_eq!(Path::new(&path), expected_path, "test code can search the host PATH");

    for forbidden_binary in ["gh", "bk", "codex", "claude", "cube"] {
        assert!(
            !expected_path.join(forbidden_binary).exists(),
            "{forbidden_binary} is reachable through the curated test PATH"
        );
    }
}

#[test]
fn network_is_denied_by_the_test_sandbox() {
    let address: SocketAddr = "1.1.1.1:53".parse().unwrap();
    let error = TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1))
        .expect_err("the test sandbox unexpectedly allowed an external network connection");
    assert!(
        matches!(
            error.kind(),
            ErrorKind::PermissionDenied | ErrorKind::NetworkUnreachable
        ),
        "external network failed for an incidental reason instead of sandbox denial: {error}"
    );
}

#[test]
fn writes_outside_the_test_sandbox_are_denied() {
    let outside_tmp = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/var/tmp"
    };
    let path = format!("{outside_tmp}/mono-hermeticity-guard-{}", std::process::id());
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) => {}
        Err(error) => {
            panic!("outside-sandbox write failed for an incidental reason instead of sandbox denial: {error}")
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
            panic!("the test sandbox unexpectedly allowed a write to {path}");
        }
    }
}
