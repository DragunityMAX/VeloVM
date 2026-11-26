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

# IPv4 with explicit port.
LISTEN_ADDR="127.0.0.1:6000"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "127.0.0.1:6000" "ipv4-port-vmm"
assert_eq "$VIEWER_ADDR" "127.0.0.1:6000" "ipv4-port-viewer"
assert_eq "$LISTEN_PORT" "6000" "ipv4-port-num"

# IPv4-mapped IPv6 wildcard normalizes viewer address to 127.0.0.1.
LISTEN_ADDR="::ffff:0.0.0.0"
resolve_listen_endpoint
assert_eq "$VMM_LISTEN_ADDR" "[::ffff:0.0.0.0]:5901" "ipv4-mapped-any-vmm"
assert_eq "$VIEWER_ADDR" "127.0.0.1:5901" "ipv4-mapped-any-viewer"

assert_rejects() {
    local addr="$1"
    local label="$2"
    if (
        LISTEN_ADDR="$addr"
        resolve_listen_endpoint >/dev/null 2>&1
    ); then
        echo "FAIL ($label): expected '$addr' to be rejected" >&2
        exit 1
    fi
}

# Non-IP hostnames, empty hosts, invalid ports, and malformed IP literals must be rejected.
assert_rejects "localhost:6000" "hostname-localhost-port"
assert_rejects "LOCALHOST:6000" "hostname-upper-localhost-port"
assert_rejects "localhost" "hostname-localhost-bare"
assert_rejects "example.com:6000" "hostname-domain-port"
assert_rejects "[localhost]:6000" "bracketed-hostname"
assert_rejects "[127.0.0.1]:6000" "bracketed-ipv4-port"
assert_rejects ":6000" "empty-host-port"
assert_rejects "" "empty-addr"
assert_rejects "999.0.0.1:6000" "invalid-ipv4-octet"
assert_rejects "01.0.0.1:6000" "leading-zero-ipv4-octet"
assert_rejects "127.0.0.1:0" "port-zero"
assert_rejects "127.0.0.1:70000" "port-overflow"
assert_rejects "127.0.0.1:abc" "port-non-numeric"

echo "PASS: start-velovm.sh resolve_listen_endpoint checks passed"
