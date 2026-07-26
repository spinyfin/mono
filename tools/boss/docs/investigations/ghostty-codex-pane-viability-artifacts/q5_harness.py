import os, pty, select, time, signal, fcntl, termios, struct, re, sys
SCRATCH="/tmp/codex-pane-spike"
REPO="/Users/brianduff/.local/share/cube/workspaces/mono-agent-126"
CODEX="/Users/brianduff/.local/bin/codex"
log = open(f"{SCRATCH}/q5_harness.log","w", buffering=1)
def p(*a):
    print(*a, flush=True)
    print(*a, file=log, flush=True)

master, slave = pty.openpty()
winsize = struct.pack("HHHH", 40, 120, 0, 0)
fcntl.ioctl(slave, termios.TIOCSWINSZ, winsize)
pid = os.fork()
if pid == 0:
    os.setsid()
    try:
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    except Exception:
        pass
    os.dup2(slave,0); os.dup2(slave,1); os.dup2(slave,2)
    if slave>2: os.close(slave)
    os.close(master)
    os.chdir(REPO)
    os.execv(CODEX, [
        "codex", "--no-alt-screen",
        "--dangerously-bypass-approvals-and-sandbox",
        "-C", REPO,
        "run the shell command: sleep 60; then reply with exactly: should-not-finish-if-aborted"
    ])
    os._exit(127)
os.close(slave)
open(f"{SCRATCH}/q5_tui_pid.txt","w").write(str(pid))
p(f"tui pid={pid}")

buf=b""
out=open(f"{SCRATCH}/q5_capture.bin","wb")
start=time.time()
phase="wait_running"

def still_alive():
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False

while time.time()-start < 50:
    t = time.time()-start
    if phase=="wait_running" and t >= 8:
        p(f"t={t:.1f} sending ESC")
        os.write(master, b"\x1b")
        open(f"{SCRATCH}/q5_esc_time.txt","w").write(str(t))
        phase="after_esc"
    elif phase=="after_esc" and t >= 12:
        alive = still_alive()
        p(f"t={t:.1f} after esc, process_alive={alive}")
        open(f"{SCRATCH}/q5_alive_after_esc.txt","w").write(str(alive))
        if alive:
            msg = b"reply with exactly: after-esc-pong\r"
            p(f"t={t:.1f} sending follow-up")
            os.write(master, msg)
            open(f"{SCRATCH}/q5_followup_time.txt","w").write(str(t))
            phase="after_followup"
        else:
            phase="dead"
            break
    elif phase=="after_followup" and t >= 40:
        break

    r,_,_ = select.select([master],[],[],0.3)
    if r:
        try:
            data=os.read(master,8192)
        except OSError as e:
            p(f"read err {e}")
            break
        if not data:
            p("eof")
            break
        buf += data
        out.write(data); out.flush()

alive = still_alive()
open(f"{SCRATCH}/q5_alive_final.txt","w").write(str(alive))
p(f"final alive={alive} bytes={len(buf)}")
out.close()
open(f"{SCRATCH}/q5_capture.txt","w").write(buf.decode("utf-8","replace"))

# Also check rollout files written during this window for abort events
# Find newest rollouts
import glob, json
from pathlib import Path
home = Path.home() / ".codex" / "sessions"
rollouts = sorted(home.rglob("rollout-*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True)[:5]
open(f"{SCRATCH}/q5_recent_rollouts.txt","w").write("\n".join(str(x) for x in rollouts))
# scan newest for abort-related
for rp in rollouts[:2]:
    types=[]
    abortish=[]
    for line in open(rp):
        try:
            o=json.loads(line)
        except Exception:
            continue
        t=o.get("type")
        types.append(t)
        payload=o.get("payload") or {}
        blob=json.dumps(o).lower()
        if any(k in blob for k in ["abort", "cancel", "interrupt", "turn_aborted", "task_complete", "error"]):
            abortish.append(line[:300])
    p(f"rollout {rp.name}: n={len(types)} abortish={len(abortish)}")
    open(f"{SCRATCH}/q5_rollout_scan_{rp.name}.txt","w").write(
        "types sample: " + str(types[:30]) + "\n" + "\n".join(abortish[:20])
    )

if alive:
    try: os.kill(pid, signal.SIGTERM)
    except ProcessLookupError: pass
try: os.waitpid(pid, 0)
except ChildProcessError: pass

# keyword scan of capture
low = buf.decode("utf-8","replace").lower()
for key in ["abort","cancelled","canceled","interrupted","stopped","after-esc-pong","should-not-finish","esc"]:
    p(f"capture contains {key!r}: {key.encode() in buf.lower() if False else key in low}")

# Better: strip CSI then search contiguous alnum runs
import re as _re
t = buf.decode("utf-8","replace")
t2 = _re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]","", t)
t2 = _re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)","", t2)
# remove spaces between single chars roughly: take all non-whitespace chars in order for search
compact = _re.sub(r"\s+","", t2)
open(f"{SCRATCH}/q5_compact.txt","w").write(compact[:5000])
for key in ["after-esc-pong","should-not-finish","aborted","interrupted","Stopped","Esc"]:
    p(f"compact has {key}: {key.lower() in compact.lower()}")
p("DONE")
open(f"{SCRATCH}/q5_done","w").write("ok")
