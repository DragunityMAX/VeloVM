#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

DISK_PATH=""
ISO_PATH=""
MEMORY="8G"
CPUS="4"
VRAM_MB="32"
SHM_PATH="/dev/shm/openvmm-fastdesk-vram.shm"
LISTEN_ADDR="127.0.0.1:5901"

usage() {
    cat <<EOF
Usage: $(basename "$0") --disk <PATH> [OPTIONS]

Options:
  --disk <PATH>       Path to VM disk image (VHDX/VHD/RAW) [required]
  --iso <PATH>        Optional installer ISO attached as a VMBus SCSI DVD
  --memory <SIZE>     Guest RAM size (default: $MEMORY)
  --cpus <N>          Virtual processor count (default: $CPUS)
  --vram-mb <MB>      SynthVid framebuffer VRAM in MiB (default: $VRAM_MB)
  --shm <PATH>        Shared-memory VRAM file path (default: $SHM_PATH)
  --listen <ADDR>     FastDesk TCP listen address (default: $LISTEN_ADDR)
  -h, --help          Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --disk)
            DISK_PATH="${2:-}"
            shift 2
            ;;
        --iso)
            ISO_PATH="${2:-}"
            shift 2
            ;;
        --memory)
            MEMORY="${2:-}"
            shift 2
            ;;
        --cpus)
            CPUS="${2:-}"
            shift 2
            ;;
        --vram-mb)
            VRAM_MB="${2:-}"
            shift 2
            ;;
        --shm)
            SHM_PATH="${2:-}"
            shift 2
            ;;
        --listen)
            LISTEN_ADDR="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [[ -z "$DISK_PATH" ]]; then
    echo "Error: --disk <PATH> is required." >&2
    usage >&2
    exit 1
fi

if [[ ! -f "$DISK_PATH" ]]; then
    echo "Error: disk image not found: $DISK_PATH" >&2
    exit 1
fi

if [[ -n "$ISO_PATH" && ! -f "$ISO_PATH" ]]; then
    echo "Error: ISO image not found: $ISO_PATH" >&2
    exit 1
fi

resolve_first() {
    for candidate in "$@"; do
        if [[ -e "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

OPENVMM_BIN="$(resolve_first \
    "$SCRIPT_DIR/openvmm" \
    "$SCRIPT_DIR/../openvmm/target/release/openvmm" \
    "$SCRIPT_DIR/../openvmm/target/debug/openvmm")" || {
    echo "Error: openvmm binary not found next to script or in openvmm/target." >&2
    exit 1
}

VIEWER_BIN="$(resolve_first \
    "$SCRIPT_DIR/fastdesk-viewer" \
    "$SCRIPT_DIR/../fastdesk-viewer/target/release/fastdesk-viewer" \
    "$SCRIPT_DIR/../fastdesk-viewer/target/debug/fastdesk-viewer")" || {
    echo "Error: fastdesk-viewer binary not found next to script or in fastdesk-viewer/target." >&2
    exit 1
}

UEFI_FD="$(resolve_first \
    "$SCRIPT_DIR/firmware/MSVM.fd" \
    "$SCRIPT_DIR/../openvmm/.packages/hyperv.uefi.mscoreuefi.x64.RELEASE/MsvmX64/RELEASE_VS2022/FV/MSVM.fd")" || {
    echo "Error: UEFI firmware MSVM.fd not found at $SCRIPT_DIR/firmware/MSVM.fd." >&2
    exit 1
}

mkdir -p "$(dirname "$SHM_PATH")"
truncate -s "${VRAM_MB}M" "$SHM_PATH"

VMM_ARGS=(
    --uefi "firmware=$UEFI_FD"
    -m "$MEMORY"
    -p "$CPUS"
    --vmbus-scsi id=scsi0
    --disk "file:${DISK_PATH},on=scsi0"
    --net consomme
    --com1 null
    --fastdesk
    --vram-mb "$VRAM_MB"
    --fastdesk-shm "$SHM_PATH"
    --fastdesk-listen "$LISTEN_ADDR"
)

if [[ -n "$ISO_PATH" ]]; then
    VMM_ARGS+=(--disk "file:${ISO_PATH},dvd,on=scsi0")
fi

VMM_PID=""
cleanup() {
    if [[ -n "$VMM_PID" ]] && kill -0 "$VMM_PID" 2>/dev/null; then
        kill "$VMM_PID" 2>/dev/null || true
        wait "$VMM_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

echo "Starting OpenVMM FastDesk (${VRAM_MB} MiB VRAM, SHM=${SHM_PATH})..."
"$OPENVMM_BIN" "${VMM_ARGS[@]}" &
VMM_PID=$!

sleep 0.7
if ! kill -0 "$VMM_PID" 2>/dev/null; then
    wait "$VMM_PID" || true
    echo "Error: openvmm exited prematurely." >&2
    exit 1
fi

echo "Launching fastdesk-viewer -> tcp://${LISTEN_ADDR}"
"$VIEWER_BIN" --connect "tcp://${LISTEN_ADDR}"
