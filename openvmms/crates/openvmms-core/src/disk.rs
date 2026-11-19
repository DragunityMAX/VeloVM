use crate::models::CreateDiskPayload;
use crate::models::DiskFormat;
use crate::models::DiskInfo;
use anyhow::Context;
use anyhow::bail;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Validates that `path` has a file name and does not collide with another disk format's
/// reserved extension.
pub fn validate_disk_path_for_format(path: &Path, format: DiskFormat) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        bail!("disk path must specify a file name");
    }
    let lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match format {
        DiskFormat::Iso => bail!("cannot create an ISO file; specify Raw or Vhdx format"),
        DiskFormat::Vhdx => {
            if matches!(lower.as_deref(), Some("iso" | "vmdk" | "vmgs" | "vhd")) {
                bail!(
                    "disk path {} is incompatible with vhdx format: extension .{} is reserved for another disk format (use .vhdx)",
                    path.display(),
                    lower.as_deref().unwrap_or_default()
                );
            }
        }
        DiskFormat::Raw => {
            if matches!(
                lower.as_deref(),
                Some("vhdx" | "vhd" | "vmdk" | "vmgs" | "iso")
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
        DiskFormat::Raw if validate_disk_path_for_format(path, format).is_ok() => return,
        DiskFormat::Vhdx if raw_ext == Some("vhdx") => return,
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
    let file = match fs::OpenOptions::new()
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
            let sized = file
                .set_len(req.size_bytes)
                .with_context(|| format!("sizing raw disk {}", req.path.display()));
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
            // `--force` overwrites only the placeholder this operation reserved above.
            cmd.arg("create")
                .arg(&req.path)
                .arg("--format")
                .arg("vhdx")
                .arg("--size")
                .arg(req.size_bytes.to_string())
                .arg("--force");
            if req.fixed {
                cmd.arg("--format-options").arg("allocation=fixed");
            }
            let output = match cmd.output() {
                Ok(output) => output,
                Err(err) => {
                    remove_owned_disk(&req.path);
                    return Err(err).with_context(|| format!("running {}", img_bin.display()));
                }
            };
            if !output.status.success() {
                remove_owned_disk(&req.path);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                bail!(
                    "openvmm-img create failed ({}): {}{}",
                    output.status,
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

pub fn inspect_disk(path: &Path, openvmm_img_bin: Option<&Path>) -> anyhow::Result<DiskInfo> {
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

    let is_vhdx = ext.as_deref() == Some("vhdx") || crate::models::has_vhdx_magic(path);
    if is_vhdx
        && let Some(img_bin) = openvmm_img_bin
    {
        let output = Command::new(img_bin)
            .arg("info")
            .arg(path)
            .arg("--json")
            .output()
            .with_context(|| format!("running {}", img_bin.display()))?;
        if output.status.success() {
            let val: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("parsing openvmm-img info JSON output")?;
            let virtual_size_bytes = val["disk_size"].as_u64().unwrap_or(file_size_bytes);
            let disk_type = val["type"].as_str().unwrap_or("vhdx").to_string();
            let block_size_bytes = val["block_size"].as_u64().and_then(|v| u32::try_from(v).ok());
            let sector_size_bytes = val["logical_sector_size"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok());
            let parent_path = val["relative_path"]
                .as_str()
                .or_else(|| val["absolute_win32_path"].as_str())
                .map(ToOwned::to_owned);
            return Ok(DiskInfo {
                path: path.to_path_buf(),
                format: "vhdx".to_string(),
                disk_type,
                virtual_size_bytes,
                file_size_bytes,
                block_size_bytes,
                sector_size_bytes,
                parent_path,
                read_only,
            });
        }
    }

    let (format, disk_type) = match ext.as_deref() {
        _ if is_vhdx => ("vhdx".to_string(), "vhdx".to_string()),
        Some("iso") => ("iso".to_string(), "read-only image".to_string()),
        Some("vhd") => ("vhd".to_string(), "vhd".to_string()),
        _ => ("raw".to_string(), "sparse/flat".to_string()),
    };

    Ok(DiskInfo {
        path: path.to_path_buf(),
        format,
        disk_type,
        virtual_size_bytes: file_size_bytes,
        file_size_bytes,
        block_size_bytes: None,
        sector_size_bytes: Some(512),
        parent_path: None,
        read_only,
    })
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
            validate_disk_path_for_format(&p, format).unwrap();
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
                "#!/bin/sh\nset -eu\ncmd=\"$1\"\npath=\"$2\"\nif [ \"$cmd\" = \"create\" ]; then\n  sleep 0.05\n  printf \"VHDX_WINNER_PAYLOAD\" > \"$path\"\nelif [ \"$cmd\" = \"info\" ]; then\n  printf '{\"disk_size\":16777216,\"type\":\"dynamic\",\"block_size\":33554432,\"logical_sector_size\":512}\\n'\nelse\n  exit 1\nfi\n",
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

            // Case-insensitive .VHDX and explicit --format vhdx on .img both succeed
            // and inspect/infer as VHDX via magic bytes.
            let magic_stub = dir.path().join("magic-stub-openvmm-img.sh");
            fs::write(
                &magic_stub,
                "#!/bin/sh\nset -eu\ncmd=\"$1\"\npath=\"$2\"\nif [ \"$cmd\" = \"create\" ]; then\n  printf \"vhdxfileDUMMY\" > \"$path\"\nelif [ \"$cmd\" = \"info\" ]; then\n  printf '{\"disk_size\":16777216,\"type\":\"dynamic\",\"block_size\":33554432,\"logical_sector_size\":512}\\n'\nelse\n  exit 1\nfi\n",
            )
            .unwrap();
            fs::set_permissions(&magic_stub, fs::Permissions::from_mode(0o755)).unwrap();

            for custom_name in ["data.img", "Disk.VHDX"] {
                let p = dir.path().join(custom_name);
                let created = create_disk(
                    &CreateDiskPayload {
                        path: p.clone(),
                        size_bytes,
                        format: DiskFormat::Vhdx,
                        fixed: false,
                    },
                    Some(&magic_stub),
                )
                .unwrap();
                assert_eq!(created.format, "vhdx", "{custom_name}");
                assert_eq!(created.virtual_size_bytes, size_bytes, "{custom_name}");
                assert_eq!(DiskFormat::from_path(&p), DiskFormat::Vhdx, "{custom_name}");
            }
        }
    }
}
