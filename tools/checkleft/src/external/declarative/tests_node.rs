use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn archive_checksum_fails_closed() {
    let digest = format!("{:x}", Sha256::digest(b"archive"));
    verify(b"archive", &digest).unwrap();
    assert!(verify(b"corrupt", &digest).is_err());
}

#[test]
fn hostile_host_environment_does_not_change_check_result() {
    let temp = tempfile::tempdir().unwrap();
    let pinned = temp.path().join("pinned");
    std::fs::create_dir(&pinned).unwrap();
    for (name, script) in [
        ("node", "#!/bin/sh\necho pinned-node\n"),
        (
            "npx",
            "#!/bin/sh\nset -eu\ntest \"$1\" = --yes\ntest \"$2\" = fixture-check@1.0.0\nshift 2\nif [ \"$1\" = require-declaration ]; then test \"$DECLARED_VALUE\" = yes; fi\nnode\ntest -z \"${RUSTUP_TOOLCHAIN:-}\"\ntest ! -e \"$HOME/.npmrc\"\n",
        ),
    ] {
        let file = pinned.join(name);
        std::fs::write(&file, script).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut outputs = Vec::new();
    for version in ["v20.7.0", "v99.0.0"] {
        let host = temp.path().join(version);
        std::fs::create_dir(&host).unwrap();
        std::fs::write(host.join("node"), format!("#!/bin/sh\necho {version}\n")).unwrap();
        std::fs::set_permissions(host.join("node"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(host.join(".npmrc"), "invalid host config").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "external::declarative::node::tests::environment_child",
                "--nocapture",
            ])
            .env("CHECKLEFT_ENV_TEST_PROGRAM", pinned.join("npx"))
            .env("PATH", &host)
            .env("HOME", &host)
            .env("RUSTUP_TOOLCHAIN", version)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("pinned-node"));
        outputs.push(output.status.code());
    }
    assert_eq!(outputs[0], outputs[1]);
}

#[test]
fn environment_child() {
    let Some(program) = std::env::var_os("CHECKLEFT_ENV_TEST_PROGRAM") else {
        return;
    };
    // Seed a complete cache entry, then exercise production npm resolution.
    // Network access is denied by Bazel; a cache miss must fail this test.
    let (platform, digest) = distribution().unwrap();
    let cached_bin = directories::ProjectDirs::from("", "", "checkleft")
        .unwrap()
        .cache_dir()
        .join("node")
        .join(format!("node-v{VERSION}-{platform}-{digest}"))
        .join("bin");
    std::fs::create_dir_all(&cached_bin).unwrap();
    let fixture_bin = Path::new(&program).parent().unwrap();
    for name in ["node", "npx"] {
        std::fs::copy(fixture_bin.join(name), cached_bin.join(name)).unwrap();
    }
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("input.rs"), "fn main() {}").unwrap();
    let manifest = r#"
id = "test/environment"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.rs"]
[limits]
timeout_ms = 30000
[needs.tool.default.npm]
package = "fixture-check"
version = "1.0.0"
[[invocations]]
id = "probe"
run = "tool"
mode = "batch"
args = ["{{files}}"]
exit = { "0" = "findings", default = "error" }
[invocations.transform]
kind = "linelist"
message = "controlled environment"
"#;
    let package = crate::external::parse_external_check_package_manifest(manifest).unwrap();
    let crate::external::ExternalCheckPackageImplementation::Declarative(package) = package.implementation else {
        panic!("expected declarative package");
    };
    let changeset = crate::input::ChangeSet::new(vec![crate::input::ChangedFile {
        path: "input.rs".into(),
        kind: crate::input::ChangeKind::Modified,
        old_path: None,
    }]);
    let result = crate::external::run_declarative_check(
        repo.path(),
        "test/environment",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .unwrap();
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].message, "controlled environment");
    assert_eq!(
        result.findings[0].location.as_ref().unwrap().path,
        Path::new("pinned-node")
    );
    let config: toml::Value = toml::from_str("[environment]\nDECLARED_VALUE = \"yes\"").unwrap();
    let mut declared_package = package.clone();
    let crate::external::declarative::InvocationKind::Tool(tool) = &mut declared_package.invocations[0].kind else {
        panic!("expected tool invocation");
    };
    tool.args.insert(0, "require-declaration".into());
    let declared = crate::external::run_declarative_check(
        repo.path(),
        "test/environment",
        &declared_package,
        &changeset,
        &config,
        None,
    )
    .unwrap();
    assert_eq!(declared, result);
    let config: toml::Value = toml::from_str("[environment]\nPATH = \"/host/tools\"").unwrap();
    let error =
        crate::external::run_declarative_check(repo.path(), "test/environment", &package, &changeset, &config, None)
            .unwrap_err();
    assert!(format!("{error:#}").contains("framework-owned PATH"));
    print!("pinned-node");
}
fn archive_fixture(include_npx: bool) -> Vec<u8> {
    let source = tempfile::tempdir().unwrap();
    let bin = source.path().join("fixture/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("node"), "node").unwrap();
    if include_npx {
        fs::write(bin.join("npx"), "npx").unwrap();
    }
    let output = output_with_timeout(
        Command::new("tar")
            .arg("-czf")
            .arg("-")
            .arg("-C")
            .arg(source.path())
            .arg("fixture"),
        "test/node",
        "create fixture archive",
        30_000,
    )
    .unwrap();
    assert!(output.status.success());
    output.stdout
}

#[test]
fn archive_publishes_complete_tree_and_preserves_existing_destination() {
    let cache = tempfile::tempdir().unwrap();
    let bytes = archive_fixture(true);
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let publish = || {
        publish_archive(
            &bytes,
            &digest,
            "fixture",
            cache.path(),
            "test/node",
            CheckDeadline::new(30_000),
        )
        .unwrap()
    };
    let npx = publish();
    assert_eq!(fs::read_to_string(&npx).unwrap(), "npx");
    assert_eq!(fs::read_to_string(npx.with_file_name("node")).unwrap(), "node");
    fs::write(npx.with_file_name("marker"), "existing").unwrap();
    assert_eq!(publish(), npx);
    assert_eq!(fs::read_to_string(npx.with_file_name("marker")).unwrap(), "existing");
}

#[test]
fn incomplete_archive_is_not_published() {
    let cache = tempfile::tempdir().unwrap();
    let bytes = archive_fixture(false);
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let error = publish_archive(
        &bytes,
        &digest,
        "fixture",
        cache.path(),
        "test/node",
        CheckDeadline::new(30_000),
    )
    .unwrap_err();
    assert!(error.to_string().contains("lacks node/npx"));
    assert!(!cache.path().join(format!("fixture-{digest}")).exists());
}

#[test]
fn cache_lock_obeys_deadline_and_releases_on_drop() {
    let cache = tempfile::tempdir().unwrap();
    let path = cache.path().join("release.lock");
    let first = cache_lock(&path, CheckDeadline::new(1000)).unwrap();
    assert!(
        cache_lock(&path, CheckDeadline::new(0))
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    drop(first);
    assert!(cache_lock(&path, CheckDeadline::new(1000)).is_ok());
}

#[test]
fn linux_requires_supported_glibc() {
    for version in ["glibc 2.28\n", "glibc 2.39", "glibc 3.0"] {
        assert!(supported_glibc(version), "{version}");
    }
    for version in ["glibc 2.27", "musl 1.2", "", "glibc invalid"] {
        assert!(!supported_glibc(version), "{version}");
    }
}
