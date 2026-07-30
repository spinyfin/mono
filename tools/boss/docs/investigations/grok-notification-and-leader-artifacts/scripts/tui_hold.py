# Start a grok TUI in a pty and hold it alive; print its pid, then idle.
import os, pty, sys, time, signal
home, ws, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
pid, fd = pty.fork()
if pid == 0:
    os.environ.update({"GROK_HOME": home, "TERM": "xterm-256color"})
    os.chdir(ws)
    os.execvp("grok", ["grok", "--model", "grok-4.5", "--always-approve", "--trust",
                       "--no-subagents", "--no-memory", "--no-alt-screen"])
    os._exit(127)
open(sys.argv[4], "w").write(str(pid))
os.set_blocking(fd, False)
end = time.time() + secs
while time.time() < end:
    try: os.read(fd, 65536)
    except OSError: pass
    time.sleep(0.2)
