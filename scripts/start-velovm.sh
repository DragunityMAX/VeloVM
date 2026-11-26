#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

DISK_PATH=""
ISO_PATH=""
MEMORY="8G"
CPUS="4"
VRAM_MB="32"
SHM_PATH=""
SHM_CUSTOM=0
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
  --shm <PATH>        Shared-memory VRAM file path (default: unique /dev/shm or \$TMPDIR/velovm-<uid>/vram.shm.*, mode 0600)
  --listen <ADDR>     FastDesk TCP listen address (default: $LISTEN_ADDR)
  -h, --help          Show this help
EOF
}

require_arg() {
    local opt="$1"
    local val="${2:-}"
    if [[ -z "$val" ]]; then
        echo "Error: $opt requires a non-empty argument." >&2
        usage >&2
        exit 1
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --disk)
            require_arg "$1" "${2:-}"
            DISK_PATH="$2"
            shift 2
            ;;
        --iso)
            require_arg "$1" "${2:-}"
            ISO_PATH="$2"
            shift 2
            ;;
        --memory)
            require_arg "$1" "${2:-}"
            MEMORY="$2"
            shift 2
            ;;
        --cpus)
            require_arg "$1" "${2:-}"
            CPUS="$2"
            shift 2
            ;;
        --vram-mb)
            require_arg "$1" "${2:-}"
            VRAM_MB="$2"
            shift 2
            ;;
        --shm)
            require_arg "$1" "${2:-}"
            SHM_PATH="$2"
            SHM_CUSTOM=1
            shift 2
            ;;
        --listen)
            require_arg "$1" "${2:-}"
            LISTEN_ADDR="$2"
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

validate_vmm_arg_path() {
    local label="$1"
    local path="$2"
    case "$path" in
        *","*|*";"*|*"["*|*"]"*)
            echo "Error: $label path cannot contain ',', ';', '[', or ']' (unsupported by openvmm option parsing): $path" >&2
            exit 1
            ;;
    esac
}

if [[ ! -f "$DISK_PATH" ]]; then
    echo "Error: disk image not found: $DISK_PATH" >&2
    exit 1
fi
validate_vmm_arg_path "disk image" "$DISK_PATH"

if [[ -n "$ISO_PATH" ]]; then
    if [[ ! -f "$ISO_PATH" ]]; then
        echo "Error: ISO image not found: $ISO_PATH" >&2
        exit 1
    fi
    validate_vmm_arg_path "ISO image" "$ISO_PATH"
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
    "$SCRIPT_DIR/velovm-viewer" \
    "$SCRIPT_DIR/../velovm-viewer/target/release/velovm-viewer" \
    "$SCRIPT_DIR/../velovm-viewer/target/debug/velovm-viewer")" || {
    echo "Error: velovm-viewer binary not found next to script or in velovm-viewer/target." >&2
    exit 1
}

case "$(uname -m)" in
    arm64|aarch64)
        ARCH_UEFI_PKG="$SCRIPT_DIR/../openvmm/.packages/hyperv.uefi.mscoreuefi.AARCH64.RELEASE/MsvmAARCH64/RELEASE_CLANGPDB/FV/MSVM.fd"
        ;;
    *)
        ARCH_UEFI_PKG="$SCRIPT_DIR/../openvmm/.packages/hyperv.uefi.mscoreuefi.x64.RELEASE/MsvmX64/RELEASE_VS2022/FV/MSVM.fd"
        ;;
esac

UEFI_FD="$(resolve_first \
    "$SCRIPT_DIR/firmware/MSVM.fd" \
    "$ARCH_UEFI_PKG")" || {
    echo "Error: UEFI firmware MSVM.fd not found at $SCRIPT_DIR/firmware/MSVM.fd." >&2
    exit 1
}
validate_vmm_arg_path "UEFI firmware" "$UEFI_FD"

