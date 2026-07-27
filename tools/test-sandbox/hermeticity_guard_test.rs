use std::env;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;

#[test]
fn credentials_and_host_tools_are_not_in_the_test_environment() {
    for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GITHUB_TOKEN", "GH_TOKEN"] {
        assert!(env::var_os(key).is_none(), "{key} reached repository-owned test code");
    }

    let test_srcdir = env::var_os("TEST_SRCDIR").expect("Bazel must set TEST_SRCDIR");
    let test_workspace = env::var_os("TEST_WORKSPACE").expect("Bazel must set TEST_WORKSPACE");
    let expected_xcode_bin = Path::new(&test_srcdir).join(test_workspace).join("tools/test-sandbox");
    let expected_runtime_bin = Path::new(&test_srcdir).join("+test_runtime_repository+test_runtime_tools/bin");
    let path = env::var_os("PATH").expect("the hermetic wrapper must set PATH");
    let path_entries: Vec<_> = env::split_paths(&path).collect();
    assert_eq!(
        path_entries,
        [expected_xcode_bin, expected_runtime_bin],
        "test code can search outside Bazel-declared runtime inputs"
    );

    for forbidden_binary in ["gh", "bk", "codex", "claude", "cube"] {
        assert!(
            !path_entries.iter().any(|entry| entry.join(forbidden_binary).exists()),
            "{forbidden_binary} is reachable through the declared test PATH"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn absolute_host_executables_are_denied() {
    let host_gh = Path::new("/opt/homebrew/bin/gh");
    if host_gh.exists() {
        let error = Command::new(host_gh)
            .arg("--version")
            .status()
            .expect_err("an absolute undeclared host executable bypassed the runtime boundary");
        assert_eq!(
            error.kind(),
            ErrorKind::PermissionDenied,
            "the undeclared host executable failed for an incidental reason: {error}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn keychain_files_and_securityd_ipc_are_denied() {
    let error = File::open("/Library/Keychains/System.keychain")
        .expect_err("the test sandbox exposed a host Keychain database");
    assert_eq!(
        error.kind(),
        ErrorKind::PermissionDenied,
        "the Keychain read failed for an incidental reason: {error}"
    );

    if let Some(user) = env::var_os("USER") {
        let user_keychains = Path::new("/Users").join(user).join("Library/Keychains");
        if user_keychains.exists() {
            let error =
                std::fs::read_dir(&user_keychains).expect_err("the test sandbox exposed the user's Keychain directory");
            assert_eq!(
                error.kind(),
                ErrorKind::PermissionDenied,
                "the user Keychain read failed for an incidental reason: {error}"
            );
        }
    }

    let error = Command::new("/usr/bin/security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .expect_err("the absolute security CLI bypassed the executable boundary");
    assert_eq!(
        error.kind(),
        ErrorKind::PermissionDenied,
        "security failed for an incidental reason: {error}"
    );

    unsafe extern "C" {
        fn SecKeychainCopyDefault(keychain: *mut *mut std::ffi::c_void) -> i32;
    }
    let mut keychain = std::ptr::null_mut();
    // SAFETY: Security.framework initializes the out pointer when the call
    // succeeds; this guard only checks the OSStatus and never dereferences it.
    let status = unsafe { SecKeychainCopyDefault(&mut keychain) };
    assert_ne!(
        status, 0,
        "Security.framework unexpectedly reached securityd and returned the default Keychain"
    );
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
