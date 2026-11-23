use crate::models::HostConfig;
use crate::models::VmConfig;
use crate::models::default_runtime_dir;
use anyhow::Context;
use anyhow::bail;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TokenSecurityError(pub String);

impl std::fmt::Display for TokenSecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TokenSecurityError {}

pub fn is_token_security_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<TokenSecurityError>().is_some())
}

#[derive(Debug, Clone)]
pub struct RegistryStore {
    data_dir: PathBuf,
}

impl RegistryStore {
    /// Returns a lightweight store handle without creating directories or altering permissions.
    pub fn open(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn new(data_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let data_dir = data_dir.into();
        let vms_dir = data_dir.join("vms");
        let disks_dir = data_dir.join("disks");
        let data_existed = data_dir.exists();
        let vms_existed = vms_dir.exists();
        let disks_existed = disks_dir.exists();

        fs::create_dir_all(&vms_dir)
            .with_context(|| format!("creating VM store dir under {}", data_dir.display()))?;
        fs::create_dir_all(&disks_dir)
            .with_context(|| format!("creating disks dir under {}", data_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !data_existed {
                let _ = fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700));
            }
            if !vms_existed {
                let _ = fs::set_permissions(&vms_dir, fs::Permissions::from_mode(0o700));
            }
            if !disks_existed {
                let _ = fs::set_permissions(&disks_dir, fs::Permissions::from_mode(0o700));
            }
        }
        #[cfg(not(unix))]
        let _ = (data_existed, vms_existed, disks_existed);
        Ok(Self { data_dir })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn host_config_path(&self) -> PathBuf {
        self.data_dir.join("config.toml")
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("daemon.token")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join("daemon.lock")
    }

    pub fn runtime_addr_path(&self) -> PathBuf {
        self.data_dir.join("daemon.addr")
    }

    pub fn write_runtime_listen_addr(&self, addr: &str) -> anyhow::Result<()> {
        atomic_write(&self.runtime_addr_path(), addr.as_bytes(), true)
    }

    pub fn read_runtime_listen_addr(&self) -> Option<String> {
        let raw = fs::read_to_string(self.runtime_addr_path()).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    pub fn clear_runtime_listen_addr(&self) {
        let _ = fs::remove_file(self.runtime_addr_path());
    }

    /// Acquires an exclusive non-blocking lock on `<data_dir>/daemon.lock` for the
    /// lifetime of the returned `File`, preventing two `openvmmsd` instances from
    /// managing the same registry simultaneously.
    pub fn try_acquire_daemon_lock(&self) -> anyhow::Result<fs::File> {
        let path = self.lock_path();
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&path)
            .with_context(|| format!("opening daemon lock at {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let fl = libc::flock {
                l_type: libc::F_WRLCK as _,
                l_whence: libc::SEEK_SET as _,
                l_start: 0,
                l_len: 0,
                l_pid: 0,
            };
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &fl) };
            if rc == 0 {
                return Ok(file);
            }
            let err = std::io::Error::last_os_error();
            if matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
                bail!(
                    "another openvmmsd instance is already running for data directory {}",
                    self.data_dir.display()
                );
            }
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err).with_context(|| format!("acquiring daemon lock at {}", path.display()));
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    #[cfg(windows)]
                    {
                        set_path_owner_only_windows(&path)?;
                    }
                    return Ok(file);
                }
                Err(fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        bail!(
                            "another openvmmsd instance is already running for data directory {}",
                            self.data_dir.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(fs::TryLockError::Error(err)) => {
                    return Err(err)
                        .with_context(|| format!("acquiring daemon lock at {}", path.display()));
                }
            }
        }
    }

    /// Returns `true` if another open file handle currently holds `<data_dir>/daemon.lock`.
    pub fn is_daemon_locked(&self) -> bool {
        let path = self.lock_path();
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let Ok(file) = fs::File::open(&path) else {
                return false;
            };
            let mut fl = libc::flock {
                l_type: libc::F_WRLCK as _,
                l_whence: libc::SEEK_SET as _,
                l_start: 0,
                l_len: 0,
                l_pid: 0,
            };
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut fl) } == 0 {
                return fl.l_type != libc::F_UNLCK as libc::c_short;
            }
        }
        let Ok(file) = fs::File::open(&path) else {
            return false;
        };
        match file.try_lock_shared() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(_)) => false,
        }
    }

    pub fn vms_dir(&self) -> PathBuf {
        self.data_dir.join("vms")
    }

    pub fn disks_dir(&self) -> PathBuf {
        self.data_dir.join("disks")
    }

    pub fn vm_dir(&self, id: Uuid) -> PathBuf {
        self.vms_dir().join(id.to_string())
    }

    pub fn vm_config_path(&self, id: Uuid) -> PathBuf {
        self.vm_dir(id).join("vm.toml")
    }

    pub fn vm_log_path(&self, id: Uuid) -> PathBuf {
        self.vm_dir(id).join("logs").join("openvmm.log")
    }

    pub fn load_host_config_or_default(&self) -> anyhow::Result<HostConfig> {
        let path = self.host_config_path();
        if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: HostConfig = toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            if cfg.data_dir.as_os_str().is_empty() {
                cfg.data_dir = self.data_dir.clone();
            }
            if cfg.runtime_dir.as_os_str().is_empty() {
                cfg.runtime_dir = default_runtime_dir(&self.data_dir);
            }
            Ok(cfg)
        } else {
            Ok(HostConfig {
                data_dir: self.data_dir.clone(),
                runtime_dir: default_runtime_dir(&self.data_dir),
                ..Default::default()
            })
        }
    }

    pub fn load_or_init_host_config(&self) -> anyhow::Result<HostConfig> {
        let path = self.host_config_path();
        if path.exists() {
            self.load_host_config_or_default()
        } else {
            let cfg = HostConfig {
                data_dir: self.data_dir.clone(),
                runtime_dir: default_runtime_dir(&self.data_dir),
                ..Default::default()
            };
            self.save_host_config(&cfg)?;
            Ok(cfg)
        }
    }

    pub fn save_host_config(&self, cfg: &HostConfig) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(cfg).context("serializing host config")?;
        atomic_write(&self.host_config_path(), text.as_bytes(), false)
    }

    pub fn load_or_create_token(&self) -> anyhow::Result<String> {
        #[cfg(unix)]
        {
            self.load_or_create_token_with(enforce_unix_0600)
        }
        #[cfg(not(unix))]
        {
            self.load_or_create_token_default()
        }
    }

    #[cfg(unix)]
    fn load_or_create_token_with<F>(&self, enforce_0600: F) -> anyhow::Result<String>
    where
        F: FnOnce(&fs::File) -> std::io::Result<Option<u32>>,
    {
        let path = self.token_path();
        if let Some(read) = open_and_read_token(&path, false)? {
            if read.has_hard_links {
                let a = Uuid::new_v4().as_u128();
                let b = Uuid::new_v4().as_u128();
                let new_token = format!("{a:032x}{b:032x}");
                tracing::warn!(
                    path = %path.display(),
                    "rotating daemon.token after detecting hard-linked token file"
                );
                atomic_write(&path, new_token.as_bytes(), true)?;
                return Ok(new_token);
            }
            if read.was_permissive {
                if !read.token.is_empty() {
                    let a = Uuid::new_v4().as_u128();
                    let b = Uuid::new_v4().as_u128();
                    let new_token = format!("{a:032x}{b:032x}");
                    if try_rotate_token_if_0600_enforced(&path, &new_token, enforce_0600)? {
                        tracing::warn!(
                            path = %path.display(),
                            "regenerated daemon.token after detecting overly permissive file permissions"
                        );
                        return Ok(new_token);
                    }
                    tracing::warn!(
                        path = %path.display(),
                        "filesystem does not enforce 0600 permissions on daemon.token; keeping existing token"
                    );
                    return Ok(read.token);
                }
                tracing::warn!(
                    path = %path.display(),
                    "regenerating daemon.token after detecting overly permissive file permissions"
                );
            } else if !read.token.is_empty() {
                return Ok(read.token);
            }
        }
        let a = Uuid::new_v4().as_u128();
        let b = Uuid::new_v4().as_u128();
        let token = format!("{a:032x}{b:032x}");
        atomic_write(&path, token.as_bytes(), true)?;
        Ok(token)
    }

    #[cfg(not(unix))]
    fn load_or_create_token_default(&self) -> anyhow::Result<String> {
        let path = self.token_path();
        if let Some(read) = open_and_read_token(&path, false)? {
            if read.was_permissive {
                #[cfg(windows)]
                if !read.token.is_empty() {
                    let a = Uuid::new_v4().as_u128();
                    let b = Uuid::new_v4().as_u128();
                    let new_token = format!("{a:032x}{b:032x}");
                    if try_rotate_token_if_owner_only_enforced_windows(&path, &new_token)? {
                        tracing::warn!(
                            path = %path.display(),
                            "regenerated daemon.token after detecting overly permissive file permissions"
                        );
                        return Ok(new_token);
                    }
                    tracing::warn!(
                        path = %path.display(),
                        "filesystem does not enforce owner-only ACLs on daemon.token; keeping existing token"
                    );
                    return Ok(read.token);
                }
                tracing::warn!(
                    path = %path.display(),
                    "regenerating daemon.token after detecting overly permissive file permissions"
                );
            } else if !read.token.is_empty() {
                return Ok(read.token);
            }
        }
        let a = Uuid::new_v4().as_u128();
        let b = Uuid::new_v4().as_u128();
        let token = format!("{a:032x}{b:032x}");
        atomic_write(&path, token.as_bytes(), true)?;
        Ok(token)
    }

    pub fn read_token(&self) -> anyhow::Result<String> {
        let path = self.token_path();
        let Some(read) = open_and_read_token(&path, true)
            .with_context(|| format!("reading daemon token at {}", path.display()))?
        else {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                .with_context(|| format!("reading daemon token at {}", path.display()));
        };
        if read.has_hard_links {
            return Err(anyhow::Error::new(TokenSecurityError(format!(
                "refusing to use hard-linked daemon token file {} (remove the extra hard link or restart openvmmsd to rotate daemon.token)",
                path.display()
            ))));
        }
        if read.token.is_empty() {
            bail!("daemon token file {} is empty", path.display());
        }
        Ok(read.token)
    }

    pub fn load_all_vms(&self) -> anyhow::Result<Vec<VmConfig>> {
        let vms_dir = self.vms_dir();
        if !vms_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&vms_dir)
            .with_context(|| format!("reading {}", vms_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let cfg_path = entry.path().join("vm.toml");
            if !cfg_path.is_file() {
                continue;
            }
            let text = match fs::read_to_string(&cfg_path) {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(path = %cfg_path.display(), error = %err, "skipping unreadable vm.toml");
                    continue;
                }
            };
            let mut vm: VmConfig = match toml::from_str(&text) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(path = %cfg_path.display(), error = %err, "skipping corrupt vm.toml");
                    continue;
                }
            };
            vm.normalize();
            out.push(vm);
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn save_vm(&self, vm: &VmConfig) -> anyhow::Result<()> {
        self.save_vm_with_options(vm, true, true)
    }

    pub fn save_vm_with_options(
        &self,
        vm: &VmConfig,
        strict_disks_absolute: bool,
        strict_nic_conflicts: bool,
    ) -> anyhow::Result<()> {
        vm.validate_with_options(strict_disks_absolute, strict_nic_conflicts)?;
        self.save_vm_unchecked(vm)
    }

    pub fn save_vm_unchecked(&self, vm: &VmConfig) -> anyhow::Result<()> {
        let vm_dir = self.vm_dir(vm.id);
        fs::create_dir_all(vm_dir.join("logs"))
            .with_context(|| format!("creating VM directory {}", vm_dir.display()))?;
        let text = toml::to_string_pretty(vm).context("serializing VM config")?;
        atomic_write(&self.vm_config_path(vm.id), text.as_bytes(), false)
    }

    pub fn delete_vm_dir(&self, id: Uuid) -> anyhow::Result<()> {
        let vm_dir = self.vm_dir(id);
        match fs::symlink_metadata(&vm_dir) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("inspecting {}", vm_dir.display()));
            }
        }
        // Remove logs first so that if removing open log files fails (e.g. on
        // Windows), vm.toml and state.vmgs remain intact and the VM does not
        // vanish or lose TPM/UEFI state on restart. Remove vm.toml before
        // state.vmgs because deleting vm.toml is the point of no return and any
        // orphaned state.vmgs left by a transient failure is cleaned up on the
        // next daemon startup via `cleanup_orphaned_vmgs`.
        let logs_dir = vm_dir.join("logs");
        match fs::remove_dir_all(&logs_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", logs_dir.display())),
        }
        let vm_toml = vm_dir.join("vm.toml");
        match fs::remove_file(&vm_toml) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", vm_toml.display())),
        }
        let vmgs_path = vm_dir.join("state.vmgs");
        match fs::remove_file(&vmgs_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    vm_id = %id,
                    path = %vmgs_path.display(),
                    error = %err,
                    "failed to remove state.vmgs during VM deletion; continuing cleanup"
                );
            }
        }
        // Remove the VM directory only if it is now empty.
        let _ = fs::remove_dir(&vm_dir);
        Ok(())
    }

    /// Removes orphaned `state.vmgs` files left behind in UUID-named VM directories
    /// that lack `vm.toml`, and removes the directory if empty (preserving user disks).
    pub fn cleanup_orphaned_vmgs(&self) {
        let vms_dir = self.vms_dir();
        let Ok(entries) = fs::read_dir(&vms_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let dir_path = entry.path();
            let Some(name) = dir_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if Uuid::parse_str(name).is_err() {
                continue;
            }
            let vm_toml = dir_path.join("vm.toml");
            match fs::symlink_metadata(&vm_toml) {
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(
                        vm_id = %name,
                        path = %vm_toml.display(),
                        error = %err,
                        "skipping orphaned state.vmgs cleanup because vm.toml metadata could not be inspected"
                    );
                    continue;
                }
            }
            let vmgs_path = dir_path.join("state.vmgs");
            if vmgs_path.is_file() {
                match fs::remove_file(&vmgs_path) {
                    Ok(()) => {
                        tracing::info!(
                            vm_id = %name,
                            path = %vmgs_path.display(),
                            "removed orphaned state.vmgs"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            vm_id = %name,
                            path = %vmgs_path.display(),
                            error = %err,
                            "failed to remove orphaned state.vmgs"
                        );
                    }
                }
            }
            let _ = fs::remove_dir(&dir_path);
        }
    }
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) {
    if let Ok(dir) = fs::File::open(parent)
        && let Err(err) = dir.sync_all()
        && {
            #[allow(unreachable_patterns)]
            let ignored = matches!(
                err.raw_os_error(),
                Some(libc::EINVAL | libc::EOPNOTSUPP | libc::ENOTSUP | libc::EROFS)
            );
            !ignored
        }
    {
        tracing::warn!(
            dir = %parent.display(),
            error = %err,
            "failed to sync parent directory after atomic write"
        );
    }
}