VMM_PID=""
VMM_READ_FD_OPEN=0
VMM_STDIN_FD_OPEN=0
VMM_FIFO_DIR=""
SHM_RUN_DIR=""
SHM_AUTO_CLEANUP=0
cleanup() {
    if [[ "$VMM_READ_FD_OPEN" -eq 1 ]]; then
        exec 8<&- 2>/dev/null || true
        VMM_READ_FD_OPEN=0
    fi
    if [[ "$VMM_STDIN_FD_OPEN" -eq 1 ]]; then
        exec 9>&- 2>/dev/null || true
        VMM_STDIN_FD_OPEN=0
    fi
    if [[ -n "${VMM_FIFO_DIR:-}" ]]; then
        rm -rf -- "$VMM_FIFO_DIR" 2>/dev/null || true
        VMM_FIFO_DIR=""
    fi
    if [[ -n "${VMM_PID:-}" ]] && kill -0 "$VMM_PID" 2>/dev/null; then
        kill "$VMM_PID" 2>/dev/null || true
        wait "$VMM_PID" 2>/dev/null || true
    fi
    VMM_PID=""
    if [[ "$SHM_AUTO_CLEANUP" -eq 1 && -n "${SHM_PATH:-}" ]]; then
        rm -f -- "$SHM_PATH" 2>/dev/null || true
        SHM_PATH=""
    fi
    if [[ -n "${SHM_RUN_DIR:-}" ]]; then
        rmdir -- "$SHM_RUN_DIR" 2>/dev/null || true
        SHM_RUN_DIR=""
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ "$SHM_CUSTOM" -eq 0 ]]; then
    if [[ -d /dev/shm && -w /dev/shm ]]; then
        SHM_BASE="/dev/shm"
    else
        SHM_BASE="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}"
        SHM_BASE="${SHM_BASE%/}"
        [[ -z "$SHM_BASE" ]] && SHM_BASE="/tmp"
    fi
    SHM_DIR="${SHM_BASE}/velovm-$(id -u)"
    (umask 077 && mkdir -p "$SHM_DIR")
    if [[ -L "$SHM_DIR" || ! -d "$SHM_DIR" || ! -O "$SHM_DIR" ]]; then
        echo "Error: unsafe shared-memory directory: $SHM_DIR" >&2
        exit 1
    fi
    chmod 0700 "$SHM_DIR"
    SHM_RUN_DIR="$(umask 077 && mktemp -d "${SHM_DIR}/vram.XXXXXXXXXX")"
    SHM_PATH="${SHM_RUN_DIR}/vram.shm"
    SHM_AUTO_CLEANUP=1
else
    SHM_DIR="$(dirname "$SHM_PATH")"
    if [[ ! -e "$SHM_DIR" ]]; then
        (umask 077 && mkdir -p "$SHM_DIR")
        chmod 0700 "$SHM_DIR"
    fi
    if [[ ! -d "$SHM_DIR" ]]; then
        echo "Error: shared-memory parent is not a directory: $SHM_DIR" >&2
        exit 1
    fi
    if [[ -L "$SHM_PATH" ]]; then
        echo "Error: refusing symlink shared-memory file: $SHM_PATH" >&2
        exit 1
    fi
    if [[ -e "$SHM_PATH" ]]; then
        if [[ ! -f "$SHM_PATH" || ! -O "$SHM_PATH" ]]; then
            echo "Error: unsafe existing shared-memory file: $SHM_PATH" >&2
            exit 1
        fi
        rm -f -- "$SHM_PATH"
    fi
fi

