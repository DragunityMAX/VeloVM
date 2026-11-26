#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$SCRIPT_DIR/../start-velovm.sh"

# Extract validate_vmm_arg_path from the launcher without executing it.
eval "$(sed -n '/^validate_vmm_arg_path()/,/^}/p' "$LAUNCHER")"

assert_rejects() {
    local label="$1"
    local path="$2"
    if (validate_vmm_arg_path "$label" "$path") >/dev/null 2>&1; then
        echo "FAIL: expected validate_vmm_arg_path to reject '$path'" >&2
        exit 1
    fi
}

assert_accepts() {
    local label="$1"
    local path="$2"
    if ! (validate_vmm_arg_path "$label" "$path") >/dev/null 2>&1; then
        echo "FAIL: expected validate_vmm_arg_path to accept '$path'" >&2
        exit 1
    fi
}

for bad in \
    "/tmp/Win11, clean.vhdx" \
    "/tmp/Win11;create=10G.vhdx" \
    "/tmp/Win11[1].vhdx" \
    "/tmp/Win11].vhdx" \
    "/tmp/Win11[.vhdx"; do
    assert_rejects "disk image" "$bad"
done

for good in \
    "/tmp/Win11 clean.vhdx" \
    "/tmp/dir-with_chars (1)/win11.vhdx"; do
    assert_accepts "disk image" "$good"
done

# End-to-end check using a temporary launcher copy with stub openvmm, viewer, and firmware.
TMP_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TMP_ROOT"' EXIT

cp "$LAUNCHER" "$TMP_ROOT/start-velovm.sh"
chmod +x "$TMP_ROOT/start-velovm.sh"
printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP_ROOT/openvmm"
printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP_ROOT/velovm-viewer"
chmod +x "$TMP_ROOT/openvmm" "$TMP_ROOT/velovm-viewer"
mkdir -p "$TMP_ROOT/firmware"
: > "$TMP_ROOT/firmware/MSVM.fd"

GOOD_DISK="$TMP_ROOT/good.vhdx"
: > "$GOOD_DISK"

BAD_DISK="$TMP_ROOT/Win11, clean.vhdx"
: > "$BAD_DISK"
if err_out="$("$TMP_ROOT/start-velovm.sh" --disk "$BAD_DISK" 2>&1)"; then
    echo "FAIL: expected start-velovm.sh to reject disk path with comma" >&2
    exit 1
fi
if [[ "$err_out" != *"disk image path cannot contain"* ]]; then
    echo "FAIL: unexpected error message for bad disk: $err_out" >&2
    exit 1
fi

BAD_ISO="$TMP_ROOT/Win11;dvd.iso"
: > "$BAD_ISO"
if err_out="$("$TMP_ROOT/start-velovm.sh" --disk "$GOOD_DISK" --iso "$BAD_ISO" 2>&1)"; then
    echo "FAIL: expected start-velovm.sh to reject ISO path with semicolon" >&2
    exit 1
fi
if [[ "$err_out" != *"ISO image path cannot contain"* ]]; then
    echo "FAIL: unexpected error message for bad ISO: $err_out" >&2
    exit 1
fi

BAD_FW_DIR="$TMP_ROOT/fw[0]"
mkdir -p "$BAD_FW_DIR/firmware"
cp "$LAUNCHER" "$BAD_FW_DIR/start-velovm.sh"
chmod +x "$BAD_FW_DIR/start-velovm.sh"
cp "$TMP_ROOT/openvmm" "$TMP_ROOT/velovm-viewer" "$BAD_FW_DIR/"
: > "$BAD_FW_DIR/firmware/MSVM.fd"
if err_out="$("$BAD_FW_DIR/start-velovm.sh" --disk "$GOOD_DISK" 2>&1)"; then
    echo "FAIL: expected start-velovm.sh to reject firmware path with brackets" >&2
    exit 1
fi
if [[ "$err_out" != *"UEFI firmware path cannot contain"* ]]; then
    echo "FAIL: unexpected error message for bad firmware: $err_out" >&2
    exit 1
fi

echo "PASS: start-velovm.sh path character validation checks passed"
