use crate::models::CreateDiskPayload;
use crate::models::DiskFormat;
use crate::models::DiskInfo;
use anyhow::Context;
use anyhow::bail;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Returns `true` if `path` starts with the 8-byte VHDX container signature (`b"vhdxfile"`).
pub fn file_has_vhdx_signature(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).is_ok() && &magic == b"vhdxfile"
}

/// Validates that `path` has a file name and does not collide with another disk format's
/// reserved extension (used on relative inputs before client path resolution).
pub fn validate_disk_extension_for_format(path: &Path, format: DiskFormat) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        bail!("disk path must specify a file name");
    }
    let lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match format {
        DiskFormat::Iso => bail!("cannot create an ISO file; specify Raw or Vhdx format"),
        DiskFormat::Vhd => bail!("creating VHD1 disks is not supported; specify Raw or Vhdx format"),
        DiskFormat::Vhdx => {
            if lower.as_deref() != Some("vhdx") {
                bail!(
                    "disk path {} is incompatible with vhdx format: VHDX disks must use the .vhdx extension",
                    path.display(),
                );
            }
        }
        DiskFormat::Raw => {
            if matches!(
                lower.as_deref(),
                Some("vhdx" | "avhdx" | "vhd" | "avhd" | "vmdk" | "vmgs" | "iso")
            ) {
                bail!(
                    "disk path {} is incompatible with raw format: extension .{} is reserved for another disk format (use e.g. .img or .raw)",
                    path.display(),
                    lower.as_deref().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

/// Validates that `path` is an absolute path with a file name and does not collide
/// with another disk format's reserved extension.
pub fn validate_disk_path_for_format(path: &Path, format: DiskFormat) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        bail!("disk path must specify a file name");
    }
    if !path.is_absolute() {
        bail!("disk path '{}' must be an absolute path", path.display());
    }
    validate_disk_extension_for_format(path, format)
}

/// Adjusts mismatched disk extensions so GUI defaults match the requested disk format.
pub fn sync_disk_path_extension(path: &mut PathBuf, format: DiskFormat) {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let raw_ext = path.extension().and_then(|e| e.to_str());
    match format {
        DiskFormat::Raw if validate_disk_extension_for_format(path, format).is_ok() => return,
        DiskFormat::Vhdx if raw_ext == Some("vhdx") => return,
        DiskFormat::Vhd if raw_ext == Some("vhd") => return,
        _ => {}
    }
    match (format, ext.as_deref()) {
        (DiskFormat::Raw, _) => {
            path.set_extension("img");
        }
        (
            DiskFormat::Vhdx,
            None | Some("vhdx" | "vhd" | "img" | "raw" | "iso" | "vmdk" | "vmgs"),
        ) => {
            path.set_extension("vhdx");
        }
        (DiskFormat::Vhdx, Some(_)) => {
            // Preserve unrelated dotted name segments (e.g. `win.v2`) and append.
            path.as_mut_os_string().push(".vhdx");
        }
        (DiskFormat::Vhd, _) => {
            path.set_extension("vhd");
        }
        (DiskFormat::Iso, _) => {}
    }
}

/// Removes a disk file this operation created. Callers must only invoke this on
/// paths they reserved with `create_new`, so another writer's file is never removed.
fn remove_owned_disk(path: &Path) {
    let _ = fs::remove_file(path);
}

pub fn create_disk(
    req: &CreateDiskPayload,
    openvmm_img_bin: Option<&Path>,
) -> anyhow::Result<DiskInfo> {
    create_disk_cancellable(req, openvmm_img_bin, &AtomicBool::new(false))
}

pub fn create_disk_cancellable(
    req: &CreateDiskPayload,
    openvmm_img_bin: Option<&Path>,
    cancelled: &AtomicBool,
) -> anyhow::Result<DiskInfo> {
    if cancelled.load(Ordering::Acquire) {
        bail!("disk creation cancelled");
    }
    if req.size_bytes < 512 || req.size_bytes % 512 != 0 {
        bail!("disk size must be at least 512 bytes and a multiple of 512 bytes");
    }
    validate_disk_path_for_format(&req.path, req.format)?;
    let vhdx_img_bin = if req.format == DiskFormat::Vhdx {
        Some(openvmm_img_bin.context(
            "openvmm-img binary not found; configure openvmm_img_bin in host settings or create a raw disk",
        )?)
    } else {
        None
    };
    if let Some(parent) = req.path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating disk parent directory {}", parent.display()))?;
    }

    // Atomically reserve the path. Exactly one concurrent creator can succeed here, and
    // from this point on the file belongs to this operation, so failure cleanup is safe.
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&req.path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("disk file already exists: {}", req.path.display());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("creating disk {}", req.path.display()));
        }
    };

    match vhdx_img_bin {
        None => {
            let sized = if req.fixed {
                allocate_fixed_raw(&mut file, req.size_bytes, cancelled)
                    .with_context(|| format!("allocating fixed raw disk {}", req.path.display()))
            } else {
                #[cfg(windows)]
                mark_file_sparse(&file);
                file.set_len(req.size_bytes)
                    .with_context(|| format!("sizing raw disk {}", req.path.display()))
            };
            drop(file);
            let res = sized.and_then(|()| inspect_disk(&req.path, openvmm_img_bin));
            if res.is_err() {
                remove_owned_disk(&req.path);
            }
            res
        }
        Some(img_bin) => {
            drop(file);
            let mut cmd = Command::new(img_bin);
            // `--force` overwrites only the placeholder this operation reserved above;
            // `--` prevents paths starting with `-` from being parsed as CLI flags.
            cmd.arg("create")
                .arg("--format")
                .arg("vhdx")
                .arg("--size")
                .arg(req.size_bytes.to_string())
                .arg("--force");
            if req.fixed {
                cmd.arg("--format-options").arg("allocation=fixed");
            }
            cmd.arg("--")
                .arg(&req.path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(err) => {
                    remove_owned_disk(&req.path);
                    return Err(err).with_context(|| format!("running {}", img_bin.display()));
                }
            };
            let stdout_handle = child.stdout.take().map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    buf
                })
            });
            let stderr_handle = child.stderr.take().map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    buf
                })
            });

            let status = loop {
                if cancelled.load(Ordering::Acquire) {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = stdout_handle {
                        let _ = h.join();
                    }
                    if let Some(h) = stderr_handle {
                        let _ = h.join();
                    }
                    remove_owned_disk(&req.path);
                    bail!("disk creation cancelled");
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(err) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        if let Some(h) = stdout_handle {
                            let _ = h.join();
                        }
                        if let Some(h) = stderr_handle {
                            let _ = h.join();
                        }
                        remove_owned_disk(&req.path);
                        return Err(err).with_context(|| format!("waiting for {}", img_bin.display()));
                    }
                }
            };
            let stdout_bytes = stdout_handle
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            let stderr_bytes = stderr_handle
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            if !status.success() {
                remove_owned_disk(&req.path);
                let stderr = String::from_utf8_lossy(&stderr_bytes);
                let stdout = String::from_utf8_lossy(&stdout_bytes);
                bail!(
                    "openvmm-img create failed ({status}): {}{}",
                    stderr.trim(),
                    if stdout.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", stdout.trim())
                    }
                );
            }
            let res = inspect_disk(&req.path, Some(img_bin));
            if res.is_err() {
                remove_owned_disk(&req.path);
            }
            res
        }
    }
}

