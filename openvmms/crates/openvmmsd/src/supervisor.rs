use anyhow::Context;
use anyhow::bail;
use chrono::Utc;
use openvmms_core::disk;
use openvmms_core::models::AttachDiskPayload;
use openvmms_core::models::CreateDiskPayload;
use openvmms_core::models::CreateVmPayload;
use openvmms_core::models::DaemonEvent;
use openvmms_core::models::DiskAttachment;
use openvmms_core::models::DiskFormat;
use openvmms_core::models::DiskInfo;
use openvmms_core::models::DiskKind;
use openvmms_core::models::FastDeskTransport;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::IpProto;
use openvmms_core::models::LiveVmProperties;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::NicAttachment;
use openvmms_core::models::NicBackendConfig;
use openvmms_core::models::PortForward;
use openvmms_core::models::ResolvedHostPaths;
use openvmms_core::models::SecureBootTemplateConfig;
use openvmms_core::models::ShutdownVmPayload;
use openvmms_core::models::UpdateVmPayload;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmConfig;
use openvmms_core::models::VmRuntimeState;
use openvmms_core::models::VmStatus;
use openvmms_core::models::VmSummary;
use openvmms_core::models::resolve_host_paths;
use openvmms_core::store::RegistryStore;
use openvmms_core::ttrpc::TtrpcClient;
use openvmms_core::vmservice;
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

struct ManagedVm {
    config: VmConfig,
    runtime: VmRuntimeState,
    child: Option<Child>,
    monitor_handle: Option<tokio::task::AbortHandle>,
    generation: u64,
    start_cancel: Option<watch::Sender<bool>>,
    stop_wait: Option<watch::Receiver<bool>>,
}

struct ActiveListener {
    addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl ManagedVm {
    fn summary(&self) -> VmSummary {
        VmSummary {
            config: self.config.clone(),
            runtime: self.runtime.clone(),
        }
    }

    fn active_rpc_socket(&self, action: &str) -> anyhow::Result<Option<PathBuf>> {
        match self.runtime.status {
            VmStatus::Starting | VmStatus::Stopping => bail!(
                "cannot {action} while VM '{}' is {}",
                self.config.name,
                self.runtime.status
            ),
            VmStatus::Running | VmStatus::Paused => Ok(Some(
                self.runtime
                    .rpc_socket_path
                    .clone()
                    .context("VM is active but has no RPC socket")?,
            )),
            VmStatus::Off | VmStatus::Error => Ok(None),
        }
    }
}

pub struct Supervisor {
    store: RegistryStore,
    auth_token: String,
    host_config: RwLock<HostConfig>,
    host_config_lock: tokio::sync::Mutex<()>,
    active_listener: std::sync::Mutex<Option<ActiveListener>>,
    draining_listeners: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    vms: RwLock<HashMap<Uuid, ManagedVm>>,
    events_tx: broadcast::Sender<DaemonEvent>,
    shutdown_notify: Notify,
    shutdown_watch: watch::Sender<bool>,
}

impl Supervisor {
    pub fn new(data_dir: &Path, listen_override: Option<&str>) -> anyhow::Result<Arc<Self>> {
        let store = RegistryStore::new(data_dir)?;
        let mut host_config = store.load_or_init_host_config()?;
        if let Some(listen) = listen_override {
            host_config.listen_addr = listen.to_string();
        }
        let auth_token = store.load_or_create_token()?;
        let loaded_vms = store.load_all_vms()?;

        let mut vm_map = HashMap::new();
        for cfg in loaded_vms {
            vm_map.insert(
                cfg.id,
                ManagedVm {
                    config: cfg,
                    runtime: VmRuntimeState::default(),
                    child: None,
                    monitor_handle: None,
                    generation: 0,
                    start_cancel: None,
                    stop_wait: None,
                },
            );
        }

        let (events_tx, _) = broadcast::channel(256);
        let (shutdown_watch, _) = watch::channel(false);
        Ok(Arc::new(Self {
            store,
            auth_token,
            host_config: RwLock::new(host_config),
            host_config_lock: tokio::sync::Mutex::new(()),
            active_listener: std::sync::Mutex::new(None),
            draining_listeners: std::sync::Mutex::new(Vec::new()),
            vms: RwLock::new(vm_map),
            events_tx,
            shutdown_notify: Notify::new(),
            shutdown_watch,
        }))
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn listen_addr(&self) -> String {
        self.host_config.read().unwrap().listen_addr.clone()
    }

    pub fn host_config(&self) -> HostConfig {
        self.host_config.read().unwrap().clone()
    }

    pub fn resolved_paths(&self) -> ResolvedHostPaths {
        let cfg = self.host_config.read().unwrap().clone();
        resolve_host_paths(&cfg)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events_tx.subscribe()
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_watch.subscribe()
    }

    pub fn mark_shutting_down(&self) {
        let _ = self.events_tx.send(DaemonEvent::DaemonStopping);
        let _ = self.shutdown_watch.send(true);
    }

    pub fn request_shutdown(&self) {
        self.mark_shutting_down();
        self.shutdown_notify.notify_waiters();
    }

    pub async fn wait_for_shutdown_signal(&self) {
        self.shutdown_notify.notified().await;
    }

    fn emit(&self, event: DaemonEvent) {
        let _ = self.events_tx.send(event);
    }

    pub fn host_info(&self) -> HostInfo {
        let cfg = self.host_config();
        let resolved_paths = resolve_host_paths(&cfg);
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "localhost".to_string());
        HostInfo {
            hostname,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            data_dir: cfg.data_dir,
            runtime_dir: cfg.runtime_dir,
            listen_addr: cfg.listen_addr,
            resolved_paths,
        }
    }

