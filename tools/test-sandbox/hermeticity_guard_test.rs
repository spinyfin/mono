use std::env;
#[cfg(target_os = "macos")]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

fn assert_write_denied(path: &Path) {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) => {}
        Err(error) => {
            panic!(
                "outside-sandbox write to {} failed for an incidental reason instead of sandbox denial: {error}",
                path.display()
            )
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
            panic!("the test sandbox unexpectedly allowed a write to {}", path.display());
        }
    }
}

#[test]
fn credentials_and_host_tools_are_not_in_the_test_environment() {
    for key in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "BOSS_SHAKE_APP_ID",
        "BOSS_SHAKE_INSTALLATION_ID",
        "BOSS_SHAKE_PRIVATE_KEY_PEM",
    ] {
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

#[cfg(target_os = "linux")]
#[test]
fn linux_private_temp_root_is_short_and_action_private() {
    use std::os::unix::ffi::OsStrExt;

    let private_root = env::var_os("TEST_TMPDIR").expect("Bazel must set TEST_TMPDIR");
    let private_root = Path::new(&private_root);
    // `Path::starts_with` matches whole path *components*, not string prefixes:
    // "/tmp/mono-test.XXXXXX".starts_with("/tmp/mono-test.") is false because the
    // final component "mono-test.XXXXXX" isn't equal to the component "mono-test.".
    // Compare raw bytes instead so the random mktemp suffix doesn't break the check.
    assert!(
        private_root.as_os_str().as_bytes().starts_with(b"/tmp/mono-test."),
        "Linux TEST_TMPDIR must live in the per-action /tmp tmpfs: {}",
        private_root.display()
    );
    assert!(
        private_root.as_os_str().as_bytes().len() + "/tmp/engine.sock".len() < 108,
        "Linux TEST_TMPDIR is too long for sockaddr_un.sun_path: {}",
        private_root.display()
    );
    assert_eq!(
        env::var_os("TMPDIR").as_deref(),
        Some(private_root.join("tmp").as_os_str()),
        "TMPDIR must be nested directly beneath the short private root"
    );

    // The prefix/length checks above only inspect the string the wrapper's
    // `mktemp` produced; they hold even if `--sandbox_tmpfs_path=/tmp` were
    // dropped from .bazelrc, since the wrapper unconditionally names its
    // private root under /tmp. Verify /tmp is actually the per-action tmpfs
    // (not the shared host /tmp) by reading the live mount table.
    let mounts = std::fs::read_to_string("/proc/self/mounts").expect("/proc/self/mounts must be readable");
    let tmp_is_tmpfs = mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let mount_point = fields.next();
        let fs_type = fields.next();
        mount_point == Some("/tmp") && fs_type == Some("tmpfs")
    });
    assert!(
        tmp_is_tmpfs,
        "Linux TEST_TMPDIR must live in the per-action /tmp tmpfs, but /proc/self/mounts shows no tmpfs mounted at /tmp: {mounts}"
    );

    // An action-private tmpfs holds only this action's own entries; a shared
    // host /tmp would additionally contain other actions' or the host's
    // leftover temp files.
    let private_root_name = private_root
        .file_name()
        .expect("TEST_TMPDIR must have a file name")
        .as_bytes();
    for entry in std::fs::read_dir("/tmp").expect("/tmp must be readable") {
        let entry = entry.expect("reading /tmp entries must not fail");
        let name = entry.file_name();
        assert!(
            name.as_bytes() == private_root_name || name.as_bytes().starts_with(b"mono-test."),
            "/tmp contains an entry not owned by this action, indicating a shared host /tmp rather than a per-action tmpfs: {}",
            entry.path().display()
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

    const BOOTSTRAP_NOT_PRIVILEGED: i32 = 1100;
    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_look_up(bootstrap_port: u32, service_name: *const std::ffi::c_char, service: *mut u32) -> i32;
    }
    let service_name = std::ffi::CString::new("com.apple.securityd").unwrap();
    let mut service = 0;
    // SAFETY: bootstrap_port is initialized by libSystem. bootstrap_look_up
    // reads the NUL-terminated service name and initializes the integer out
    // port; the guard never sends to or otherwise owns that port.
    let status = unsafe { bootstrap_look_up(bootstrap_port, service_name.as_ptr(), &mut service) };
    assert_eq!(
        status, BOOTSTRAP_NOT_PRIVILEGED,
        "securityd Mach lookup was not rejected with the sandbox-specific denial (status {status})"
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
    assert_write_denied(Path::new(&path));
}

#[test]
fn private_and_declared_output_paths_remain_writable() {
    let private_root = env::var_os("TEST_TMPDIR").expect("Bazel must set TEST_TMPDIR");
    let private_file = Path::new(&private_root).join("hermeticity-private-write");
    std::fs::write(&private_file, b"private").expect("the private test root must remain writable");
    let renamed_private_file = Path::new(&private_root).join("hermeticity-private-renamed");
    std::fs::rename(&private_file, &renamed_private_file)
        .expect("renames inside the private test root must remain writable");
    let nested_private_root = Path::new(&private_root).join("hermeticity-nested");
    std::fs::create_dir(&nested_private_root).expect("private nested directories must remain writable");
    let nested_source = nested_private_root.join("source");
    let nested_destination = nested_private_root.join("destination");
    std::fs::write(&nested_source, b"nested").expect("private nested files must remain writable");
    std::fs::rename(&nested_source, &nested_destination)
        .expect("nested renames inside the private test root must remain writable");

    let output_root =
        env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").expect("Bazel must declare an undeclared-output directory");
    let output_file = Path::new(&output_root).join("hermeticity-declared-write");
    std::fs::write(&output_file, b"declared").expect("the declared Bazel output directory must remain writable");
}

#[cfg(target_os = "macos")]
#[test]
fn xcode_capability_retains_the_write_allowlist() {
    if env::var("MONO_TEST_XCODE_TOOLCHAIN").as_deref() != Ok("1") {
        return;
    }

    if let Some(host_tmpdir) = env::var_os("MONO_TEST_HOST_TMPDIR") {
        let host_tmpdir = Path::new(&host_tmpdir);
        let private_root = env::var_os("TEST_TMPDIR").unwrap();
        if host_tmpdir.is_dir() && !host_tmpdir.starts_with(Path::new(&private_root)) {
            let path = host_tmpdir.join(format!("mono-xcode-host-temp-{}", std::process::id()));
            assert_write_denied(&path);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn xcode_capability_uses_the_configured_developer_root() {
    if env::var("MONO_TEST_XCODE_TOOLCHAIN").as_deref() != Ok("1") {
        return;
    }

    let developer_root = env::var_os("MONO_TEST_XCODE_DEVELOPER_DIR")
        .expect("the Xcode capability must publish its configured developer root");
    let developer_root =
        std::fs::canonicalize(developer_root).expect("the configured developer root must be canonicalizable");

    let output = Command::new("/usr/bin/xcrun")
        .args(["--find", "xcodebuild"])
        .output()
        .expect("the Xcode capability must permit the system xcrun launcher");
    assert!(
        output.status.success(),
        "xcrun could not resolve xcodebuild: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved = String::from_utf8(output.stdout).expect("xcrun output must be UTF-8");
    let resolved = std::fs::canonicalize(resolved.trim()).expect("xcrun's xcodebuild path must exist");
    assert!(
        resolved.starts_with(&developer_root),
        "xcrun resolved {} outside configured developer root {}",
        resolved.display(),
        developer_root.display()
    );

    let output = Command::new("/usr/bin/xcodebuild")
        .arg("-version")
        .output()
        .expect("the system xcodebuild launcher must reach the configured developer executable");
    assert!(
        output.status.success(),
        "configured xcodebuild was not executable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