resolve_listen_endpoint() {
    LISTEN_HOST=""
    LISTEN_PORT="5901"
    VIEWER_ADDR="$LISTEN_ADDR"
    VMM_LISTEN_ADDR="$LISTEN_ADDR"
    if [[ "$LISTEN_ADDR" =~ ^\[([^\]]+)\]:([0-9]+)$ ]]; then
        LISTEN_HOST="${BASH_REMATCH[1]}"
        LISTEN_PORT="${BASH_REMATCH[2]}"
    elif [[ "$LISTEN_ADDR" =~ ^([^:]+):([0-9]+)$ ]]; then
        LISTEN_HOST="${BASH_REMATCH[1]}"
        LISTEN_PORT="${BASH_REMATCH[2]}"
    elif [[ "$LISTEN_ADDR" == *:* ]]; then
        LISTEN_HOST="${LISTEN_ADDR#[}"
        LISTEN_HOST="${LISTEN_HOST%]}"
        VIEWER_ADDR="[${LISTEN_HOST}]:${LISTEN_PORT}"
        VMM_LISTEN_ADDR="[${LISTEN_HOST}]:${LISTEN_PORT}"
    else
        LISTEN_HOST="$LISTEN_ADDR"
        VIEWER_ADDR="${LISTEN_ADDR}:${LISTEN_PORT}"
    fi

    if [[ ! "$LISTEN_PORT" =~ ^[0-9]+$ ]] || (( LISTEN_PORT < 1 || LISTEN_PORT > 65535 )); then
        echo "Error: invalid TCP listen port in --listen: $LISTEN_ADDR" >&2
        exit 1
    fi

    local listen_host_lc
    listen_host_lc="$(printf '%s' "$LISTEN_HOST" | tr '[:upper:]' '[:lower:]')"
    if [[ "$listen_host_lc" == "0.0.0.0" || "$listen_host_lc" == "::ffff:0.0.0.0" || "$listen_host_lc" == "0:0:0:0:0:ffff:0.0.0.0" ]]; then
        VIEWER_ADDR="127.0.0.1:${LISTEN_PORT}"
    elif [[ "$listen_host_lc" == *:*:* && "$listen_host_lc" =~ ^[0:]+$ ]]; then
        VIEWER_ADDR="[::1]:${LISTEN_PORT}"
    fi
}

resolve_listen_endpoint

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
    --fastdesk-listen "$VMM_LISTEN_ADDR"
)

if [[ -n "$ISO_PATH" ]]; then
    VMM_ARGS+=(--disk "file:${ISO_PATH},dvd,on=scsi0")
fi

STARTUP_TIMEOUT_MS="${FASTDESK_STARTUP_TIMEOUT_MS:-30000}"

