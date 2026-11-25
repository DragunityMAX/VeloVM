# VeloVM

A high-speed, low-latency virtualization manager and 2D desktop display and input pipeline built on a fork of [`microsoft/openvmm`](https://github.com/microsoft/openvmm) (upstream base commit `c4419be15ab702194f0a25c294bd2f6187804824`; carried vendored patches are tracked in [`patches/README.md`](patches/README.md)) with the FastDesk low-latency display pipeline, paired with a standalone native desktop viewer (`velovm-viewer`) and turnkey Windows/Linux launcher scripts.

## Prebuilt GitHub Actions Artifacts & Quickstart

The GitHub Actions workflow ([`.github/workflows/build.yml`](.github/workflows/build.yml)) builds, tests, smoke-benchmarks, and packages self-contained bundles on every push, pull request, manual `workflow_dispatch`, and `v*` tag (publishing a GitHub Release on tags):

- **Windows x64 bundle (`velovm-windows-x64`)**:
  - `openvmm.exe` (`x86_64-pc-windows-msvc` with WHP + FastDesk + vTPM 2.0 + RPC extensions enabled)
  - `openvmm-img.exe` (VHDX creation, inspection, and verification tool)
  - `velovm-viewer.exe` (native Win32 GDI + zero-copy shared VRAM viewer)
  - `velovmd.exe`, `velovm.exe`, `velovm-gui.exe` (VeloVM daemon, CLI, and Hyper-V-style desktop manager)
  - `firmware/MSVM.fd` (`microsoft/mu_msvm` `v26.0.44` x64 UEFI firmware)
  - `Start-VeloVM.ps1` (auto-discovers `openvmm.exe`, `velovm-viewer.exe`, and `firmware/MSVM.fd`)
  - `Setup-HyperVGpuPvAndHvsock.ps1`
- **Linux x64 bundle (`velovm-linux-x64`)**:
  - `openvmm`, `openvmm-img`, `velovm-viewer`, `velovmd`, `velovm`, `velovm-gui`, `firmware/MSVM.fd`, and `start-velovm.sh`
- **macOS Apple Silicon bundle (`velovm-macos-arm64`)**:
  - `openvmm` (`aarch64-apple-darwin` with Hypervisor.framework `virt_hvf` + FastDesk + vTPM 2.0 + RPC extensions, ad-hoc signed with `com.apple.security.hypervisor`)
  - `openvmm-img`, `velovm-viewer` (Cocoa + zero-copy shared VRAM), `velovmd`, `velovm`, `velovm-gui`, `firmware/MSVM.fd` (`microsoft/mu_msvm` `v26.0.44` ARM64 UEFI firmware), and `start-velovm.sh`

### Quickstart with VeloVM (Hyper-V-Style Manager GUI & CLI)

Launch the native desktop manager (auto-starts `velovmd` on `127.0.0.1:47620` if needed):
```powershell
.\velovm-gui.exe
```
Or manage VMs from the CLI (`velovm`):
```powershell
# Create a Windows 11 VM with a 64 GiB VHDX disk and installer ISO
.\velovm.exe vm create Win11-Dev --preset win11 --cpus 4 --memory 8G --create-disk 64G --iso C:\ISOs\Win11.iso

# Start the VM and launch VeloVM zero-copy viewer
.\velovm.exe vm start Win11-Dev
.\velovm.exe vm connect Win11-Dev

# Live management while running: ISO swap, disk hot-plug, NAT port forwards, pause/resume/reset/shutdown
.\velovm.exe vm iso eject Win11-Dev
.\velovm.exe vm port-add Win11-Dev "tcp:127.0.0.1:3389->3389"
.\velovm.exe vm shutdown Win11-Dev
```

### Quickstart on Windows 11 (After Extracting `velovm-windows-x64`)

Ensure Windows Hypervisor Platform is enabled once (elevated PowerShell):
```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Boot an existing Hyper-V Windows 11 `.vhdx`:
```powershell
.\Start-VeloVM.ps1 -DiskPath C:\VMs\win11.vhdx -Memory 8G -Processors 4
```

Or create a new raw disk and install Windows 11 from an ISO:
```powershell
.\Start-VeloVM.ps1 -DiskPath C:\VMs\win11.img -CreateDiskSize 64G -IsoPath C:\ISOs\Win11.iso -Memory 8G -Processors 4
```

### Quickstart on macOS (Apple Silicon / macOS 13+ Ventura or Newer)

- **Requirements:** Apple Silicon (`arm64` / `aarch64-apple-darwin`) running macOS 13 or newer, and **ARM64** guest images/ISOs (Windows 11 ARM64 or Linux ARM64; Intel `x86_64` macOS hosts and x86_64 guests are not supported by Hypervisor.framework).
- **Gatekeeper & Hypervisor Entitlements:** The release bundle ad-hoc signs `openvmm` with `com.apple.security.hypervisor`. If you downloaded the archive with a web browser, clear the quarantine attribute before running:
  ```bash
  xattr -dr com.apple.quarantine /path/to/velovm-macos-arm64
  ```
  If you rebuild `openvmm` from source, sign the binary with `openvmm/build_support/macos/entitlements.xml` (otherwise `hv_vm_create` fails with `HV_DENIED`):
  ```bash
  codesign --entitlements openvmm/build_support/macos/entitlements.xml -f -s - ./openvmm/target/release/openvmm
  ```
- **Launch via VeloVM or Standalone Script:**
  ```bash
  ./velovm-gui
  # Or via CLI:
  ./velovm vm create Linux-ARM64 --preset linux --cpus 4 --memory 8G --create-disk 32G --iso /path/to/ubuntu-arm64.iso
  ./velovm vm start Linux-ARM64
  ./velovm vm connect Linux-ARM64
  # Or standalone launcher (runs on stock /bin/bash):
  ./start-velovm.sh --disk /path/to/arm64.vhdx --memory 8G --cpus 4
  ```

### Triggering CI Builds & Releases
- **Any push or PR** to `main` / `master` (or **Run workflow** in the GitHub Actions tab) builds and uploads `velovm-windows-x64`, `velovm-linux-x64`, and `velovm-macos-arm64` workflow artifacts.
- **Tagging a release** automatically attaches `velovm-windows-x64.zip`, `velovm-linux-x64.tar.gz`, `velovm-macos-arm64.tar.gz`, and `SHA256SUMS.txt` to a GitHub Release:
  ```bash
  git tag v0.1.0
  git push origin v0.1.0
  ```

## Why Stock OpenVMM VNC Is Slow for 2D Desktops

Stock OpenVMM (`--gfx`) drives its VNC worker (`workers/vnc_worker`) on a fixed **30 ms polling timer (~33 FPS cap)**, hardcodes **8 MiB VRAM** (`FRAMEBUFFER_SIZE`), advertises only **`1024x768` and `1280x1024`** resolutions, ignores SynthVid hardware pointer shape/position messages (forcing guest-side software cursor compositing), and uses zlib over VNC/RFB TCP loopback.

## What FastDesk (`--fastdesk`) Changes

### 1. Upgraded OpenVMM `SynthVid` & `framebuffer` Stack
- **32 MiB VRAM & 4K Resolutions** (`openvmm/vm/devices/framebuffer`, `openvmm/vm/devices/uidevices/src/video/mod.rs`):
  - Keeps `FRAMEBUFFER_SIZE` (8 MiB) as default for `--gfx` and OpenHCL compatibility, and adds `FASTDESK_FRAMEBUFFER_SIZE` (32 MiB) plus `--vram-mb <MB>`.
  - `Video::offer()` dynamically sets `mmio_megabytes` from configured VRAM size.
  - `SendSupportedResolutions` advertises modern resolutions up to `1920x1080` (at 8 MiB) and up to `2560x1440`, `2560x1600`, and `3840x2160` (at 32 MiB), defaulting to `1920x1080`.
- **Sub-Scanline Dirty Reads** (`View::read_rect_row` in `openvmm/vm/devices/framebuffer/src/lib.rs`):
  - Reads only `[x .. x + w)` pixels per dirty row from guest VRAM instead of entire scanlines.
- **Hardware Cursor Offloading** (`openvmm/vm/devices/video_core`, `openvmm/vm/devices/uidevices/src/video/mod.rs`):
  - Emits `FeatureChangeMessage` enabling `is_dirt_needed`, `is_pointer_position_updates_needed`, and `is_pointer_shape_updates_needed`.
  - Parses `PointerPositionMessage` and multi-chunk `PointerShapeMessage` (`CursorAssembler` supporting both 32-bpp ARGB/XRGB and 2-bpp monochrome AND/XOR cursors up to 96×96) and forwards `CursorUpdate` events over a mesh channel.

### 2. Event-Driven `FastDeskWorker` & Shared Protocol
- **`openvmm/workers/fastdesk_proto`**: `#![forbid(unsafe_code)]` crate providing binary length-prefixed messages, 16×16 `DirtyTileTracker` with horizontal+vertical rectangle coalescing, and lossless LZ4 (`lz4_flex`) compression with automatic `RawBgra` fallback.
- **`openvmm/workers/fastdesk_worker_defs` & `openvmm/workers/fastdesk_worker`**:
  - Event-driven coordinator woken immediately on `SYNTHVID_DIRT` (`dirty_recv`) and `CursorUpdate` (`cursor_recv`). Once guest `SYNTHVID_DIRT` is active, the 16 ms fallback poll timer is disabled (`std::future::pending()`) so idle desktops consume zero VRAM bandwidth and active frames push with zero timer delay.
  - Supports **Zero-Copy Shared Memory (`ShmDirtyOnly`)** when `--fastdesk-shm <PATH>` is provided (0 pixel bytes sent over the socket; `velovm-viewer` maps the backing VRAM file read-only via `memmap2`) as well as **LZ4 Dirty-Tile** streaming over TCP (`FASTDESK_WORKER_TCP`) or `AF_HYPERV` / `AF_VSOCK` (`FASTDESK_WORKER_VMSOCKET`).
  - Integrated into `openvmm` via `--fastdesk`, `--fastdesk-listen <ADDR>` (default `127.0.0.1:5901`), `--fastdesk-vsock-port <PORT>`, `--fastdesk-shm <PATH>`, and `--vram-mb <MB>`.

### 3. VeloVM Management Stack (`velovm/`)
- Separate Cargo workspace (`velovm/`) providing a Hyper-V-style control plane on top of OpenVMM's `vmservice.VM` `ttrpc` server over `AF_UNIX` (Windows 10/11 & Linux):
  - **`velovm-core`**: Shared registry/VM config (`vm.toml`), cross-platform pure-Rust `ttrpc` client (`socket2`), disk orchestration, and authenticated HTTP/SSE client.
  - **`velovmd`**: Management daemon supervising per-VM `openvmm --rpc` processes, zero-copy SHM VRAM files, `WaitVM` lifecycle tracking, live hot-plug SCSI/ISO/`consomme` port forwards, virtual switch discovery, and local REST + SSE API (`127.0.0.1:47620` + `daemon.token`). Sending `listen_addr: "127.0.0.1:0"` in `PATCH /api/v1/host/config` intentionally rebinds the live HTTP listener to a fresh ephemeral port on each request while preserving `"127.0.0.1:0"` in `config.toml`; omitting `listen_addr` in `PATCH` leaves the current bound port unchanged, and `PUT /api/v1/host/config` only rebinds when `listen_addr` changes from the currently configured value.
  - **`velovm`**: Scriptable CLI with formatted tables and `--json` output (including `vm disk attach --format <vhdx|vhd|raw|iso>`).
  - **`velovm-gui`**: Native desktop GUI (`eframe`/`egui`) with a 3-pane Hyper-V Manager layout, New VM Wizard, Disk Manager (VHDX/Raw), live Storage/ISO/Networking tabs, log viewer, and one-click FastDesk launch.
  - **Security Model (Single-User Desktop Host):** VeloVM and FastDesk are designed for single-user workstation hosts rather than multi-tenant shared servers. The `velovmd` REST/SSE API binds to `127.0.0.1` only and requires the per-installation `daemon.token` (`0600` on Unix, current-user DACL on Windows), while each VM's per-instance FastDesk console listener (`fastdesk.listen_addr`) binds an unauthenticated loopback TCP address (`127.0.0.1`) or explicit `AF_HYPERV`/`AF_VSOCK` endpoint (`vsock_port`). Only trusted local OS accounts with desktop session access should share the host while VMs are running.
  - **Disk Image Locking:** Writable disk images use advisory file locks (`flock` on Unix, exclusive write sharing on Windows) to prevent two VMs from modifying the same image or active backing layer concurrently. On filesystems that report advisory locks as unsupported (`ENOLCK`, `EOPNOTSUPP`, `ENOTSUP`, `ENOSYS`, or `ErrorKind::Unsupported`), disk opens log a warning and continue unlocked; note that on Linux, `flock` on network filesystems such as NFS, CIFS, or 9p typically succeeds locally without warning and does not protect against concurrent writes across different hosts.

### 4. Standalone Native Viewer (`velovm-viewer`)
- Located in `velovm-viewer/` (separate Cargo workspace so Win32 GDI/windowing links cleanly without OpenVMM's `onecore_apiset.lib` constraint).
- Supports `tcp://host:port`, `hvsock://<vm_guid>/<port_or_guid>` (`AF_HYPERV` on Windows), and `vsock://<cid>:<port>` (`AF_VSOCK` on Linux).
- Client-side alpha-blended hardware cursor compositing (restoring only the bounded cursor rect from backing framebuffer) and raw XT Set 1 / `0xE0` keyboard scancodes + `[0..=32767]` HID absolute mouse + wheel input.
- Includes `--bench --self-test` for reproducible latency, FPS, compression ratio, and pixel-checksum verification.

## Verified Self-Test Benchmark (`1920x1080`, 300 Frames)

Measured via `./target/release/velovm-viewer --bench --self-test --frames 300 --resolution 1920x1080`:

| Mode | Throughput | Total Time (300 frames) | Viewer Apply P50 | Viewer Apply P99 | Wire Data / Raw Pixels | Verification |
|---|---|---:|---:|---:|---|---|
| **LZ4 Tile (TCP)** | **6710.9 FPS** | 44.70 ms | 87 µs | 206 µs | 2.14 MiB / 95.80 MiB (44.8×) | `CHECKSUM OK` |
| **SHM Zero-Copy** | **15645.4 FPS** | 19.18 ms | 69 µs | 126 µs | 0.00 MiB / 95.80 MiB | `CHECKSUM OK` |

## Building & Testing

### 1. OpenVMM Workspace
```bash
cd openvmm
cargo test -p video_core -p framebuffer -p uidevices -p fastdesk_worker
cd workers/fastdesk_proto && cargo test
cd ../..
cargo build -p openvmm --features tpm
./target/debug/openvmm --help | grep -E "fastdesk|vram-mb|tpm"
```

### 2. Standalone `velovm-viewer` (Linux Native + Windows `.exe`)
```bash
cd velovm-viewer
cargo test
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
```
Built binaries:
- Linux ELF: `velovm-viewer/target/release/velovm-viewer`
- Windows x86_64 PE: `velovm-viewer/target/x86_64-pc-windows-gnu/release/velovm-viewer.exe`

### 3. VeloVM Workspace (`velovmd`, `velovm`, `velovm-gui`)
```bash
cd velovm
cargo test
cargo build --release
./target/release/velovm-gui --self-test
```

## Windows Usage Scripts

- **VeloVM on Windows 11 WHP (Zero-Copy Shared VRAM)**:
  See [`scripts/Start-VeloVM.ps1`](scripts/Start-VeloVM.ps1):
  ```powershell
  .\scripts\Start-VeloVM.ps1 -DiskPath C:\VMs\win11.vhdx -Memory 8G -Processors 4
  ```
- **Standard Hyper-V Windows VM (GPU-PV + `HostDriverStore` + `AF_HYPERV` Registration)**:
  See [`scripts/Setup-HyperVGpuPvAndHvsock.ps1`](scripts/Setup-HyperVGpuPvAndHvsock.ps1):
  ```powershell
  # Elevated Administrator PowerShell:
  .\scripts\Setup-HyperVGpuPvAndHvsock.ps1 -VmName "Win11-Dev" -PartitionPercent 50
  ```
