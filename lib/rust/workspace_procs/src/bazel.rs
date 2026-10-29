//! Recognising a Bazel server and the output base it serves.
//!
//! A Bazel server is the one process deliberately left alive inside a
//! workspace at teardown: it is meant to stay warm across runs and exits on
//! its own idle timeout. It is also the one process whose working directory
//! *is* the workspace root (the client starts it with `cwd` set there and it
//! never leaves), so a working-directory sweep must be able to tell it apart
//! without a name allowlist on everything else.
//!
//! The client renames the server's `argv[0]` to `bazel(<workspace basename>)`
//! and passes `--output_base=<dir>` and `--workspace_directory=<dir>` on its
//! command line, while the kernel still reports the executable as `java`.
//! Observed live on a development host:
//!
//! ```text
//! pid 877  ucomm=java  argv[0]=bazel(mono-agent-677)
//!   ... -jar .../install/<hash>/A-server.jar ...
//!   --output_base=/Users/me/Library/Caches/bazel/_bazel_me/cd39fd98…
//!   --workspace_directory=/Users/me/.local/share/cube/workspaces/mono-agent-677
//! ```
//!
//! Either marker on its own identifies the server: the renamed `argv[0]`, or
//! the server jar together with an `--output_base=` argument.

use std::path::{Path, PathBuf};

use crate::process_table::LiveProcess;

const OUTPUT_BASE_FLAG: &str = "--output_base=";
const WORKSPACE_DIRECTORY_FLAG: &str = "--workspace_directory=";
const SERVER_JAR_SUFFIX: &str = "/A-server.jar";

/// Whether `process` is a Bazel server (of any workspace).
pub fn is_bazel_server(process: &LiveProcess) -> bool {
    let renamed = process.argv.first().is_some_and(|a| a.starts_with("bazel("));
    let server_jar = process.argv.iter().any(|a| a.ends_with(SERVER_JAR_SUFFIX))
        && process.argv.iter().any(|a| a.starts_with(OUTPUT_BASE_FLAG));
    renamed || server_jar
}

/// The `--output_base=` directory a Bazel server's command line names.
pub fn server_output_base(process: &LiveProcess) -> Option<PathBuf> {
    flag_value(process, OUTPUT_BASE_FLAG)
}

/// The `--workspace_directory=` a Bazel server's command line names.
pub fn server_workspace_directory(process: &LiveProcess) -> Option<PathBuf> {
    flag_value(process, WORKSPACE_DIRECTORY_FLAG)
}

fn flag_value(process: &LiveProcess, flag: &str) -> Option<PathBuf> {
    process
        .argv
        .iter()
        .find_map(|a| a.strip_prefix(flag))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The output base a workspace's own `bazel-out` convenience symlink points
/// into: `<output_base>/execroot/<name>/bazel-out`. Returns `None` when the
/// workspace has never been built or the link has an unexpected shape.
///
/// This is what lets a sweep own a test binary stranded in a Bazel sandbox
/// directory (whose working directory is under the output base, not under
/// the workspace) without running a `bazel info` probe inside a workspace
/// that is being torn down.
pub fn output_base_from_symlink(workspace: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(workspace.join("bazel-out")).ok()?;
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    // <output_base>/execroot/<name>/bazel-out
    let execroot_child = target.parent()?;
    let execroot = execroot_child.parent()?;
    if execroot.file_name()? != "execroot" {
        return None;
    }
    execroot.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_table::fake_process;

    fn with_argv(argv: &[&str]) -> LiveProcess {
        let mut p = fake_process(5, 1, "java", Some("/ws"));
        p.argv = argv.iter().map(|s| s.to_string()).collect();
        p
    }

    #[test]
    fn renamed_argv0_marks_a_server() {
        assert!(is_bazel_server(&with_argv(&["bazel(mono-agent-677)", "-Xverify:none"])));
    }

    #[test]
    fn server_jar_with_output_base_marks_a_server() {
        let p = with_argv(&[
            "java",
            "-jar",
            "/u/Library/Caches/bazel/_bazel_u/install/abc/A-server.jar",
            "--output_base=/u/Library/Caches/bazel/_bazel_u/cd39",
            "--workspace_directory=/u/ws/mono-agent-677",
        ]);
        assert!(is_bazel_server(&p));
        assert_eq!(
            server_output_base(&p),
            Some(PathBuf::from("/u/Library/Caches/bazel/_bazel_u/cd39"))
        );
        assert_eq!(
            server_workspace_directory(&p),
            Some(PathBuf::from("/u/ws/mono-agent-677"))
        );
    }

    #[test]
    fn ordinary_java_and_a_bazel_client_are_not_servers() {
        assert!(!is_bazel_server(&with_argv(&["java", "-jar", "app.jar"])));
        // The client binary: no renamed argv[0], no server jar.
        assert!(!is_bazel_server(&with_argv(&["bazel", "test", "//..."])));
        assert!(!is_bazel_server(&with_argv(&[])));
        assert_eq!(server_output_base(&with_argv(&["bazel(x)", "--output_base="])), None);
    }

    #[test]
    fn output_base_is_derived_from_the_bazel_out_symlink() {
        let dir = std::env::temp_dir().join(format!("wsprocs-bazel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(output_base_from_symlink(&ws), None, "unbuilt workspace has no link");
        let base = dir.join("_bazel_me").join("0123456789abcdef0123456789abcdef");
        std::os::unix::fs::symlink(base.join("execroot/_main/bazel-out"), ws.join("bazel-out")).unwrap();
        assert_eq!(output_base_from_symlink(&ws), Some(base));
        std::fs::remove_file(ws.join("bazel-out")).unwrap();
        std::os::unix::fs::symlink("/somewhere/odd", ws.join("bazel-out")).unwrap();
        assert_eq!(output_base_from_symlink(&ws), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