fn allocate_fixed_raw(
    file: &mut fs::File,
    size_bytes: u64,
    cancelled: &AtomicBool,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        const FALLOC_CHUNK: u64 = 64 * 1024 * 1024;
        if let Ok(total_i64) = libc::off_t::try_from(size_bytes) {
            let mut offset = 0u64;
            let mut fallback = false;
            while offset < size_bytes {
                if cancelled.load(Ordering::Acquire) {
                    bail!("disk creation cancelled");
                }
                let len = (size_bytes - offset).min(FALLOC_CHUNK) as libc::off_t;
                let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, offset as libc::off_t, len) };
                if rc == 0 {
                    offset += len as u64;
                    continue;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if offset == 0
                    && matches!(
                        err.raw_os_error(),
                        Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL)
                    )
                {
                    fallback = true;
                    break;
                }
                return Err(err.into());
            }
            if !fallback {
                let _ = total_i64;
                file.sync_all()?;
                return Ok(());
            }
            file.seek(SeekFrom::Start(0))?;
        }
    }
    write_fixed_raw_zeros(file, size_bytes, cancelled)
}

fn write_fixed_raw_zeros(
    file: &mut fs::File,
    size_bytes: u64,
    cancelled: &AtomicBool,
) -> anyhow::Result<()> {
    let chunk = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    while written < size_bytes {
        if cancelled.load(Ordering::Acquire) {
            bail!("disk creation cancelled");
        }
        let n = (size_bytes - written).min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..n])?;
        written += n as u64;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn mark_file_sparse(file: &fs::File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
    let mut bytes_returned: u32 = 0;
    // Best-effort: succeeds on NTFS/ReFS and leaves filesystems without sparse
    // support (e.g. FAT32/exFAT) unaffected.
    let _ = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
}

struct Vhd1Meta {
    disk_type: String,
    virtual_size_bytes: u64,
    block_size_bytes: Option<u32>,
}

fn parse_vhd1_footer_bytes(footer: &[u8; 512], file_size_bytes: u64) -> Option<(u32, u64, u64)> {
    if &footer[0..8] != b"conectix" {
        return None;
    }
    let version = u32::from_be_bytes([footer[12], footer[13], footer[14], footer[15]]);
    if version != 0x0001_0000 {
        return None;
    }
    let stored_checksum = u32::from_be_bytes([footer[64], footer[65], footer[66], footer[67]]);
    let sum: u32 = footer
        .iter()
        .enumerate()
        .filter(|(i, _)| !(64..68).contains(i))
        .map(|(_, &b)| b as u32)
        .sum();
    if !sum != stored_checksum {
        return None;
    }
    let disk_type = u32::from_be_bytes([footer[60], footer[61], footer[62], footer[63]]);
    if !matches!(disk_type, 2..=4) {
        return None;
    }
    let current_size = u64::from_be_bytes([
        footer[48], footer[49], footer[50], footer[51], footer[52], footer[53], footer[54],
        footer[55],
    ]);
    if current_size == 0 || !current_size.is_multiple_of(512) {
        return None;
    }
    let data_offset = u64::from_be_bytes([
        footer[16], footer[17], footer[18], footer[19], footer[20], footer[21], footer[22],
        footer[23],
    ]);
    if disk_type == 2 && current_size > file_size_bytes.saturating_sub(512) {
        return None;
    }
    Some((disk_type, current_size, data_offset))
}

fn parse_vhd1_metadata(path: &Path, file_size_bytes: u64) -> anyhow::Result<Option<Vhd1Meta>> {
    if file_size_bytes < 512 {
        return Ok(None);
    }
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening {} to inspect VHD1 footer", path.display()))?;

    let mut footer = [0u8; 512];
    file.seek(SeekFrom::End(-512))?;
    file.read_exact(&mut footer)?;
    let parsed = parse_vhd1_footer_bytes(&footer, file_size_bytes).or_else(|| {
        if file.seek(SeekFrom::Start(0)).is_ok() && file.read_exact(&mut footer).is_ok() {
            // Only dynamic/differencing VHDs carry a backup footer copy at offset 0.
            parse_vhd1_footer_bytes(&footer, file_size_bytes)
                .filter(|(disk_type, _, _)| matches!(disk_type, 3 | 4))
        } else {
            None
        }
    });
    let Some((disk_type_code, virtual_size_bytes, data_offset)) = parsed else {
        return Ok(None);
    };

    let disk_type = match disk_type_code {
        2 => "fixed".to_string(),
        3 => "dynamic".to_string(),
        4 => "differencing".to_string(),
        _ => unreachable!(),
    };
    let mut block_size_bytes = None;
    if matches!(disk_type_code, 3 | 4)
        && data_offset != u64::MAX
        && data_offset
            .checked_add(1024)
            .is_some_and(|end| end <= file_size_bytes)
    {
        let mut dyn_hdr = [0u8; 1024];
        if file.seek(SeekFrom::Start(data_offset)).is_ok()
            && file.read_exact(&mut dyn_hdr).is_ok()
            && &dyn_hdr[0..8] == b"cxsparse"
        {
            let hdr_version = u32::from_be_bytes([dyn_hdr[24], dyn_hdr[25], dyn_hdr[26], dyn_hdr[27]]);
            let stored_checksum =
                u32::from_be_bytes([dyn_hdr[36], dyn_hdr[37], dyn_hdr[38], dyn_hdr[39]]);
            let sum: u32 = dyn_hdr
                .iter()
                .enumerate()
                .filter(|(i, _)| !(36..40).contains(i))
                .map(|(_, &b)| b as u32)
                .sum();
            if hdr_version == 0x0001_0000 && !sum == stored_checksum {
                let block_size =
                    u32::from_be_bytes([dyn_hdr[32], dyn_hdr[33], dyn_hdr[34], dyn_hdr[35]]);
                if block_size > 0 {
                    block_size_bytes = Some(block_size);
                }
            }
        }
    }

    Ok(Some(Vhd1Meta {
        disk_type,
        virtual_size_bytes,
        block_size_bytes,
    }))
}

