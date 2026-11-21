// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest disk helpers.

use anyhow::Context;
use std::path::Path;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

fn disk_open_error(path: &Path, verb: &str) -> String {
    let mut msg = format!("{verb} '{}'", path.display());

    // On windows, attempt to detect we ran under wsl by reading the WSLENV and
    // bail out with a helpful hint that it needs to be a windows path.
    if cfg!(windows) && std::env::var_os("WSLENV").is_some() {
        msg += ". Linux paths are not supported when running Windows executables \
                under WSL, make sure the path is a valid Windows path \
                (use `wslpath -w` to convert)";
    }

    msg
}

/// Options for opening a disk file.
#[derive(Clone, Copy)]
pub struct OpenDiskOptions {
    /// Open the disk as read-only.
    pub read_only: bool,
    /// Bypass the OS page cache for direct disk I/O.
    pub direct: bool,
}

#[cfg(unix)]
fn acquire_disk_lock(file: &std::fs::File, path: &Path, read_only: bool) -> anyhow::Result<()> {
    if file.metadata().is_ok_and(|m| m.is_file()) {
        let res = if read_only {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match res {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "disk image '{}' is already locked for writing or in use by another process",
                    path.display()
                );
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| disk_open_error(path, "failed to lock"));
            }
        }
    }
    Ok(())
}

fn open_disk_file_with_lock(
    path: &Path,
    read_only: bool,
    create: bool,
    direct: bool,
    verb: &str,
) -> anyhow::Result<std::fs::File> {
    let writable = !read_only || create;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(writable);
    if create {
        opts.create(true).truncate(false);
    }

    #[cfg(target_os = "linux")]
    if direct {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECT);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = direct;

    #[cfg(windows)]
    if writable && (create || path.is_file()) {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny concurrent writers (`FILE_SHARE_READ` without `FILE_SHARE_WRITE`)
        // on writable opens and disk creation.
        opts.share_mode(0x0000_0001);
    }

    let file = match opts.open(path) {
        Ok(file) => file,
        #[cfg(windows)]
        Err(err) if writable && err.raw_os_error() == Some(32) => {
            anyhow::bail!(
                "disk image '{}' is already locked for writing or in use by another process",
                path.display()
            );
        }
        Err(err) => return Err(err).with_context(|| disk_open_error(path, verb)),
    };

    #[cfg(unix)]
    acquire_disk_lock(&file, path, !writable)?;

    Ok(file)
}

/// Opens `path` as a VHDX disk regardless of its file extension.
pub async fn open_vhdx_disk(
    path: &Path,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    #[cfg(windows)]
    {
        if !options.direct
            && !path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("vhdx"))
        {
            return disklayer_vhdx::chain::open_vhdx_chain(path, options.read_only).await;
        }
        Ok(Resource::new(disk_vhdmp::OpenVhdmpDiskConfig(
            disk_vhdmp::VhdmpDisk::options()
                .read_only(options.read_only)
                .cached_io(!options.direct)
                .open(path)
                .with_context(|| disk_open_error(path, "failed to open"))?,
        )))
    }
    #[cfg(not(windows))]
    {
        if options.direct {
            anyhow::bail!("direct I/O is not supported for .vhdx files");
        }
        disklayer_vhdx::chain::open_vhdx_chain(path, options.read_only).await
    }
}

/// Opens `path` strictly as a raw file or block device without format sniffing.
pub fn open_raw_disk_file(
    path: &Path,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    open_raw_disk(path, options, None)
}