    /// Spawns an HTTP API server on `listener` and shuts down any previously active listener.
    pub fn serve_on_listener(
        self: &Arc<Self>,
        listener: tokio::net::TcpListener,
    ) -> anyhow::Result<SocketAddr> {
        let bound_addr = listener.local_addr()?;
        let app = crate::server::build_router(Arc::clone(self), self.auth_token.clone());
        let mut shutdown_rx = self.subscribe_shutdown();
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let already_stopping = *shutdown_rx.borrow() || *stop_rx.borrow();
                    if already_stopping {
                        return;
                    }
                    tokio::select! {
                        _ = shutdown_rx.changed() => {}
                        _ = stop_rx.changed() => {}
                    }
                })
                .await;
            if let Err(err) = result {
                warn!(address = %bound_addr, error = %err, "openvmmsd HTTP listener exited with error");
            }
        });
        let previous = self.active_listener.lock().unwrap().replace(ActiveListener {
            addr: bound_addr,
            stop_tx,
            handle,
        });
        if let Some(mut old) = previous {
            let _ = old.stop_tx.send(true);
            let drain = tokio::spawn(async move {
                if tokio::time::timeout(Duration::from_millis(200), &mut old.handle)
                    .await
                    .is_err()
                {
                    old.handle.abort();
                    let _ = old.handle.await;
                }
            });
            self.draining_listeners.lock().unwrap().push(drain);
        }
        Ok(bound_addr)
    }

    /// Waits for all active HTTP listener tasks started by this supervisor to exit.
    pub async fn join_all_listeners(&self) {
        let active = self.active_listener.lock().unwrap().take();
        if let Some(listener) = active {
            let _ = listener.stop_tx.send(true);
            let _ = listener.handle.await;
        }
        let drains = {
            let mut guard = self.draining_listeners.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for handle in drains {
            let _ = handle.await;
        }
    }

    pub async fn update_host_config(
        self: &Arc<Self>,
        mut new_cfg: HostConfig,
    ) -> anyhow::Result<HostConfig> {
        let _lock = self.host_config_lock.lock().await;
        if new_cfg.data_dir.as_os_str().is_empty() {
            new_cfg.data_dir = self.store.data_dir().to_path_buf();
        }
        if new_cfg.runtime_dir.as_os_str().is_empty() {
            new_cfg.runtime_dir = openvmms_core::models::default_runtime_dir(&new_cfg.data_dir);
        }

        let trimmed_addr = new_cfg
            .listen_addr
            .trim()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');
        let target_addr: SocketAddr = trimmed_addr
            .parse()
            .with_context(|| format!("invalid listen address '{}'", new_cfg.listen_addr))?;

        let current_addr = self
            .active_listener
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.addr);
        if let Some(prev_addr) = current_addr
            && prev_addr != target_addr
        {
            let listener = tokio::net::TcpListener::bind(target_addr)
                .await
                .with_context(|| format!("binding openvmmsd HTTP server on {target_addr}"))?;
            let bound_addr = self.serve_on_listener(listener)?;
            new_cfg.listen_addr = bound_addr.to_string();
        } else {
            new_cfg.listen_addr = target_addr.to_string();
        }

        self.store.save_host_config(&new_cfg)?;
        *self.host_config.write().unwrap() = new_cfg.clone();
        self.emit(DaemonEvent::HostConfigUpdated);
        Ok(new_cfg)
    }

    pub fn list_switches(&self) -> Vec<VirtualSwitchInfo> {
        let mut out = vec![VirtualSwitchInfo {
            id: "consomme".to_string(),
            name: "Default NAT (Consomme)".to_string(),
            kind: "nat".to_string(),
            description: "User-mode NAT with live TCP/UDP host port forwarding".to_string(),
        }];

        #[cfg(windows)]
        {
            out.extend(discover_windows_switches());
        }
        #[cfg(target_os = "linux")]
        {
            out.extend(discover_linux_tap_interfaces());
        }
        out
    }

    fn resolve_vm_id(&self, id_or_name: &str) -> anyhow::Result<Uuid> {
        let guard = self.vms.read().unwrap();
        if let Ok(uuid) = Uuid::parse_str(id_or_name)
            && guard.contains_key(&uuid)
        {
            return Ok(uuid);
        }
        let target = id_or_name.trim().to_ascii_lowercase();
        let mut matches = guard
            .values()
            .filter(|m| m.config.name.to_ascii_lowercase() == target)
            .map(|m| m.config.id);
        if let Some(first) = matches.next() {
            if matches.next().is_some() {
                bail!("multiple VMs share name '{id_or_name}'; specify UUID");
            }
            return Ok(first);
        }
        bail!("VM not found: {id_or_name}");
    }

    pub fn list_vms(&self) -> Vec<VmSummary> {
        let guard = self.vms.read().unwrap();
        let mut items: Vec<VmSummary> = guard.values().map(ManagedVm::summary).collect();
        items.sort_by(|a, b| {
            a.config
                .name
                .to_lowercase()
                .cmp(&b.config.name.to_lowercase())
                .then(a.config.id.cmp(&b.config.id))
        });
        items
    }

    pub fn get_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let guard = self.vms.read().unwrap();
        Ok(guard.get(&id).context("VM not found")?.summary())
    }

    pub async fn get_vm_with_live_properties(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let (status, sock_path) = {
            let guard = self.vms.read().unwrap();
            let vm = guard.get(&id).context("VM not found")?;
            (vm.runtime.status, vm.runtime.rpc_socket_path.clone())
        };

        if matches!(status, VmStatus::Running | VmStatus::Paused)
            && let Some(sock) = sock_path
            && let Ok(props) = TtrpcClient::new(sock)
                .properties_vm(vmservice::PropertiesVmRequest {
                    types: vec![
                        vmservice::properties_vm_request::PropertiesType::Memory as i32,
                        vmservice::properties_vm_request::PropertiesType::Processor as i32,
                    ],
                })
                .await
        {
            let state_str = match vmservice::VmState::from_i32(props.state) {
                Some(vmservice::VmState::Uninitialized) => "Uninitialized",
                Some(vmservice::VmState::Paused) => "Paused",
                Some(vmservice::VmState::Running) => "Running",
                Some(vmservice::VmState::Halted) => "Halted",
                None => "Unknown",
            }
            .to_string();
            let live = LiveVmProperties {
                state: state_str,
                halt_reason: props.halt_reason,
                working_set_bytes: props.memory_stats.map(|m| m.working_set_bytes),
                total_runtime_ns: props.processor_stats.map(|p| p.total_runtime_ns),
            };
            let mut guard = self.vms.write().unwrap();
            if let Some(vm) = guard.get_mut(&id) {
                vm.runtime.live_properties = Some(live);
                return Ok(vm.summary());
            }
        }

        self.get_vm(&id.to_string())
    }

    pub fn create_vm(&self, payload: CreateVmPayload) -> anyhow::Result<VmSummary> {
        let trimmed_name = payload.name.trim().to_string();
        if trimmed_name.is_empty() {
            bail!("VM name cannot be empty");
        }

        {
            let guard = self.vms.read().unwrap();
            if guard
                .values()
                .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
            {
                bail!("a VM named '{trimmed_name}' already exists");
            }
        }

        let probe_id = Uuid::new_v4();
        let vm_dir = self.store.vm_dir(probe_id);
        let mut cfg = VmConfig::from_preset(trimmed_name.clone(), payload.preset, &vm_dir);
        cfg.id = probe_id;
        cfg.vmgs_path = Some(vm_dir.join("state.vmgs"));
        cfg.description = payload.description;

        if let Some(vcpus) = payload.vcpus {
            cfg.vcpus = vcpus;
        }
        if let Some(memory_mb) = payload.memory_mb {
            cfg.memory_mb = memory_mb;
        }
        if let Some(nested) = payload.nested_virt {
            cfg.nested_virt = nested;
        }
        if let Some(fw) = payload.uefi_firmware.filter(|p| !p.as_os_str().is_empty()) {
            cfg.uefi_firmware = Some(fw);
        }
        if let Some(sb) = payload.secure_boot_enabled {
            cfg.secure_boot_enabled = sb;
        }
        if let Some(tpl) = payload.secure_boot_template {
            cfg.secure_boot_template = tpl;
        }
        if let Some(fd) = payload.fastdesk {
            cfg.fastdesk = fd;
        }
        if let Some(nics) = payload.nics {
            cfg.nics = nics;
        }

        if let Some(iso_path) = &payload.iso_path
            && !iso_path.is_file()
        {
            bail!("ISO image not found: {}", iso_path.display());
        }

        let paths = self.resolved_paths();

        for mut disk_att in payload.disks {
            if disk_att.kind == DiskKind::Dvd {
                disk_att.read_only = true;
                disk_att.format = DiskFormat::Iso;
            }
            cfg.disks.push(disk_att);
        }

        let pending_disk_req = if let Some(mut disk_req) = payload.create_disk {
            if disk_req.path.as_os_str().is_empty() {
                let ext = match disk_req.format {
                    DiskFormat::Vhdx => "vhdx",
                    _ => "img",
                };
                let stem = sanitize_disk_stem(&cfg.name);
                let short_id = &cfg.id.to_string()[..8];
                disk_req.path = self
                    .store
                    .disks_dir()
                    .join(format!("{stem}-{short_id}-disk0.{ext}"));
            } else {
                disk::sync_disk_path_extension(&mut disk_req.path, disk_req.format);
            }
            let lun = cfg.next_free_lun();
            cfg.disks.push(DiskAttachment {
                lun,
                kind: DiskKind::HardDisk,
                path: disk_req.path.clone(),
                read_only: false,
                format: disk_req.format,
            });
            Some(disk_req)
        } else {
            None
        };

        if let Some(iso_path) = payload.iso_path {
            let lun = cfg.next_free_lun();
            cfg.disks.push(DiskAttachment {
                lun,
                kind: DiskKind::Dvd,
                path: iso_path,
                read_only: true,
                format: DiskFormat::Iso,
            });
        }

        cfg.validate()?;

        let created_disk_path = if let Some(disk_req) = pending_disk_req {
            let info = disk::create_disk(&disk_req, paths.openvmm_img_bin.as_deref())?;
            Some(info.path)
        } else {
            None
        };

        let persist_res: anyhow::Result<VmSummary> = (|| {
            let mut guard = self.vms.write().unwrap();
            if guard
                .values()
                .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
            {
                bail!("a VM named '{trimmed_name}' already exists");
            }
            self.store.save_vm(&cfg)?;

            let managed = ManagedVm {
                config: cfg,
                runtime: VmRuntimeState::default(),
                child: None,
                monitor_handle: None,
                generation: 0,
                start_cancel: None,
                stop_wait: None,
            };
            let summary = managed.summary();
            guard.insert(summary.config.id, managed);
            Ok(summary)
        })();

        let summary = match persist_res {
            Ok(summary) => summary,
            Err(err) => {
                if let Some(path) = created_disk_path {
                    let _ = fs::remove_file(path);
                }
                let _ = self.store.delete_vm_dir(probe_id);
                return Err(err);
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub fn update_vm(&self, id_or_name: &str, payload: UpdateVmPayload) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let mut guard = self.vms.write().unwrap();

        let hardware_change = payload.vcpus.is_some()
            || payload.memory_mb.is_some()
            || payload.nested_virt.is_some()
            || payload.uefi_firmware.is_some()
            || payload.secure_boot_enabled.is_some()
            || payload.secure_boot_template.is_some()
            || payload.fastdesk.is_some()
            || payload.disks.is_some()
            || payload.nics.is_some();

        if let Some(new_name) = &payload.name {
            let trimmed = new_name.trim();
            if trimmed.is_empty() {
                bail!("VM name cannot be empty");
            }
            if guard
                .values()
                .any(|m| m.config.id != id && m.config.name.eq_ignore_ascii_case(trimmed))
            {
                bail!("another VM is already named '{trimmed}'");
            }
        }

        let vm = guard.get_mut(&id).context("VM not found")?;
        if hardware_change && !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
            bail!(
                "cannot edit hardware configuration while VM '{}' is {}",
                vm.config.name,
                vm.runtime.status
            );
        }

        let mut updated = vm.config.clone();
        if let Some(name) = payload.name {
            updated.name = name.trim().to_string();
        }
        if let Some(desc) = payload.description {
            updated.description = desc;
        }
        if let Some(vcpus) = payload.vcpus {
            updated.vcpus = vcpus;
        }
        if let Some(mem) = payload.memory_mb {
            updated.memory_mb = mem;
        }
        if let Some(nested) = payload.nested_virt {
            updated.nested_virt = nested;
        }
        if let Some(fw) = payload.uefi_firmware {
            updated.uefi_firmware = fw.filter(|p| !p.as_os_str().is_empty());
        }
        if let Some(sb) = payload.secure_boot_enabled {
            updated.secure_boot_enabled = sb;
        }
        if let Some(tpl) = payload.secure_boot_template {
            updated.secure_boot_template = tpl;
        }
        if let Some(fd) = payload.fastdesk {
            updated.fastdesk = fd;
        }
        if let Some(disks) = payload.disks {
            updated.disks = disks;
        }
        if let Some(nics) = payload.nics {
            updated.nics = nics;
        }
        updated.updated_at = Utc::now();

        updated.validate()?;
        self.store.save_vm(&updated)?;
        vm.config = updated;
        let summary = vm.summary();
        drop(guard);

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub fn delete_vm(&self, id_or_name: &str, delete_disks: bool) -> anyhow::Result<()> {
        let id = self.resolve_vm_id(id_or_name)?;
        let disks_to_delete = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get(&id).context("VM not found")?;
            if !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                bail!(
                    "cannot delete VM '{}' while status is {}; stop it first",
                    vm.config.name,
                    vm.runtime.status
                );
            }
            let vm = guard.remove(&id).unwrap();
            if delete_disks {
                vm.config
                    .disks
                    .into_iter()
                    .filter(|d| d.kind == DiskKind::HardDisk && !d.read_only)
                    .map(|d| d.path)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };

        for path in disks_to_delete {
            if path.is_file()
                && let Err(err) = fs::remove_file(&path)
            {
                warn!(path = %path.display(), error = %err, "failed to delete VM disk");
            }
        }
        self.store.delete_vm_dir(id)?;
        self.emit(DaemonEvent::VmDeleted { id });
        Ok(())
    }

    fn is_start_generation_active(&self, id: Uuid, start_gen: u64) -> bool {
        self.vms
            .read()
            .unwrap()
            .get(&id)
            .is_some_and(|vm| vm.generation == start_gen && vm.runtime.status == VmStatus::Starting)
    }

    pub async fn start_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let (cfg, host_cfg, paths, start_gen, mut cancel_rx) = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            if !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                bail!(
                    "VM '{}' is already {}",
                    vm.config.name,
                    vm.runtime.status
                );
            }
            vm.generation += 1;
            let start_gen = vm.generation;
            let (cancel_tx, cancel_rx) = watch::channel(false);
            vm.start_cancel = Some(cancel_tx);
            vm.runtime.status = VmStatus::Starting;
            vm.runtime.last_error = None;
            vm.runtime.live_properties = None;
            (
                vm.config.clone(),
                self.host_config(),
                self.resolved_paths(),
                start_gen,
                cancel_rx,
            )
        };

        self.emit_vm_snapshot(id);

        let result = self
            .start_vm_inner(id, start_gen, &mut cancel_rx, cfg, host_cfg, paths)
            .await;
        match result {
            Ok(summary) => Ok(summary),
            Err(err) => {
                self.cleanup_failed_start(id, start_gen, &format!("{err:#}"));
                Err(err)
            }
        }
    }

    async fn start_vm_inner(
        self: &Arc<Self>,
        id: Uuid,
        start_gen: u64,
        cancel_rx: &mut watch::Receiver<bool>,
        cfg: VmConfig,
        host_cfg: HostConfig,
        paths: ResolvedHostPaths,
    ) -> anyhow::Result<VmSummary> {
        let openvmm_bin = paths.openvmm_bin.context(
            "openvmm binary not found; configure openvmm_bin in Host Settings or place openvmm next to openvmmsd",
        )?;
        let uefi_path = cfg
            .uefi_firmware
            .clone()
            .or(paths.uefi_firmware_path)
            .context(
                "UEFI firmware (MSVM.fd) not found; configure uefi_firmware_path in Host Settings",
            )?;
        if !uefi_path.is_file() {
            bail!("UEFI firmware not found at {}", uefi_path.display());
        }

        for d in &cfg.disks {
            if !d.path.is_file() {
                bail!("attached disk/ISO not found: {}", d.path.display());
            }
        }

        if *cancel_rx.borrow() || !self.is_start_generation_active(id, start_gen) {
            bail!("VM startup cancelled by stop request");
        }

        let runtime_dir = &host_cfg.runtime_dir;
        fs::create_dir_all(runtime_dir)
            .with_context(|| format!("creating runtime dir {}", runtime_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700));
        }

        let sock_path = runtime_dir.join(format!("{id}.sock"));
        let _ = fs::remove_file(&sock_path);

        let shm_path = if cfg.fastdesk.enabled && cfg.fastdesk.use_shm {
            let path = runtime_dir.join(format!("vram-{id}.shm"));
            preallocate_shm_file(&path, cfg.fastdesk.vram_mb)?;
            Some(path)
        } else {
            None
        };

        let log_path = self.store.vm_log_path(id);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stderr_log_path = log_path.with_extension("stderr.log");
        let stderr_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stderr_log_path)
            .with_context(|| format!("opening {}", stderr_log_path.display()))?;

        let rpc_arg = format!("path={},transport=ttrpc", sock_path.display());
        let mut cmd = Command::new(&openvmm_bin);
        cmd.arg("--rpc")
            .arg(rpc_arg)
            .arg("--log-file")
            .arg(&log_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", openvmm_bin.display()))?;
        let pid = child.id();

        {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM vanished")?;
            if vm.generation != start_gen || vm.runtime.status != VmStatus::Starting {
                let _ = child.kill();
                let _ = child.wait();
                if vm.runtime.rpc_socket_path.as_ref() != Some(&sock_path) {
                    cleanup_runtime_files(
                        Some(sock_path),
                        shm_path.map(|p| p.to_string_lossy().into_owned()),
                    );
                }
                bail!("VM startup cancelled by stop request");
            }
            vm.child = Some(child);
            vm.runtime.pid = Some(pid);
            vm.runtime.rpc_socket_path = Some(sock_path.clone());
            vm.runtime.shm_path = shm_path.as_ref().map(|p| p.to_string_lossy().into_owned());
        }

        let client = TtrpcClient::new(&sock_path);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if *cancel_rx.borrow() || !self.is_start_generation_active(id, start_gen) {
                bail!("VM startup cancelled by stop request");
            }
            {
                let mut guard = self.vms.write().unwrap();
                if let Some(vm) = guard.get_mut(&id)
                    && vm.generation == start_gen
                    && let Some(child) = vm.child.as_mut()
                    && let Some(status) = child.try_wait()?
                {
                    let err_tail = read_log_tail(&stderr_log_path, 20)
                        .or_else(|_| read_log_tail(&log_path, 20))
                        .unwrap_or_default();
                    bail!(
                        "openvmm exited prematurely with {status}{}",
                        if err_tail.trim().is_empty() {
                            String::new()
                        } else {
                            format!(":\n{}", err_tail.trim())
                        }
                    );
                }
            }
            if sock_path.exists() && run_cancellable_rpc(cancel_rx, client.ping()).await.is_ok() {
                break;
            }
            if *cancel_rx.borrow() {
                bail!("VM startup cancelled by stop request");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "timed out waiting for OpenVMM RPC socket at {}",
                    sock_path.display()
                );
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancel_rx.changed() => {
                    bail!("VM startup cancelled by stop request");
                }
            }
        }

        let create_req = build_create_vm_request(&cfg, &uefi_path, shm_path.as_deref())?;
        run_cancellable_rpc(cancel_rx, client.create_vm(create_req))
            .await
            .context("OpenVMM CreateVM failed")?;
        run_cancellable_rpc(cancel_rx, client.resume_vm())
            .await
            .context("OpenVMM ResumeVM failed")?;

        let props = run_cancellable_rpc(
            cancel_rx,
            client.properties_vm(vmservice::PropertiesVmRequest { types: vec![] }),
        )
        .await
        .context("querying VM properties")?;

        let fastdesk_endpoint = props
            .fastdesk_listen_addr
            .as_deref()
            .map(normalize_fastdesk_endpoint);

        let summary = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM vanished")?;
            if vm.generation != start_gen || vm.runtime.status != VmStatus::Starting {
                bail!("VM startup cancelled by stop request");
            }
            vm.start_cancel = None;
            vm.runtime.status = VmStatus::Running;
            vm.runtime.started_at = Some(Utc::now());
            vm.runtime.fastdesk_endpoint = fastdesk_endpoint;
            if let Some(shm) = props.fastdesk_shm_path {
                vm.runtime.shm_path = Some(shm);
            }

            let sup = Arc::clone(self);
            let sock_clone = sock_path.clone();
            let monitor = tokio::spawn(async move {
                sup.monitor_vm(id, start_gen, sock_clone).await;
            });
            vm.monitor_handle = Some(monitor.abort_handle());
            vm.summary()
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        info!(vm_id = %id, name = %summary.config.name, pid, "VM started");
        Ok(summary)
    }

    fn cleanup_failed_start(&self, id: Uuid, start_gen: u64, err_msg: &str) {
        let mut guard = self.vms.write().unwrap();
        if let Some(vm) = guard.get_mut(&id) {
            if vm.generation != start_gen {
                return;
            }
            vm.start_cancel = None;
            if let Some(abort) = vm.monitor_handle.take() {
                abort.abort();
            }
            if let Some(mut child) = vm.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            cleanup_runtime_files(vm.runtime.rpc_socket_path.take(), vm.runtime.shm_path.take());
            vm.runtime.pid = None;
            vm.runtime.started_at = None;
            vm.runtime.fastdesk_endpoint = None;
            vm.runtime.status = VmStatus::Error;
            vm.runtime.last_error = Some(err_msg.to_string());
            let summary = vm.summary();
            drop(guard);
            self.emit(DaemonEvent::VmUpdated { vm: summary });
        }
    }

    async fn monitor_vm(self: Arc<Self>, id: Uuid, run_gen: u64, sock_path: PathBuf) {
        let client = TtrpcClient::new(&sock_path);
        let wait_res = client.wait_vm().await;

        let stop_done_tx = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                return;
            };
            if vm.generation != run_gen || vm.runtime.status == VmStatus::Stopping {
                return;
            }
            let (stop_done_tx, stop_done_rx) = watch::channel(false);
            vm.runtime.status = VmStatus::Stopping;
            vm.monitor_handle = None;
            vm.stop_wait = Some(stop_done_rx);
            stop_done_tx
        };

        let props = client
            .properties_vm(vmservice::PropertiesVmRequest { types: vec![] })
            .await
            .ok();

        let _ = tokio::time::timeout(Duration::from_secs(3), client.teardown_vm()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), client.quit()).await;

        let mut child_opt = {
            let mut guard = self.vms.write().unwrap();
            guard
                .get_mut(&id)
                .filter(|vm| vm.generation == run_gen)
                .and_then(|vm| vm.child.take())
        };

        let exit_status = if let Some(mut child) = child_opt.take() {
            tokio::task::spawn_blocking(move || {
                let start = std::time::Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(st)) => return Some(st),
                        Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        _ => {
                            let _ = child.kill();
                            return child.wait().ok();
                        }
                    }
                }
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        let summary = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                let _ = stop_done_tx.send(true);
                return;
            };
            if vm.generation != run_gen {
                let _ = stop_done_tx.send(true);
                return;
            }
            cleanup_runtime_files(vm.runtime.rpc_socket_path.take(), vm.runtime.shm_path.take());
            vm.runtime.pid = None;
            vm.runtime.started_at = None;
            vm.runtime.fastdesk_endpoint = None;
            vm.runtime.live_properties = None;
            vm.monitor_handle = None;
            vm.stop_wait = None;

            let halt_reason = props.and_then(|p| p.halt_reason);
            match wait_res {
                Ok(()) => {
                    vm.runtime.status = VmStatus::Off;
                    vm.runtime.last_error = halt_reason.filter(|r| r != "PowerOff");
                }
                Err(err) => {
                    if exit_status.is_some_and(|st| st.success()) {
                        vm.runtime.status = VmStatus::Off;
                        vm.runtime.last_error = None;
                    } else {
                        vm.runtime.status = VmStatus::Error;
                        vm.runtime.last_error = Some(format!("VM exited: {err:#}"));
                    }
                }
            }
            vm.summary()
        };

        let _ = stop_done_tx.send(true);
        self.emit(DaemonEvent::VmUpdated { vm: summary });
    }

    pub async fn stop_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let (wait_rx, stop_task_args) = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            if matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                vm.runtime.status = VmStatus::Off;
                vm.runtime.last_error = None;
                return Ok(vm.summary());
            }
            if vm.runtime.status == VmStatus::Stopping {
                if let Some(rx) = vm.stop_wait.clone() {
                    (Some(rx), None)
                } else {
                    bail!(
                        "cannot stop VM '{}' while it is already Stopping",
                        vm.config.name
                    );
                }
            } else {
                vm.generation += 1;
                let stop_gen = vm.generation;
                if let Some(cancel_tx) = vm.start_cancel.take() {
                    let _ = cancel_tx.send(true);
                }
                let (stop_done_tx, stop_done_rx) = watch::channel(false);
                vm.stop_wait = Some(stop_done_rx.clone());
                vm.runtime.status = VmStatus::Stopping;
                (
                    Some(stop_done_rx),
                    Some((
                        stop_gen,
                        vm.runtime.rpc_socket_path.take(),
                        vm.runtime.shm_path.take(),
                        vm.child.take(),
                        vm.monitor_handle.take(),
                        stop_done_tx,
                    )),
                )
            }
        };

        if let Some((stop_gen, sock_opt, shm_opt, child_opt, abort_opt, stop_done_tx)) =
            stop_task_args
        {
            if let Some(abort) = abort_opt {
                abort.abort();
            }
            self.emit_vm_snapshot(id);
            let sup = Arc::clone(self);
            tokio::spawn(async move {
                if let Some(sock) = &sock_opt {
                    let client = TtrpcClient::new(sock);
                    let _ = tokio::time::timeout(Duration::from_secs(4), client.teardown_vm()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), client.quit()).await;
                }

                if let Some(mut child) = child_opt {
                    let _ = tokio::task::spawn_blocking(move || {
                        let start = std::time::Instant::now();
                        loop {
                            match child.try_wait() {
                                Ok(Some(_)) => break,
                                Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                                _ => {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break;
                                }
                            }
                        }
                    })
                    .await;
                }

                cleanup_runtime_files(sock_opt, shm_opt);
                let summary = {
                    let mut guard = sup.vms.write().unwrap();
                    if let Some(vm) = guard.get_mut(&id) {
                        if vm.generation == stop_gen {
                            vm.runtime.status = VmStatus::Off;
                            vm.runtime.pid = None;
                            vm.runtime.started_at = None;
                            vm.runtime.fastdesk_endpoint = None;
                            vm.runtime.live_properties = None;
                            vm.runtime.last_error = None;
                            vm.stop_wait = None;
                            Some(vm.summary())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                let _ = stop_done_tx.send(true);
                if let Some(summary) = summary {
                    sup.emit(DaemonEvent::VmUpdated { vm: summary });
                }
            });
        }

        if let Some(mut rx) = wait_rx {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }

        let mut emit_normalized = None;
        let summary = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            if matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                let changed = vm.runtime.status == VmStatus::Error || vm.runtime.last_error.is_some();
                vm.runtime.status = VmStatus::Off;
                vm.runtime.last_error = None;
                let s = vm.summary();
                if changed {
                    emit_normalized = Some(s.clone());
                }
                Ok(s)
            } else {
                Ok(vm.summary())
            }
        };
        if let Some(s) = emit_normalized {
            self.emit(DaemonEvent::VmUpdated { vm: s });
        }
        summary
    }

    pub async fn shutdown_vm(
        &self,
        id_or_name: &str,
        req: ShutdownVmPayload,
    ) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let sock = self.require_running_socket(id)?;
        let client = TtrpcClient::new(sock);

        let mechanism = if req.acpi {
            vmservice::shutdown_vm_request::Mechanism::Acpi as i32
        } else {
            vmservice::shutdown_vm_request::Mechanism::HypervIc as i32
        };

        let first = client
            .shutdown_vm(vmservice::ShutdownVmRequest {
                mechanism,
                force: req.force,
            })
            .await;

        if let Err(err) = first {
            if !req.acpi {
                // Fall back to ACPI power button when guest shutdown IC is not ready.
                client
                    .shutdown_vm(vmservice::ShutdownVmRequest {
                        mechanism: vmservice::shutdown_vm_request::Mechanism::Acpi as i32,
                        force: false,
                    })
                    .await
                    .with_context(|| {
                        format!("Hyper-V Shutdown IC failed ({err:#}) and ACPI fallback failed")
                    })?;
            } else {
                return Err(err);
            }
        }

        self.get_vm(&id.to_string())
    }

    pub async fn pause_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let sock = self.require_running_socket(id)?;
        TtrpcClient::new(sock).pause_vm().await?;

        let summary = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            vm.runtime.status = VmStatus::Paused;
            vm.summary()
        };
        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn resume_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let sock = self.require_active_socket(id)?;
        TtrpcClient::new(sock).resume_vm().await?;

        let summary = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            vm.runtime.status = VmStatus::Running;
            vm.summary()
        };
        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn reset_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let sock = self.require_active_socket(id)?;
        TtrpcClient::new(sock).reset_vm().await?;
        self.get_vm(&id.to_string())
    }

    pub async fn attach_disk(
        &self,
        id_or_name: &str,
        req: AttachDiskPayload,
    ) -> anyhow::Result<VmSummary> {
        if !req.path.is_file() {
            bail!("disk file not found: {}", req.path.display());
        }
        let id = self.resolve_vm_id(id_or_name)?;
        let format = req
            .format
            .unwrap_or_else(|| DiskFormat::from_path(&req.path));
        let kind = if format == DiskFormat::Iso {
            DiskKind::Dvd
        } else {
            DiskKind::HardDisk
        };

        let pending = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            let active_sock = vm.active_rpc_socket("attach disk")?;
            let lun = req.lun.unwrap_or_else(|| vm.config.next_free_lun());
            if vm.config.disks.iter().any(|d| d.lun == lun) {
                bail!("SCSI LUN {lun} is already occupied");
            }
            let attachment = DiskAttachment {
                lun,
                kind,
                path: req.path,
                read_only: req.read_only || kind == DiskKind::Dvd,
                format,
            };
            match active_sock {
                Some(sock) => Ok((attachment, sock)),
                None => {
                    let mut updated = vm.config.clone();
                    updated.disks.push(attachment);
                    updated.disks.sort_by_key(|d| d.lun);
                    updated.updated_at = Utc::now();
                    self.store.save_vm(&updated)?;
                    vm.config = updated;
                    Err(vm.summary())
                }
            }
        };

        let summary = match pending {
            Err(summary) => summary,
            Ok((attachment, sock)) => {
                TtrpcClient::new(sock)
                    .modify_resource(vmservice::ModifyResourceRequest {
                        r#type: vmservice::ModifyType::Add as i32,
                        resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                            to_proto_scsi_disk(&attachment),
                        )),
                    })
                    .await
                    .context("hot-adding SCSI disk to running VM")?;

                let mut guard = self.vms.write().unwrap();
                let vm = guard.get_mut(&id).context("VM not found")?;
                if !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused) {
                    bail!(
                        "cannot complete disk attach because VM '{}' changed state to {}",
                        vm.config.name,
                        vm.runtime.status
                    );
                }
                if vm.config.disks.iter().any(|d| d.lun == attachment.lun) {
                    bail!("SCSI LUN {} is already occupied", attachment.lun);
                }
                let mut updated = vm.config.clone();
                updated.disks.push(attachment);
                updated.disks.sort_by_key(|d| d.lun);
                updated.updated_at = Utc::now();
                self.store.save_vm(&updated)?;
                vm.config = updated;
                vm.summary()
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn detach_disk(&self, id_or_name: &str, lun: u32) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let pending = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            let active_sock = vm.active_rpc_socket("detach disk")?;
            let disk = vm
                .config
                .disks
                .iter()
                .find(|d| d.lun == lun)
                .cloned()
                .with_context(|| format!("no disk attached at SCSI LUN {lun}"))?;
            match active_sock {
                Some(sock) => Ok((disk, sock)),
                None => {
                    let mut updated = vm.config.clone();
                    updated.disks.retain(|d| d.lun != lun);
                    updated.updated_at = Utc::now();
                    self.store.save_vm(&updated)?;
                    vm.config = updated;
                    Err(vm.summary())
                }
            }
        };

        let summary = match pending {
            Err(summary) => summary,
            Ok((existing, sock)) => {
                TtrpcClient::new(sock)
                    .modify_resource(vmservice::ModifyResourceRequest {
                        r#type: vmservice::ModifyType::Remove as i32,
                        resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                            vmservice::ScsiDisk {
                                controller: 0,
                                lun: existing.lun,
                                ..Default::default()
                            },
                        )),
                    })
                    .await
                    .context("hot-removing SCSI disk from running VM")?;

                let mut guard = self.vms.write().unwrap();
                let vm = guard.get_mut(&id).context("VM not found")?;
                if !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused) {
                    bail!(
                        "cannot complete disk detach because VM '{}' changed state to {}",
                        vm.config.name,
                        vm.runtime.status
                    );
                }
                let mut updated = vm.config.clone();
                updated.disks.retain(|d| d.lun != lun);
                updated.updated_at = Utc::now();
                self.store.save_vm(&updated)?;
                vm.config = updated;
                vm.summary()
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn mount_iso(
        &self,
        id_or_name: &str,
        req: MountIsoPayload,
    ) -> anyhow::Result<VmSummary> {
        if !req.path.is_file() {
            bail!("ISO image not found: {}", req.path.display());
        }
        let id = self.resolve_vm_id(id_or_name)?;
        let existing_dvd_lun = {
            let guard = self.vms.read().unwrap();
            let vm = guard.get(&id).context("VM not found")?;
            let _ = vm.active_rpc_socket("mount ISO")?;
            match req.lun {
                Some(l) => match vm.config.disks.iter().find(|d| d.lun == l) {
                    Some(d) if d.kind != DiskKind::Dvd => {
                        bail!(
                            "SCSI LUN {l} is occupied by a non-DVD disk ({}); specify a different LUN or omit --lun",
                            d.path.display()
                        );
                    }
                    Some(d) => Some(d.lun),
                    None => None,
                },
                None => vm
                    .config
                    .disks
                    .iter()
                    .find(|d| d.kind == DiskKind::Dvd)
                    .map(|d| d.lun),
            }
        };

        if let Some(lun) = existing_dvd_lun {
            let _ = self.detach_disk(id_or_name, lun).await?;
            self.attach_disk(
                id_or_name,
                AttachDiskPayload {
                    path: req.path,
                    lun: Some(lun),
                    read_only: true,
                    format: Some(DiskFormat::Iso),
                },
            )
            .await
        } else {
            self.attach_disk(
                id_or_name,
                AttachDiskPayload {
                    path: req.path,
                    lun: req.lun,
                    read_only: true,
                    format: Some(DiskFormat::Iso),
                },
            )
            .await
        }
    }

    pub async fn eject_iso(&self, id_or_name: &str, lun: Option<u32>) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let target_lun = {
            let guard = self.vms.read().unwrap();
            let vm = guard.get(&id).context("VM not found")?;
            let _ = vm.active_rpc_socket("eject ISO")?;
            match lun {
                Some(l) => vm
                    .config
                    .disks
                    .iter()
                    .find(|d| d.lun == l && d.kind == DiskKind::Dvd)
                    .map(|d| d.lun)
                    .with_context(|| format!("no ISO/DVD attached at LUN {l}"))?,
                None => vm
                    .config
                    .disks
                    .iter()
                    .find(|d| d.kind == DiskKind::Dvd)
                    .map(|d| d.lun)
                    .context("no ISO/DVD drive attached to VM")?,
            }
        };
        self.detach_disk(id_or_name, target_lun).await
    }

    pub async fn add_port_forward(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        self.modify_port_forward(id_or_name, nic_id, port, true)
            .await
    }

    pub async fn remove_port_forward(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        self.modify_port_forward(id_or_name, nic_id, port, false)
            .await
    }

    async fn modify_port_forward(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
        add: bool,
    ) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let active_sock = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM not found")?;
            let sock_opt = vm.active_rpc_socket("modify port forwarding")?;
            {
                let nic = vm
                    .config
                    .nics
                    .iter()
                    .find(|n| n.id == nic_id)
                    .with_context(|| format!("NIC {nic_id} not found"))?;
                let NicBackendConfig::Consomme { port_forwards, .. } = &nic.backend else {
                    bail!("port forwarding is only supported on Consomme NAT NICs");
                };
                if add && port_forwards.contains(&port) {
                    bail!("port forward {port} already exists");
                }
                if !add && !port_forwards.contains(&port) {
                    bail!("port forward {port} not found");
                }
            }
            match sock_opt {
                Some(sock) => Ok(sock),
                None => {
                    let mut updated = vm.config.clone();
                    let nic = updated
                        .nics
                        .iter_mut()
                        .find(|n| n.id == nic_id)
                        .context("NIC vanished")?;
                    if let NicBackendConfig::Consomme { port_forwards, .. } = &mut nic.backend {
                        if add {
                            port_forwards.push(port.clone());
                        } else {
                            port_forwards.retain(|p| p != &port);
                        }
                    }
                    updated.updated_at = Utc::now();
                    self.store.save_vm(&updated)?;
                    vm.config = updated;
                    Err(vm.summary())
                }
            }
        };

        let summary = match active_sock {
            Err(summary) => summary,
            Ok(sock) => {
                let modify_type = if add {
                    vmservice::ModifyType::Update as i32
                } else {
                    vmservice::ModifyType::Remove as i32
                };
                TtrpcClient::new(sock)
                    .modify_resource(vmservice::ModifyResourceRequest {
                        r#type: modify_type,
                        resource: Some(vmservice::modify_resource_request::Resource::NicConfig(
                            vmservice::NicConfig {
                                nic_id: nic_id.to_string(),
                                backend: Some(vmservice::nic_config::Backend::Consomme(
                                    vmservice::ConsommeBackend {
                                        ports: vec![to_proto_port_config(&port)],
                                        ..Default::default()
                                    },
                                )),
                                ..Default::default()
                            },
                        )),
                    })
                    .await
                    .context("updating Consomme port forward on running VM")?;

                let mut guard = self.vms.write().unwrap();
                let vm = guard.get_mut(&id).context("VM not found")?;
                if !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused) {
                    bail!(
                        "cannot complete port forward update because VM '{}' changed state to {}",
                        vm.config.name,
                        vm.runtime.status
                    );
                }
                let mut updated = vm.config.clone();
                let nic = updated
                    .nics
                    .iter_mut()
                    .find(|n| n.id == nic_id)
                    .context("NIC vanished")?;
                if let NicBackendConfig::Consomme { port_forwards, .. } = &mut nic.backend {
                    if add {
                        if !port_forwards.contains(&port) {
                            port_forwards.push(port);
                        }
                    } else {
                        port_forwards.retain(|p| p != &port);
                    }
                }
                updated.updated_at = Utc::now();
                self.store.save_vm(&updated)?;
                vm.config = updated;
                vm.summary()
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    #[cfg(test)]
    pub fn set_vm_status_for_test(&self, id: Uuid, status: VmStatus) {
        if let Some(vm) = self.vms.write().unwrap().get_mut(&id) {
            vm.runtime.status = status;
        }
    }

    pub fn create_disk(&self, req: CreateDiskPayload) -> anyhow::Result<DiskInfo> {
        let paths = self.resolved_paths();
        disk::create_disk(&req, paths.openvmm_img_bin.as_deref())
    }

    pub fn inspect_disk(&self, path: &Path) -> anyhow::Result<DiskInfo> {
        let paths = self.resolved_paths();
        disk::inspect_disk(path, paths.openvmm_img_bin.as_deref())
    }

    pub fn vm_logs(&self, id_or_name: &str, tail: usize) -> anyhow::Result<String> {
        let id = self.resolve_vm_id(id_or_name)?;
        let log_path = self.store.vm_log_path(id);
        let stderr_path = log_path.with_extension("stderr.log");
        let mut out = String::new();
        if log_path.is_file() {
            out = read_log_tail(&log_path, tail.max(1))?;
        }
        if stderr_path.is_file() {
            let err_tail = read_log_tail(&stderr_path, 40)?;
            if !err_tail.trim().is_empty() {
                if !out.is_empty() {
                    out.push_str("\n--- stderr ---\n");
                }
                out.push_str(&err_tail);
            }
        }
        Ok(out)
    }

    pub async fn shutdown_all_vms(self: &Arc<Self>) {
        let ids: Vec<Uuid> = {
            let guard = self.vms.read().unwrap();
            guard
                .values()
                .filter(|vm| !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error))
                .map(|vm| vm.config.id)
                .collect()
        };
        for id in ids {
            if let Err(err) = self.stop_vm(&id.to_string()).await {
                warn!(vm_id = %id, error = %err, "failed to stop VM during daemon shutdown");
            }
        }
    }

    fn require_running_socket(&self, id: Uuid) -> anyhow::Result<PathBuf> {
        let guard = self.vms.read().unwrap();
        let vm = guard.get(&id).context("VM not found")?;
        if vm.runtime.status != VmStatus::Running {
            bail!(
                "VM '{}' is {} (must be Running)",
                vm.config.name,
                vm.runtime.status
            );
        }
        vm.runtime
            .rpc_socket_path
            .clone()
            .context("VM has no active RPC socket")
    }

    fn require_active_socket(&self, id: Uuid) -> anyhow::Result<PathBuf> {
        let guard = self.vms.read().unwrap();
        let vm = guard.get(&id).context("VM not found")?;
        if !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused) {
            bail!(
                "VM '{}' is {} (must be Running or Paused)",
                vm.config.name,
                vm.runtime.status
            );
        }
        vm.runtime
            .rpc_socket_path
            .clone()
            .context("VM has no active RPC socket")
    }

    fn emit_vm_snapshot(&self, id: Uuid) {
        let guard = self.vms.read().unwrap();
        if let Some(vm) = guard.get(&id) {
            self.emit(DaemonEvent::VmUpdated { vm: vm.summary() });
        }
    }
}

