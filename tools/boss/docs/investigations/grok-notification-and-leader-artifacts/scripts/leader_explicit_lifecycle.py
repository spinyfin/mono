import os, signal, subprocess, sys, time
SP, home, ws = sys.argv[1], sys.argv[2], sys.argv[3]
env = dict(os.environ, GROK_HOME=home)
# Own process group, exactly like a pane's leaf process group.
p = subprocess.Popen(["grok", "agent", "leader", "--relay-on-demand", "--no-auto-update"], env=env, cwd=ws,
                     stdout=open(f"{SP}/leader.out","wb"), stderr=subprocess.STDOUT,
                     preexec_fn=os.setsid)
pgid = os.getpgid(p.pid)
print(f"leader pid={p.pid} pgid={pgid}")
time.sleep(10)
print("alive after 10s:", p.poll() is None)
print("socket exists:", os.path.exists(f"{home}/leader.sock"), "->", 
      [f for f in os.listdir(home) if f.startswith("leader")])
sys.stdout.flush()
print("--- SIGTERM the process GROUP (Boss reap shape) ---")
try:
    os.killpg(pgid, signal.SIGTERM)
except ProcessLookupError:
    print("pgid already gone")
time.sleep(4)
alive = p.poll() is None
print("leader alive 4s after SIGTERM group:", alive)
if alive:
    print("!! SURVIVED SIGTERM -- escalating SIGKILL")
    os.killpg(pgid, signal.SIGKILL); time.sleep(2)
    print("alive after SIGKILL:", p.poll() is None)
print("exit code:", p.poll())
print("socket after reap:", os.path.exists(f"{home}/leader.sock"),
      [f for f in os.listdir(home) if f.startswith("leader")])
