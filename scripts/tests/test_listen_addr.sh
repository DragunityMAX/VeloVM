#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$SCRIPT_DIR/../start-velovm.sh"

# Extract resolve_listen_endpoint without executing the launcher.
eval "$(sed -n '/^resolve_listen_endpoint()/,/^}/p' "$LAUNCHER")"

assert_eq() {
    local got="$1"
    local expected="$2"
    local label="$3"
    if [[ "$got" != "$expected" ]]; then
        echo "FAIL ($label): got '$got', expected '$expected'" >&2
        exit 1
    fi
}

# Bracketed IPv6 without port must normalize both VMM_LISTEN_ADDR and VIEWER_ADDR.
LISTEN_ADDR="[::1]"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "[::1]:5901" "bracketed-ipv6-vmm"
assert_eq "$VIEWER_ADDR" "[::1]:5901" "bracketed-ipv6-viewer"
assert_eq "$LISTEN_HOST" "::1" "bracketed-ipv6-host"
assert_eq "$LISTEN_PORT" "5901" "bracketed-ipv6-port"

# Unbracketed IPv6 without port.
LISTEN_ADDR="::1"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "[::1]:5901" "bare-ipv6-vmm"
assert_eq "$VIEWER_ADDR" "[::1]:5901" "bare-ipv6-viewer"

# Bracketed wildcard IPv6 without port binds :: while viewer connects to ::1.
LISTEN_ADDR="[::]"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "[::]:5901" "bracketed-ipv6-any-vmm"
assert_eq "$VIEWER_ADDR" "[::1]:5901" "bracketed-ipv6-any-viewer"

# Bracketed IPv6 with explicit port preserves port.
LISTEN_ADDR="[::1]:5905"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "[::1]:5905" "bracketed-ipv6-port-vmm"
assert_eq "$VIEWER_ADDR" "[::1]:5905" "bracketed-ipv6-port-viewer"
assert_eq "$LISTEN_PORT" "5905" "bracketed-ipv6-port-num"

# IPv4 without port.
LISTEN_ADDR="127.0.0.1"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "127.0.0.1" "ipv4-vmm"
assert_eq "$VIEWER_ADDR" "127.0.0.1:5901" "ipv4-viewer"

echo "PASS: start-velovm.sh resolve_listen_endpoint checks passed"
