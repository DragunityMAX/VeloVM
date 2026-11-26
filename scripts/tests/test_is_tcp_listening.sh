#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$SCRIPT_DIR/../start-velovm.sh"

# Extract process-tree and TCP readiness helpers without executing the launcher.
eval "$(sed -n '/^collect_process_tree_pids()/,/^}/p; /^is_tcp_listening_lsof()/,/^}/p; /^matching_tcp_inodes()/,/^}/p; /^is_any_tcp_listening()/,/^}/p; /^is_tcp_listening()/,/^}/p' "$LAUNCHER")"

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

# Test 3: is_tcp_listening ignores unrelated listeners on the same port and accepts VMM_PID/child listeners.
PORT_FILE2="$(mktemp)"
python3 - "$PORT_FILE2" <<'PY' &
import signal, socket, sys, time
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen(1)
with open(sys.argv[1], 'w') as f:
    f.write(str(s.getsockname()[1]))
time.sleep(10)
PY
OTHER_PID=$!
sleep 0.1
for _ in $(seq 1 40); do
    if [[ -s "$PORT_FILE2" ]]; then
        break
    fi
    sleep 0.05
done
LISTEN_HOST="127.0.0.1"
LISTEN_PORT="$(cat "$PORT_FILE2")"

if ! is_any_tcp_listening; then
    echo "FAIL: is_any_tcp_listening did not detect unrelated listener on $LISTEN_HOST:$LISTEN_PORT" >&2
    kill "$OTHER_PID" 2>/dev/null || true
    rm -f "$PORT_FILE2"
    exit 1
fi

sleep 10 &
UNRELATED_VMM=$!
VMM_PID="$UNRELATED_VMM"
if is_tcp_listening; then
    echo "FAIL: is_tcp_listening accepted another process's listener on port $LISTEN_PORT" >&2
    kill "$OTHER_PID" "$UNRELATED_VMM" 2>/dev/null || true
    rm -f "$PORT_FILE2"
    exit 1
fi

VMM_PID="$OTHER_PID"
if ! is_tcp_listening; then
    echo "FAIL: is_tcp_listening did not accept VMM_PID's own listener on port $LISTEN_PORT" >&2
    kill "$OTHER_PID" "$UNRELATED_VMM" 2>/dev/null || true
    rm -f "$PORT_FILE2"
    exit 1
fi
kill "$UNRELATED_VMM" 2>/dev/null || true
wait "$UNRELATED_VMM" 2>/dev/null || true

# Test 4: End-to-end pre-start rejection when port is already in use, and non-zero openvmm exit propagation.
TMP_ROOT="$(mktemp -d)"
trap 'rm -f "$PORT_FILE" "$PORT_FILE2"; rm -rf "$TMP_ROOT"; kill "$PARENT_PID" "${SOCK_PARENT:-}" "$OTHER_PID" 2>/dev/null || true; wait "$PARENT_PID" "${SOCK_PARENT:-}" "$OTHER_PID" 2>/dev/null || true' EXIT

cp "$LAUNCHER" "$TMP_ROOT/start-velovm.sh"
chmod +x "$TMP_ROOT/start-velovm.sh"
mkdir -p "$TMP_ROOT/firmware"
: > "$TMP_ROOT/firmware/MSVM.fd"
: > "$TMP_ROOT/disk.vhdx"
printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP_ROOT/velovm-viewer"
printf '#!/usr/bin/env bash\necho "should not start" > "%s/started"\nexit 0\n' "$TMP_ROOT" > "$TMP_ROOT/openvmm"
chmod +x "$TMP_ROOT/openvmm" "$TMP_ROOT/velovm-viewer"

if err_out="$("$TMP_ROOT/start-velovm.sh" --disk "$TMP_ROOT/disk.vhdx" --listen "127.0.0.1:${LISTEN_PORT}" 2>&1)"; then
    echo "FAIL: start-velovm.sh succeeded despite port $LISTEN_PORT already listening" >&2
    exit 1
fi
if [[ -e "$TMP_ROOT/started" ]]; then
    echo "FAIL: start-velovm.sh spawned openvmm even though port $LISTEN_PORT was already in use" >&2
    exit 1
fi

kill "$OTHER_PID" 2>/dev/null || true
wait "$OTHER_PID" 2>/dev/null || true

# Now test openvmm exiting non-zero after listener is ready while viewer exits 0.
cat > "$TMP_ROOT/openvmm" <<'EOF'
#!/usr/bin/env python3
import signal, socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
addr = sys.argv[sys.argv.index("--fastdesk-listen") + 1]
host, port = addr.rsplit(":", 1)
s.bind((host, int(port)))
s.listen(1)
conn, _ = s.accept()
conn.close()
s.close()
sys.exit(42)
EOF
cat > "$TMP_ROOT/velovm-viewer" <<'EOF'
#!/usr/bin/env python3
import socket, sys, time
target = sys.argv[sys.argv.index("--connect") + 1].removeprefix("tcp://")
host, port = target.rsplit(":", 1)
c = socket.create_connection((host, int(port)), timeout=2)
c.close()
time.sleep(0.2)
sys.exit(0)
EOF
chmod +x "$TMP_ROOT/openvmm" "$TMP_ROOT/velovm-viewer"

rc=0
"$TMP_ROOT/start-velovm.sh" --disk "$TMP_ROOT/disk.vhdx" --listen "127.0.0.1:${LISTEN_PORT}" >/dev/null 2>&1 || rc=$?
if [[ "$rc" -ne 42 ]]; then
    echo "FAIL: expected start-velovm.sh to propagate openvmm exit code 42, got $rc" >&2
    exit 1
fi

echo "PASS: start-velovm.sh multi-process lsof and /proc readiness checks passed"