#[cfg(unix)]
fn enforce_unix_0600(file: &fs::File) -> std::io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;

    match file.set_permissions(fs::Permissions::from_mode(0o600)) {
        Ok(()) => Ok(Some(file.metadata()?.permissions().mode() & 0o777)),
        #[allow(unreachable_patterns)]
        Err(err)
            if err.kind() == std::io::ErrorKind::Unsupported
                || matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
                ) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn try_rotate_token_if_0600_enforced<F>(
    path: &Path,
    new_token: &str,
    enforce_0600: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(&fs::File) -> std::io::Result<Option<u32>>,
{
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    let mode = enforce_0600(tmp.as_file()).context("setting owner-only permissions")?;
    if !matches!(mode, Some(m) if (m & 0o077) == 0) {
        return Ok(false);
    }
    tmp.write_all(new_token.as_bytes())
        .with_context(|| format!("writing temp file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temp file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("persisting {}", path.display()))?;
    sync_parent_dir(parent);
    Ok(true)
}

pub fn atomic_write(path: &Path, data: &[u8], owner_only: bool) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    #[cfg(windows)]
    if owner_only {
        let mut tmp_guard = TempPathGuard(None);
        let (tmp_path, mut tmp_file) = create_owner_only_temp_file_windows(parent)
            .with_context(|| format!("creating owner-only temp file in {}", parent.display()))?;
        tmp_guard.0 = Some(tmp_path.clone());
        tmp_file
            .write_all(data)
            .with_context(|| format!("writing temp file for {}", path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("syncing temp file for {}", path.display()))?;
        drop(tmp_file);
        persist_temp_file_windows(&tmp_path, path)
            .with_context(|| format!("persisting {}", path.display()))?;
        tmp_guard.0 = None;
        return Ok(());
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    #[cfg(unix)]
    if owner_only {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("setting owner-only permissions")?;
    }
    #[cfg(not(any(unix, windows)))]
    let _ = owner_only;
    tmp.write_all(data)
        .with_context(|| format!("writing temp file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temp file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("persisting {}", path.display()))?;
    #[cfg(unix)]
    sync_parent_dir(parent);
    Ok(())
}

#[cfg(windows)]
fn current_user_token_buf()
-> anyhow::Result<Vec<std::mem::MaybeUninit<windows_sys::Win32::Security::TOKEN_USER>>> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut raw_token: HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("OpenProcessToken failed");
    }
    let _token = HandleGuard(raw_token);

    let mut needed = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            raw_token,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if ok != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
        return Err(std::io::Error::last_os_error()).context("GetTokenInformation size query failed");
    }
    let elem_size = std::mem::size_of::<TOKEN_USER>();
    let count = (needed as usize).div_ceil(elem_size).max(1);
    let mut buf = vec![MaybeUninit::<TOKEN_USER>::uninit(); count];
    let buf_bytes = u32::try_from(count * elem_size).context("TOKEN_USER buffer overflow")?;
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            raw_token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            buf_bytes,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("GetTokenInformation(TokenUser) failed");
    }
    anyhow::ensure!(
        (returned as usize) >= elem_size,
        "GetTokenInformation(TokenUser) returned truncated buffer ({returned} bytes)"
    );
    Ok(buf)
}

struct TokenRead {
    token: String,
    was_permissive: bool,
    has_hard_links: bool,
}

#[cfg(unix)]
fn validate_token_uid(file_uid: u32, euid: u32, allow_root: bool) -> bool {
    file_uid == euid || (allow_root && euid == 0)
}

fn open_and_read_token(path: &Path, allow_root_reader: bool) -> anyhow::Result<Option<TokenRead>> {
    use std::io::Read;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        let mut opts = fs::OpenOptions::new();
        opts.read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let mut file = match opts.open(path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err)
                if err.raw_os_error() == Some(libc::ELOOP) || {
                    #[cfg(any(
                        target_os = "freebsd",
                        target_os = "dragonfly",
                        target_os = "netbsd",
                        target_os = "openbsd",
                        target_os = "macos"
                    ))]
                    {
                        err.raw_os_error() == Some(libc::EMLINK)
                    }
                    #[cfg(not(any(
                        target_os = "freebsd",
                        target_os = "dragonfly",
                        target_os = "netbsd",
                        target_os = "openbsd",
                        target_os = "macos"
                    )))]
                    {
                        false
                    }
                } =>
            {
                return Err(anyhow::Error::new(TokenSecurityError(format!(
                    "refusing to use non-regular or symlinked daemon token file {}",
                    path.display()
                ))));
            }
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        let meta = file
            .metadata()
            .with_context(|| format!("inspecting {}", path.display()))?;
        if !meta.file_type().is_file() {
            return Err(anyhow::Error::new(TokenSecurityError(format!(
                "refusing to use non-regular or symlinked daemon token file {}",
                path.display()
            ))));
        }
        let euid = unsafe { libc::geteuid() };
        if !validate_token_uid(meta.uid(), euid, allow_root_reader) {
            return Err(anyhow::Error::new(TokenSecurityError(format!(
                "refusing to use daemon token file {} owned by uid {} (expected {})",
                path.display(),
                meta.uid(),
                euid
            ))));
        }
        let has_hard_links = meta.nlink() > 1;
        let mut was_permissive = false;
        let mode = meta.permissions().mode() & 0o777;
        if (mode & 0o077) != 0 {
            was_permissive = true;
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:04o}"),
                "daemon.token has overly permissive permissions (expected 0600)"
            );
        }
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(TokenRead {
            token: content.trim().to_string(),
            was_permissive,
            has_hard_links,
        }))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

        let _ = allow_root_reader;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

        let sym_meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("inspecting {}", path.display()));
            }
        };
        if sym_meta.file_type().is_symlink() || !sym_meta.file_type().is_file() {
            return Err(anyhow::Error::new(TokenSecurityError(format!(
                "refusing to use non-regular or symlinked daemon token file {}",
                path.display()
            ))));
        }

        let mut opts = fs::OpenOptions::new();
        opts.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = match opts.open(path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        let meta = file
            .metadata()
            .with_context(|| format!("inspecting {}", path.display()))?;
        if !meta.file_type().is_file()
            || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(anyhow::Error::new(TokenSecurityError(format!(
                "refusing to use non-regular or symlinked daemon token file {}",
                path.display()
            ))));
        }
        let mut was_permissive = false;
        if !is_handle_owner_only_windows(&file, path)? {
            was_permissive = true;
            tracing::warn!(
                path = %path.display(),
                "daemon.token has overly permissive Windows ACL (expected owner-only)"
            );
        }
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(TokenRead {
            token: content.trim().to_string(),
            was_permissive,
            has_hard_links: false,
        }))
    }
}

