#!/bin/bash
set -euo pipefail
# Manual probe: an executor deadline must interrupt the wrapper around this
# live process, rather than a fixture sending its own termination signal.
exec python3 -c 'import threading; threading.current_thread().name = "timeout_capture_fixture"; print("timeout capture fixture ready", flush=True); threading.Event().wait()'
