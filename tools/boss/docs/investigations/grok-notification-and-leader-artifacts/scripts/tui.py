#!/usr/bin/env python3
"""Drive an interactive grok TUI in a pty. Not GhosttyKit -- we only need the
hook payloads here, not pane scraping (that is already characterised)."""
import os, pty, select, signal, subprocess, sys, time

def run(args, env, script, capture, settle=6.0):
    """script: list of (delay_secs, bytes_to_send). Returns (exit_code, pid)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(env)
        os.execvp(args[0], args)
        os._exit(127)
    os.set_blocking(fd, False)
    out = bytearray()
    def pump(dur):
        end = time.time() + dur
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.2)
            if r:
                try:
                    d = os.read(fd, 65536)
                except OSError:
                    return False
                if not d:
                    return False
                out.extend(d)
        return True
    for delay, data in script:
        if not pump(delay):
            break
        try:
            os.write(fd, data)
        except OSError:
            break
    pump(settle)
    # reap
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.time() + 5
    code = None
    while time.time() < deadline:
        w, st = os.waitpid(pid, os.WNOHANG)
        if w:
            code = st
            break
        time.sleep(0.1)
    if code is None:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        _, code = os.waitpid(pid, 0)
    open(capture, "wb").write(bytes(out))
    return code, pid
