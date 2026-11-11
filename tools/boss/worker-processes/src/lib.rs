//! Execution-owned process cleanup shared by local hosts, spokes and legacy SSH.
//!
//! The Python payload runs on the owning host without requiring a new remote
//! binary. Python is already required by the worker launch wrapper. Keep this
//! payload identical across transports so detached-process ownership has one
//! implementation.

use anyhow::{Result, ensure};
use serde::Deserialize;

pub const SCRIPT: &str = include_str!("processes.py");

/// Materialize a versioned payload so tmux's command-size limit is respected.
pub async fn install() -> Result<String> {
    let output = tokio::process::Command::new("python3")
        .args(["-c", SCRIPT, "--install", SCRIPT])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "installing process registration: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let paths: std::collections::BTreeMap<String, String> = serde_json::from_slice(&output.stdout)?;
    paths
        .get("BOSS_PROCESS_SCRIPT")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("process registrar path missing"))
}

/// Startup registration is necessary on macOS, which omits the environment
/// of platform shells from process queries. The record pins PID and birth time.
pub async fn capture(key: &str, owner: &str) -> Result<()> {
    let output = tokio::process::Command::new("python3")
        .args(["-c", SCRIPT, "--capture", key, owner])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "capturing worker descendants: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[derive(Debug, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Cleanup {
    pub terminated: Vec<u32>,
    pub killed: Vec<u32>,
    pub remaining: Vec<u32>,
    pub occupants: Vec<u32>,
    pub unknown: Vec<u32>,
    pub owners: std::collections::BTreeMap<String, Vec<u32>>,
    pub elapsed_ms: u64,
}

impl Cleanup {
    pub fn empty(&self) -> bool {
        self.remaining.is_empty() && self.occupants.is_empty() && self.unknown.is_empty()
    }

    pub fn report(&self, owner: &str) {
        tracing::info!(owner, ?self.terminated, ?self.killed, ?self.remaining,
            ?self.occupants, ?self.unknown, self.elapsed_ms, "worker process cleanup");
    }
}