fn sanitize_disk_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "vm".to_string()
    } else {
        trimmed.to_string()
    }
}

fn preallocate_shm_file(path: &Path, vram_mb: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(path)
        .with_context(|| format!("creating FastDesk SHM file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    let size = (vram_mb.clamp(8, 256) as u64) * 1024 * 1024;
    file.set_len(size)
        .with_context(|| format!("sizing FastDesk SHM file {}", path.display()))?;
    Ok(())
}

fn cleanup_runtime_files(sock_path: Option<PathBuf>, shm_path: Option<String>) {
    if let Some(sock) = sock_path {
        let _ = fs::remove_file(sock);
    }
    if let Some(shm) = shm_path {
        let _ = fs::remove_file(shm);
    }
}

fn normalize_fastdesk_endpoint(raw: &str) -> String {
    let normalized = if raw.starts_with("tcp://")
        || raw.starts_with("hvsock://")
        || raw.starts_with("vsock://")
    {
        raw.to_string()
    } else {
        format!("tcp://{raw}")
    };
    if let Some(rest) = normalized.strip_prefix("tcp://0.0.0.0:") {
        return format!("tcp://127.0.0.1:{rest}");
    }
    if let Some(rest) = normalized.strip_prefix("tcp://[::]:") {
        return format!("tcp://[::1]:{rest}");
    }
    normalized
}

fn build_create_vm_request(
    cfg: &VmConfig,
    uefi_path: &Path,
    shm_path: Option<&Path>,
) -> anyhow::Result<vmservice::CreateVmRequest> {
    let sb_template = match cfg.secure_boot_template {
        SecureBootTemplateConfig::None => {
            vmservice::uefi::initial_variables::SecureBootTemplate::None as i32
        }
        SecureBootTemplateConfig::MicrosoftWindows => {
            vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftWindows as i32
        }
        SecureBootTemplateConfig::MicrosoftUefiCa => {
            vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftUefiCertificateAuthority
                as i32
        }
    };

    let vmgs = cfg.vmgs_path.as_ref().map(|path| {
        let create_size_bytes = (!path.exists()).then_some(4 * 1024 * 1024);
        vmservice::VmgsConfig {
            backend: Some(vmservice::DiskBackend {
                kind: Some(vmservice::disk_backend::Kind::File(vmservice::FileDisk {
                    path: path.to_string_lossy().into_owned(),
                    direct: false,
                    create_size_bytes,
                })),
            }),
            provision_condition: vmservice::vmgs_config::ProvisionCondition::OnEmpty as i32,
        }
    });

    let scsi_disks = cfg.disks.iter().map(to_proto_scsi_disk).collect();
    let nic_config = cfg.nics.iter().map(to_proto_nic_config).collect();

    let fastdesk_config = if cfg.fastdesk.enabled {
        Some(vmservice::FastDeskConfig {
            vram_mb: cfg.fastdesk.vram_mb,
            listen_addr: cfg.fastdesk.listen_addr.clone(),
            vsock_port: match cfg.fastdesk.transport {
                FastDeskTransport::HvSock => Some(cfg.fastdesk.vsock_port.unwrap_or(5901)),
                FastDeskTransport::Tcp => None,
            },
            shm_path: shm_path.map(|p| p.to_string_lossy().into_owned()),
            max_clients: 0,
        })
    } else {
        None
    };

    Ok(vmservice::CreateVmRequest {
        config: Some(vmservice::VmConfig {
            memory_config: Some(vmservice::MemoryConfig {
                memory_mb: cfg.memory_mb,
                ..Default::default()
            }),
            processor_config: Some(vmservice::ProcessorConfig {
                processor_count: cfg.vcpus,
                ..Default::default()
            }),
            boot_config: Some(vmservice::vm_config::BootConfig::Uefi(vmservice::Uefi {
                firmware_path: uefi_path.to_string_lossy().into_owned(),
                initial_variables: Some(vmservice::uefi::InitialVariables {
                    secure_boot_template: sb_template,
                    custom_uefi_json_path: None,
                }),
                secure_boot_enabled: cfg.secure_boot_enabled,
            })),
            devices_config: Some(vmservice::DevicesConfig {
                scsi_disks,
                nic_config,
                enable_scsi_controller: true,
                ..Default::default()
            }),
            nested_virt: cfg.nested_virt,
            vmgs,
            fastdesk_config,
            ..Default::default()
        }),
        log_id: cfg.id.to_string(),
    })
}

fn to_proto_scsi_disk(d: &DiskAttachment) -> vmservice::ScsiDisk {
    let disk_type = if d.kind == DiskKind::Dvd {
        vmservice::DiskType::ScsiDiskTypePhysical as i32
    } else {
        match d.format {
            DiskFormat::Vhdx => vmservice::DiskType::ScsiDiskTypeVhdx as i32,
            DiskFormat::Raw | DiskFormat::Iso => vmservice::DiskType::ScsiDiskTypePhysical as i32,
        }
    };
    vmservice::ScsiDisk {
        controller: 0,
        lun: d.lun,
        host_path: d.path.to_string_lossy().into_owned(),
        r#type: disk_type,
        read_only: d.read_only || d.kind == DiskKind::Dvd,
        is_dvd: d.kind == DiskKind::Dvd,
    }
}

fn to_proto_port_config(pf: &PortForward) -> vmservice::PortConfig {
    vmservice::PortConfig {
        host_port: pf.host_port as u32,
        guest_port: pf.guest_port as u32,
        protocol: match pf.protocol {
            IpProto::Tcp => vmservice::IpProtocol::Tcp as i32,
            IpProto::Udp => vmservice::IpProtocol::Udp as i32,
        },
        host_address: pf.host_addr.clone(),
    }
}

fn to_proto_nic_config(nic: &NicAttachment) -> vmservice::NicConfig {
    let backend = match &nic.backend {
        NicBackendConfig::Consomme {
            cidr,
            port_forwards,
            internal_dns,
            allow_host_local_access,
        } => vmservice::nic_config::Backend::Consomme(vmservice::ConsommeBackend {
            cidr: cidr.clone().unwrap_or_default(),
            ports: port_forwards.iter().map(to_proto_port_config).collect(),
            internal_dns: Some(*internal_dns),
            allow_host_local_access: *allow_host_local_access,
            ..Default::default()
        }),
        NicBackendConfig::VmSwitch { switch_id, .. } => {
            vmservice::nic_config::Backend::Dio(vmservice::DioBackend {
                switch_id: switch_id.clone(),
                port_id: String::new(),
            })
        }
        NicBackendConfig::Tap { name } => {
            vmservice::nic_config::Backend::Tap(vmservice::TapBackend {
                source: Some(vmservice::tap_backend::Source::Name(name.clone())),
            })
        }
    };
    vmservice::NicConfig {
        nic_id: nic.id.to_string(),
        mac_address: nic.mac_address.clone(),
        nic_name: nic.name.clone(),
        frontend: vmservice::NicFrontend::Vmbus as i32,
        backend: Some(backend),
        ..Default::default()
    }
}

fn read_log_tail(path: &Path, tail: usize) -> anyhow::Result<String> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut ring = std::collections::VecDeque::with_capacity(tail);
    for line in reader.lines() {
        let line = line?;
        if ring.len() == tail {
            ring.pop_front();
        }
        ring.push_back(line);
    }
    Ok(ring.into_iter().collect::<Vec<_>>().join("\n"))
}

#[cfg(windows)]
fn discover_windows_switches() -> Vec<VirtualSwitchInfo> {
    let mut out = vec![VirtualSwitchInfo {
        id: "c08cb7b8-9b3c-408e-8e30-5e16a3aeb444".to_string(),
        name: "Default Switch".to_string(),
        kind: "vmswitch".to_string(),
        description: "Windows Hyper-V Default Switch (HCN)".to_string(),
    }];

    if let Ok(output) = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-VMSwitch | Select-Object @{n='Id';e={$_.Id.ToString()}},Name,SwitchType | ConvertTo-Json -Compress",
        ])
        .output()
        && output.status.success()
        && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
    {
        let arr = match val {
            serde_json::Value::Array(items) => items,
            obj @ serde_json::Value::Object(_) => vec![obj],
            _ => Vec::new(),
        };
        for item in arr {
            if let (Some(id), Some(name)) = (item["Id"].as_str(), item["Name"].as_str()) {
                let id_lower = id.to_ascii_lowercase();
                if !out.iter().any(|s| s.id.eq_ignore_ascii_case(&id_lower)) {
                    out.push(VirtualSwitchInfo {
                        id: id_lower,
                        name: name.to_string(),
                        kind: "vmswitch".to_string(),
                        description: format!(
                            "Hyper-V Virtual Switch ({})",
                            item["SwitchType"].as_str().unwrap_or("Unknown")
                        ),
                    });
                }
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn discover_linux_tap_interfaces() -> Vec<VirtualSwitchInfo> {
    discover_linux_tap_interfaces_in(Path::new("/sys/class/net"))
}

fn discover_linux_tap_interfaces_in(sys_class_net: &Path) -> Vec<VirtualSwitchInfo> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(sys_class_net) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(info) = parse_linux_tap_in_dir(&entry.path(), &name) {
                out.push(info);
            }
        }
    }
    out
}