pub fn open_shm_file_nofollow(path: &Path) -> anyhow::Result<fs::File> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.file_type().is_file())
    {
        bail!(
            "refusing to use non-regular or symlinked FastDesk SHM path {}",
            path.display()
        );
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts
        .open(path)
        .with_context(|| format!("creating FastDesk SHM file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        set_path_owner_only_windows(path)?;
    }
    Ok(file)
}

#[cfg(windows)]
struct TempPathGuard(Option<PathBuf>);

#[cfg(windows)]
impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(windows)]
struct LocalFreeAcl(*mut windows_sys::Win32::Security::ACL);

#[cfg(windows)]
impl Drop for LocalFreeAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0.cast());
            }
        }
    }
}

#[cfg(windows)]
fn build_owner_only_dacl_windows() -> anyhow::Result<LocalFreeAcl> {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, GENERIC_ALL};
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::NO_INHERITANCE;

    let token_buf = current_user_token_buf()?;
    let token_user = unsafe { token_buf[0].assume_init_ref() };
    let user_sid = token_user.User.Sid;

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user_sid.cast(),
        },
    };

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let err = unsafe { SetEntriesInAclW(1, &ea, std::ptr::null(), &mut new_dacl) };
    if err != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(err as i32))
            .context("SetEntriesInAclW failed");
    }
    Ok(LocalFreeAcl(new_dacl))
}

