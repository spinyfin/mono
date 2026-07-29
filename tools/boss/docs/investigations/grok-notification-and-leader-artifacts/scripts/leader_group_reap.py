import os, pty, signal, subprocess, sys, time
H, W = sys.argv[1], sys.argv[2]
def live(p):
    """Authoritative liveness: ps, excluding zombies (STAT starting 'Z')."""
    r = subprocess.run(["ps","-o","stat=","-p",str(p)], capture_output=True, text=True)
    s = r.stdout.strip()
    return bool(s) and not s.startswith("Z")
pid, fd = pty.fork()
if pid == 0:
    os.environ.update({"GROK_HOME": H, "TERM": "xterm-256color"})
    os.chdir(W)
    os.execvp("grok", ["grok","--model","grok-4.5","--always-approve","--trust",
                       "--no-subagents","--no-memory","--no-alt-screen"])
    os._exit(127)
os.set_blocking(fd, False)
t0 = time.time()
while time.time() - t0 < 20:
    try: os.read(fd, 65536)
    except OSError: pass
    time.sleep(0.2)
pg = os.getpgid(pid)
kids = [int(l.split()[0]) for l in subprocess.run(["ps","-eo","pid,ppid,comm"],
        capture_output=True, text=True).stdout.splitlines() if l.split()[1:2]==[str(pid)]]
print(f"TUI pid={pid} pgid={pg}")
for k in kids:
    print(f"  child(leader) pid={k} pgid={os.getpgid(k)} in_pane_group={os.getpgid(k)==pg}")
print(f"--- killpg({pg}, SIGTERM)  [exactly Boss's pane reap] ---")
os.killpg(pg, signal.SIGTERM)
for mark in (2,5,10):
    time.sleep(mark - (0 if mark==2 else prev)); prev = mark
    try: os.waitpid(pid, os.WNOHANG)      # reap zombie so ps is honest
    except ChildProcessError: pass
    print(f"  t+{mark}s TUI_live={live(pid)} " +
          " ".join(f"leader{k}_live={live(k)}" for k in kids))
prev = 10
for k in kids:
    print(f"leader {k} ppid after reap = " +
          subprocess.run(["ps","-o","ppid=","-p",str(k)],capture_output=True,text=True).stdout.strip() or "(gone)")
# cleanup: leave no leaked processes behind
for k in kids:
    try: os.kill(k, signal.SIGKILL)
    except OSError: pass
try: os.killpg(pg, signal.SIGKILL)
except OSError: pass
