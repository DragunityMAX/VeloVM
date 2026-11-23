// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX chain helpers.
//!
//! Functions for opening one or more VHDX files as a
//! [`LayeredDiskHandle`] ready for
//! resource resolution.

use anyhow::Context;
use disk_backend_resources::DiskLayerDescription;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::VhdxDiskLayerHandle;
use guid::Guid;
use std::path::Path;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

#[cfg(unix)]
fn is_unsupported_lock_error(err: &std::io::Error) -> bool {
    #[cfg_attr(target_os = "linux", expect(unreachable_patterns))]
    let unsupported_errno = matches!(
        err.raw_os_error(),
        Some(libc::ENOLCK | libc::EOPNOTSUPP | libc::ENOTSUP | libc::ENOSYS)
    );
    err.kind() == std::io::ErrorKind::Unsupported || unsupported_errno
}

#[cfg(unix)]
fn handle_disk_lock_result(
    res: Result<(), std::fs::TryLockError>,
    path: &Path,
    read_only: bool,
) -> anyhow::Result<()> {
    match res {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!(
                "disk image '{}' is already locked for writing or in use by another process",
                path.display()
            );
        }
        Err(std::fs::TryLockError::Error(err)) if is_unsupported_lock_error(&err) => {
            // Match QEMU's fallback on filesystems without advisory lock support
            // (e.g., ENOLCK / EOPNOTSUPP / ENOTSUP / ENOSYS on NFS, CIFS, or 9p): warn and continue.
            tracing::warn!(
                path = %path.display(),
                read_only,
                error = %err,
                "disk locking unavailable on this filesystem; continuing without file lock"
            );
            Ok(())
        }
        Err(std::fs::TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("acquiring file lock on {}", path.display()))
        }
    }
}

/// Acquires a Unix advisory file lock on `file` when supported by the filesystem.
///
/// Logs a warning and returns `Ok(())` when the filesystem reports that locking
/// is unsupported.
#[cfg(unix)]
pub fn acquire_disk_lock(file: &std::fs::File, path: &Path, read_only: bool) -> anyhow::Result<()> {
    if file.metadata().is_ok_and(|m| m.is_file()) {
        let res = if read_only {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        handle_disk_lock_result(res, path, read_only)?;
    }
    Ok(())
}

fn open_vhdx_file_locked(
    path: &Path,
    read_only: bool,
    layer_idx: Option<usize>,
) -> anyhow::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(!read_only);

    #[cfg(windows)]
    if path.is_file() {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny concurrent writers (`FILE_SHARE_READ` without `FILE_SHARE_WRITE`)
        // for both writable leaves and read-only parent layers.
        opts.share_mode(0x0000_0001);
    }

    let file = match opts.open(path) {
        Ok(file) => file,
        #[cfg(windows)]
        Err(err) if err.raw_os_error() == Some(32) => {
            anyhow::bail!(
                "disk image '{}' is already locked for writing or in use by another process",
                path.display()
            );
        }
        Err(err) => {
            return match layer_idx {
                Some(i) => Err(err).with_context(|| {
                    format!("failed to open vhdx layer {}: {}", i, path.display())
                }),
                None => Err(err.into()),
            };
        }
    };

    #[cfg(unix)]
    acquire_disk_lock(&file, path, read_only)?;

    Ok(file)
}

/// Open a single VHDX file as a [`LayeredDiskHandle`] with one layer.
///
/// Use this for base (non-differencing) VHDX files. For differencing chains,
/// use [`open_vhdx_chain_explicit`] or [`open_vhdx_chain`].
///
/// The file is opened for read+write unless `read_only` is true.
pub fn open_vhdx_single(path: &Path, read_only: bool) -> anyhow::Result<Resource<DiskHandleKind>> {
    let file = open_vhdx_file_locked(path, read_only, None)?;

    Ok(Resource::new(LayeredDiskHandle::single_layer(
        VhdxDiskLayerHandle { file, read_only },
    )))
}