#[cfg(windows)]
fn create_owner_only_temp_file_windows(parent: &Path) -> anyhow::Result<(PathBuf, fs::File)> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    const SECURITY_DESCRIPTOR_REVISION_1: u32 = 1;

    let dacl = build_owner_only_dacl_windows()?;
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let sd_ptr: PSECURITY_DESCRIPTOR = (&raw mut sd).cast();
    if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION_1) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("InitializeSecurityDescriptor failed");
    }
    if unsafe { SetSecurityDescriptorDacl(sd_ptr, 1, dacl.0.cast_const(), 0) } == 0 {
        return Err(std::io::Error::last_os_error()).context("SetSecurityDescriptorDacl failed");
    }
    if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("SetSecurityDescriptorControl failed");
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_ptr,
        bInheritHandle: 0,
    };

    for _ in 0..8 {
        let tmp_path = parent.join(format!(".openvmms-tmp-{}", Uuid::new_v4()));
        let wide: Vec<u16> = tmp_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let file = unsafe { fs::File::from_raw_handle(handle as RawHandle) };
            return Ok((tmp_path, file));
        }
        let err = std::io::Error::last_os_error();
        if matches!(
            err.raw_os_error(),
            Some(code) if code == ERROR_FILE_EXISTS as i32 || code == ERROR_ALREADY_EXISTS as i32
        ) {
            continue;
        }
        return Err(err).with_context(|| format!("CreateFileW failed for {}", tmp_path.display()));
    }
    bail!(
        "exhausted retries creating unique temp file in {}",
        parent.display()
    );
}