parse_epochrealtime_ms() {
    local raw="$1"
    local sec frac
    IFS='.,' read -r sec frac <<< "$raw"
    frac="${frac}000"
    echo $(( 10#$sec * 1000 + 10#${frac:0:3} ))
}

now_ms() {
    if [[ -n "${EPOCHREALTIME:-}" ]]; then
        parse_epochrealtime_ms "$EPOCHREALTIME"
    else
        echo $(( $(date +%s) * 1000 ))
    fi
}

collect_process_tree_pids() {
    local root_pid="$1"
    local queue=("$root_pid")
    local all_pids=("$root_pid")
    local idx=0
    while [[ $idx -lt ${#queue[@]} ]]; do
        local pid="${queue[$idx]}"
        idx=$((idx + 1))
        local child
        while IFS= read -r child; do
            [[ "$child" =~ ^[0-9]+$ ]] || continue
            queue+=("$child")
            all_pids+=("$child")
        done < <(pgrep -P "$pid" 2>/dev/null || true)
    done
    local IFS=,
    echo "${all_pids[*]}"
}

is_tcp_listening_lsof() {
    if [[ -n "${VMM_PID:-}" ]]; then
        local pid_list="$VMM_PID"
        if command -v pgrep >/dev/null 2>&1; then
            pid_list="$(collect_process_tree_pids "$VMM_PID")"
        fi
        lsof -nP -a -p "$pid_list" -iTCP:"$LISTEN_PORT" -sTCP:LISTEN >/dev/null 2>&1
        return $?
    fi
    lsof -nP -iTCP:"$LISTEN_PORT" -sTCP:LISTEN >/dev/null 2>&1
    return $?
}

matching_tcp_inodes() {
    local port_hex ip4_hex=""
    port_hex="$(printf '%04X' "$LISTEN_PORT")"
    if [[ "$LISTEN_HOST" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]]; then
        ip4_hex="$(printf '%02X%02X%02X%02X' "${BASH_REMATCH[4]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[1]}")"
    fi

    local proc_files=()
    [[ -r /proc/net/tcp ]] && proc_files+=(/proc/net/tcp)
    [[ -r /proc/net/tcp6 ]] && proc_files+=(/proc/net/tcp6)
    if [[ ${#proc_files[@]} -gt 0 ]]; then
        awk -v port=":$port_hex" -v ip4="$ip4_hex" '
            FNR > 1 && $4 == "0A" {
                addr = toupper($2)
                if (length(addr) > length(port) && substr(addr, length(addr) - length(port) + 1) == port) {
                    matched = 0
                    if (ip4 == "" || ip4 == "00000000") {
                        matched = 1
                    } else {
                        host = substr(addr, 1, length(addr) - length(port))
                        if (host == ip4 || host == "00000000" || host == "00000000000000000000000000000000" || host == ("0000000000000000FFFF0000" ip4)) {
                            matched = 1
                        }
                    }
                    if (matched && $10 != "" && $10 != "0") {
                        print $10
                    }
                }
            }
        ' "${proc_files[@]}"
        return 0
    fi
    return 1
}

is_any_tcp_listening() {
    if [[ -r /proc/net/tcp || -r /proc/net/tcp6 ]]; then
        local inodes
        inodes="$(matching_tcp_inodes)"
        [[ -n "$inodes" ]]
        return $?
    elif command -v ss >/dev/null 2>&1; then
        ss -Htan "( sport = :$LISTEN_PORT )" 2>/dev/null | grep "LISTEN" >/dev/null
        return $?
    elif command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$LISTEN_PORT" -sTCP:LISTEN >/dev/null 2>&1
        return $?
    fi
    return 1
}

is_tcp_listening() {
    [[ -n "${VMM_PID:-}" ]] || return 1

    if [[ -r /proc/net/tcp || -r /proc/net/tcp6 ]]; then
        local inodes
        inodes="$(matching_tcp_inodes)"
        [[ -n "$inodes" ]] || return 1

        local -A want_inodes=()
        local inode pid_list pid fd target sock_inode
        while IFS= read -r inode; do
            [[ -n "$inode" ]] && want_inodes["$inode"]=1
        done <<< "$inodes"

        pid_list="$(collect_process_tree_pids "$VMM_PID")"
        local -a pids=()
        IFS=',' read -r -a pids <<< "$pid_list"
        for pid in "${pids[@]}"; do
            [[ -n "$pid" && -d "/proc/$pid/fd" ]] || continue
            for fd in /proc/"$pid"/fd/*; do
                [[ -e "$fd" || -L "$fd" ]] || continue
                target="$(readlink "$fd" 2>/dev/null || true)"
                if [[ "$target" =~ ^socket:\[([0-9]+)\]$ ]]; then
                    sock_inode="${BASH_REMATCH[1]}"
                    if [[ -n "${want_inodes[$sock_inode]:-}" ]]; then
                        return 0
                    fi
                fi
            done
        done
        return 1
    elif command -v ss >/dev/null 2>&1; then
        local ss_out pid_list pid
        ss_out="$(ss -Htanp "( sport = :$LISTEN_PORT )" 2>/dev/null | grep "LISTEN" || true)"
        [[ -n "$ss_out" ]] || return 1
        pid_list="$(collect_process_tree_pids "$VMM_PID")"
        local -a pids=()
        IFS=',' read -r -a pids <<< "$pid_list"
        for pid in "${pids[@]}"; do
            [[ -n "$pid" ]] || continue
            if [[ "$ss_out" == *"pid=${pid},"* || "$ss_out" == *"pid=${pid})"* ]]; then
                return 0
            fi
        done
        return 1
    elif command -v lsof >/dev/null 2>&1; then
        is_tcp_listening_lsof
        return $?
    fi
    return 1
}

if [[ "$LISTEN_PORT" -ne 0 ]] && is_any_tcp_listening; then
    echo "Error: FastDesk listen address ${LISTEN_ADDR} is already in use." >&2
    exit 1
fi

FIFO_TMP="${TMPDIR:-/tmp}"
FIFO_TMP="${FIFO_TMP%/}"
[[ -z "$FIFO_TMP" ]] && FIFO_TMP="/tmp"
VMM_FIFO_DIR="$(umask 077 && mktemp -d "${FIFO_TMP}/velovm-stdin.XXXXXXXXXX")"
VMM_FIFO_PATH="${VMM_FIFO_DIR}/stdin.fifo"
mkfifo -m 0600 "$VMM_FIFO_PATH"
exec 7<>"$VMM_FIFO_PATH"
exec 8<"$VMM_FIFO_PATH"
VMM_READ_FD_OPEN=1
exec 9>"$VMM_FIFO_PATH"
VMM_STDIN_FD_OPEN=1
exec 7>&-
rm -rf -- "$VMM_FIFO_DIR"
VMM_FIFO_DIR=""

echo "Starting VeloVM (${VRAM_MB} MiB VRAM, SHM=${SHM_PATH})..."
(
    exec <&8
    exec 8<&-
    exec 9>&-
    exec "$OPENVMM_BIN" "${VMM_ARGS[@]}"
) &
VMM_PID=$!
exec 8<&-
VMM_READ_FD_OPEN=0

echo "Waiting for VeloVM FastDesk listener on ${VMM_LISTEN_ADDR}..."
START_MS="$(now_ms)"
DEADLINE_MS=$(( START_MS + STARTUP_TIMEOUT_MS ))
while true; do
    if ! kill -0 "$VMM_PID" 2>/dev/null; then
        wait "$VMM_PID" || true
        echo "Error: openvmm exited prematurely." >&2
        exit 1
    fi
    if is_tcp_listening; then
        break
    fi
    if [[ "$(now_ms)" -ge "$DEADLINE_MS" ]]; then
        echo "Error: timed out waiting for VeloVM FastDesk to listen on ${VMM_LISTEN_ADDR}." >&2
        exit 1
    fi
    sleep 0.05
done

echo "Launching velovm-viewer -> tcp://${VIEWER_ADDR}"
while true; do
    ATTEMPT_START_MS="$(now_ms)"
    VIEWER_RC=0
    "$VIEWER_BIN" --connect "tcp://${VIEWER_ADDR}" || VIEWER_RC=$?
    if [[ "$VIEWER_RC" -eq 0 ]]; then
        if ! kill -0 "$VMM_PID" 2>/dev/null; then
            VMM_RC=0
            wait "$VMM_PID" || VMM_RC=$?
            VMM_PID=""
            if [[ "$VMM_RC" -ne 0 ]]; then
                echo "Error: openvmm exited with code ${VMM_RC}." >&2
                exit "$VMM_RC"
            fi
        fi
        break
    fi
    ATTEMPT_ELAPSED_MS=$(( $(now_ms) - ATTEMPT_START_MS ))
    if ! kill -0 "$VMM_PID" 2>/dev/null; then
        VMM_RC=0
        wait "$VMM_PID" || VMM_RC=$?
        VMM_PID=""
        echo "Error: openvmm exited prematurely (code ${VMM_RC})." >&2
        if [[ "$VMM_RC" -ne 0 ]]; then
            exit "$VMM_RC"
        fi
        exit 1
    fi
    if [[ "$ATTEMPT_ELAPSED_MS" -ge 2000 || "$(now_ms)" -ge "$DEADLINE_MS" ]]; then
        exit "$VIEWER_RC"
    fi
    sleep 0.1
done
