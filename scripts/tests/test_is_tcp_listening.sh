#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$SCRIPT_DIR/../start-velovm.sh"

# Extract process-tree and lsof readiness helpers without executing the launcher.
eval "$(sed -n '/^collect_process_tree_pids()/,/^}/p; /^is_tcp_listening_lsof()/,/^}/p' "$LAUNCHER")"

# Test 1: collect_process_tree_pids includes parent and child PIDs.
python3 - <<'PY' &
import signal, subprocess, sys, time
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"])
try:
    time.sleep(10)
finally:
    child.terminate()
    child.wait()
PY
PARENT_PID=$!
trap 'kill "$PARENT_PID" 2>/dev/null || true; wait "$PARENT_PID" 2>/dev/null || true' EXIT

# Wait briefly for child process to spawn.
CHILD_PID=""
for _ in $(seq 1 40); do
    CHILD_PID="$(pgrep -P "$PARENT_PID" | head -n1 || true)"
    if [[ -n "$CHILD_PID" ]]; then
        break
    fi
    sleep 0.05
done
if [[ -z "$CHILD_PID" ]]; then
    echo "FAIL: test child process did not spawn" >&2
    exit 1
fi

PIDS="$(collect_process_tree_pids "$PARENT_PID")"
if [[ ",$PIDS," != *",$PARENT_PID,"* || ",$PIDS," != *",$CHILD_PID,"* ]]; then
    echo "FAIL: collect_process_tree_pids returned '$PIDS', expected both $PARENT_PID and $CHILD_PID" >&2
    exit 1
fi

# Test 2: is_tcp_listening_lsof detects a listening socket owned by a child process
# even when VMM_PID points only to the parent process.
if command -v lsof >/dev/null 2>&1; then
    PORT_FILE="$(mktemp)"
    python3 - "$PORT_FILE" <<'PY' &
import signal, socket, subprocess, sys, time
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
port_file = sys.argv[1]
code = f"""
import socket, time
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen(1)
with open({port_file!r}, 'w') as f:
    f.write(str(s.getsockname()[1]))
time.sleep(10)
"""
child = subprocess.Popen([sys.executable, "-c", code])
try:
    time.sleep(10)
finally:
    child.terminate()
    child.wait()
PY
    SOCK_PARENT=$!
    trap 'rm -f "$PORT_FILE"; kill "$PARENT_PID" "$SOCK_PARENT" 2>/dev/null || true; wait "$PARENT_PID" "$SOCK_PARENT" 2>/dev/null || true' EXIT

    LISTEN_PORT=""
    for _ in $(seq 1 40); do
        if [[ -s "$PORT_FILE" ]]; then
            LISTEN_PORT="$(cat "$PORT_FILE")"
            break
        fi
        sleep 0.05
    done
    if [[ -z "$LISTEN_PORT" ]]; then
        echo "FAIL: child test listener did not write port" >&2
        exit 1
    fi

    VMM_PID="$SOCK_PARENT"
    if ! is_tcp_listening_lsof; then
        echo "FAIL: is_tcp_listening_lsof did not detect child listener on port $LISTEN_PORT (VMM_PID=$VMM_PID)" >&2
        exit 1
    fi
fi

echo "PASS: start-velovm.sh multi-process lsof readiness checks passed"