/// Opens the resources needed for using a disk from a file at `path`.
///
/// If the file ends with .vhd and is a fixed VHD1, it will be opened using
/// the user-mode VHD parser. Otherwise, if the file ends with .vhd, the
/// file will be opened using the kernel-mode VHD parser (Windows only).
///
/// If the file ends with .vhdx (case-insensitively), the VHDX backend is used
/// (kernel-mode VHD parser on Windows, or the pure-Rust VHDX parser on Linux).
///
/// If the file ends with .vmdk, the VMDK backend is used for all platforms.
pub async fn open_disk_type(
    path: &Path,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let read_only = options.read_only;
    let ensure_no_direct = |ext| {
        if options.direct {
            anyhow::bail!("direct I/O is not supported for {ext} files");
        };
        Ok(())
    };
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    Ok(match ext.as_deref() {
        Some("vmdk") => {
            ensure_no_direct(".vmdk")?;
            disk_vmdk::open_vmdk(path, read_only)
                .with_context(|| disk_open_error(path, "failed to open"))?
        }
        Some("vhd") => {
            let file = open_disk_file_with_lock(path, read_only, false, false, "failed to open")?;

            match disk_vhd1::Vhd1Disk::open_fixed(file, read_only) {
                Ok(vhd) => {
                    ensure_no_direct("fixed .vhd")?;
                    Resource::new(disk_backend_resources::FixedVhd1DiskHandle(
                        vhd.into_inner(),
                    ))
                }
                Err(disk_vhd1::OpenError::NotFixed) => {
                    #[cfg(windows)]
                    {
                        Resource::new(disk_vhdmp::OpenVhdmpDiskConfig(
                            disk_vhdmp::VhdmpDisk::options()
                                .read_only(read_only)
                                .cached_io(!options.direct)
                                .open(path)
                                .with_context(|| disk_open_error(path, "failed to open"))?,
                        ))
                    }
                    #[cfg(not(windows))]
                    anyhow::bail!("non-fixed VHD not supported on Linux");
                }
                Err(err) => return Err(err.into()),
            }
        }
        Some("vhdx") => open_vhdx_disk(path, options).await?,
        Some("iso") if !read_only => {
            anyhow::bail!("iso file cannot be opened as read/write")
        }
        Some("vmgs") => {
            ensure_no_direct(".vmgs")?;
            // VMGS files are fixed VHD1s. Don't bother to validate the footer
            // here; let the resource resolver do that later.
            let file = open_disk_file_with_lock(path, read_only, false, false, "failed to open")?;

            Resource::new(disk_backend_resources::FixedVhd1DiskHandle(file))
        }
        _ => open_raw_disk(path, options, None)?,
    })
}

/// Create and open the resources needed for using a disk from a file at `path`.
pub fn create_disk_type(
    path: &Path,
    size: u64,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    Ok(match ext.as_deref() {
        Some("vhd") | Some("vmgs") => {
            if options.direct {
                anyhow::bail!("direct I/O is not supported for VHD files");
            }
            let file = open_disk_file_with_lock(
                path,
                options.read_only,
                true,
                false,
                "failed to create",
            )?;

            file.set_len(0)?;
            file.set_len(size)?;
            disk_vhd1::Vhd1Disk::make_fixed(&file)?;
            Resource::new(disk_backend_resources::FixedVhd1DiskHandle(file))
        }
        Some("vhdx") => {
            anyhow::bail!("creating vhdx not supported")
        }
        Some("vmdk") => {
            anyhow::bail!("creating vmdk not supported")
        }
        Some("iso") => {
            anyhow::bail!("creating iso not supported")
        }
        _ => open_raw_disk(path, options, Some(size))?,
    })
}