fn parse_linux_tap_in_dir(iface_path: &Path, name: &str) -> Option<VirtualSwitchInfo> {
    const IFF_TAP: u32 = 0x0002;
    let tun_flags_raw = fs::read_to_string(iface_path.join("tun_flags")).ok()?;
    let trimmed = tun_flags_raw
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let flags = u32::from_str_radix(trimmed, 16).ok()?;
    if (flags & IFF_TAP) == 0 {
        return None;
    }
    let master_bridge = fs::read_link(iface_path.join("master"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let description = match master_bridge {
        Some(bridge) => format!("Linux persistent TAP interface (attached to bridge '{bridge}')"),
        None => "Linux persistent TAP interface".to_string(),
    };
    Some(VirtualSwitchInfo {
        id: name.to_string(),
        name: name.to_string(),
        kind: "tap".to_string(),
        description,
    })
}

async fn run_cancellable_rpc<F, T>(
    cancel_rx: &mut watch::Receiver<bool>,
    fut: F,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    if *cancel_rx.borrow() {
        bail!("VM startup cancelled by stop request");
    }
    tokio::select! {
        res = fut => res,
        _ = cancel_rx.changed() => {
            bail!("VM startup cancelled by stop request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_switch_discovery_filters_bridges_and_tun_devices() {
        let dir = tempfile::tempdir().unwrap();
        let net = dir.path();

        // Linux bridge -> must NOT appear
        fs::create_dir_all(net.join("br0").join("bridge")).unwrap();

        // Layer-3 TUN device (IFF_TUN = 0x0001, no IFF_TAP) -> must NOT appear
        fs::create_dir_all(net.join("tun0")).unwrap();
        fs::write(net.join("tun0").join("tun_flags"), "0x1001\n").unwrap();

        // Persistent TAP attached to bridge br0 (IFF_TAP = 0x0002) -> MUST appear
        fs::create_dir_all(net.join("tap0")).unwrap();
        fs::write(net.join("tap0").join("tun_flags"), "0x1002\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(net.join("br0"), net.join("tap0").join("master")).unwrap();

        let switches = discover_linux_tap_interfaces_in(net);
        assert_eq!(switches.len(), 1);
        assert_eq!(switches[0].id, "tap0");
        assert_eq!(switches[0].kind, "tap");
        #[cfg(unix)]
        assert!(switches[0].description.contains("br0"));
    }
}
