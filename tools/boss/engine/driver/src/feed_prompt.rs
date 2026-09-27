//! Feed the worker's initial prompt into a TUI CLI without putting the
//! prompt bytes on argv (macOS `ARG_MAX` is 1 MiB; a review-guide prompt
//! can be several MiB).
//!
//! Interactive Claude 2.1.283, Codex 0.153.4, and Grok 1.0.41 all take the
//! first user message as a positional string. Native file/stdin channels on
//! those versions are headless (`grok --prompt-file`, `claude -p` + stdin,
//! `codex exec -`) and would change the worker from a long-lived pane into
//! a one-shot process. This helper keeps the TUI: it execs the CLI on a
//! PTY (so `isatty(stdin)` stays true) and writes the prompt file into that
//! PTY after the CLI disables canonical mode and enables bracketed paste.
//! The prompt is a bracketed paste (CRLF normalized to LF), followed by a
//! separate `\r` after a 500-ms settling interval to submit.
//!
//! The spawn command names this script and the prompt *path* only — never
//! the prompt body — so `execve` stays well under `ARG_MAX`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Workspace-relative path of the feeder script written at provision/spawn.
pub const FEED_PROMPT_SCRIPT_REL_PATH: &str = ".boss/feed-initial-prompt";

/// Bytes of the feeder. Invoked as
/// `python3 .boss/feed-initial-prompt <prompt-relpath> <cli> [args...]`.
pub const FEED_PROMPT_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Feed a prompt file into a TUI CLI over a PTY so the prompt never lands on argv."""
import errno
import fcntl
import os
import select
import signal
import struct
import sys
import termios
import time
import tty

READY_SECS = float(os.environ.get("BOSS_FEED_PROMPT_READY_SECS", "120"))
CHUNK = 4096


def copy_winsize(src_fd, dst_fd):
    try:
        packed = fcntl.ioctl(src_fd, termios.TIOCGWINSZ, b"\x00" * 8)
        fcntl.ioctl(dst_fd, termios.TIOCSWINSZ, packed)
    except OSError:
        pass


def icanon(fd):
    try:
        return bool(termios.tcgetattr(fd)[3] & termios.ICANON)
    except termios.error:
        return True


def drain_to(dst, data):
    if dst is None or not data:
        return
    off = 0
    while off < len(data):
        try:
            n = os.write(dst, data[off:])
        except OSError as err:
            if err.errno in (errno.EIO, errno.EPIPE, errno.EBADF):
                return
            raise
        if n == 0:
            return
        off += n


def wait_raw(master, pane_in, pane_out, timeout):
    started = time.monotonic()
    output_tail = b""
    paste_enabled = False
    while time.monotonic() - started < timeout:
        if paste_enabled and not icanon(master):
            return True
        fds = [master] + ([pane_in] if pane_in is not None else [])
        r, _, _ = select.select(fds, [], [], 0.05)
        if pane_in is not None and pane_in in r:
            data = os.read(pane_in, 65536)
            if data:
                drain_to(master, data)
            else:
                pane_in = None
        if master in r:
            try:
                data = os.read(master, 65536)
            except OSError:
                return False
            if not data:
                return False
            output_tail += data
            enabled = output_tail.rfind(b"\x1b[?2004h")
            disabled = output_tail.rfind(b"\x1b[?2004l")
            if max(enabled, disabled) >= 0:
                paste_enabled = enabled > disabled
            output_tail = output_tail[-7:]
            drain_to(pane_out, data)
    return False


def write_prompt(master, payload, pane_out):
    off = 0
    while off < len(payload):
        r, w, _ = select.select([master], [master], [], 1.0)
        if master in r:
            try:
                data = os.read(master, 65536)
            except OSError:
                return False
            if not data:
                return False
            drain_to(pane_out, data)
        if master in w:
            try:
                n = os.write(master, payload[off : off + CHUNK])
            except OSError:
                return False
            off += n
    return True


def relay(master, pane_in, pane_out, pid):
    while True:
        fds = [master]
        if pane_in is not None:
            fds.append(pane_in)
        try:
            r, _, _ = select.select(fds, [], [], 0.25)
        except (select.error, ValueError):
            break
        if master in r:
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            drain_to(pane_out, data)
        if pane_in is not None and pane_in in r:
            try:
                data = os.read(pane_in, 65536)
            except OSError:
                pane_in = None
            else:
                if not data:
                    pane_in = None
                else:
                    try:
                        drain_to(master, data)
                    except OSError:
                        break
        wpid, status = os.waitpid(pid, os.WNOHANG)
        if wpid != 0:
            return status
    _, status = os.waitpid(pid, 0)
    return status


def main():
    if len(sys.argv) < 3:
        sys.stderr.write("usage: feed-initial-prompt PROMPT_FILE CLI [args...]\n")
        return 2
    prompt_path = sys.argv[1]
    cli = sys.argv[2:]
    # Recheck the actual shell environment at the final exec boundary too:
    # login-shell rc files can add values after the engine's preflight.
    argv_bytes = sum(len(os.fsencode(arg)) + 1 + struct.calcsize("P") for arg in cli)
    env_bytes = sum(len(k) + len(v) + 2 + struct.calcsize("P") for k, v in os.environb.items())
    limit = os.sysconf("SC_ARG_MAX")
    if argv_bytes + env_bytes + 4096 > limit:
        sys.stderr.write(
            "feed-initial-prompt: refusing exec: argv %s bytes + environment %s bytes "
            "+ 4096 launcher bytes exceeds ARG_MAX %s bytes\n" % (argv_bytes, env_bytes, limit)
        )
        return 126
    try:
        prompt = open(prompt_path, "rb").read()
    except OSError as err:
        sys.stderr.write("feed-initial-prompt: cannot read %s: %s\n" % (prompt_path, err))
        return 2
    payload = b"\x1b[200~" + prompt.replace(b"\r\n", b"\n") + b"\x1b[201~"

    pid, master = pty_fork()
    if pid == 0:
        os.execvp(cli[0], cli)
        os._exit(127)

    def stop_child(sig):
        # forkpty creates a separate session; teardown must reach the whole
        # CLI group, including stdio servers which may ignore SIGHUP/TERM.
        def signal_group(signum):
            try:
                os.killpg(pid, signum)
            except OSError as err:
                # Darwin reports EPERM for a zombie-only process group.
                if err.errno not in (errno.EPERM, errno.ESRCH):
                    raise

        try:
            signal_group(sig)
            deadline = time.monotonic() + 3.0
            while time.monotonic() < deadline:
                if os.waitpid(pid, os.WNOHANG)[0]:
                    break
                time.sleep(0.05)
            signal_group(signal.SIGKILL)
        finally:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass

    def terminate(sig, _frame):
        stop_child(sig)
        raise SystemExit(128 + sig)

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)
    pane_in = sys.stdin.fileno() if not sys.stdin.closed else None
    pane_out = sys.stdout.fileno() if not sys.stdout.closed else None
    saved = None

    def resize(_sig=None, _frame=None):
        if pane_in is not None:
            copy_winsize(pane_in, master)

    signal.signal(signal.SIGWINCH, resize)
    try:
        if pane_in is not None and os.isatty(pane_in):
            saved = termios.tcgetattr(pane_in)
            tty.setraw(pane_in)
        resize()
        started = time.monotonic()
        if not wait_raw(master, pane_in, pane_out, READY_SECS):
            sys.stderr.write(
                "feed-initial-prompt: %s did not enable raw mode and bracketed paste after %.2fs "
                "(exited or readiness timeout); prompt was not injected\n"
                % (cli[0], time.monotonic() - started)
            )
            stop_child(signal.SIGTERM)
            return 125
        if not write_prompt(master, payload, pane_out):
            sys.stderr.write("feed-initial-prompt: CLI exited before the prompt was fully written\n")
            stop_child(signal.SIGTERM)
            return 125
        # Let the TUI finish its paste/debounce handling before Enter arrives.
        time.sleep(0.5)
        if not write_prompt(master, b"\r", pane_out):
            sys.stderr.write("feed-initial-prompt: CLI exited before prompt submission\n")
            stop_child(signal.SIGTERM)
            return 125
        status = relay(master, pane_in, pane_out, pid)
        if os.WIFEXITED(status):
            return os.WEXITSTATUS(status)
        if os.WIFSIGNALED(status):
            return 128 + os.WTERMSIG(status)
        return 1
    finally:
        if saved is not None:
            termios.tcsetattr(pane_in, termios.TCSANOW, saved)
        os.close(master)


def pty_fork():
    import pty

    return pty.fork()


if __name__ == "__main__":
    sys.exit(main())
"#;

/// Write [`FEED_PROMPT_SCRIPT`] to `<workspace>/.boss/feed-initial-prompt`.
pub fn write_feed_prompt_script(workspace: &Path) -> Result<PathBuf> {
    let path = workspace.join(FEED_PROMPT_SCRIPT_REL_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for the prompt-feed script", parent.display()))?;
    }
    std::fs::write(&path, FEED_PROMPT_SCRIPT)
        .with_context(|| format!("writing prompt-feed script to {}", path.display()))?;
    Ok(path)
}

/// Prefix `command` so the pane runs the feeder with the driver's prompt file
/// instead of expanding that file onto the CLI's argv.
///
/// `command` is the driver argv (flags + binary), including its trailing
/// newline. The feeder path and prompt path are the only additions — the
/// prompt *body* never appears here.
pub fn wrap_spawn_command_to_feed_prompt(command: &str, config_dir: &str, filename: &str) -> String {
    let body = command.trim_end_matches('\n').trim_end();
    let rel = format!("{config_dir}/{filename}");
    format!(
        "python3 {FEED_PROMPT_SCRIPT_REL_PATH} {} {body}\n",
        boss_ssh_transport::shell_quote(&rel),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn wrap_names_the_script_and_prompt_path_not_the_body() {
        let wrapped = wrap_spawn_command_to_feed_prompt(
            "claude --model opus --permission-mode auto\n",
            ".claude",
            "initial-prompt.txt",
        );
        assert!(
            wrapped.starts_with("python3 .boss/feed-initial-prompt '.claude/initial-prompt.txt' claude"),
            "got {wrapped}"
        );
        assert!(wrapped.ends_with('\n'));
        assert!(!wrapped.contains("$(cat"));
        assert!(wrapped.len() < 200, "wrapper must stay tiny, got {}", wrapped.len());
    }

    #[test]
    fn write_script_creates_boss_dir() {
        let dir = TempDir::new().unwrap();
        let path = write_feed_prompt_script(dir.path()).unwrap();
        assert_eq!(path, dir.path().join(FEED_PROMPT_SCRIPT_REL_PATH));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("Feed a prompt file into a TUI CLI"));
    }

    #[cfg(unix)]
    #[test]
    fn feeder_terminal_contract() {
        let dir = TempDir::new().unwrap();
        let script = write_feed_prompt_script(dir.path()).unwrap();
        let harness = dir.path().join("terminal-tests.py");
        std::fs::write(&harness, include_str!("feed_prompt_tests.py")).unwrap();
        let output = Command::new("python3")
            .arg(&harness)
            .arg(&script)
            .arg(dir.path())
            .output()
            .expect("run feeder terminal tests");
        assert!(
            output.status.success(),
            "terminal tests failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