#[cfg(windows)]
fn persist_temp_file_windows(tmp_path: &Path, dest: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};

    let src_wide: Vec<u16> = tmp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let dst_wide: Vec<u16> = dest
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe { MoveFileExW(src_wide.as_ptr(), dst_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING) }
        == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("MoveFileExW failed for {}", dest.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn try_rotate_token_if_owner_only_enforced_windows(
    path: &Path,
    new_token: &str,
) -> anyhow::Result<bool> {
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    let mut tmp_guard = TempPathGuard(None);
    let (tmp_path, mut tmp_file) = match create_owner_only_temp_file_windows(parent) {
        Ok(pair) => pair,
        Err(_) => return Ok(false),
    };
    tmp_guard.0 = Some(tmp_path.clone());
    if !matches!(is_handle_owner_only_windows(&tmp_file, &tmp_path), Ok(true)) {
        return Ok(false);
    }
    tmp_file
        .write_all(new_token.as_bytes())
        .with_context(|| format!("writing temp file for {}", path.display()))?;
    tmp_file
        .sync_all()
        .with_context(|| format!("syncing temp file for {}", path.display()))?;
    drop(tmp_file);
    persist_temp_file_windows(&tmp_path, path)
        .with_context(|| format!("persisting {}", path.display()))?;
    tmp_guard.0 = None;
    Ok(true)
}

#[cfg(all(windows, test))]
fn try_rotate_token_if_owner_only_enforced_windows_with<F>(
    path: &Path,
    new_token: &str,
    enforce_owner_only: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(&Path, &fs::File) -> anyhow::Result<bool>,
{
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    if !enforce_owner_only(tmp.path(), tmp.as_file())? {
        return Ok(false);
    }
    tmp.write_all(new_token.as_bytes())
        .with_context(|| format!("writing temp file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temp file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("persisting {}", path.display()))?;
    Ok(true)
}

#[cfg(windows)]
fn is_handle_owner_only_windows(file: &fs::File, path: &Path) -> anyhow::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    struct LocalFreeSd(PSECURITY_DESCRIPTOR);
    impl Drop for LocalFreeSd {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let err = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(err as i32))
            .with_context(|| format!("GetSecurityInfo failed for {}", path.display()));
    }
    let _sd_guard = LocalFreeSd(sd);

    let mut control = 0u16;
    let mut rev = 0u32;
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) } == 0 {
        return Err(std::io::Error::last_os_error()).context("GetSecurityDescriptorControl failed");
    }
    if (control & SE_DACL_PROTECTED) == 0 || dacl.is_null() {
        return Ok(false);
    }

    let mut size_info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size_info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("GetAclInformation failed");
    }
    if size_info.AceCount != 1 {
        return Ok(false);
    }

    let mut ace_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace_ptr) } == 0 {
        return Err(std::io::Error::last_os_error()).context("GetAce failed");
    }
    let ace = unsafe { &*(ace_ptr.cast::<ACCESS_ALLOWED_ACE>()) };
    if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE {
        return Ok(false);
    }
    let ace_sid = (&raw const ace.SidStart)
        .cast::<core::ffi::c_void>()
        .cast_mut();
    let token_buf = current_user_token_buf()?;
    let token_user = unsafe { token_buf[0].assume_init_ref() };
    Ok(unsafe { EqualSid(ace_sid, token_user.User.Sid) } != 0)
}

