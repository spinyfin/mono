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
//! PTY after the CLI disables canonical mode, then a `\r` to submit.
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

READY_SECS = float(os.environ.get("BOSS_FEED_PROMPT_READY_SECS", "20"))
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


def wait_raw(master, pane_out, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not icanon(master):
            return True
        r, _, _ = select.select([master], [], [], 0.05)
        if master in r:
            try:
                data = os.read(master, 65536)
            except OSError:
                return False
            if not data:
                return False
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
                        os.write(master, data)
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
    try:
        prompt = open(prompt_path, "rb").read()
    except OSError as err:
        sys.stderr.write("feed-initial-prompt: cannot read %s: %s\n" % (prompt_path, err))
        return 2
    payload = prompt + b"\r"

    pid, master = pty_fork()
    if pid == 0:
        os.execvp(cli[0], cli)
        os._exit(127)

    pane_in = sys.stdin.fileno() if not sys.stdin.closed else None
    pane_out = sys.stdout.fileno() if not sys.stdout.closed else None
    if pane_in is not None:
        copy_winsize(pane_in, master)
    if not wait_raw(master, pane_out, READY_SECS):
        sys.stderr.write(
            "feed-initial-prompt: timed out waiting for %s to enter raw mode; "
            "not injecting the prompt (canonical mode would drop it past MAX_CANON)\n"
            % cli[0]
        )
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
        os.waitpid(pid, 0)
        return 125
    if not write_prompt(master, payload, pane_out):
        sys.stderr.write("feed-initial-prompt: CLI exited before the prompt was fully written\n")
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
        os.waitpid(pid, 0)
        return 125
    status = relay(master, pane_in, pane_out, pid)
    try:
        os.close(master)
    except OSError:
        pass
    if os.WIFEXITED(status):
        return os.WEXITSTATUS(status)
    if os.WIFSIGNALED(status):
        return 128 + os.WTERMSIG(status)
    return 1


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
    fn feeder_delivers_a_prompt_larger_than_one_mib_without_putting_it_on_argv() {
        let dir = TempDir::new().unwrap();
        let script = write_feed_prompt_script(dir.path()).unwrap();
        let prompt_path = dir.path().join("prompt.txt");
        // 1 MiB + 1, matching the diagnosed ARG_MAX failure (prompt > 1 MiB).
        let mut prompt = "ARGMAX_FEED_MARKER\n".to_owned();
        prompt.push_str(&"P".repeat(1_048_576 + 1 - prompt.len()));
        assert!(prompt.len() > 1_048_576);
        std::fs::write(&prompt_path, &prompt).unwrap();

        let fake_cli = dir.path().join("fake-cli.py");
        let received = dir.path().join("received.bin");
        let argv_out = dir.path().join("argv.txt");
        std::fs::write(
            &fake_cli,
            format!(
                "#!/usr/bin/env python3\n\
                 import os, sys, tty\n\
                 tty.setraw(0)\n\
                 buf = b''\n\
                 while True:\n\
                 \tchunk = os.read(0, 65536)\n\
                 \tif not chunk:\n\
                 \t\tbreak\n\
                 \tbuf += chunk\n\
                 \tif b'\\r' in buf:\n\
                 \t\tbreak\n\
                 open({received:?}, 'wb').write(buf.split(b'\\r')[0])\n\
                 open({argv_out:?}, 'w').write('\\n'.join(sys.argv))\n"
            ),
        )
        .unwrap();

        let status = Command::new("python3")
            .arg(&script)
            .arg(&prompt_path)
            .arg("python3")
            .arg(&fake_cli)
            .env("BOSS_FEED_PROMPT_READY_SECS", "5")
            .current_dir(dir.path())
            .output()
            .expect("run feeder");
        assert!(
            status.status.success(),
            "feeder failed: status={} stdout={} stderr={}",
            status.status,
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr),
        );

        let got = std::fs::read(&received).unwrap_or_default();
        assert_eq!(
            got,
            prompt.as_bytes(),
            "CLI must receive the full prompt body (got {} bytes, expected {})",
            got.len(),
            prompt.len(),
        );
        let argv = std::fs::read_to_string(&argv_out).unwrap_or_default();
        assert!(
            !argv.contains("ARGMAX_FEED_MARKER"),
            "prompt body must not appear on the CLI argv: {argv}"
        );
        assert!(argv.len() < 4096, "CLI argv must stay tiny, got {} bytes", argv.len());
    }
}
