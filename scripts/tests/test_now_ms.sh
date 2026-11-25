#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$SCRIPT_DIR/../start-velovm.sh"

# Extract parse_epochrealtime_ms and now_ms from the launcher without executing it.
eval "$(sed -n '/^parse_epochrealtime_ms()/,/^}/p; /^now_ms()/,/^}/p' "$LAUNCHER")"

assert_eq() {
    local got="$1"
    local expected="$2"
    local label="$3"
    if [[ "$got" != "$expected" ]]; then
        echo "FAIL ($label): got '$got', expected '$expected'" >&2
        exit 1
    fi
}

assert_eq "$(parse_epochrealtime_ms '1732440000.081234')" "1732440000081" "dot-decimal"
assert_eq "$(parse_epochrealtime_ms '1732440000,081234')" "1732440000081" "comma-decimal-08"
assert_eq "$(parse_epochrealtime_ms '1732440000,095000')" "1732440000095" "comma-decimal-09"
assert_eq "$(parse_epochrealtime_ms '1732440000.5')" "1732440000500" "short-frac"
assert_eq "$(parse_epochrealtime_ms '1732440000')" "1732440000000" "no-frac"

EPOCHREALTIME_TEST_DOT="$(EPOCHREALTIME='1732440000.081234' now_ms)"
assert_eq "$EPOCHREALTIME_TEST_DOT" "1732440000081" "now_ms-dot"

# Verify live now_ms succeeds under comma-decimal LC_NUMERIC if available.
for loc in C de_DE.UTF-8 fr_FR.UTF-8; do
    val="$(LC_NUMERIC="$loc" now_ms 2>/dev/null)"
    if [[ ! "$val" =~ ^[0-9]+$ ]]; then
        echo "FAIL (locale $loc): non-numeric output '$val'" >&2
        exit 1
    fi
done

echo "PASS: start-velovm.sh now_ms() locale checks passed"