/// Open a VHDX differencing chain from an explicit list of file paths.
///
/// `paths` must be ordered from **leaf** (child, index 0) to **base**
/// (parent, last index). The leaf is opened for read+write (unless
/// `read_only` is true); all parent files are opened read-only.
///
/// Returns a [`LayeredDiskHandle`] with layers ordered top (leaf) to
/// bottom (base), matching the order expected by
/// [`LayeredDisk`](disk_layered::LayeredDisk).
///
/// # Errors
///
/// Returns an error if:
/// - `paths` is empty
/// - Any file cannot be opened
///
/// # Example
///
/// ```no_run
/// # use disklayer_vhdx::chain::open_vhdx_chain_explicit;
/// # use std::path::Path;
/// let resource = open_vhdx_chain_explicit(
///     &[Path::new("child.vhdx"), Path::new("base.vhdx")],
///     false,
/// ).unwrap();
/// ```
pub fn open_vhdx_chain_explicit(
    paths: &[&Path],
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    anyhow::ensure!(!paths.is_empty(), "vhdx chain must have at least one file");

    let layers: Vec<DiskLayerDescription> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let is_leaf = i == 0;
            let layer_read_only = !is_leaf || read_only;

            let file = open_vhdx_file_locked(path, layer_read_only, Some(i))?;

            let handle = VhdxDiskLayerHandle {
                file,
                read_only: layer_read_only,
            };

            Ok(DiskLayerDescription {
                layer: handle.into_resource(),
                read_cache: false,
                write_through: false,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(Resource::new(LayeredDiskHandle { layers }))
}

/// Open a VHDX differencing chain by auto-walking parent locators.
///
/// Starting from the file at `path`, reads each VHDX file's parent locator
/// to discover the next parent in the chain, continuing until a base
/// (non-differencing) disk is found.
///
/// The leaf file is opened for read+write (unless `read_only` is true);
/// all parent files are opened read-only.
///
/// Parent path resolution order:
/// 1. `relative_path` — resolved relative to the child's directory
/// 2. `volume_path` — volume GUID path (Windows-only)
/// 3. `absolute_win32_path` — absolute path (Windows-only)
///
/// # Errors
///
/// Returns an error if:
/// - The leaf file cannot be opened or parsed
/// - A parent locator specifies no usable path
/// - A parent file cannot be found at any of the locator paths
/// - The chain exceeds a reasonable depth limit (detect cycles)
pub async fn open_vhdx_chain(
    path: &Path,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    // Reasonable depth limit to detect cycles or absurdly long chains.
    const MAX_CHAIN_DEPTH: usize = 256;

    let mut paths: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    let mut current_path = path.to_path_buf();

    // Parent linkage GUID recorded by the child whose parent we are about
    // to open. `None` for the leaf (nothing links to it). When set, it is
    // validated against the opened parent's data write GUID to detect a
    // parent that was moved, replaced, or modified out from under the
    // chain — which would otherwise silently produce corrupt reads.
    let mut expected_linkage: Option<Guid> = None;

    loop {
        if paths.len() > MAX_CHAIN_DEPTH {
            anyhow::bail!(
                "vhdx chain exceeds maximum depth of {} — possible cycle",
                MAX_CHAIN_DEPTH
            );
        }

        // Open the current file read-only just to read metadata.
        // The actual read-write open happens later via open_vhdx_chain_explicit.
        let bf = crate::io::BlockingFile::open(&current_path, true)
            .with_context(|| format!("failed to open vhdx file: {}", current_path.display()))?;
        let vhdx = vhdx::VhdxFile::open(bf)
            .read_only()
            .await
            .with_context(|| format!("failed to parse vhdx file: {}", current_path.display()))?;

        // If this file was reached as a parent, verify that the linkage
        // GUID recorded by the child matches the parent's current data
        // write GUID. A mismatch means the parent is not the one the child
        // was created against (moved/replaced/modified), so reads through
        // the chain would be silently corrupt.
        if let Some(expected) = expected_linkage {
            let actual = vhdx.data_write_guid();
            anyhow::ensure!(
                expected == actual,
                "vhdx parent linkage mismatch for {}: child recorded parent \
                 data write GUID {expected}, but parent reports {actual} \
                 (parent was moved, replaced, or modified)",
                current_path.display(),
            );
        }

        if !vhdx.has_parent() {
            // Base disk — chain is complete.
            break;
        }

        // Read the parent locator.
        let locator = vhdx
            .parent_locator()
            .await
            .with_context(|| {
                format!(
                    "failed to read parent locator from: {}",
                    current_path.display()
                )
            })?
            .context("differencing disk has no parent locator")?;

        let parent = locator.vhdx_parent().with_context(|| {
            format!("invalid VHDX parent locator in {}", current_path.display())
        })?;
        expected_linkage = Some(parent.linkage());

        let child_dir = current_path.parent().unwrap_or_else(|| Path::new("."));

        // Try to resolve the parent path in order of preference.
        let parent_path = resolve_parent_path(child_dir, &parent).with_context(|| {
            format!(
                "could not find parent for vhdx file: {}",
                current_path.display()
            )
        })?;

        paths.push(parent_path.clone());
        current_path = parent_path;
    }

    // Convert PathBufs to Path references for open_vhdx_chain_explicit.
    let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    open_vhdx_chain_explicit(&path_refs, read_only)
}

/// Try to resolve a parent path from the locator's well-known keys.
///
/// Tries relative_path, then (on Windows) volume_path and absolute_win32_path.
/// Returns the first path that exists on disk, or an error if none work.
fn resolve_parent_path(
    child_dir: &Path,
    parent: &vhdx::VhdxParent,
) -> anyhow::Result<std::path::PathBuf> {
    let candidates: Vec<_> = parent.candidate_paths(child_dir).collect();

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("parent locator contains no usable paths on this platform");
    }

    // None of the candidates exist. Report all attempted paths.
    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    anyhow::bail!("parent not found at any locator path: {}", tried.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parent_lookup_ignores_windows_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parent.vhdx");
        std::fs::write(&path, []).unwrap();
        let parent = vhdx::VhdxParent::unlinked()
            .with_volume_path(path.to_str().unwrap())
            .unwrap()
            .with_absolute_win32_path(path.to_str().unwrap())
            .unwrap();
        assert!(resolve_parent_path(directory.path(), &parent).is_err());
        let parent = parent.with_relative_path(r".\parent.vhdx").unwrap();
        assert_eq!(
            resolve_parent_path(directory.path(), &parent).unwrap(),
            path
        );
    }

    #[pal_async::async_test]
    async fn auto_walk_relative_parent_checks_linkage() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let file = crate::io::BlockingFile::open(&parent_path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 2 * 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&file, &mut params).await.unwrap();
        drop(file);

        for (linkage, valid) in [(params.data_write_guid, true), (Guid::new_random(), false)] {
            let parent = vhdx::VhdxParent::new(linkage)
                .with_relative_path(r".\parent.vhdx")
                .unwrap();
            let file = crate::io::BlockingFile::open(&child_path, false).unwrap();
            let mut params = vhdx::CreateParams {
                disk_size: 1024 * 1024,
                disk_type: vhdx::DiskType::Differencing(parent),
                ..Default::default()
            };
            vhdx::create(&file, &mut params).await.unwrap();
            drop(file);
            let result = open_vhdx_chain(&child_path, true).await;
            if valid {
                assert!(result.is_ok(), "{result:?}");
            } else {
                let error = result.unwrap_err();
                assert!(format!("{error:#}").contains("parent linkage mismatch"));
            }
        }
    }

    #[test]
    fn open_single_creates_one_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let path2 = path.clone();
        pal_async::DefaultPool::run_with(|_driver| async move {
            let bf = crate::io::BlockingFile::open(&path2, false).unwrap();
            let mut params = vhdx::CreateParams {
                disk_size: 1024 * 1024,
                ..Default::default()
            };
            vhdx::create(&bf, &mut params).await.unwrap();
        });

        let resource = open_vhdx_single(&path, false).unwrap();
        let _ = resource;
    }

    #[test]
    fn explicit_chain_empty_errors() {
        let result = open_vhdx_chain_explicit(&[], false);
        assert!(result.is_err());
    }

    #[test]
    fn explicit_chain_single_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.vhdx");

        let path2 = path.clone();
        pal_async::DefaultPool::run_with(|_driver| async move {
            let bf = crate::io::BlockingFile::open(&path2, false).unwrap();
            let mut params = vhdx::CreateParams {
                disk_size: 1024 * 1024,
                ..Default::default()
            };
            vhdx::create(&bf, &mut params).await.unwrap();
        });

        let resource = open_vhdx_chain_explicit(&[path.as_path()], false).unwrap();
        let _ = resource;
    }

    #[test]
    fn explicit_chain_missing_file_errors() {
        let result = open_vhdx_chain_explicit(&[Path::new("nonexistent.vhdx")], false);
        assert!(result.is_err());
    }

    #[pal_async::async_test]
    async fn auto_walk_base_disk() {
        // Create a base (non-differencing) VHDX, then auto-walk it.
        // Should produce a single-layer chain.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.vhdx");

        let bf = crate::io::BlockingFile::open(&path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&bf, &mut params).await.unwrap();
        drop(bf);

        let resource = open_vhdx_chain(&path, false).await.unwrap();
        let _ = resource;
    }

    #[pal_async::async_test]
    async fn rejects_concurrent_writable_vhdx_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.vhdx");

        let bf = crate::io::BlockingFile::open(&path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&bf, &mut params).await.unwrap();
        drop(bf);

        let first = open_vhdx_chain(&path, false).await.unwrap();
        let err = open_vhdx_chain(&path, false).await.unwrap_err();
        assert!(err.to_string().contains("already locked for writing"), "{err:#}");

        drop(first);
        open_vhdx_chain(&path, false).await.unwrap();
    }

    #[pal_async::async_test]
    async fn readonly_parent_layer_blocks_concurrent_writer_and_allows_concurrent_readers() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.vhdx");
        let child_a_path = dir.path().join("child_a.vhdx");
        let child_b_path = dir.path().join("child_b.vhdx");

        let bf = crate::io::BlockingFile::open(&base_path, false).unwrap();
        let mut base_params = vhdx::CreateParams {
            disk_size: 2 * 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&bf, &mut base_params).await.unwrap();
        drop(bf);

        for child_path in [&child_a_path, &child_b_path] {
            let parent = vhdx::VhdxParent::new(base_params.data_write_guid)
                .with_relative_path(r".\base.vhdx")
                .unwrap();
            let cf = crate::io::BlockingFile::open(child_path, false).unwrap();
            let mut child_params = vhdx::CreateParams {
                disk_size: 2 * 1024 * 1024,
                disk_type: vhdx::DiskType::Differencing(parent),
                ..Default::default()
            };
            vhdx::create(&cf, &mut child_params).await.unwrap();
            drop(cf);
        }

        // Opening child A takes a shared lock on base.vhdx; child B can share it read-only.
        let child_a = open_vhdx_chain(&child_a_path, false).await.unwrap();
        let child_b = open_vhdx_chain(&child_b_path, false).await.unwrap();

        // A concurrent writable open of base.vhdx must be rejected while parents hold shared locks.
        let err = open_vhdx_chain(&base_path, false).await.unwrap_err();
        assert!(err.to_string().contains("already locked for writing"), "{err:#}");

        drop(child_a);
        drop(child_b);

        // Once both chains close, base.vhdx can be opened writable, which in turn blocks child opens.
        let base_rw = open_vhdx_chain(&base_path, false).await.unwrap();
        let child_err = open_vhdx_chain(&child_a_path, false).await.unwrap_err();
        assert!(
            child_err.to_string().contains("already locked for writing"),
            "{child_err:#}"
        );
        drop(base_rw);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_lock_error_warns_and_real_io_errors_fail() {
        let p = Path::new("/mnt/9p/base.vhdx");
        handle_disk_lock_result(
            Err(std::fs::TryLockError::Error(std::io::Error::from(
                std::io::ErrorKind::Unsupported,
            ))),
            p,
            false,
        )
        .expect("unsupported filesystem lock error should warn and continue without locking");
        handle_disk_lock_result(
            Err(std::fs::TryLockError::Error(std::io::Error::from_raw_os_error(
                libc::ENOLCK,
            ))),
            p,
            true,
        )
        .expect("ENOLCK filesystem lock error should warn and continue without locking");

        for raw_err in [libc::EBADF, libc::EIO] {
            handle_disk_lock_result(
                Err(std::fs::TryLockError::Error(std::io::Error::from_raw_os_error(
                    raw_err,
                ))),
                p,
                false,
            )
            .expect_err("real I/O or bad-fd lock errors must fail");
        }

        let err = handle_disk_lock_result(Err(std::fs::TryLockError::WouldBlock), p, false)
            .unwrap_err();
        assert!(err.to_string().contains("already locked for writing"), "{err:#}");
    }
}