#[cfg(windows)]
pub(crate) fn set_path_owner_only_windows(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let dacl_guard = build_owner_only_dacl_windows()?;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let err = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl_guard.0.cast_const(),
            std::ptr::null(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(err as i32))
            .with_context(|| format!("SetNamedSecurityInfoW failed for {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VmPreset;

    #[test]
    fn host_and_vm_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();

        let token1 = store.load_or_create_token().unwrap();
        let token2 = store.load_or_create_token().unwrap();
        assert_eq!(token1, token2);
        assert_eq!(token1.len(), 64);

        let host_cfg = store.load_or_init_host_config().unwrap();
        assert_eq!(host_cfg.data_dir, dir.path());

        let vm_dir = store.vm_dir(Uuid::new_v4());
        let vm = VmConfig::from_preset("Win11-Test", VmPreset::Windows11, &vm_dir);
        store.save_vm(&vm).unwrap();

        // Even if a user disk file happens to reside inside vm_dir, deleting
        // VM metadata with delete_vm_dir must preserve that disk file.
        let disk_in_vm_dir = store.vm_dir(vm.id).join("disk0.img");
        fs::write(&disk_in_vm_dir, b"PRESERVE_ME").unwrap();

        let loaded = store.load_all_vms().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, vm.id);
        assert_eq!(loaded[0].name, "Win11-Test");

        store.delete_vm_dir(vm.id).unwrap();
        assert!(store.load_all_vms().unwrap().is_empty());
        assert!(disk_in_vm_dir.exists());
        assert!(!store.vm_config_path(vm.id).exists());
    }

    #[test]
    fn daemon_lock_prevents_duplicate_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        assert!(!store.is_daemon_locked());

        let lock = store.try_acquire_daemon_lock().unwrap();
        assert!(store.is_daemon_locked());
        let err = store.try_acquire_daemon_lock().unwrap_err();
        assert!(err.to_string().contains("already running"), "{err:#}");

        drop(lock);
        assert!(!store.is_daemon_locked());
        let _reacquired = store.try_acquire_daemon_lock().unwrap();
    }

    #[test]
    fn corrupt_vm_toml_is_skipped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();

        let good_dir = store.vm_dir(Uuid::new_v4());
        let good = VmConfig::from_preset("Valid-VM", VmPreset::LinuxDesktop, &good_dir);
        store.save_vm(&good).unwrap();

        let bad_id = Uuid::new_v4();
        let bad_dir = store.vm_dir(bad_id);
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("vm.toml"), "not = [valid toml").unwrap();

        let loaded = store.load_all_vms().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, good.id);
    }

    #[cfg(unix)]
    #[test]
    fn token_is_0600_and_existing_data_dir_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let store = RegistryStore::new(dir.path()).unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "pre-existing --data-dir permissions must not be overwritten");

        let original_token = store.load_or_create_token().unwrap();
        let tok_mode = fs::metadata(store.token_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(tok_mode, 0o600);

        // Overly permissive existing daemon.token must be tightened to 0600 AND rotated.
        fs::set_permissions(store.token_path(), fs::Permissions::from_mode(0o644)).unwrap();
        let rotated_token = store.load_or_create_token().unwrap();
        let repaired_mode = fs::metadata(store.token_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(repaired_mode, 0o600);
        assert_ne!(
            rotated_token, original_token,
            "daemon.token must be regenerated when loose permissions are detected"
        );
        assert_eq!(store.read_token().unwrap(), rotated_token);

        // Symlinked daemon.token (both live and dangling) must be rejected.
        let decoys = dir.path().join("decoy.token");
        fs::write(&decoys, "deadbeef").unwrap();
        fs::remove_file(store.token_path()).unwrap();
        std::os::unix::fs::symlink(&decoys, store.token_path()).unwrap();
        assert!(store.load_or_create_token().is_err());
        assert!(store.read_token().is_err());

        fs::remove_file(store.token_path()).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing-target.token"), store.token_path())
            .unwrap();
        assert!(store.load_or_create_token().is_err());
        assert!(store.read_token().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn failed_token_rotation_does_not_tighten_old_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let original_token = store.load_or_create_token().unwrap();

        fs::set_permissions(store.token_path(), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();

        struct RestoreDirPerm(PathBuf);
        impl Drop for RestoreDirPerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            }
        }
        let _restore = RestoreDirPerm(dir.path().to_path_buf());

        // Skip permission-denial check when running as root (CAP_DAC_OVERRIDE bypasses 0500).
        if fs::File::create(dir.path().join("probe")).is_err() {
            assert!(store.load_or_create_token().is_err());
            let mode = fs::metadata(store.token_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o644,
                "failed rotation must leave permissive mode intact so next restart still rotates"
            );
            assert_eq!(fs::read_to_string(store.token_path()).unwrap(), original_token);

            drop(_restore);
            let rotated = store.load_or_create_token().unwrap();
            assert_ne!(rotated, original_token);
            let new_mode = fs::metadata(store.token_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(new_mode, 0o600);
        }
    }

    #[cfg(windows)]
    #[test]
    fn token_has_protected_owner_only_dacl_on_windows() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let _ = store.load_or_create_token().unwrap();

        let wide: Vec<u16> = store
            .token_path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let err = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(err, ERROR_SUCCESS);
        struct LocalFreeSd(PSECURITY_DESCRIPTOR);
        impl Drop for LocalFreeSd {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe {
                        LocalFree(self.0);
                    }
                }
            }
        }
        let _sd_guard = LocalFreeSd(sd);

        let mut control = 0u16;
        let mut rev = 0u32;
        assert_ne!(unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) }, 0);
        assert_ne!(control & SE_DACL_PROTECTED, 0, "daemon.token DACL must be protected from inheritance");

        let mut size_info = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        assert_ne!(
            unsafe {
                GetAclInformation(
                    dacl,
                    (&mut size_info as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        assert_eq!(size_info.AceCount, 1, "expected exactly 1 ACE on daemon.token");

        let mut ace_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        assert_ne!(unsafe { GetAce(dacl, 0, &mut ace_ptr) }, 0);
        let ace = unsafe { &*(ace_ptr.cast::<ACCESS_ALLOWED_ACE>()) };
        let ace_sid = (&raw const ace.SidStart).cast::<core::ffi::c_void>().cast_mut();

        let token_buf = current_user_token_buf().unwrap();
        let token_user = unsafe { token_buf[0].assume_init_ref() };
        assert_ne!(unsafe { EqualSid(ace_sid, token_user.User.Sid) }, 0);
    }

    #[test]
    fn is_daemon_locked_never_blocks_acquire() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            let stop = Arc::clone(&stop);
            readers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = store.is_daemon_locked();
                }
            }));
        }

        for _ in 0..200 {
            let lock = store
                .try_acquire_daemon_lock()
                .expect("is_daemon_locked probe blocked try_acquire_daemon_lock");
            assert!(store.is_daemon_locked());
            drop(lock);
        }

        stop.store(true, Ordering::Release);
        for r in readers {
            r.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn delete_vm_dir_preserves_vm_toml_when_log_removal_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let vm = VmConfig::from_preset("KeepToml", VmPreset::Custom, dir.path());
        store.save_vm(&vm).unwrap();

        let vmgs_path = store.vm_dir(vm.id).join("state.vmgs");
        fs::write(&vmgs_path, b"tpm-state").unwrap();

        let logs_dir = store.vm_dir(vm.id).join("logs");
        let log_file = logs_dir.join("openvmm.log");
        fs::write(&log_file, b"still open").unwrap();
        fs::set_permissions(&logs_dir, fs::Permissions::from_mode(0o555)).unwrap();

        struct RestoreLogsPerm(PathBuf);
        impl Drop for RestoreLogsPerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let _restore = RestoreLogsPerm(logs_dir.clone());

        // If running as non-root, deleting the non-empty read-only logs dir must fail
        // BEFORE vm.toml or state.vmgs is touched, so load_all_vms() still sees the VM
        // and its TPM/UEFI state stays intact.
        if fs::remove_file(&log_file).is_err() {
            let err = store.delete_vm_dir(vm.id).unwrap_err();
            assert!(err.to_string().contains("removing"), "{err:#}");
            assert!(store.vm_config_path(vm.id).is_file());
            assert!(vmgs_path.is_file());
            let loaded = store.load_all_vms().unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].id, vm.id);
        }

        drop(_restore);
        store.delete_vm_dir(vm.id).unwrap();
        assert!(!store.vm_config_path(vm.id).exists());
        assert!(!vmgs_path.exists());
    }

    #[test]
    fn load_all_vms_normalizes_legacy_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let mut vm = VmConfig::from_preset("TrimMe", VmPreset::Custom, dir.path());
        vm.name = "  TrimMe  ".into();
        vm.nics[0].mac_address = format!("  {}  ", crate::models::generate_hyperv_mac());
        vm.nics[0].backend = crate::models::NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![crate::models::PortForward {
                protocol: crate::models::IpProto::Tcp,
                host_addr: " 127.0.0.1 ".into(),
                host_port: 8080,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        let vm_dir = store.vm_dir(vm.id);
        fs::create_dir_all(&vm_dir).unwrap();
        let text = toml::to_string_pretty(&vm).unwrap();
        fs::write(store.vm_config_path(vm.id), text).unwrap();

        let loaded = store.load_all_vms().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "TrimMe");
        assert_eq!(loaded[0].nics[0].mac_address, vm.nics[0].mac_address.trim());
        match &loaded[0].nics[0].backend {
            crate::models::NicBackendConfig::Consomme { port_forwards, .. } => {
                assert_eq!(port_forwards[0].host_addr, "127.0.0.1");
            }
            other => panic!("unexpected backend: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissive_token_kept_when_filesystem_does_not_enforce_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let original_token = store.load_or_create_token().unwrap();
        fs::set_permissions(store.token_path(), fs::Permissions::from_mode(0o644)).unwrap();

        // When chmod leaves group/other bits set (e.g. drvfs without metadata, vfat, fixed-mode CIFS),
        // load_or_create_token warns and preserves the existing token.
        let kept = store
            .load_or_create_token_with(|_| Ok(Some(0o644)))
            .unwrap();
        assert_eq!(kept, original_token);
        assert_eq!(
            fs::read_to_string(store.token_path()).unwrap(),
            original_token
        );

        // When chmod is unsupported on the filesystem, the existing token is also preserved.
        let kept_unsupported = store.load_or_create_token_with(|_| Ok(None)).unwrap();
        assert_eq!(kept_unsupported, original_token);
    }

    #[test]
    fn cleanup_orphaned_vmgs_removes_orphans_and_preserves_disks_and_active_vms() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();

        // 1. Orphaned UUID directory with only state.vmgs -> state.vmgs and directory removed.
        let orphan_only_id = Uuid::new_v4();
        let orphan_only_dir = store.vm_dir(orphan_only_id);
        fs::create_dir_all(&orphan_only_dir).unwrap();
        fs::write(orphan_only_dir.join("state.vmgs"), b"secret-tpm").unwrap();

        // 2. Orphaned UUID directory with state.vmgs + user disk -> state.vmgs removed, disk kept.
        let orphan_with_disk_id = Uuid::new_v4();
        let orphan_with_disk_dir = store.vm_dir(orphan_with_disk_id);
        fs::create_dir_all(&orphan_with_disk_dir).unwrap();
        fs::write(orphan_with_disk_dir.join("state.vmgs"), b"secret-tpm").unwrap();
        let user_disk = orphan_with_disk_dir.join("disk0.vhdx");
        fs::write(&user_disk, b"user-data").unwrap();

        // 3. Active VM with vm.toml + state.vmgs -> untouched.
        let active = VmConfig::from_preset("ActiveVm", VmPreset::Custom, dir.path());
        store.save_vm(&active).unwrap();
        let active_vmgs = store.vm_dir(active.id).join("state.vmgs");
        fs::write(&active_vmgs, b"active-tpm").unwrap();

        // 4. Non-UUID directory with state.vmgs -> untouched.
        let non_uuid_dir = store.vms_dir().join("not-a-uuid");
        fs::create_dir_all(&non_uuid_dir).unwrap();
        fs::write(non_uuid_dir.join("state.vmgs"), b"keep").unwrap();

        store.cleanup_orphaned_vmgs();

        assert!(!orphan_only_dir.exists());
        assert!(!orphan_with_disk_dir.join("state.vmgs").exists());
        assert!(user_disk.is_file());
        assert!(active_vmgs.is_file());
        assert!(store.vm_config_path(active.id).is_file());
        assert!(non_uuid_dir.join("state.vmgs").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_rejects_hard_linked_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let _token = store.load_or_create_token().unwrap();
        let alias = dir.path().join("token-hardlink");
        fs::hard_link(store.token_path(), &alias).unwrap();

        let err = store.read_token().unwrap_err();
        assert!(
            format!("{err:#}").contains("hard-linked"),
            "expected hard-linked token rejection, got: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_token_rotates_hard_linked_token_file() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let original = store.load_or_create_token().unwrap();
        let backup_link = dir.path().join("backup-link");
        fs::hard_link(store.token_path(), &backup_link).unwrap();

        let rotated = store.load_or_create_token().unwrap();
        assert_ne!(rotated, original);
        assert_eq!(fs::read_to_string(&backup_link).unwrap(), original);
        assert_eq!(fs::metadata(store.token_path()).unwrap().nlink(), 1);
        assert_eq!(store.read_token().unwrap(), rotated);
    }

    #[cfg(unix)]
    #[test]
    fn validate_token_uid_allows_root_only_when_permitted() {
        assert!(validate_token_uid(1000, 1000, false));
        assert!(validate_token_uid(1000, 1000, true));
        assert!(!validate_token_uid(1000, 0, false));
        assert!(validate_token_uid(1000, 0, true));
        assert!(!validate_token_uid(1000, 1001, true));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_orphaned_vmgs_preserves_state_on_dangling_vm_toml_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let vm_id = Uuid::new_v4();
        let vm_dir = store.vm_dir(vm_id);
        fs::create_dir_all(&vm_dir).unwrap();
        let vmgs = vm_dir.join("state.vmgs");
        fs::write(&vmgs, b"tpm-state").unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing-vm.toml"), vm_dir.join("vm.toml"))
            .unwrap();

        store.cleanup_orphaned_vmgs();

        assert!(
            vmgs.is_file(),
            "state.vmgs should be preserved when vm.toml is a dangling symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_vm_dir_propagates_permission_error_on_unreadable_vms_dir() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let vm_id = Uuid::new_v4();
        let vm_dir = store.vm_dir(vm_id);
        fs::create_dir_all(&vm_dir).unwrap();
        fs::write(vm_dir.join("vm.toml"), b"id = \"test\"").unwrap();

        let vms_dir = store.vms_dir();
        let orig_mode = fs::metadata(&vms_dir).unwrap().permissions().mode();
        fs::set_permissions(&vms_dir, fs::Permissions::from_mode(0o000)).unwrap();
        let res = store.delete_vm_dir(vm_id);
        fs::set_permissions(&vms_dir, fs::Permissions::from_mode(orig_mode)).unwrap();

        assert!(
            res.is_err(),
            "delete_vm_dir must propagate EACCES instead of returning Ok(())"
        );
        assert!(vm_dir.join("vm.toml").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn windows_token_rotation_keeps_existing_when_acl_not_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path()).unwrap();
        let original = store.load_or_create_token().unwrap();
        let rotated = try_rotate_token_if_owner_only_enforced_windows_with(
            &store.token_path(),
            "new-token",
            |_, _| Ok(false),
        )
        .unwrap();
        assert!(!rotated);
        assert_eq!(fs::read_to_string(store.token_path()).unwrap(), original);
    }
}
