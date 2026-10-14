//! Standalone Node provisioning. Pins are from
//! https://nodejs.org/dist/v24.8.0/SHASUMS256.txt.
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::external::timeout::{CheckDeadline, output_with_timeout};

const VERSION: &str = "24.8.0";

fn distribution() -> Result<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok((
            "linux-x64",
            "daf68404b478b4c3616666580d02500a24148c0f439e4d0134d65ce70e90e655",
        )),
        ("linux", "aarch64") => Ok((
            "linux-arm64",
            "5eb16b14af5a5f494ed54770822144e847c744fe590f8df093ad4927cf3dd7fd",
        )),
        ("macos", "aarch64") => Ok((
            "darwin-arm64",
            "d81191a1866760eb918caa976c023036bc1fc7405ea31b148905211522045767",
        )),
        ("macos", "x86_64") => Ok((
            "darwin-x64",
            "6fd8496b59baa8f86a24e3eb03308b763091716ffc6b6e1094d1a5e5696dd6dd",
        )),
        (os, arch) => bail!("pinned Node is unavailable for {os}/{arch}"),
    }
}

pub(super) fn provision(check_id: &str, deadline: CheckDeadline) -> Result<PathBuf> {
    let (platform, digest) = distribution()?;
    let cache = directories::ProjectDirs::from("", "", "checkleft")
        .context("cannot locate checkleft cache for pinned Node")?
        .cache_dir()
        .join("node");
    fs::create_dir_all(&cache)?;
    let name = format!("node-v{VERSION}-{platform}");
    let destination = cache.join(format!("{name}-{digest}"));
    let npx = destination.join("bin/npx");
    if npx.is_file() && destination.join("bin/node").is_file() {
        return Ok(npx);
    }
    let staging = tempfile::tempdir_in(&cache)?;
    let url = format!("https://nodejs.org/dist/v{VERSION}/{name}.tar.gz");
    // Resolution is synchronous, and may itself be called inside a Tokio runtime.
    // Use an owned runtime on a scoped thread, as opposed to nesting block_on.
    let bytes = std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<Vec<u8>> {
                let _ = rustls::crypto::ring::default_provider().install_default();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let client = reqwest::Client::builder()
                            .timeout(Duration::from_millis(deadline.remaining_ms().max(1)))
                            .build()?;
                        Ok(client
                            .get(url)
                            .send()
                            .await?
                            .error_for_status()?
                            .bytes()
                            .await?
                            .to_vec())
                    })
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Node download thread panicked"))?
    })?;
    verify(&bytes, digest)?;
    let archive = staging.path().join("node.tar.gz");
    fs::write(&archive, bytes)?;
    let mut extract = Command::new("/usr/bin/tar");
    extract.arg("-xzf").arg(&archive).arg("-C").arg(staging.path());
    let output = output_with_timeout(&mut extract, check_id, "extract pinned Node", deadline.remaining_ms())?;
    if !output.status.success() {
        bail!("Node extraction failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let tree = staging.path().join(&name);
    if !tree.join("bin/node").is_file() || !tree.join("bin/npx").is_file() {
        bail!("verified Node archive lacks node/npx");
    }
    if let Err(error) = fs::rename(&tree, &destination) {
        // Another check may have atomically published the same verified release.
        if !npx.is_file() || !destination.join("bin/node").is_file() {
            return Err(error).context("publish pinned Node cache entry");
        }
    }
    Ok(npx)
}

fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("Node archive sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
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
        let error = crate::external::run_declarative_check(
            repo.path(),
            "test/environment",
            &package,
            &changeset,
            &config,
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("framework-owned PATH"));
        print!("pinned-node");
    }
}
