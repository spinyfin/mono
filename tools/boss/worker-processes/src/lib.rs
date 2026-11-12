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
/// `workspace` scopes identity matches to that tree; descendants found while
/// the pane is alive are recorded as lineage so a later cleanup can still
/// signal them after they chdir out.
pub async fn capture(key: &str, owner: &str, workspace: &str) -> Result<()> {
    let output = tokio::process::Command::new("python3")
        .args(["-c", SCRIPT, "--capture", key, owner, workspace])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "capturing worker descendants: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// False only when `pid` is proven not to be the shell that carried this
/// execution's identity (gone, or reused by a process without it).
pub async fn shell_alive(key: &str, owner: &str, pid: u32) -> Result<bool> {
    let output = tokio::process::Command::new("python3")
        .args(["-c", SCRIPT, "--alive", key, owner, &pid.to_string()])
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "probing worker shell: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
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

    /// Only unowned processes remain in the workspace: none survived
    /// signalling, none has an unreadable identity and none carries any run's
    /// identity, live or terminal. An owned occupant is never "stray", so a
    /// running run's processes can never be mistaken for abandoned ones.
    pub fn stray_only(&self) -> bool {
        self.remaining.is_empty() && self.unknown.is_empty() && self.owners.is_empty() && !self.occupants.is_empty()
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

/// Occupancy scan with no signalling. Empty owner means identity never matches,
/// so this is a cwd census of the workspace.
pub fn scan(workspace: &str) -> Result<Cleanup> {
    let output = std::process::Command::new("python3")
        .args(["-c", SCRIPT, "BOSS_RUN_ID", "", workspace])
        .output()?;
    ensure!(
        output.status.success(),
        "workspace occupancy scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
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
    def test_linux_scan_excludes_namespace_init(self):
        with tempfile.TemporaryDirectory() as directory:
            entries = [Path(directory) / name for name in ("0", "1", "42", "self")]
            for entry in entries:
                entry.mkdir()
            with patch("sys.platform", "linux"), patch.object(Path, "iterdir", return_value=iter(entries)):
                self.assertEqual(pids(), [42])

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
            with patch(__name__ + ".pids", return_value=[42, 43]), patch(__name__ + ".inspect", side_effect=lambda pid, buffer=None: facts[pid]):
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
                base = {key: value for key, value in os.environ.items() if key not in ("ZDOTDIR", "BASH_ENV")}
                subprocess.run([sys.executable, "-c", launcher], check=True, cwd=directory,
                               env={**base, **install(SOURCE), "HOME": directory, "BOSS_RUN_ID": owner})
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

    def test_malformed_registry_record_is_treated_as_unregistered(self):
        private_registry()
        for index, text in enumerate(("{", "[]", "{\"birth\": [1, 2]}", "{\"birth\": [1, 2], \"identity\": 3}")):
            (registry() / f"{900000 + index}.json").write_text(text)
            self.assertEqual(registered_environment(900000 + index, (1, 2)), [])

    def test_registration_failure_does_not_kill_the_shell(self):
        if os.geteuid() == 0:
            self.skipTest("root ignores directory modes")
        with tempfile.TemporaryDirectory() as directory:
            hooks = install(SOURCE)
            locked = Path(directory) / "locked"
            locked.mkdir(mode=0o500)
            try:
                result = subprocess.run(["/bin/bash", "-c", "echo ran"], capture_output=True, text=True,
                                        env={**os.environ, **hooks, "BOSS_RUN_ID": "run", "BOSS_PROCESS_ROOT": str(locked / "registry")})
            finally:
                locked.chmod(0o700)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("ran", result.stdout)
            self.assertIn("registration failed", result.stderr)

    def test_zsh_launch_keeps_user_startup_files_and_registers(self):
        if not Path("/bin/zsh").exists():
            self.skipTest("zsh unavailable")
        with tempfile.TemporaryDirectory() as home:
            Path(home, ".zshenv").write_text("export BOSS_TEST_ENV=zshenv\nexport ZDOTDIR=$HOME/xdg\n")
            xdg = Path(home, "xdg")
            xdg.mkdir()
            (xdg / ".zprofile").write_text("export BOSS_TEST_PROFILE=zprofile\n")
            (xdg / ".zshrc").write_text("export BOSS_TEST_RC=zshrc\n")
            base = {key: value for key, value in os.environ.items() if key not in ("ZDOTDIR", "BASH_ENV")}
            hooks = install(SOURCE)
            result = subprocess.run(
                ["/bin/zsh", "-l", "-i", "-c",
                 "echo $$ $BOSS_TEST_ENV $BOSS_TEST_PROFILE $BOSS_TEST_RC ${ZDOTDIR:-unset} $HISTFILE"],
                capture_output=True, text=True, stdin=subprocess.DEVNULL,
                env={**base, **hooks, "HOME": home, "BOSS_RUN_ID": "zsh-run"})
            pid, *seen = result.stdout.split()
            self.assertEqual(seen[:3], ["zshenv", "zprofile", "zshrc"], result.stderr)
            self.assertEqual(seen[3], hooks["ZDOTDIR"], "the hook stays installed for nested shells")
            self.assertTrue(seen[4].endswith("/xdg/.zsh_history"), seen[4])
            self.assertTrue((registry() / f"{pid}.json").exists())

    def test_nested_detached_zsh_registers_and_is_cleaned_up(self):
        if not Path("/bin/zsh").exists():
            self.skipTest("zsh unavailable")
        owner = str(uuid.uuid4())
        with tempfile.TemporaryDirectory() as home:
            Path(home, ".zprofile").write_text("export BOSS_TEST_PROFILE=zprofile\n")
            marker = Path(home, "child.pid")
            base = {key: value for key, value in os.environ.items() if key not in ("ZDOTDIR", "BASH_ENV")}
            hooks = install(SOURCE)
            # The child outlives its registered parent, which exits at once.
            command = f'/bin/zsh -c \'echo $$ > {marker}; sleep 60\' >/dev/null 2>&1 </dev/null & sleep 1'
            result = subprocess.run(
                ["/bin/zsh", "-l", "-i", "-c", command],
                capture_output=True, text=True, stdin=subprocess.DEVNULL, cwd=home,
                env={**base, **hooks, "HOME": home, "BOSS_RUN_ID": owner})
            self.assertEqual(result.returncode, 0, result.stderr)
            pid = int(marker.read_text())
            try:
                self.assertTrue((registry() / f"{pid}.json").exists(), "the nested zsh registered itself")
                cleaned = run("BOSS_RUN_ID", owner, home)
                self.assertIn(pid, cleaned["terminated"] + cleaned["killed"])
                self.assertEqual(cleaned["remaining"], [])
            finally:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_reused_pid_without_the_identity_is_not_the_shell(self):
        owner = str(uuid.uuid4())
        idle = [sys.executable, "-c", "import time; time.sleep(60)"]
        stranger = subprocess.Popen(idle, env={**os.environ, "BOSS_RUN_ID": "someone-else"})
        carrier = subprocess.Popen(idle, env={**os.environ, "BOSS_RUN_ID": owner})
        try:
            time.sleep(0.1)
            self.assertFalse(shell_alive("BOSS_RUN_ID", owner, stranger.pid))
            self.assertTrue(shell_alive("BOSS_RUN_ID", owner, carrier.pid))
        finally:
            for child in (stranger, carrier):
                child.kill()
                child.wait()
        self.assertFalse(shell_alive("BOSS_RUN_ID", owner, carrier.pid))

    def test_detached_child_and_other_execution(self):
        owner = str(uuid.uuid4())
        with tempfile.TemporaryDirectory() as directory:
            other = subprocess.Popen(["sleep", "60"], cwd=directory,
                                     env={**os.environ, "BOSS_RUN_ID": str(uuid.uuid4())})
            # /bin/sh has no startup hook. Model the launch wrapper registering its
            # shell before accepting commands, as bash-env and .zshenv do.
            launch = f'{shlex.quote(sys.executable)} -c {shlex.quote(SOURCE)} --register "$$"; echo ready; while :; do sleep 1; done'
            child = subprocess.Popen(["/bin/sh", "-c", launch], cwd=directory,
                                     stdout=subprocess.PIPE, start_new_session=True,
                                     env={**os.environ, "BOSS_RUN_ID": owner})
            try:
                self.assertEqual(child.stdout.readline(), b"ready\n")
                result = run("BOSS_RUN_ID", owner, directory)
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
        with tempfile.TemporaryDirectory() as directory:
            child = subprocess.Popen([sys.executable, "-u", "-c",
                                      "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('ready'); time.sleep(60)"],
                                     cwd=directory, stdout=subprocess.PIPE, start_new_session=True,
                                     env={**os.environ, "BOSS_SPAWN_TOKEN": owner})
            try:
                self.assertEqual(child.stdout.readline(), b"ready\n")
                result = run("BOSS_SPAWN_TOKEN", owner, directory)
                self.assertIn(child.pid, result["terminated"])
                self.assertIn(child.pid, result["killed"])
                self.assertEqual(result["remaining"], [])
                print("TERM-resistant cleanup ms:", result["elapsed_ms"], flush=True)
            finally:
                child.kill() if child.poll() is None else None
                child.wait()
                child.stdout.close()

    def test_identity_outside_the_workspace_is_not_signalled(self):
        owner = str(uuid.uuid4())
        with tempfile.TemporaryDirectory() as workspace, tempfile.TemporaryDirectory() as elsewhere:
            idle = [sys.executable, "-c", "import time; time.sleep(60)"]
            inside = subprocess.Popen(idle, cwd=workspace,
                                      env={**os.environ, "BOSS_RUN_ID": owner})
            outside = subprocess.Popen(idle, cwd=elsewhere,
                                       env={**os.environ, "BOSS_RUN_ID": owner})
            try:
                result = run("BOSS_RUN_ID", owner, workspace)
                self.assertIn(inside.pid, result["terminated"])
                self.assertIsNone(outside.poll())
                self.assertNotIn(outside.pid, result["terminated"] + result["killed"])
            finally:
                for child in (inside, outside):
                    child.kill()
                    child.wait()

    def test_registered_shell_outside_the_workspace_is_signalled(self):
        owner = str(uuid.uuid4())
        with tempfile.TemporaryDirectory() as workspace, tempfile.TemporaryDirectory() as elsewhere:
            child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                     cwd=elsewhere, env={**os.environ, "BOSS_RUN_ID": owner})
            try:
                birth, *_ = inspect(child.pid)
                record_identity(child.pid, birth, {"BOSS_RUN_ID": owner})
                result = run("BOSS_RUN_ID", owner, workspace)
                self.assertIn(child.pid, result["terminated"] + result["killed"])
            finally:
                child.kill()
                child.wait()

    def test_lineage_descendant_outside_the_workspace_is_signalled(self):
        owner = str(uuid.uuid4())
        with tempfile.TemporaryDirectory() as workspace, tempfile.TemporaryDirectory() as elsewhere:
            inner = "import os,time; os.chdir(%r); time.sleep(60)" % elsewhere
            script = ("import os,sys,time,subprocess\n"
                      f"child=subprocess.Popen([sys.executable,'-c',{inner!r}])\n"
                      "print(child.pid, flush=True)\n"
                      "time.sleep(60)\n")
            parent = subprocess.Popen([sys.executable, "-u", "-c", script], cwd=workspace,
                                      stdout=subprocess.PIPE, env={**os.environ, "BOSS_RUN_ID": owner})
            try:
                child_pid = int(parent.stdout.readline())
                result = run("BOSS_RUN_ID", owner, workspace)
                self.assertIn(parent.pid, result["terminated"] + result["killed"])
                self.assertIn(child_pid, result["terminated"] + result["killed"])
            finally:
                parent.kill() if parent.poll() is None else None
                parent.wait()
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except (ProcessLookupError, NameError):
                    pass

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

    #[test]
    fn stray_only_requires_that_no_occupant_has_an_owner() {
        let stray = super::Cleanup {
            terminated: vec![],
            killed: vec![],
            remaining: vec![],
            occupants: vec![7],
            unknown: vec![],
            owners: Default::default(),
            elapsed_ms: 0,
        };
        assert!(stray.stray_only());
        let owned = super::Cleanup {
            owners: [("run-a".into(), vec![7])].into(),
            ..stray
        };
        assert!(!owned.stray_only());
        assert!(!owned.empty());
    }
}