pub fn inspect_disk(path: &Path, openvmm_img_bin: Option<&Path>) -> anyhow::Result<DiskInfo> {
    inspect_disk_with_explicit_tool(path, None, openvmm_img_bin)
}

pub fn inspect_disk_with_explicit_tool(
    path: &Path,
    explicit_img_bin: Option<&Path>,
    resolved_img_bin: Option<&Path>,
) -> anyhow::Result<DiskInfo> {
    if !path.is_absolute() {
        bail!("disk path '{}' must be an absolute path", path.display());
    }
    let meta = fs::metadata(path)
        .with_context(|| format!("reading disk metadata for {}", path.display()))?;
    if !meta.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    let file_size_bytes = meta.len();
    let read_only = meta.permissions().readonly();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    enum DetectedFormat {
        Vhdx,
        Vhd(Option<Vhd1Meta>),
        Iso,
        Raw,
    }

    // Classify solely by extension so `inspect_disk` matches `DiskFormat::from_path`
    // and VM attach without sniffing raw disk contents (even for unknown extensions
    // like `.dsk` or `.blob`).
    let detected = match ext.as_deref() {
        Some("vhdx" | "avhdx") => DetectedFormat::Vhdx,
        Some("vhd" | "avhd") => DetectedFormat::Vhd(parse_vhd1_metadata(path, file_size_bytes)?),
        Some("iso") => DetectedFormat::Iso,
        _ => DetectedFormat::Raw,
    };

    match detected {
        DetectedFormat::Vhdx => {
            if let Some(explicit) = explicit_img_bin.filter(|p| !p.as_os_str().is_empty())
                && !explicit.is_file()
            {
                bail!(
                    "configured openvmm-img binary not found at '{}'",
                    explicit.display()
                );
            }
            let img_bin = resolved_img_bin.with_context(|| {
                format!(
                    "cannot inspect VHDX metadata for {}: openvmm-img binary not found; configure openvmm_img_bin in host settings",
                    path.display()
                )
            })?;
            let output = Command::new(img_bin)
                .arg("info")
                .arg("--json")
                .arg("--")
                .arg(path)
                .output()
                .with_context(|| format!("running {}", img_bin.display()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                bail!(
                    "openvmm-img info failed ({}): {}{}",
                    output.status,
                    stderr.trim(),
                    if stdout.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", stdout.trim())
                    }
                );
            }
            let val: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("parsing openvmm-img info JSON output")?;
            let virtual_size_bytes = val["disk_size"]
                .as_u64()
                .context("missing disk_size in openvmm-img info JSON output")?;
            let disk_type = val["type"].as_str().unwrap_or("vhdx").to_string();
            let block_size_bytes = val["block_size"].as_u64().and_then(|v| u32::try_from(v).ok());
            let sector_size_bytes = val["logical_sector_size"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok());
            let parent_path = val["relative_path"]
                .as_str()
                .or_else(|| val["absolute_win32_path"].as_str())
                .map(ToOwned::to_owned);
            Ok(DiskInfo {
                path: path.to_path_buf(),
                format: "vhdx".to_string(),
                disk_type,
                virtual_size_bytes,
                file_size_bytes,
                block_size_bytes,
                sector_size_bytes,
                parent_path,
                read_only,
                warning: None,
            })
        }
        DetectedFormat::Vhd(Some(vhd)) => {
            #[cfg(not(windows))]
            let warning = (vhd.disk_type != "fixed").then(|| {
                format!(
                    "{} VHD1 images are not supported by OpenVMM on this platform (only fixed VHD1 images can be attached; convert to VHDX or fixed VHD)",
                    vhd.disk_type
                )
            });
            #[cfg(windows)]
            let warning = None;
            Ok(DiskInfo {
                path: path.to_path_buf(),
                format: "vhd".to_string(),
                disk_type: vhd.disk_type,
                virtual_size_bytes: vhd.virtual_size_bytes,
                file_size_bytes,
                block_size_bytes: vhd.block_size_bytes,
                sector_size_bytes: Some(512),
                parent_path: None,
                read_only,
                warning,
            })
        }
        DetectedFormat::Vhd(None) => bail!(
            "invalid or corrupt VHD1 image: missing valid VHD footer in {}",
            path.display()
        ),
        DetectedFormat::Iso => Ok(DiskInfo {
            path: path.to_path_buf(),
            format: "iso".to_string(),
            disk_type: "read-only image".to_string(),
            virtual_size_bytes: file_size_bytes,
            file_size_bytes,
            block_size_bytes: None,
            sector_size_bytes: Some(512),
            parent_path: None,
            read_only,
            warning: None,
        }),
        DetectedFormat::Raw => Ok(DiskInfo {
            path: path.to_path_buf(),
            format: "raw".to_string(),
            disk_type: "sparse/flat".to_string(),
            virtual_size_bytes: file_size_bytes,
            file_size_bytes,
            block_size_bytes: None,
            sector_size_bytes: Some(512),
            parent_path: None,
            read_only,
            warning: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_inspect_raw_disk() {
        let dir = tempfile::tempdir().unwrap();
        let disk_path = dir.path().join("test.img");
        let info = create_disk(
            &CreateDiskPayload {
                path: disk_path.clone(),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Raw,
                fixed: false,
            },
            None,
        )
        .unwrap();
        assert_eq!(info.format, "raw");
        assert_eq!(info.virtual_size_bytes, 16 * 1024 * 1024);
        assert!(disk_path.exists());
    }

    #[test]
    fn create_disk_rejects_format_extension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        for (name, format) in [
            ("disk.vhdx", DiskFormat::Raw),
            ("disk.VHDX", DiskFormat::Raw),
            ("disk.vhd", DiskFormat::Raw),
            ("disk.vmdk", DiskFormat::Raw),
            ("disk.vmgs", DiskFormat::Raw),
            ("disk.iso", DiskFormat::Raw),
            ("data.img", DiskFormat::Vhdx),
            ("disk.vhd", DiskFormat::Vhdx),
            ("disk.vmdk", DiskFormat::Vhdx),
            ("disk.vmgs", DiskFormat::Vhdx),
            ("disk.iso", DiskFormat::Vhdx),
        ] {
            let path = dir.path().join(name);
            let err = create_disk(
                &CreateDiskPayload {
                    path: path.clone(),
                    size_bytes: 16 * 1024 * 1024,
                    format,
                    fixed: false,
                },
                Some(Path::new("/nonexistent/openvmm-img")),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("incompatible with"),
                "{name}: {err:#}"
            );
            assert!(!path.exists(), "{name} must not be created");
        }

        let vhd_err = create_disk(
            &CreateDiskPayload {
                path: dir.path().join("legacy.vhd"),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Vhd,
                fixed: false,
            },
            Some(Path::new("/nonexistent/openvmm-img")),
        )
        .unwrap_err();
        assert!(
            vhd_err.to_string().contains("VHD1"),
            "{vhd_err:#}"
        );

        let empty_err = create_disk(
            &CreateDiskPayload {
                path: PathBuf::new(),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Raw,
                fixed: false,
            },
            Some(Path::new("/nonexistent/openvmm-img")),
        )
        .unwrap_err();
        assert!(
            empty_err.to_string().contains("must specify a file name"),
            "{empty_err:#}"
        );
    }

    #[test]
    fn failed_create_never_removes_another_writers_file() {
        let dir = tempfile::tempdir().unwrap();
        for (name, format) in [("other.vhdx", DiskFormat::Vhdx), ("other.img", DiskFormat::Raw)] {
            let path = dir.path().join(name);
            fs::write(&path, b"owned by another writer").unwrap();
            let err = create_disk(
                &CreateDiskPayload {
                    path: path.clone(),
                    size_bytes: 16 * 1024 * 1024,
                    format,
                    fixed: false,
                },
                // A binary that always fails, so any cleanup path would be exercised.
                Some(Path::new("/nonexistent/openvmm-img")),
            )
            .unwrap_err();
            assert!(err.to_string().contains("already exists"), "{err:#}");
            assert_eq!(fs::read(&path).unwrap(), b"owned by another writer");
        }
    }

    #[test]
    fn failed_vhdx_tool_removes_only_reserved_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.vhdx");
        create_disk(
            &CreateDiskPayload {
                path: path.clone(),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Vhdx,
                fixed: false,
            },
            Some(Path::new("/nonexistent/openvmm-img")),
        )
        .unwrap_err();
        assert!(!path.exists());
    }

    #[test]
    fn sync_extension_matches_inferred_format() {
        let cases = [
            ("a.vhdx", DiskFormat::Raw, "a.img"),
            ("a.VHDX", DiskFormat::Raw, "a.img"),
            ("a.vmdk", DiskFormat::Raw, "a.img"),
            ("a.vmgs", DiskFormat::Raw, "a.img"),
            ("a.iso", DiskFormat::Raw, "a.img"),
            ("a.img", DiskFormat::Raw, "a.img"),
            ("a", DiskFormat::Raw, "a"),
            ("a.img", DiskFormat::Vhdx, "a.vhdx"),
            ("a", DiskFormat::Vhdx, "a.vhdx"),
            ("win.v2", DiskFormat::Vhdx, "win.v2.vhdx"),
            ("a.vhd", DiskFormat::Vhdx, "a.vhdx"),
            ("a.VHDX", DiskFormat::Vhdx, "a.vhdx"),
        ];
        for (input, format, expected) in cases {
            let mut p = PathBuf::from(input);
            sync_disk_path_extension(&mut p, format);
            assert_eq!(p, PathBuf::from(expected), "{input} as {format:?}");
            assert_eq!(DiskFormat::from_path(&p), format);
            validate_disk_extension_for_format(&p, format).unwrap();
            assert!(
                validate_disk_path_for_format(&p, format).is_err(),
                "relative path {p:?} must be rejected by validate_disk_path_for_format"
            );
            let abs = std::env::temp_dir().join(&p);
            validate_disk_path_for_format(&abs, format).unwrap();
        }
    }

    #[test]
    fn concurrent_create_disk_preserves_winners_file() {
        use std::sync::Arc;
        use std::sync::Barrier;

        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("race.img");
        let size_bytes = 16 * 1024 * 1024;
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = raw_path.clone();
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    create_disk(
                        &CreateDiskPayload {
                            path,
                            size_bytes,
                            format: DiskFormat::Raw,
                            fixed: false,
                        },
                        None,
                    )
                })
            })
            .collect();

        let mut ok_count = 0usize;
        let mut err_count = 0usize;
        for h in handles {
            match h.join().unwrap() {
                Ok(_) => ok_count += 1,
                Err(err) => {
                    assert!(err.to_string().contains("already exists"), "{err:#}");
                    err_count += 1;
                }
            }
        }
        assert_eq!(ok_count, 1);
        assert_eq!(err_count, 7);
        assert_eq!(fs::metadata(&raw_path).unwrap().len(), size_bytes);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let stub_bin = dir.path().join("stub-openvmm-img.sh");
            fs::write(
                &stub_bin,
                "#!/bin/sh\nset -eu\ncmd=\"$1\"\nshift\nsaw_sep=0\npath=\"\"\nfor arg in \"$@\"; do\n  if [ \"$saw_sep\" -eq 1 ]; then\n    path=\"$arg\"\n    break\n  fi\n  if [ \"$arg\" = \"--\" ]; then\n    saw_sep=1\n  fi\ndone\ntest \"$saw_sep\" -eq 1 && test -n \"$path\"\nif [ \"$cmd\" = \"create\" ]; then\n  sleep 0.05\n  printf \"VHDX_WINNER_PAYLOAD\" > \"$path\"\nelif [ \"$cmd\" = \"info\" ]; then\n  printf '{\"disk_size\":16777216,\"type\":\"dynamic\",\"block_size\":33554432,\"logical_sector_size\":512}\\n'\nelse\n  exit 1\nfi\n",
            )
            .unwrap();
            fs::set_permissions(&stub_bin, fs::Permissions::from_mode(0o755)).unwrap();

            let vhdx_path = dir.path().join("race.vhdx");
            let vhdx_barrier = Arc::new(Barrier::new(8));
            let vhdx_handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = vhdx_path.clone();
                    let bin = stub_bin.clone();
                    let b = Arc::clone(&vhdx_barrier);
                    std::thread::spawn(move || {
                        b.wait();
                        create_disk(
                            &CreateDiskPayload {
                                path,
                                size_bytes,
                                format: DiskFormat::Vhdx,
                                fixed: false,
                            },
                            Some(&bin),
                        )
                    })
                })
                .collect();

            let mut vhdx_ok = 0usize;
            let mut vhdx_err = 0usize;
            for h in vhdx_handles {
                match h.join().unwrap() {
                    Ok(_) => vhdx_ok += 1,
                    Err(err) => {
                        assert!(err.to_string().contains("already exists"), "{err:#}");
                        vhdx_err += 1;
                    }
                }
            }
            assert_eq!(vhdx_ok, 1);
            assert_eq!(vhdx_err, 7);
            assert_eq!(fs::read(&vhdx_path).unwrap(), b"VHDX_WINNER_PAYLOAD");

            // Case-insensitive .VHDX succeeds as VHDX, .vhd classifies as Vhd, and
            // inspect_disk treats `.img` and unknown extensions (e.g. `.blob`) as raw
            // without content sniffing even if they contain `vhdxfile` magic.
            let vhdx_stub = dir.path().join("vhdx-stub-openvmm-img.sh");
            fs::write(
                &vhdx_stub,
                "#!/bin/sh\nset -eu\ncmd=\"$1\"\nshift\nsaw_sep=0\npath=\"\"\nfor arg in \"$@\"; do\n  if [ \"$saw_sep\" -eq 1 ]; then\n    path=\"$arg\"\n    break\n  fi\n  if [ \"$arg\" = \"--\" ]; then\n    saw_sep=1\n  fi\ndone\ntest \"$saw_sep\" -eq 1 && test -n \"$path\"\nif [ \"$cmd\" = \"create\" ]; then\n  printf \"vhdxfileDUMMY\" > \"$path\"\nelif [ \"$cmd\" = \"info\" ]; then\n  printf '{\"disk_size\":16777216,\"type\":\"dynamic\",\"block_size\":33554432,\"logical_sector_size\":512}\\n'\nelse\n  exit 1\nfi\n",
            )
            .unwrap();
            fs::set_permissions(&vhdx_stub, fs::Permissions::from_mode(0o755)).unwrap();

            let upper_vhdx = dir.path().join("Disk.VHDX");
            let created = create_disk(
                &CreateDiskPayload {
                    path: upper_vhdx.clone(),
                    size_bytes,
                    format: DiskFormat::Vhdx,
                    fixed: false,
                },
                Some(&vhdx_stub),
            )
            .unwrap();
            assert_eq!(created.format, "vhdx");
            assert_eq!(created.virtual_size_bytes, size_bytes);
            assert_eq!(DiskFormat::from_path(&upper_vhdx), DiskFormat::Vhdx);

            // Leading-dash filenames must be passed safely after `--`.
            let dash_vhdx = dir.path().join("-leading-dash.vhdx");
            let dash_created = create_disk(
                &CreateDiskPayload {
                    path: dash_vhdx.clone(),
                    size_bytes,
                    format: DiskFormat::Vhdx,
                    fixed: false,
                },
                Some(&vhdx_stub),
            )
            .unwrap();
            assert_eq!(dash_created.format, "vhdx");

            assert_eq!(DiskFormat::from_path(Path::new("legacy.vhd")), DiskFormat::Vhd);

            let raw_with_magic = dir.path().join("raw-with-magic.img");
            fs::write(&raw_with_magic, b"vhdxfileDUMMY").unwrap();
            assert_eq!(DiskFormat::from_path(&raw_with_magic), DiskFormat::Raw);
            let inspected_raw = inspect_disk(&raw_with_magic, Some(&vhdx_stub)).unwrap();
            assert_eq!(inspected_raw.format, "raw");

            let unknown_with_magic = dir.path().join("unknown-with-magic.blob");
            fs::write(&unknown_with_magic, b"vhdxfileDUMMY").unwrap();
            let inspected = inspect_disk(&unknown_with_magic, Some(&vhdx_stub)).unwrap();
            assert_eq!(inspected.format, "raw");
        }
    }

    #[test]
    fn inspect_disk_vhdx_returns_error_when_tool_missing_or_fails() {
        let dir = tempfile::tempdir().unwrap();
        let vhdx = dir.path().join("locked.vhdx");
        fs::write(&vhdx, b"SMALL_HEADER").unwrap();

        let no_bin_err = inspect_disk(&vhdx, None).unwrap_err();
        assert!(
            no_bin_err.to_string().contains("openvmm-img binary not found"),
            "{no_bin_err:#}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let failing_stub = dir.path().join("fail-img.sh");
            fs::write(
                &failing_stub,
                "#!/bin/sh\necho 'sharing violation' >&2\nexit 1\n",
            )
            .unwrap();
            fs::set_permissions(&failing_stub, fs::Permissions::from_mode(0o755)).unwrap();

            let err = inspect_disk(&vhdx, Some(&failing_stub)).unwrap_err();
            assert!(
                err.to_string().contains("openvmm-img info failed"),
                "{err:#}"
            );
            assert!(err.to_string().contains("sharing violation"), "{err:#}");
        }
    }

    #[test]
    fn fixed_raw_disk_allocates_blocks_and_cancellation_cleans_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let fixed_path = dir.path().join("fixed.img");
        let sparse_path = dir.path().join("sparse.img");
        let size = 4 * 1024 * 1024;

        let fixed_info = create_disk(
            &CreateDiskPayload {
                path: fixed_path.clone(),
                size_bytes: size,
                format: DiskFormat::Raw,
                fixed: true,
            },
            None,
        )
        .unwrap();
        assert_eq!(fixed_info.virtual_size_bytes, size);
        assert_eq!(fs::metadata(&fixed_path).unwrap().len(), size);
        let mut head = [1u8; 4096];
        fs::File::open(&fixed_path)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        assert!(head.iter().all(|&b| b == 0), "fixed raw disk must read back as zeros");

        let _sparse_info = create_disk(
            &CreateDiskPayload {
                path: sparse_path.clone(),
                size_bytes: size,
                format: DiskFormat::Raw,
                fixed: false,
            },
            None,
        )
        .unwrap();

        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;
            let attrs = fs::metadata(&sparse_path).unwrap().file_attributes();
            assert_ne!(
                attrs & FILE_ATTRIBUTE_SPARSE_FILE,
                0,
                "expected dynamic raw disk on Windows NTFS to have FILE_ATTRIBUTE_SPARSE_FILE (attrs={attrs:#x})"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let fixed_blocks = fs::metadata(&fixed_path).unwrap().blocks();
            let sparse_blocks = fs::metadata(&sparse_path).unwrap().blocks();
            assert!(
                fixed_blocks * 512 >= size,
                "expected fixed raw disk to allocate >= {size} bytes, got {} bytes",
                fixed_blocks * 512
            );
            assert_eq!(sparse_blocks, 0, "expected sparse raw disk to have 0 allocated blocks");
        }

        // Pre-cancelled raw creation must fail and leave no file behind.
        let cancelled_raw = dir.path().join("cancelled.img");
        let flag = AtomicBool::new(true);
        let err = create_disk_cancellable(
            &CreateDiskPayload {
                path: cancelled_raw.clone(),
                size_bytes: size,
                format: DiskFormat::Raw,
                fixed: true,
            },
            None,
            &flag,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err:#}");
        assert!(!cancelled_raw.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use std::sync::Arc;
            let slow_stub = dir.path().join("slow-img.sh");
            let started_flag = dir.path().join("slow_started");
            fs::write(
                &slow_stub,
                format!(
                    "#!/bin/sh\ntouch '{}'\nsleep 5\n",
                    started_flag.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&slow_stub, fs::Permissions::from_mode(0o755)).unwrap();

            let vhdx_cancel_path = dir.path().join("cancelled.vhdx");
            let cancel_flag = Arc::new(AtomicBool::new(false));
            let cancel_flag_worker = Arc::clone(&cancel_flag);
            let p = vhdx_cancel_path.clone();
            let handle = std::thread::spawn(move || {
                create_disk_cancellable(
                    &CreateDiskPayload {
                        path: p,
                        size_bytes: size,
                        format: DiskFormat::Vhdx,
                        fixed: true,
                    },
                    Some(&slow_stub),
                    &cancel_flag_worker,
                )
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !started_flag.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            cancel_flag.store(true, Ordering::Release);
            let res = handle.join().unwrap();
            assert!(res.is_err());
            assert!(!vhdx_cancel_path.exists());
        }
    }

    #[test]
    fn create_and_inspect_real_vhdx_disk_when_binary_present() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.join("../../..");
        let img_name = if cfg!(windows) {
            "openvmm-img.exe"
        } else {
            "openvmm-img"
        };
        let img_bin = [
            repo_root.join("openvmm/target/release").join(img_name),
            repo_root.join("openvmm/target/debug").join(img_name),
        ]
        .into_iter()
        .find(|p| p.is_file());

        let Some(img_bin) = img_bin else {
            if std::env::var_os("OPENVMM_REQUIRE_REAL_BINARY").is_some() {
                panic!(
                    "OPENVMM_REQUIRE_REAL_BINARY is set, but real {img_name} binary was not found under {}",
                    repo_root.display()
                );
            }
            eprintln!("skipping create_and_inspect_real_vhdx_disk: {img_name} not built yet");
            return;
        };

        let dir = tempfile::tempdir().unwrap();
        let dyn_path = dir.path().join("dynamic.vhdx");
        let dyn_info = create_disk(
            &CreateDiskPayload {
                path: dyn_path.clone(),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Vhdx,
                fixed: false,
            },
            Some(&img_bin),
        )
        .unwrap();
        assert_eq!(dyn_info.format, "vhdx");
        assert_eq!(dyn_info.virtual_size_bytes, 16 * 1024 * 1024);
        assert!(file_has_vhdx_signature(&dyn_path));

        let fixed_path = dir.path().join("fixed.vhdx");
        let fixed_info = create_disk(
            &CreateDiskPayload {
                path: fixed_path.clone(),
                size_bytes: 8 * 1024 * 1024,
                format: DiskFormat::Vhdx,
                fixed: true,
            },
            Some(&img_bin),
        )
        .unwrap();
        assert_eq!(fixed_info.format, "vhdx");
        assert_eq!(fixed_info.virtual_size_bytes, 8 * 1024 * 1024);
        assert!(fixed_info.file_size_bytes >= 8 * 1024 * 1024);
        assert!(file_has_vhdx_signature(&fixed_path));
    }

    fn build_vhd1_footer(disk_type: u32, current_size: u64, data_offset: u64) -> [u8; 512] {
        let mut footer = [0u8; 512];
        footer[0..8].copy_from_slice(b"conectix");
        footer[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes());
        footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        footer[16..24].copy_from_slice(&data_offset.to_be_bytes());
        footer[40..48].copy_from_slice(&current_size.to_be_bytes());
        footer[48..56].copy_from_slice(&current_size.to_be_bytes());
        footer[60..64].copy_from_slice(&disk_type.to_be_bytes());
        let sum: u32 = footer.iter().map(|&b| b as u32).sum();
        footer[64..68].copy_from_slice(&(!sum).to_be_bytes());
        footer
    }

    #[test]
    fn inspect_disk_reports_vhd1_virtual_sizes_by_extension() {
        let dir = tempfile::tempdir().unwrap();

        // Relative path must be rejected by inspect_disk.
        assert!(inspect_disk(Path::new("relative.img"), None).is_err());

        // Fixed VHD1: virtual_size_bytes must exclude the 512-byte trailing footer.
        let fixed_vhd = dir.path().join("fixed.vhd");
        let fixed_virtual = 64 * 1024u64;
        let mut fixed_bytes = vec![0u8; fixed_virtual as usize];
        fixed_bytes.extend_from_slice(&build_vhd1_footer(2, fixed_virtual, u64::MAX));
        fs::write(&fixed_vhd, &fixed_bytes).unwrap();
        let fixed_info = inspect_disk(&fixed_vhd, None).unwrap();
        assert_eq!(fixed_info.format, "vhd");
        assert_eq!(fixed_info.disk_type, "fixed");
        assert_eq!(fixed_info.virtual_size_bytes, fixed_virtual);
        assert_eq!(fixed_info.file_size_bytes, fixed_virtual + 512);
        assert_eq!(fixed_info.warning, None);

        // Fixed VHD1 written with a 32-bit 0xFFFFFFFF data_offset (instead of u64::MAX) is valid.
        let fixed_32bit_offset_vhd = dir.path().join("fixed-32bit-offset.vhd");
        let mut fixed_32bit_bytes = vec![0u8; fixed_virtual as usize];
        fixed_32bit_bytes.extend_from_slice(&build_vhd1_footer(2, fixed_virtual, 0xFFFF_FFFF));
        fs::write(&fixed_32bit_offset_vhd, &fixed_32bit_bytes).unwrap();
        let fixed_32bit_info = inspect_disk(&fixed_32bit_offset_vhd, None).unwrap();
        assert_eq!(fixed_32bit_info.format, "vhd");
        assert_eq!(fixed_32bit_info.disk_type, "fixed");
        assert_eq!(fixed_32bit_info.virtual_size_bytes, fixed_virtual);
        assert_eq!(fixed_32bit_info.warning, None);

        // `.avhd` extension also inspects as VHD1.
        let avhd_path = dir.path().join("snapshot.avhd");
        fs::write(&avhd_path, &fixed_bytes).unwrap();
        let avhd_info = inspect_disk(&avhd_path, None).unwrap();
        assert_eq!(avhd_info.format, "vhd");
        assert_eq!(avhd_info.disk_type, "fixed");

        // Dynamic VHD1: virtual_size_bytes comes from footer.current_size (much larger than file size),
        // and block_size_bytes comes from the dynamic header (`cxsparse`) at bytes 32..36 (while
        // bytes 28..32 store Max Table Entries).
        let build_dyn_hdr = |version: u32, max_table_entries: u32, block_size: u32| {
            let mut dyn_hdr = [0u8; 1024];
            dyn_hdr[0..8].copy_from_slice(b"cxsparse");
            dyn_hdr[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
            dyn_hdr[16..24].copy_from_slice(&1536u64.to_be_bytes());
            dyn_hdr[24..28].copy_from_slice(&version.to_be_bytes());
            dyn_hdr[28..32].copy_from_slice(&max_table_entries.to_be_bytes());
            dyn_hdr[32..36].copy_from_slice(&block_size.to_be_bytes());
            let sum: u32 = dyn_hdr.iter().map(|&b| b as u32).sum();
            dyn_hdr[36..40].copy_from_slice(&(!sum).to_be_bytes());
            dyn_hdr
        };

        let dyn_vhd = dir.path().join("dynamic.vhd");
        let dyn_virtual = 10 * 1024 * 1024 * 1024u64;
        let footer = build_vhd1_footer(3, dyn_virtual, 512);
        let dyn_hdr = build_dyn_hdr(0x0001_0000, 5120, 2 * 1024 * 1024);
        let mut dyn_bytes = Vec::new();
        dyn_bytes.extend_from_slice(&footer);
        dyn_bytes.extend_from_slice(&dyn_hdr);
        dyn_bytes.extend_from_slice(&footer);
        fs::write(&dyn_vhd, &dyn_bytes).unwrap();
        let dyn_info = inspect_disk(&dyn_vhd, None).unwrap();
        assert_eq!(dyn_info.format, "vhd");
        assert_eq!(dyn_info.disk_type, "dynamic");
        assert_eq!(dyn_info.virtual_size_bytes, dyn_virtual);
        assert_eq!(dyn_info.file_size_bytes, dyn_bytes.len() as u64);
        assert_eq!(dyn_info.block_size_bytes, Some(2 * 1024 * 1024));
        #[cfg(not(windows))]
        assert!(
            dyn_info
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("not supported")),
            "expected platform warning for dynamic VHD on non-Windows, got {:?}",
            dyn_info.warning
        );
        #[cfg(windows)]
        assert_eq!(dyn_info.warning, None);

        // Dynamic VHD1 with corrupted trailing footer falls back to offset-0 header copy.
        let dyn_offset0_vhd = dir.path().join("dynamic-offset0.vhd");
        let mut dyn_offset0_bytes = dyn_bytes.clone();
        let tail_start = dyn_offset0_bytes.len() - 512;
        dyn_offset0_bytes[tail_start..].fill(0);
        fs::write(&dyn_offset0_vhd, &dyn_offset0_bytes).unwrap();
        let dyn_offset0_info = inspect_disk(&dyn_offset0_vhd, None).unwrap();
        assert_eq!(dyn_offset0_info.disk_type, "dynamic");
        assert_eq!(dyn_offset0_info.block_size_bytes, Some(2 * 1024 * 1024));

        // Dynamic header with bad checksum or bad version ignores block_size_bytes.
        let mut bad_csum_bytes = dyn_bytes.clone();
        bad_csum_bytes[512 + 36] ^= 0xff;
        let bad_csum_vhd = dir.path().join("dynamic-bad-csum.vhd");
        fs::write(&bad_csum_vhd, &bad_csum_bytes).unwrap();
        assert_eq!(inspect_disk(&bad_csum_vhd, None).unwrap().block_size_bytes, None);

        let mut bad_ver_bytes = Vec::new();
        bad_ver_bytes.extend_from_slice(&footer);
        bad_ver_bytes.extend_from_slice(&build_dyn_hdr(0x0002_0000, 5120, 2 * 1024 * 1024));
        bad_ver_bytes.extend_from_slice(&footer);
        let bad_ver_vhd = dir.path().join("dynamic-bad-ver.vhd");
        fs::write(&bad_ver_vhd, &bad_ver_bytes).unwrap();
        assert_eq!(inspect_disk(&bad_ver_vhd, None).unwrap().block_size_bytes, None);

        // Fixed VHD1 at offset 0 only, oversized current_size, zero size, or unaligned size is rejected.
        let fixed_offset0_vhd = dir.path().join("fixed-offset0.vhd");
        let mut fixed_offset0_bytes = build_vhd1_footer(2, 64 * 1024, u64::MAX).to_vec();
        fixed_offset0_bytes.extend_from_slice(&vec![0u8; 64 * 1024]);
        fs::write(&fixed_offset0_vhd, &fixed_offset0_bytes).unwrap();
        assert!(inspect_disk(&fixed_offset0_vhd, None).is_err());

        let fixed_too_small_vhd = dir.path().join("fixed-too-small.vhd");
        let mut fixed_too_small_bytes = vec![0u8; 32 * 1024];
        fixed_too_small_bytes.extend_from_slice(&build_vhd1_footer(2, 64 * 1024, u64::MAX));
        fs::write(&fixed_too_small_vhd, &fixed_too_small_bytes).unwrap();
        assert!(inspect_disk(&fixed_too_small_vhd, None).is_err());

        let zero_size_vhd = dir.path().join("zero-size.vhd");
        fs::write(&zero_size_vhd, build_vhd1_footer(3, 0, 512)).unwrap();
        assert!(inspect_disk(&zero_size_vhd, None).is_err());

        let unaligned_vhd = dir.path().join("unaligned.vhd");
        fs::write(&unaligned_vhd, build_vhd1_footer(3, 1000, 512)).unwrap();
        assert!(inspect_disk(&unaligned_vhd, None).is_err());

        // Raw extensions (`.img`, `.raw`, `.bin`) are never sniffed even if containing VHD/VHDX magic.
        let raw_with_vhd_magic = dir.path().join("raw-with-vhd.img");
        fs::write(&raw_with_vhd_magic, &dyn_bytes).unwrap();
        let raw_vhd_info = inspect_disk_with_explicit_tool(
            &raw_with_vhd_magic,
            Some(Path::new("/nonexistent/openvmm-img")),
            None,
        )
        .unwrap();
        assert_eq!(raw_vhd_info.format, "raw");
        assert_eq!(raw_vhd_info.virtual_size_bytes, dyn_bytes.len() as u64);

        let raw_with_vhdx_magic = dir.path().join("raw-with-vhdx.img");
        let mut vhdx_magic = b"vhdxfile".to_vec();
        vhdx_magic.resize(4096, 0);
        fs::write(&raw_with_vhdx_magic, &vhdx_magic).unwrap();
        let raw_vhdx_info = inspect_disk_with_explicit_tool(
            &raw_with_vhdx_magic,
            Some(Path::new("/nonexistent/openvmm-img")),
            None,
        )
        .unwrap();
        assert_eq!(raw_vhdx_info.format, "raw");

        // Unknown/extensionless files (e.g. `.blob`, `.dsk`) are also treated as raw without
        // content sniffing, matching VM attach behavior.
        let renamed_vhd = dir.path().join("disguised-vhd.blob");
        fs::write(&renamed_vhd, &dyn_bytes).unwrap();
        let renamed_info = inspect_disk_with_explicit_tool(
            &renamed_vhd,
            Some(Path::new("/nonexistent/openvmm-img")),
            None,
        )
        .unwrap();
        assert_eq!(renamed_info.format, "raw");
        assert_eq!(renamed_info.virtual_size_bytes, dyn_bytes.len() as u64);

        // Inspecting non-VHDX fails only when inspecting an actual `.vhdx` with missing explicit tool.
        let bad_explicit_vhdx = dir.path().join("needs-tool.vhdx");
        fs::write(&bad_explicit_vhdx, &vhdx_magic).unwrap();
        assert!(
            inspect_disk_with_explicit_tool(
                &bad_explicit_vhdx,
                Some(Path::new("/nonexistent/openvmm-img")),
                None,
            )
            .is_err()
        );

        // Corrupt `.vhd` with invalid footer fails rather than pretending to be raw.
        let bad_vhd = dir.path().join("corrupt.vhd");
        fs::write(&bad_vhd, vec![0u8; 4096]).unwrap();
        assert!(inspect_disk(&bad_vhd, None).is_err());

        // Unknown extensions with ISO9660 magic (`CD001`) are treated as raw, while `.iso`
        // files classify as `"iso"`.
        let iso_blob = dir.path().join("installer.blob");
        let mut iso_bytes = vec![0u8; 0x9000];
        iso_bytes[0x8001..0x8006].copy_from_slice(b"CD001");
        fs::write(&iso_blob, &iso_bytes).unwrap();
        let blob_info = inspect_disk(&iso_blob, None).unwrap();
        assert_eq!(blob_info.format, "raw");

        let iso_file = dir.path().join("installer.iso");
        fs::write(&iso_file, &iso_bytes).unwrap();
        let iso_info = inspect_disk(&iso_file, None).unwrap();
        assert_eq!(iso_info.format, "iso");
    }
}
