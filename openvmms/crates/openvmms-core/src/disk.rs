use crate::models::CreateDiskPayload;
use crate::models::DiskFormat;
use crate::models::DiskInfo;
use anyhow::Context;
use anyhow::bail;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Adjusts mismatched disk extensions so OpenVMM probes match the requested disk format.
pub fn sync_disk_path_extension(path: &mut PathBuf, format: DiskFormat) {
    if path.as_os_str().is_empty() {
        return;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match (format, ext.as_deref()) {
        (DiskFormat::Raw, Some("vhdx" | "vhd")) => {
            path.set_extension("img");
        }
        (DiskFormat::Vhdx, Some("img" | "raw")) => {
            path.set_extension("vhdx");
        }
        _ => {}
    }
}

pub fn create_disk(
    req: &CreateDiskPayload,
    openvmm_img_bin: Option<&Path>,
) -> anyhow::Result<DiskInfo> {
    if req.size_bytes == 0 {
        bail!("disk size must be greater than 0");
    }
    if req.path.exists() {
        bail!("disk file already exists: {}", req.path.display());
    }
    if let Some(parent) = req.path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating disk parent directory {}", parent.display()))?;
    }

    match req.format {
        DiskFormat::Iso => {
            bail!("cannot create an ISO file; specify Raw or Vhdx format");
        }
        DiskFormat::Raw => {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&req.path)
                .with_context(|| format!("creating raw disk {}", req.path.display()))?;
            file.set_len(req.size_bytes)
                .with_context(|| format!("sizing raw disk {}", req.path.display()))?;
            inspect_disk(&req.path, openvmm_img_bin)
        }
        DiskFormat::Vhdx => {
            let img_bin = openvmm_img_bin.context(
                "openvmm-img binary not found; configure openvmm_img_bin in host settings or create a raw disk",
            )?;
            let mut cmd = Command::new(img_bin);
            cmd.arg("create")
                .arg(&req.path)
                .arg("--format")
                .arg("vhdx")
                .arg("--size")
                .arg(req.size_bytes.to_string());
            if req.fixed {
                cmd.arg("--format-options").arg("allocation=fixed");
            }
            let output = cmd
                .output()
                .with_context(|| format!("running {}", img_bin.display()))?;
            if !output.status.success() {
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
            inspect_disk(&req.path, Some(img_bin))
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

    if ext.as_deref() == Some("vhdx")
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
        Some("iso") => ("iso".to_string(), "read-only image".to_string()),
        Some("vhdx") => ("vhdx".to_string(), "vhdx".to_string()),
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
}
