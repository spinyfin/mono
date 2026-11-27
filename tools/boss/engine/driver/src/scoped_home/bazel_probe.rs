//! Live Bazel client evidence using the production scoped-home provisioner.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, ensure};

use super::HostToolPaths;
use crate::EnvDirective;

/// Run in an empty fixture host HOME; all files and servers stay in the fixture.
pub fn run(directory: &Path) -> anyhow::Result<()> {
    ensure!(directory.is_absolute(), "evidence directory must be absolute");
    let host = std::env::var_os("HOME").context("HOME must point to a fixture host home")?;
    ensure!(
        Path::new(&host) == directory.join("host"),
        "HOME must be <evidence directory>/host"
    );
    std::fs::create_dir_all(&host)?;
    std::fs::write(Path::new(&host).join(".bazelrc"), "build --announce_rc\n")?;
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(workspace.join("MODULE.bazel"), "module(name = \"scoped_home_probe\")\n")?;
    std::fs::write(workspace.join(".bazelversion"), "9.3.3\n")?;
    let scoped = directory.join("scoped");
    let control = directory.join("control");
    std::fs::create_dir_all(&control)?;
    let tools = HostToolPaths::resolve()?;
    tools.provision_into(&scoped)?;
    let expected_root = tools.caches.bazel_output_root.join(format!(
        "_bazel_{}",
        super::bazel_user_name().context("cannot resolve Bazel user")?
    ));
    println!(
        "provision_into rc:\n{}",
        std::fs::read_to_string(scoped.join(".bazelrc"))?
    );
    for (label, home, idle) in [("control", &control, 15), ("provisioned", &scoped, 10800)] {
        let command = || {
            let mut cmd = Command::new("bazel");
            cmd.current_dir(&workspace).env("HOME", home).env_remove("TEST_TMPDIR");
            for directive in tools.env_directives() {
                match directive {
                    EnvDirective::Set(key, value) => {
                        cmd.env(key, value);
                    }
                    EnvDirective::Unset(key) => {
                        cmd.env_remove(key);
                    }
                }
            }
            if label == "control" {
                cmd.env("TEST_TMPDIR", &tools.caches.bazel_output_root);
            }
            cmd
        };
        let pid_output = checked(command().args(["info", "server_pid"]).output()?)?;
        let pid = pid_output.trim();
        let capture = checked(Command::new("ps").args(["-ww", "-p", pid, "-o", "command="]).output()?)?;
        println!("{label}: {capture}");
        std::fs::write(directory.join(format!("{label}-server.txt")), &capture)?;
        // Shut down only the server just started by this fixture, even if an assertion fails.
        checked(command().arg("shutdown").output()?)?;
        ensure!(
            capture.contains(&format!("--max_idle_secs={idle}")),
            "unexpected idle setting: {capture}"
        );
        ensure!(
            capture.contains(&format!("--output_user_root={}", expected_root.display())),
            "output root differs: {capture}"
        );
        if label == "provisioned" {
            ensure!(
                capture.contains(&format!(
                    "--option_sources=output_Uuser_Uroot:{}/.bazelrc",
                    home.canonicalize()?.display()
                )),
                "pin did not come from provisioned rc: {capture}"
            );
        } else {
            ensure!(
                capture.split_whitespace().any(|arg| arg == "--option_sources="),
                "control has unexpected startup sources: {capture}"
            );
        }
    }
    Ok(())
}

fn checked(output: Output) -> anyhow::Result<String> {
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(output.status.success(), "command failed: {}", output.status);
    Ok(String::from_utf8(output.stdout)?)
}
