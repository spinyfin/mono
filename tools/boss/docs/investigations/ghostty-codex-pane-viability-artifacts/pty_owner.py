#!/usr/bin/env python3
"""Own a pty master; spawn shell running codex; allow inject; log master reads.

Apparatus honesty (Q2):
  This is a *harness-emulated* post-exit shell, not real Ghostty interactive zsh.
  After `codex exec` exits, the slave script does explicit `read -r LINE` then
  `eval \"$LINE\"`. That proves: (1) master-side inject survives codex exit in
  the tty input buffer, (2) a subsequent shell *read* can consume the line, and
  (3) *execution* only under this constructed read/eval — do not claim pure
  interactive-shell observation of "shell executes injected text".

  Stdin shape: open-tty pane (codex inherits the slave as 0/1/2). Not `</dev/null`.
"""
import os, sys, time, select, pty, subprocess, signal, threading, fcntl, termios, struct
from shutil import which

SCRATCH = os.environ.get("CODEX_PANE_SPIKE_SCRATCH", "/tmp/codex-pane-spike")
CODEX = os.environ.get("CODEX_BIN") or which("codex") or os.path.expanduser("~/.local/bin/codex")
os.makedirs(SCRATCH, exist_ok=True)

master, slave = pty.openpty()
slave_name = os.ttyname(slave)
print(f"master_fd={master} slave={slave_name}", flush=True)
open(f"{SCRATCH}/pty_slave.txt","w").write(slave_name)

# Spawn a scripted shell on the slave. Post-codex path is *harness-emulated*
# (read/eval), not an interactive prompt — see module docstring.
script = f"""#!/bin/zsh
echo $$ > {SCRATCH}/shell_pid.txt
tty > {SCRATCH}/tty.txt
python3 -c 'import os; print({{i: (os.ttyname(i) if os.isatty(i) else "notty") for i in (0,1,2)}})' > {SCRATCH}/fds_before.txt
{CODEX} exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \\
  "run: sleep 18; reply with exactly: pty-owner-done"
echo $? > {SCRATCH}/codex_exit.txt
# HARNESS-EMULATED post-exit shell (not real interactive Ghostty zsh):
# consume any line left in the tty input buffer after codex exits, record it,
# then eval it so execution is measurable. This is a constructed stand-in.
if read -r LINE; then
  print -r -- "$LINE" > {SCRATCH}/shell_got_line.txt
  eval "$LINE"
  echo $? > {SCRATCH}/eval_exit.txt
else
  echo "(read failed/eof)" > {SCRATCH}/shell_got_line.txt
fi
date > {SCRATCH}/finished.txt
"""
open(f"{SCRATCH}/inner.sh","w").write(script)
os.chmod(f"{SCRATCH}/inner.sh", 0o755)

pid = os.fork()
if pid == 0:
    os.close(master)
    os.setsid()
    # make slave controlling tty
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    os.dup2(slave, 0)
    os.dup2(slave, 1)
    os.dup2(slave, 2)
    if slave > 2:
        os.close(slave)
    os.execv("/bin/zsh", ["zsh", f"{SCRATCH}/inner.sh"])
    os._exit(127)

os.close(slave)
open(f"{SCRATCH}/owner_pid.txt","w").write(str(os.getpid()))
open(f"{SCRATCH}/child_pid.txt","w").write(str(pid))
print(f"child_pid={pid}", flush=True)

# Master reader thread: captures everything the "terminal" would display
out_path = f"{SCRATCH}/master_capture.txt"
out_f = open(out_path, "wb")
stop = threading.Event()

def reader():
    while not stop.is_set():
        r,_,_ = select.select([master], [], [], 0.2)
        if r:
            try:
                data = os.read(master, 8192)
            except OSError:
                break
            if not data:
                break
            out_f.write(data)
            out_f.flush()

t = threading.Thread(target=reader, daemon=True)
t.start()

# Wait until shell_pid appears and codex is running, then signal ready
for _ in range(100):
    if os.path.exists(f"{SCRATCH}/shell_pid.txt"):
        break
    time.sleep(0.1)

# Wait a few seconds into the codex run, then inject via MASTER (correct path for
# typed input / SendToPane). Line is expected to sit in the tty input buffer while
# codex is foreground; harness post-exit read/eval (above) measures consumption.
time.sleep(4)
payload = f"echo INJECTED_VIA_MASTER > {SCRATCH}/injected_side_effect.txt\n".encode()
print(f"injecting via master: {payload!r}", flush=True)
os.write(master, payload)
open(f"{SCRATCH}/inject_time.txt","w").write(str(time.time()))

# Wait for child to finish
deadline = time.time() + 120
while time.time() < deadline:
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid == pid:
        print(f"child exited status={status}", flush=True)
        break
    time.sleep(0.5)
else:
    print("timeout waiting child", flush=True)
    os.kill(pid, signal.SIGTERM)

time.sleep(0.5)
stop.set()
t.join(timeout=2)
out_f.close()
print("owner done", flush=True)
# print summary
for name in ["shell_pid","tty","codex_exit","shell_got_line","injected_side_effect","eval_exit","finished"]:
    p = f"{SCRATCH}/{name}.txt"
    if os.path.exists(p):
        print(f"{name}: {open(p).read()!r}")
    else:
        print(f"{name}: MISSING")
print(f"master_capture bytes: {os.path.getsize(out_path)}")
# show last 800 chars of capture
data = open(out_path,"rb").read()
print("--- master capture tail ---")
print(data[-1200:].decode("utf-8","replace"))