/// Open or create a raw file or block device, returning the appropriate
/// disk resource for the current platform.
fn open_raw_disk(
    path: &Path,
    options: OpenDiskOptions,
    size: Option<u64>,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    if options.direct && !cfg!(target_os = "linux") {
        anyhow::bail!("direct I/O is only supported on Linux");
    }

    let create = size.is_some();
    let verb = if create {
        "failed to create"
    } else {
        "failed to open"
    };
    let file = open_disk_file_with_lock(path, options.read_only, create, options.direct, verb)?;

    if let Some(size) = size {
        file.set_len(0)?;
        file.set_len(size)?;
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Resource::new(
            disk_backend_resources::BlockDeviceDiskHandle { file },
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(Resource::new(disk_backend_resources::FileDiskHandle(file)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;

    // Creates a minimal flat VMDK descriptor and source file for routing tests.
    fn create_test_vmdk() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.bin"), [0; 512]).unwrap();
        let descriptor = directory.path().join("disk.vmdk");
        std::fs::write(
            &descriptor,
            "# Disk DescriptorFile\n\
             version=1\n\
             parentCID=ffffffff\n\
             createType=\"monolithicFlat\"\n\
             # Extent description\n\
             RW 1 FLAT \"source.bin\" 0\n",
        )
        .unwrap();
        (directory, descriptor)
    }

    #[async_test]
    async fn opens_flat_vmdk_read_only() {
        let (_directory, descriptor) = create_test_vmdk();
        open_disk_type(
            &descriptor,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn rejects_vmdk_write_direct_io_and_creation() {
        let (_directory, descriptor) = create_test_vmdk();
        // Writable VMDK opens are not supported.
        assert!(
            open_disk_type(
                &descriptor,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .await
            .is_err()
        );
        // VMDK sources cannot use direct I/O.
        assert!(
            open_disk_type(
                &descriptor,
                OpenDiskOptions {
                    read_only: true,
                    direct: true,
                },
            )
            .await
            .is_err()
        );
        // The generic disk creator must not treat VMDK as a raw file.
        assert!(
            create_disk_type(
                &descriptor,
                512,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .is_err()
        );
    }

    #[async_test]
    async fn rejects_concurrent_writable_raw_disk_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        std::fs::write(&path, [0u8; 4096]).unwrap();

        let first = open_disk_type(
            &path,
            OpenDiskOptions {
                read_only: false,
                direct: false,
            },
        )
        .await
        .unwrap();

        let err = open_disk_type(
            &path,
            OpenDiskOptions {
                read_only: false,
                direct: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already locked for writing"), "{err:#}");

        drop(first);
        open_disk_type(
            &path,
            OpenDiskOptions {
                read_only: false,
                direct: false,
            },
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn uppercase_vhdx_dispatches_to_vhdx_and_raw_never_sniffs_vhdx_signature() {
        let dir = tempfile::tempdir().unwrap();

        // A file ending in .VHDX must not fall back to raw disk open.
        let upper_vhdx = dir.path().join("Disk.VHDX");
        std::fs::write(&upper_vhdx, b"not-a-valid-vhdx-payload").unwrap();
        let err = open_disk_type(
            &upper_vhdx,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap_err();
        // Raw open would have succeeded on arbitrary bytes; VHDX backend fails parsing.
        assert!(
            err.chain()
                .any(|c| c.to_string().to_ascii_lowercase().contains("vhd")),
            "{err:#}"
        );

        // A .img file starting with the VHDX header signature must open as raw (never sniffed as VHDX).
        let sig_img = dir.path().join("data.img");
        let mut raw_payload = vec![0u8; 512];
        raw_payload[..8].copy_from_slice(b"vhdxfile");
        std::fs::write(&sig_img, &raw_payload).unwrap();
        let _raw_disk = open_disk_type(
            &sig_img,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap();

        // Explicit open_vhdx_disk still parses the file as VHDX and rejects the invalid payload.
        let err = open_vhdx_disk(
            &sig_img,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.chain()
                .any(|c| c.to_string().to_ascii_lowercase().contains("vhd")),
            "{err:#}"
        );
    }

    #[async_test]
    async fn read_only_open_succeeds_while_another_handle_is_open_for_writing() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp, &[0xAB; 2048]).unwrap();
        std::io::Write::flush(&mut temp).unwrap();

        let _ro_disk = open_raw_disk_file(
            temp.path(),
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let iso_path = dir.path().join("image.iso");
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&iso_path)
            .unwrap();
        std::io::Write::write_all(&mut writer, &[0xCD; 2048]).unwrap();
        std::io::Write::flush(&mut writer).unwrap();

        let _ro_iso = open_disk_type(
            &iso_path,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn creating_raw_or_vhd_or_vmgs_disk_against_locked_disk_preserves_contents() {
        let dir = tempfile::tempdir().unwrap();

        let raw_path = dir.path().join("disk.img");
        let raw_seed = vec![0x5Au8; 4096];
        std::fs::write(&raw_path, &raw_seed).unwrap();
        let raw_guard = open_disk_type(
            &raw_path,
            OpenDiskOptions {
                read_only: false,
                direct: false,
            },
        )
        .await
        .unwrap();
        let err = create_disk_type(
            &raw_path,
            8192,
            OpenDiskOptions {
                read_only: false,
                direct: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("already locked for writing"), "{err:#}");
        drop(raw_guard);
        assert_eq!(std::fs::read(&raw_path).unwrap(), raw_seed);

        for name in ["disk.vhd", "disk.vmgs"] {
            let path = dir.path().join(name);
            let created = create_disk_type(
                &path,
                64 * 1024,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .unwrap();
            drop(created);

            let mut payload = std::fs::read(&path).unwrap();
            payload[0..8].copy_from_slice(b"KEEPME!!");
            std::fs::write(&path, &payload).unwrap();

            let open_guard = open_disk_type(
                &path,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .await
            .unwrap();
            let err = create_disk_type(
                &path,
                128 * 1024,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains("already locked for writing"), "{err:#}");
            drop(open_guard);
            assert_eq!(std::fs::read(&path).unwrap(), payload);
        }
    }
}
