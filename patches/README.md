# Vendored OpenVMM Patches

`openvmm/` vendors a fork of [`microsoft/openvmm`](https://github.com/microsoft/openvmm) starting from upstream commit `c4419be15ab702194f0a25c294bd2f6187804824` (with FastDesk integration landed on this repository's `main` branch). Changes made inside vendored crates relative to the pre-OpenVMMS baseline commit `795181d2a88ddbd3243f494f8706eb27c3cb4610` are tracked here as named patches so they can be audited, preserved, or dropped cleanly during upstream syncs; the only change under `openvmm/` outside these five patches relative to the pre-OpenVMMS baseline commit `795181d2a88ddbd3243f494f8706eb27c3cb4610` is `openvmm/Cargo.lock`.

## Patch Inventory

### `0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch`
- **Target:** `openvmm/support/mesh/mesh_rpc/src/server.rs`
- **Summary:** Cancels the connection `CancelContext` and skips writing responses in `mesh_rpc::Server::serve_connection` when the ttrpc client socket disconnects. On Linux (`#[cfg(target_os = "linux")]`, where `epoll` distinguishes `EPOLLRDHUP` half-close from `EPOLLHUP`/`EPOLLERR` full close), after read EOF it races `wait_ready(PollEvents::HUP | PollEvents::ERR)` against `send_task` completion so half-closed `shutdown(SHUT_WR)` clients still receive in-flight replies while full disconnects cancel pending RPCs promptly. This Linux `HUP | ERR` wait assumes `AF_UNIX` stream sockets (used by OpenVMM's ttrpc transport): on TCP sockets, a peer `close()` only sends `FIN` and won't trigger `HUP`. On non-Linux targets (Windows `afunix.sys` / AFD, where both `shutdown(SD_SEND)` and graceful `closesocket()` emit `AFD_POLL_DISCONNECT` rather than `AFD_POLL_ABORT`, and untested non-Linux Unix targets), it cancels immediately on read EOF.
- **Behavioral impact:** Affects every ttrpc service hosted by `mesh_rpc::Server`—on Linux, half-closed (`shutdown(SHUT_WR)`) clients continue receiving in-flight replies while full disconnects cancel pending RPCs and drop their responses; on Windows and non-Linux targets, any client read EOF (full close or half-close) cancels pending RPCs and drops responses so disconnected waiters release immediately. In particular, if `velovmd` restarts or disconnects while a `SaveVm`, `RestoreVm`, or `ShutdownVm` RPC is running, that operation is cancelled instead of finishing.
- **Why carried:** Required for `velovmd` daemon restarts without stopping VMs (`WaitVM` holds a single per-VM waiter slot in `VmService`, so closing `velovmd`'s socket must release `WaitVM` for the next daemon to re-adopt the VM cleanly).

### `0002-disk-writable-image-locking-and-vhdx-open.patch`
- **Targets:** `openvmm/openvmm/openvmm_helpers/src/disk.rs`, `openvmm/vm/devices/storage/disklayer_vhdx/Cargo.toml`, `openvmm/vm/devices/storage/disklayer_vhdx/src/chain.rs`
- **Summary:** Adds `open_vhdx_disk` (to open VHDX files regardless of extension, routing `.vhdx` or Windows direct I/O through `disk_vhdmp` and non-direct `.avhdx` or other extensions through the pure-Rust `disklayer_vhdx` parser on Windows and all VHDX opens through `disklayer_vhdx` on Linux), `open_vhd1_disk` (to open VHD1 files regardless of extension), `open_raw_disk_file` (for raw images without format sniffing), `.avhd` VHD1 dispatch and `.avhdx` VHDX dispatch in `open_disk_type` / `create_disk_type` (routing `.avhd` through `open_vhd1_disk` and `.avhdx` through `open_vhdx_disk` instead of falling back to raw). Centralizes Unix advisory locking in `disklayer_vhdx::chain::acquire_disk_lock` and acquires exclusive writable disk locks (`flock` via `File::try_lock()` on Unix, and `FILE_SHARE_READ` without `FILE_SHARE_WRITE` via `OpenOptionsExt::share_mode(0x0000_0001)` on Windows) on writable opens and raw/`.vhd`/`.vmgs` disk creation before truncating or sizing (`truncate(false)` until locked), acquires shared locks (`File::try_lock_shared()`) on Unix read-only opens while keeping default Windows share mode on read-only non-VHDX opens so ISO/DVD attaches can share concurrent handles, and acquires shared locks (`File::try_lock_shared()` on Unix, `FILE_SHARE_READ` on Windows) on VHDX parent backing layers in `disklayer_vhdx`. Unsupported Unix filesystem lock errors (`ENOLCK`, `EOPNOTSUPP`, `ENOTSUP`, `ENOSYS`, or `ErrorKind::Unsupported`) emit a `tracing::warn!` and continue without locking, while genuine I/O or bad-descriptor errors (`EBADF`, `EIO`, etc.) still fail. Note that on Linux, `flock` on NFS/CIFS/9p typically succeeds locally without returning an unsupported-lock error and does not coordinate locks across hosts.
- **Behavioral impact:** Concurrent writable opens or `create_disk_type` calls against an already-locked disk image fail cleanly with `"already locked for writing or in use by another process"` without truncating the existing file on both Unix and Windows, opening an active VHDX parent backing layer writable is blocked while a child chain holds a shared lock, `.avhd` disks are opened as VHD1 (and rejected if non-fixed on Linux) and `.avhdx` disks are opened via the VHDX backend rather than falling back to raw, read-only DVD/ISO opens on Windows succeed even when another handle has the file open, and filesystems that do not support advisory locks (`ENOLCK`/`EOPNOTSUPP`/`ENOTSUP`/`ENOSYS`/`ErrorKind::Unsupported`) warn and continue instead of failing disk opens.
- **Why carried:** Prevents concurrent disk corruption and pre-lock truncation when attaching, creating, or starting multiple VMs against the same writable disk image or VHDX backing chain, supports `.avhd` VHD1 and `.avhdx` VHDX dispatch, extension-agnostic VHDX opens for non-`.vhdx` images, and explicit raw opens for `.img` files that happen to contain a `vhdxfile` header signature, and preserves Windows read-only/DVD media opens alongside concurrent open handles.

### `0003-vmservice-ttrpc-fastdesk-dvd-reset-tpm-extensions.patch`
- **Targets:**
  - `openvmm/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto`
  - `openvmm/openvmm/openvmm_entry/src/lib.rs`
  - `openvmm/openvmm/openvmm_entry/src/ttrpc/mod.rs`
  - `openvmm/workers/fastdesk_worker/src/lib.rs`
  - `openvmm/workers/fastdesk_worker_defs/src/lib.rs`
- **Summary:** Adds `FastDeskConfig` (enforcing loopback TCP or loopback `HV_GUID_LOOPBACK` / `VMADDR_CID_LOCAL` vsock binding in the ttrpc path, while preserving wildcard `vsock_any` for the standalone `--fastdesk-vsock-port` CLI flag), `shm_path` traversal/symlink/existing-file rejection via `create_new(true)` plus teardown/exit/error cleanup of OpenVMM-created SHM files, `PropertiesVMResponse.fastdesk_listen_addr` / `fastdesk_shm_path`, `ResetVM`, `SCSIDisk.is_dvd`, `DevicesConfig.enable_scsi_controller`, `VMConfig.tpm_enabled`, and Windows `DioBackend` auto-port creation via `new_switch_port`. `SCSI_DISK_TYPE_VHDX`, `SCSI_DISK_TYPE_VHD1`, and `SCSI_DISK_TYPE_PHYSICAL` each explicitly select their respective backend (`open_vhdx_disk`, `open_vhd1_disk`, and `open_raw_disk_file`) regardless of file extension, while unrecognized enum values fall back to `open_disk_type`.
- **Behavioral impact:** **Breaking change for `SCSIDisk.type` (`DiskType`):** Upstream OpenVMM ignored `SCSIDisk.type` and always dispatched by file extension via `open_disk_type`. With this patch, `SCSI_DISK_TYPE_VHDX`, `SCSI_DISK_TYPE_VHD1`, and `SCSI_DISK_TYPE_PHYSICAL` are all explicitly honoured regardless of extension, so ttrpc clients must pass the variant matching the image format (rather than relying on `SCSIDisk.type` being ignored in favor of filename extension).
- **Why carried:** Exposes FastDesk, DVD/ISO hot-plug, reset, vTPM 2.0, and Hyper-V switch auto-port control over `vmservice.VM` ttrpc for VeloVM (`velovmd`).

### `0004-fastdesk-client-msg-limit-and-accept-hardening.patch`
- **Targets:**
  - `openvmm/workers/fastdesk_proto/src/lib.rs`
  - `openvmm/workers/fastdesk_proto/src/wire.rs`
  - `openvmm/workers/fastdesk_worker/Cargo.toml`
- **Summary:** Adds `MAX_CLIENT_MSG_PAYLOAD_BYTES` (1 KiB) and `decode_client_frame_header`, updates `read_client_msg` to enforce the client message payload size limit, rolls back partial frame bytes in `finish_frame` on `FrameTooLarge`, and adds the Unix `libc` dependency to `fastdesk_worker/Cargo.toml` (used for accept/peer-credential hardening in `fastdesk_worker`).
- **Behavioral impact:** Prevents unbounded allocations / OOM DoS from oversized client-to-server FastDesk frames, leaves output buffers clean after oversized server frame encoding failures, and ensures `fastdesk_worker` builds on Unix targets.
- **Why carried:** Tracks FastDesk protocol and `fastdesk_worker` build/hardening changes outside the `0003` ttrpc extension patch so re-applying patches onto a clean baseline preserves both security fixes and Unix compilation.

### `0005-uidevices-flip-synthvid-cursor-bitmap-to-top-down.patch`
- **Target:** `openvmm/vm/devices/uidevices/src/video/mod.rs`
- **Summary:** Flips bottom-up Windows DIB cursor scanlines (`CursorAssembler::decode_shape`) to top-down row order for both 32bpp color ARGB and 1bpp monochrome AND/XOR cursor shapes while keeping top-left relative `hotspot_x`/`hotspot_y`.
- **Behavioral impact:** `video_core::CursorShape` delivered by SynthVid is now consistently top-down for `fastdesk_worker` and `velovm-viewer` instead of vertically inverted.
- **Why carried:** Fixes inverted hardware cursors when connecting to Windows guests whose SynthVid driver emits bottom-up DIB cursor bitmaps.

### `0006-net-l2-userspace-switch-backend.patch`
- **Targets:**
  - `openvmm/Cargo.toml`
  - `openvmm/openvmm/openvmm/Cargo.toml`
  - `openvmm/openvmm/openvmm_resources/Cargo.toml`
  - `openvmm/openvmm/openvmm_resources/src/lib.rs`
  - `openvmm/vm/devices/net/net_backend_resources/src/lib.rs`
  - `openvmm/vm/devices/net/net_l2/`
- **Summary:** Adds the cross-platform `net_l2` network backend crate (`L2SocketEndpoint`, `L2SocketQueue`, `L2SocketResolver`), `L2SocketHandle` resource definition in `net_backend_resources`, and `net_l2` feature/resolver registration in `openvmm_resources` and `openvmm` (paired with `L2SocketBackend` ttrpc support in `0003`).
- **Behavioral impact:** Enables unprivileged VM-to-VM Ethernet switching over Unix domain sockets (Linux/macOS) and named pipes (Windows) using 2-byte length-prefixed Ethernet frames.
- **Why carried:** Provides cross-platform virtual switching on macOS and unprivileged Linux/Windows hosts without `CAP_NET_ADMIN` or Hyper-V Administrator privileges.

## Verifying and Regenerating

From the repository root (against pre-OpenVMMS baseline commit `795181d2a88ddbd3243f494f8706eb27c3cb4610`):

```bash
# Confirm all patches match the current vendored tree:
git apply -R --check patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch
git apply -R --check patches/0002-disk-writable-image-locking-and-vhdx-open.patch
git apply -R --check patches/0003-vmservice-ttrpc-fastdesk-dvd-reset-tpm-extensions.patch
git apply -R --check patches/0004-fastdesk-client-msg-limit-and-accept-hardening.patch
git apply -R --check patches/0005-uidevices-flip-synthvid-cursor-bitmap-to-top-down.patch
git apply -R --check patches/0006-net-l2-userspace-switch-backend.patch

# Regenerate patches from baseline commit 795181d2a88ddbd3243f494f8706eb27c3cb4610:
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/support/mesh/mesh_rpc/src/server.rs \
  > patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/openvmm/openvmm_helpers/src/disk.rs openvmm/vm/devices/storage/disklayer_vhdx/Cargo.toml openvmm/vm/devices/storage/disklayer_vhdx/src/chain.rs \
  > patches/0002-disk-writable-image-locking-and-vhdx-open.patch
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto openvmm/openvmm/openvmm_entry/src/lib.rs openvmm/openvmm/openvmm_entry/src/ttrpc/mod.rs openvmm/workers/fastdesk_worker/src/lib.rs openvmm/workers/fastdesk_worker_defs/src/lib.rs \
  > patches/0003-vmservice-ttrpc-fastdesk-dvd-reset-tpm-extensions.patch
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/workers/fastdesk_proto/src/lib.rs openvmm/workers/fastdesk_proto/src/wire.rs openvmm/workers/fastdesk_worker/Cargo.toml \
  > patches/0004-fastdesk-client-msg-limit-and-accept-hardening.patch
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/vm/devices/uidevices/src/video/mod.rs \
  > patches/0005-uidevices-flip-synthvid-cursor-bitmap-to-top-down.patch
git diff 795181d2a88ddbd3243f494f8706eb27c3cb4610 -- openvmm/Cargo.toml openvmm/openvmm/openvmm/Cargo.toml openvmm/openvmm/openvmm_resources/Cargo.toml openvmm/openvmm/openvmm_resources/src/lib.rs openvmm/vm/devices/net/net_backend_resources/src/lib.rs openvmm/vm/devices/net/net_l2 \
  > patches/0006-net-l2-userspace-switch-backend.patch

# Apply to a clean checkout of the pre-OpenVMMS baseline (then refresh `openvmm/Cargo.lock`
# because `0002` adds `libc` and `tracing` dependencies to `disklayer_vhdx` before `cargo build --locked`):
for p in patches/*.patch; do git apply "$p"; done
cargo update --manifest-path openvmm/Cargo.toml -p disklayer_vhdx
```