pub async fn cleanup(key: &str, owner: &str, workspace: &str) -> Result<Cleanup> {
    let output = tokio::process::Command::new("python3")
        .args(["-c", SCRIPT, key, owner, workspace])
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "worker process cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Cleanup = serde_json::from_slice(&output.stdout)?;
    result.report(owner);
    ensure!(
        result.remaining.is_empty(),
        "worker processes survived cleanup: {:?}",
        result.remaining
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    const TESTS: &str = r###"
import subprocess
import tempfile
import unittest
import uuid
import shlex
from unittest.mock import patch

process_root = tempfile.TemporaryDirectory()
os.environ["BOSS_PROCESS_ROOT"] = process_root.name


class OwnershipTests(unittest.TestCase):
    def test_disappearing_process_is_rechecked_without_ignoring_live_unknowns(self):
        with patch(__name__ + ".pids", return_value=[42]), patch(__name__ + ".inspect", side_effect=OSError(0, "proc_pidinfo")):
            with patch("os.kill", side_effect=ProcessLookupError(errno.ESRCH, "gone")) as probe:
                self.assertEqual(snapshot("BOSS_RUN_ID", "owner", "/workspace")[2], [])
                probe.assert_called_once_with(42, 0)
            with patch("os.kill", return_value=None):
                self.assertEqual(snapshot("BOSS_RUN_ID", "owner", "/workspace")[2], [42])

    def test_recycled_pid_is_never_signalled(self):
        with patch(__name__ + ".birth_and_parent", return_value=((2, 0), 1)), patch("os.kill") as kill:
            self.assertEqual(signal_owned({42: (1, 0)}, signal.SIGTERM), [])
            kill.assert_not_called()

    def test_resident_server_does_not_hide_other_workspace_occupants(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = os.path.realpath(directory)
            (Path(directory) / "server").mkdir()
            (Path(directory) / "server/server.pid.txt").write_text("42\n")
            args = [b"bazel(workspace)", b"-jar", b"/install/A-server.jar",
                    os.fsencode("--workspace_directory=" + workspace), os.fsencode("--output_base=" + directory)]
            facts = {42: ((1, 0), workspace, args, [b"BOSS_RUN_ID=old"], 1),
                     43: ((2, 0), workspace, [b"sleep", b"60"], [], 1)}
            with patch(__name__ + ".pids", return_value=[42, 43]), patch(__name__ + ".inspect", side_effect=facts.__getitem__):
                owned, occupants, unknown, owners = snapshot("BOSS_RUN_ID", "old", workspace)
            self.assertEqual(owned, {})
            self.assertEqual(occupants, [43])
            self.assertEqual(unknown, [])
            self.assertEqual(owners, {})

    def test_registered_shell_survives_parent_exit_and_loses_no_ownership(self):
        for shell in ("/bin/bash", "/bin/zsh") if sys.platform == "darwin" else ("/bin/bash",):
            with self.subTest(shell=shell), tempfile.TemporaryDirectory() as directory:
                owner = str(uuid.uuid4())
                pid_file = Path(directory) / "pid"
                command = f'echo $$ > {shlex.quote(str(pid_file))}; while :; do sleep 1; done'
                launcher = "import subprocess; subprocess.Popen(" + repr([shell, "-c", command]) + ", start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)"
                subprocess.run([sys.executable, "-c", launcher], check=True,
                               env={**os.environ, **install(SOURCE), "BOSS_RUN_ID": owner})
                try:
                    deadline = time.monotonic() + 10
                    while (not pid_file.exists() or not pid_file.read_text().strip()) and time.monotonic() < deadline:
                        time.sleep(0.02)
                    pid = int(pid_file.read_text())
                    result = run("BOSS_RUN_ID", owner, directory)
                    self.assertIn(pid, result["terminated"])
                    self.assertEqual(result["remaining"], [])
                    self.assertEqual(result["occupants"], [])
                    print(shell, "reparented cleanup ms:", result["elapsed_ms"], flush=True)
                finally:
                    run("BOSS_RUN_ID", owner, directory, grace=0.1)

    def test_detached_child_and_other_execution(self):
        owner = str(uuid.uuid4())
        other = subprocess.Popen(["sleep", "60"], env={**os.environ, "BOSS_RUN_ID": str(uuid.uuid4())})
        # /bin/sh has no startup hook. Model the launch wrapper registering its
        # shell before accepting commands, as bash-env and .zshenv do.
        launch = f'{shlex.quote(sys.executable)} -c {shlex.quote(SOURCE)} --register "$$"; echo ready; while :; do sleep 1; done'
        child = subprocess.Popen(["/bin/sh", "-c", launch],
                                 stdout=subprocess.PIPE, start_new_session=True, env={**os.environ, "BOSS_RUN_ID": owner})
        try:
            self.assertEqual(child.stdout.readline(), b"ready\n")
            result = run("BOSS_RUN_ID", owner, "")
            self.assertIn(child.pid, result["terminated"])
            self.assertEqual(result["remaining"], [])
            self.assertIsNotNone(child.poll())
            self.assertIsNone(other.poll())
            print("detached cleanup ms:", result["elapsed_ms"], flush=True)
        finally:
            child.kill() if child.poll() is None else None
            child.wait()
            child.stdout.close()
            other.kill()
            other.wait()

    def test_term_resistant_child_escalates(self):
        owner = str(uuid.uuid4())
        child = subprocess.Popen([sys.executable, "-u", "-c",
                                  "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('ready'); time.sleep(60)"],
                                 stdout=subprocess.PIPE, start_new_session=True,
                                 env={**os.environ, "BOSS_SPAWN_TOKEN": owner})
        try:
            self.assertEqual(child.stdout.readline(), b"ready\n")
            result = run("BOSS_SPAWN_TOKEN", owner, "")
            self.assertIn(child.pid, result["terminated"])
            self.assertIn(child.pid, result["killed"])
            self.assertEqual(result["remaining"], [])
            print("TERM-resistant cleanup ms:", result["elapsed_ms"], flush=True)
        finally:
            child.kill() if child.poll() is None else None
            child.wait()
            child.stdout.close()

    def test_resident_bazel_requires_exact_workspace_and_server_jar(self):
        with tempfile.TemporaryDirectory() as output_base:
            (Path(output_base) / "server").mkdir()
            (Path(output_base) / "server/server.pid.txt").write_text("42\n")
            args = [b"bazel(workspace)", b"-jar", b"/install/A-server.jar", b"--workspace_directory=/workspace", os.fsencode("--output_base=" + output_base)]
            self.assertTrue(resident_bazel(args, "/workspace", 42))
            self.assertFalse(resident_bazel(args, "/workspace", 43))
            self.assertFalse(resident_bazel(args, "/workspace-other", 42))
            self.assertFalse(resident_bazel([b"bazel", b"test", b"//..."], "/workspace", 42))

    def test_unowned_occupant_is_reported_and_never_killed(self):
        with tempfile.TemporaryDirectory() as directory:
            child = subprocess.Popen(["sleep", "60"], cwd=directory)
            try:
                result = run("BOSS_RUN_ID", str(uuid.uuid4()), directory)
                self.assertIn(child.pid, result["occupants"])
                self.assertIsNone(child.poll())
            finally:
                child.kill()
                child.wait()


unittest.main(argv=["worker-processes"], verbosity=2)

"###;
    #[test]
    fn owning_host_process_tests() {
        let script = format!(
            "SOURCE = {}\n{}\n{}",
            serde_json::to_string(super::SCRIPT).unwrap(),
            super::SCRIPT.replace("if __name__ == \"__main__\":", "if False:"),
            TESTS
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
