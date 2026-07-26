import os, pty, select, time, signal, fcntl, termios, struct, json
from pathlib import Path
SCRATCH="/tmp/codex-pane-spike"
REPO="/Users/brianduff/.local/share/cube/workspaces/mono-agent-126"
CODEX="/Users/brianduff/.local/bin/codex"
log=open(f"{SCRATCH}/q5b.log","w",buffering=1)
def p(*a):
    print(*a, flush=True); print(*a, file=log, flush=True)

home=Path.home()/".codex"/"sessions"/"2026"/"07"/"26"
before=set(home.glob("rollout-*.jsonl"))

master,slave=pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH",40,120,0,0))
pid=os.fork()
if pid==0:
    os.setsid()
    try: fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    except Exception: pass
    os.dup2(slave,0); os.dup2(slave,1); os.dup2(slave,2)
    if slave>2: os.close(slave)
    os.close(master)
    os.chdir(REPO)
    os.execv(CODEX,["codex","--no-alt-screen","--dangerously-bypass-approvals-and-sandbox","-C",REPO,
        "run: sleep 90; reply with exactly: should-not-finish"])
    os._exit(127)
os.close(slave)
p(f"pid={pid}")
buf=b""; out=open(f"{SCRATCH}/q5b_cap.bin","wb")
start=time.time(); phase="run"

def alive():
    try: os.kill(pid,0); return True
    except ProcessLookupError: return False

while time.time()-start < 55:
    t=time.time()-start
    if phase=="run" and t>=7:
        p(f"t={t:.1f} ESC")
        os.write(master, b"\x1b")
        phase="esc_wait"
    elif phase=="esc_wait" and t>=12:
        p(f"t={t:.1f} alive={alive()}")
        # clear any partial input then type follow-up; try Enter as \r and also \n
        # First Ctrl-U to clear line, then type, then Enter
        os.write(master, b"\x15")  # NAK / ctrl-u clear
        time.sleep(0.2)
        os.write(master, b"reply with exactly: second-turn-ok")
        time.sleep(0.3)
        os.write(master, b"\r")
        p(f"t={t:.1f} submitted follow-up with CR")
        phase="wait2"
    elif phase=="wait2" and t>=35:
        # if still nothing, try again with newline
        p(f"t={t:.1f} try submit with newline too")
        os.write(master, b"\n")
        phase="wait3"
    elif phase=="wait3" and t>=50:
        break
    r,_,_=select.select([master],[],[],0.3)
    if r:
        try: data=os.read(master,8192)
        except OSError: break
        if not data: break
        buf+=data; out.write(data); out.flush()

out.close()
open(f"{SCRATCH}/q5b_cap.txt","w").write(buf.decode("utf-8","replace"))
p(f"final alive={alive()} bytes={len(buf)}")

# find new rollout
time.sleep(0.5)
after=set(home.glob("rollout-*.jsonl"))
new=list(after-before)
if not new:
    new=[max(home.glob("rollout-*.jsonl"), key=lambda x:x.stat().st_mtime)]
for rp in new:
    p(f"rollout {rp}")
    events=[]
    for line in open(rp):
        o=json.loads(line)
        pl=o.get("payload") or {}
        events.append((o.get("type"), pl.get("type"), (pl.get("message") or pl.get("reason") or "")[:80] if isinstance(pl,dict) else ""))
        if isinstance(pl,dict) and pl.get("type") in ("turn_aborted","user_message","agent_message","task_started","task_complete"):
            p(f"  EVENT {pl.get('type')}: {json.dumps(pl)[:220]}")
    p(f"  total lines {len(events)}")

if alive():
    try: os.kill(pid, signal.SIGTERM)
    except ProcessLookupError: pass
try: os.waitpid(pid,0)
except ChildProcessError: pass
open(f"{SCRATCH}/q5b_done","w").write("ok")
p("DONE")
