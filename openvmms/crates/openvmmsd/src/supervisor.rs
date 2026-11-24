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
use openvmms_core::models::HostConfigPatch;
use openvmms_core::models::HostInfo;
use openvmms_core::models::IpProto;
use openvmms_core::models::LiveVmProperties;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::NicAttachment;
use openvmms_core::models::NicBackendConfig;
use openvmms_core::models::OrphanedPortForward;
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
use openvmms_core::models::VmValidationOptions;
use openvmms_core::models::resolve_host_paths;
use openvmms_core::models::parse_loopback_listen_addr;
use openvmms_core::store::RegistryStore;
use openvmms_core::ttrpc::TtrpcClient;
use openvmms_core::ttrpc::WaitVmOutcome;
use openvmms_core::vmservice;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ApiErrorKind {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
}

pub(crate) fn err_not_found(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApiErrorKind::NotFound(msg.into()))
}

pub(crate) fn err_conflict(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApiErrorKind::Conflict(msg.into()))
}

pub(crate) fn err_bad_request(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApiErrorKind::BadRequest(msg.into()))
}

#[allow(dead_code)]
pub(crate) fn err_internal(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApiErrorKind::Internal(msg.into()))
}

#[derive(Clone)]
enum StopOutcome {
    Pending,
    Done(Option<VmSummary>),
}

struct ManagedVm {
    config: VmConfig,
    runtime: VmRuntimeState,
    child: Option<Child>,
    adopted_proc: Option<Arc<openvmms_core::process::AdoptedProcess>>,
    monitor_cancel: Option<watch::Sender<bool>>,
    monitor_handle: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
    start_cancel: Option<watch::Sender<bool>>,
    stop_wait: Option<watch::Receiver<StopOutcome>>,
    prepare_lock: Arc<tokio::sync::Mutex<()>>,
    power_op_lock: Arc<tokio::sync::Mutex<()>>,
    hotplug_lock: Arc<tokio::sync::Mutex<()>>,
    pending_disks: Vec<DiskAttachment>,
    pending_port_forwards: Vec<(Uuid, PortForward)>,
    orphaned_disks: Vec<DiskAttachment>,
    orphaned_port_forwards: Vec<OrphanedPortForward>,
    detach_on_drop: bool,
    deleting: bool,
}

impl Drop for ManagedVm {
    fn drop(&mut self) {
        if let Some(tx) = self.monitor_cancel.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.monitor_handle.take() {
            handle.abort();
        }
        if let Some(mut child) = self.child.take()
            && !self.detach_on_drop
        {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_runtime_files(
                self.runtime.rpc_socket_path.take(),
                self.runtime.shm_path.take(),
            );
        }
    }
}

struct ActiveListener {
    addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl ManagedVm {
    fn summary(&self) -> VmSummary {
        let mut runtime = self.runtime.clone();
        runtime.orphaned_disks = self.orphaned_disks.clone();
        runtime.orphaned_port_forwards = self.orphaned_port_forwards.clone();
        VmSummary {
            config: self.config.clone(),
            runtime,
        }
    }

    fn clear_pending_reservations(&mut self) -> bool {
        let had_orphans =
            !self.orphaned_disks.is_empty() || !self.orphaned_port_forwards.is_empty();
        self.pending_disks.clear();
        self.pending_port_forwards.clear();
        self.orphaned_disks.clear();
        self.orphaned_port_forwards.clear();
        had_orphans
    }

    fn active_rpc_socket(&self, action: &str) -> anyhow::Result<Option<PathBuf>> {
        if self.deleting {
            return Err(err_conflict(format!(
                "cannot {action} while VM '{}' is being deleted",
                self.config.name
            )));
        }
        match self.runtime.status {
            VmStatus::Starting | VmStatus::Stopping => Err(err_conflict(format!(
                "cannot {action} while VM '{}' is {}",
                self.config.name, self.runtime.status
            ))),
            VmStatus::Running | VmStatus::Paused => Ok(Some(
                self.runtime
                    .rpc_socket_path
                    .clone()
                    .ok_or_else(|| err_internal("VM is active but has no RPC socket"))?,
            )),
            VmStatus::Off | VmStatus::Error => Ok(None),
        }
    }
}

pub struct Supervisor {
    instance_id: Uuid,
    store: RegistryStore,
    _daemon_lock: fs::File,
    auth_token: String,
    initial_listen_addr: String,
    host_config: RwLock<HostConfig>,
    host_config_lock: tokio::sync::Mutex<()>,
    active_listener: std::sync::Mutex<Option<ActiveListener>>,
    draining_listeners: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    vms: RwLock<HashMap<Uuid, ManagedVm>>,
    events_tx: broadcast::Sender<DaemonEvent>,
    event_seq: AtomicU64,
    shutdown_watch: watch::Sender<bool>,
    shutting_down: AtomicBool,
    runtime_dir_changing: AtomicBool,
    stop_vms_on_shutdown: AtomicBool,
    in_flight_mutations: AtomicUsize,
    mutations_drained: Notify,
}

impl Supervisor {
    pub fn new(data_dir: &Path, listen_override: Option<&str>) -> anyhow::Result<Arc<Self>> {
        let store = RegistryStore::new(data_dir)?;
        let daemon_lock = store.try_acquire_daemon_lock()?;
        let host_config = store.load_or_init_host_config()?;
        let raw_listen_addr = listen_override
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| host_config.listen_addr.clone());
        let initial_listen_addr = parse_loopback_listen_addr(&raw_listen_addr)?.to_string();
        let auth_token = store.load_or_create_token()?;
        let loaded_vms = store.load_all_vms()?;
        store.cleanup_orphaned_vmgs();

        let mut vm_map = HashMap::new();
        for cfg in loaded_vms {
            vm_map.insert(
                cfg.id,
                ManagedVm {
                    config: cfg,
                    runtime: VmRuntimeState::default(),
                    child: None,
                    adopted_proc: None,
                    monitor_cancel: None,
                    monitor_handle: None,
                    generation: 0,
                    start_cancel: None,
                    stop_wait: None,
                    prepare_lock: Arc::new(tokio::sync::Mutex::new(())),
                    power_op_lock: Arc::new(tokio::sync::Mutex::new(())),
                    hotplug_lock: Arc::new(tokio::sync::Mutex::new(())),
                    pending_disks: Vec::new(),
                    pending_port_forwards: Vec::new(),
                    orphaned_disks: Vec::new(),
                    orphaned_port_forwards: Vec::new(),
                    detach_on_drop: false,
                    deleting: false,
                },
            );
        }

        let (events_tx, _) = broadcast::channel(256);
        let (shutdown_watch, _) = watch::channel(false);
        let supervisor = Arc::new(Self {
            instance_id: Uuid::new_v4(),
            store,
            _daemon_lock: daemon_lock,
            auth_token,
            initial_listen_addr,
            host_config: RwLock::new(host_config),
            host_config_lock: tokio::sync::Mutex::new(()),
            active_listener: std::sync::Mutex::new(None),
            draining_listeners: std::sync::Mutex::new(Vec::new()),
            vms: RwLock::new(vm_map),
            events_tx,
            event_seq: AtomicU64::new(1),
            shutdown_watch,
            shutting_down: AtomicBool::new(false),
            runtime_dir_changing: AtomicBool::new(false),
            stop_vms_on_shutdown: AtomicBool::new(false),
            in_flight_mutations: AtomicUsize::new(0),
            mutations_drained: Notify::new(),
        });

        Ok(supervisor)
    }

    fn next_revision(&self) -> u64 {
        self.event_seq.fetch_add(1, Ordering::SeqCst)
    }

    fn spawn_tracked_mutation<F>(self: &Arc<Self>, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        struct MutationFlightGuard(Arc<Supervisor>);
        impl Drop for MutationFlightGuard {
            fn drop(&mut self) {
                if self.0.in_flight_mutations.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.0.mutations_drained.notify_waiters();
                }
            }
        }
        self.in_flight_mutations.fetch_add(1, Ordering::AcqRel);
        let guard = MutationFlightGuard(Arc::clone(self));
        tokio::spawn(async move {
            let _guard = guard;
            fut.await
        })
    }

    pub async fn wait_for_in_flight_mutations(&self, timeout: Duration) {
        let wait = async {
            loop {
                let notified = self.mutations_drained.notified();
                if self.in_flight_mutations.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_err() {
            let remaining = self.in_flight_mutations.load(Ordering::Acquire);
            warn!(
                remaining,
                "timed out after {timeout:?} waiting for in-flight mutations before shutdown"
            );
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire) || *self.shutdown_watch.borrow()
    }

    /// Probes existing VM RPC sockets under `runtime_dir` and re-adopts running VMs.
    /// Must only be called after the daemon has validated config and bound its HTTP listener.
    pub fn adopt_running_vms(self: &Arc<Self>) {
        let runtime_dir = self.host_config().runtime_dir;
        let expected_bin = self.resolved_paths().openvmm_bin;
        let mut adopted_vms = Vec::new();
        {
            let mut guard = self.vms.write().unwrap();
            for vm in guard.values_mut() {
                if vm.generation != 0 || vm.runtime.status != VmStatus::Off {
                    continue;
                }
                let id = vm.config.id;
                let sock_path = runtime_dir.join(format!("{id}.sock"));
                if !sock_path.exists() {
                    continue;
                }
                let pid_path = sock_path.with_extension("pid");
                let shm_candidate = runtime_dir.join(format!("vram-{id}.shm"));
                let ttrpc_client = TtrpcClient::new(&sock_path);
                match ttrpc_client.properties_vm_blocking(Duration::from_secs(2)) {
                    Ok(props)
                        if props.state == vmservice::VmState::Running as i32
                            || props.state == vmservice::VmState::Paused as i32 =>
                    {
                        let verified = openvmms_core::process::open_verified_pid_or_socket_peer(
                            &pid_path,
                            Some(&sock_path),
                            expected_bin.as_deref(),
                        );
                        vm.runtime.status = if props.state == vmservice::VmState::Paused as i32 {
                            VmStatus::Paused
                        } else {
                            VmStatus::Running
                        };
                        vm.runtime.pid = verified.as_ref().map(|p| p.pid());
                        vm.adopted_proc = verified;
                        vm.runtime.rpc_socket_path = Some(sock_path.clone());
                        vm.runtime.fastdesk_endpoint = props
                            .fastdesk_listen_addr
                            .as_deref()
                            .map(normalize_fastdesk_endpoint);
                        vm.runtime.shm_path = props.fastdesk_shm_path.or_else(|| {
                            shm_candidate
                                .exists()
                                .then(|| shm_candidate.to_string_lossy().into_owned())
                        });
                        vm.runtime.revision = self.next_revision();
                        vm.generation = 1;
                        adopted_vms.push((id, sock_path));
                        info!(vm_id = %id, name = %vm.config.name, status = %vm.runtime.status, "re-adopted running OpenVMM VM");
                    }
                    Ok(_) => {
                        if !reclaim_or_verify_inactive_socket(
                            &sock_path,
                            &pid_path,
                            expected_bin.as_deref(),
                            &AtomicBool::new(false),
                        ) {
                            cleanup_runtime_files(
                                Some(sock_path),
                                shm_candidate
                                    .exists()
                                    .then(|| shm_candidate.to_string_lossy().into_owned()),
                            );
                        }
                    }
                    Err(_) => {
                        if !ttrpc_client.is_socket_accepting() {
                            cleanup_runtime_files(
                                Some(sock_path),
                                shm_candidate
                                    .exists()
                                    .then(|| shm_candidate.to_string_lossy().into_owned()),
                            );
                        } else {
                            let verified = openvmms_core::process::open_verified_pid_or_socket_peer(
                                &pid_path,
                                Some(&sock_path),
                                expected_bin.as_deref(),
                            );
                            vm.runtime.status = VmStatus::Running;
                            vm.runtime.pid = verified.as_ref().map(|p| p.pid());
                            vm.adopted_proc = verified;
                            vm.runtime.rpc_socket_path = Some(sock_path.clone());
                            vm.runtime.shm_path = shm_candidate
                                .exists()
                                .then(|| shm_candidate.to_string_lossy().into_owned());
                            vm.runtime.revision = self.next_revision();
                            vm.generation = 1;
                            adopted_vms.push((id, sock_path));
                            warn!(
                                vm_id = %id,
                                name = %vm.config.name,
                                "re-adopted running OpenVMM VM (properties probe failed; monitor will refresh)"
                            );
                        }
                    }
                }
            }
        }

        if !adopted_vms.is_empty()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            for (id, sock_path) in adopted_vms {
                let (cancel_tx, cancel_rx) = watch::channel(false);
                let sup = Arc::clone(self);
                let monitor = handle.spawn(async move {
                    sup.monitor_vm(id, 1, sock_path, cancel_rx).await;
                });
                if let Some(vm) = self.vms.write().unwrap().get_mut(&id)
                    && vm.generation == 1
                {
                    vm.monitor_cancel = Some(cancel_tx);
                    vm.monitor_handle = Some(monitor);
                } else {
                    let _ = cancel_tx.send(true);
                    monitor.abort();
                }
            }
        }
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn listen_addr(&self) -> String {
        if let Some(active) = self.active_listener.lock().unwrap().as_ref() {
            return active.addr.to_string();
        }
        self.initial_listen_addr.clone()
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
        {
            let guard = self.vms.write().unwrap();
            self.shutting_down.store(true, Ordering::Release);
            for vm in guard.values() {
                if vm.runtime.status == VmStatus::Starting
                    && let Some(tx) = vm.start_cancel.as_ref()
                {
                    let _ = tx.send(true);
                }
            }
        }
        let _ = self.events_tx.send(DaemonEvent::DaemonStopping);
        let _ = self.shutdown_watch.send(true);
    }

    pub fn request_shutdown(&self) {
        self.request_shutdown_with_options(false);
    }

    pub fn request_shutdown_with_options(&self, stop_vms: bool) {
        if stop_vms {
            self.stop_vms_on_shutdown.store(true, Ordering::SeqCst);
        }
        self.mark_shutting_down();
    }

    pub fn should_stop_vms_on_shutdown(&self) -> bool {
        self.stop_vms_on_shutdown.load(Ordering::SeqCst)
    }

    pub fn clear_runtime_listen_addr(&self) {
        self.store.clear_runtime_listen_addr();
    }

    pub async fn wait_for_shutdown_signal(&self) {
        let mut rx = self.shutdown_watch.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
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
            listen_addr: self.listen_addr(),
            resolved_paths,
            daemon_instance_id: Some(self.instance_id),
            event_revision: self.event_seq.load(Ordering::SeqCst).saturating_sub(1),
        }
    }

    /// Spawns an HTTP API server on `listener` and shuts down any previously active listener.
    pub async fn serve_on_listener(
        self: &Arc<Self>,
        listener: tokio::net::TcpListener,
    ) -> anyhow::Result<SocketAddr> {
        let bound_addr = listener.local_addr()?;
        let store = self.store.clone();
        let addr_str = bound_addr.to_string();
        tokio::task::spawn_blocking(move || store.write_runtime_listen_addr(&addr_str))
            .await
            .map_err(|e| anyhow::anyhow!("writing runtime listen address failed: {e}"))??;
        let app = crate::server::build_router(Arc::clone(self), self.auth_token.clone(), bound_addr);
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
        const LISTENER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
        let active = self.active_listener.lock().unwrap().take();
        if let Some(mut listener) = active {
            let _ = listener.stop_tx.send(true);
            if tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut listener.handle)
                .await
                .is_err()
            {
                warn!(
                    "active HTTP listener did not drain within {LISTENER_SHUTDOWN_TIMEOUT:?}; aborting"
                );
                listener.handle.abort();
                let _ = listener.handle.await;
            }
        }
        let drains = {
            let mut guard = self.draining_listeners.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for mut handle in drains {
            if tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                warn!(
                    "draining HTTP listener did not exit within {LISTENER_SHUTDOWN_TIMEOUT:?}; aborting"
                );
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    pub async fn update_host_config(
        self: &Arc<Self>,
        new_cfg: HostConfig,
    ) -> anyhow::Result<HostConfig> {
        let sup = Arc::clone(self);
        self.spawn_tracked_mutation(async move {
            let _lock = sup.host_config_lock.lock().await;
            sup.update_host_config_locked(new_cfg, false).await
        })
        .await
        .map_err(|e| anyhow::anyhow!("update_host_config task failed: {e}"))?
    }

    pub async fn patch_host_config(
        self: &Arc<Self>,
        patch: HostConfigPatch,
    ) -> anyhow::Result<HostConfig> {
        let sup = Arc::clone(self);
        self.spawn_tracked_mutation(async move {
            let _lock = sup.host_config_lock.lock().await;
            let listen_addr_explicit = patch.listen_addr.is_some();
            let mut live = sup.host_config();
            patch.apply_to(&mut live);
            sup.update_host_config_locked(live, listen_addr_explicit)
                .await
        })
        .await
        .map_err(|e| anyhow::anyhow!("patch_host_config task failed: {e}"))?
    }

    async fn update_host_config_locked(
        self: &Arc<Self>,
        mut new_cfg: HostConfig,
        listen_addr_explicit: bool,
    ) -> anyhow::Result<HostConfig> {
        let current_cfg = self.host_config();
        for path_opt in [
            &mut new_cfg.openvmm_bin,
            &mut new_cfg.fastdesk_viewer_bin,
            &mut new_cfg.openvmm_img_bin,
            &mut new_cfg.uefi_firmware_path,
        ] {
            if path_opt
                .as_ref()
                .is_some_and(|p| p.as_os_str().is_empty())
            {
                *path_opt = None;
            }
        }
        if new_cfg.data_dir.as_os_str().is_empty() {
            new_cfg.data_dir = self.store.data_dir().to_path_buf();
        }
        if new_cfg.runtime_dir.as_os_str().is_empty() {
            new_cfg.runtime_dir = openvmms_core::models::default_runtime_dir(&new_cfg.data_dir);
        }
        if new_cfg.data_dir != current_cfg.data_dir {
            return Err(err_bad_request(
                "cannot change data_dir over the API; restart openvmmsd with --data-dir instead",
            ));
        }
        openvmms_core::models::validate_runtime_dir(&new_cfg.runtime_dir)
            .map_err(|e| err_bad_request(format!("{e:#}")))?;
        struct ResetRuntimeDirChanging<'a>(&'a AtomicBool);
        impl Drop for ResetRuntimeDirChanging<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        let changing_runtime_dir = new_cfg.runtime_dir != current_cfg.runtime_dir;
        let _runtime_dir_guard = if changing_runtime_dir {
            let guard = self.vms.write().unwrap();
            let has_active_vms = guard
                .values()
                .any(|vm| !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error));
            if has_active_vms {
                return Err(err_conflict(
                    "cannot change runtime_dir while VMs are active; stop all running VMs first",
                ));
            }
            self.runtime_dir_changing.store(true, Ordering::Release);
            Some(ResetRuntimeDirChanging(&self.runtime_dir_changing))
        } else {
            None
        };

        let target_addr = parse_loopback_listen_addr(&new_cfg.listen_addr)
            .map_err(|e| err_bad_request(format!("{e:#}")))?;

        let configured_addr = target_addr.to_string();
        let active_addr = self
            .active_listener
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.addr);
        let listen_changed = if listen_addr_explicit {
            active_addr.is_some_and(|active| active != target_addr)
                || (active_addr.is_none()
                    && (configured_addr != current_cfg.listen_addr || target_addr.port() == 0))
        } else {
            configured_addr != current_cfg.listen_addr
        };
        let new_listener = if listen_changed && active_addr.is_some() && active_addr != Some(target_addr)
        {
            let listener = tokio::net::TcpListener::bind(target_addr)
                .await
                .map_err(|err| match err.kind() {
                    std::io::ErrorKind::AddrInUse => err_conflict(format!(
                        "listen address {target_addr} is already in use: {err}"
                    )),
                    std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput => err_bad_request(format!(
                        "cannot bind openvmmsd HTTP server on {target_addr}: {err}"
                    )),
                    _ => anyhow::Error::new(err)
                        .context(format!("binding openvmmsd HTTP server on {target_addr}")),
                })?;
            Some(listener)
        } else {
            None
        };
        new_cfg.listen_addr = configured_addr;

        // Persist config.toml BEFORE updating in-memory host_config or switching the live listener
        // so a failed save leaves the daemon serving on the existing port matching config.toml.
        let store = self.store.clone();
        let cfg_to_save = new_cfg.clone();
        tokio::task::spawn_blocking(move || store.save_host_config(&cfg_to_save))
            .await
            .map_err(|e| anyhow::anyhow!("saving host config task failed: {e}"))??;

        if changing_runtime_dir {
            let _vms_guard = self.vms.write().unwrap();
            *self.host_config.write().unwrap() = new_cfg.clone();
        } else {
            *self.host_config.write().unwrap() = new_cfg.clone();
        }

        if let Some(listener) = new_listener
            && let Err(err) = self.serve_on_listener(listener).await
        {
            if changing_runtime_dir {
                let _vms_guard = self.vms.write().unwrap();
                *self.host_config.write().unwrap() = current_cfg.clone();
            } else {
                *self.host_config.write().unwrap() = current_cfg.clone();
            }
            self.rollback_host_config(current_cfg).await;
            return Err(err);
        }
        drop(_runtime_dir_guard);
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
                return Err(err_conflict(format!(
                    "multiple VMs share name '{id_or_name}'; specify UUID"
                )));
            }
            return Ok(first);
        }
        Err(err_not_found(format!("VM not found: {id_or_name}")))
    }

    pub fn list_vms(&self) -> Vec<VmSummary> {
        self.list_vms_with_revision().0
    }

    pub fn list_vms_with_revision(&self) -> (Vec<VmSummary>, u64) {
        let (mut items, event_revision) = {
            let guard = self.vms.read().unwrap();
            let rev = self.event_seq.load(Ordering::SeqCst).saturating_sub(1);
            let items: Vec<VmSummary> = guard.values().map(ManagedVm::summary).collect();
            (items, rev)
        };
        items.sort_by(|a, b| {
            a.config
                .name
                .to_lowercase()
                .cmp(&b.config.name.to_lowercase())
                .then(a.config.id.cmp(&b.config.id))
        });
        (items, event_revision)
    }

    pub fn get_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let guard = self.vms.read().unwrap();
        Ok(guard
            .get(&id)
            .ok_or_else(|| err_not_found("VM not found"))?
            .summary())
    }

    pub async fn get_vm_with_live_properties(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let (status, sock_path, rpc_gen) = {
            let guard = self.vms.read().unwrap();
            let vm = guard
                .get(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            (
                vm.runtime.status,
                vm.runtime.rpc_socket_path.clone(),
                vm.generation,
            )
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
            if let Some(vm) = guard.get_mut(&id)
                && vm.generation == rpc_gen
                && matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused)
            {
                vm.runtime.live_properties = Some(live);
                vm.runtime.revision = self.next_revision();
                return Ok(vm.summary());
            }
        }

        self.get_vm(&id.to_string())
    }

    pub fn create_vm(&self, payload: CreateVmPayload) -> anyhow::Result<VmSummary> {
        self.create_vm_cancellable(payload, &AtomicBool::new(false))
    }

    pub fn create_vm_cancellable(
        &self,
        payload: CreateVmPayload,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<VmSummary> {
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            bail!("cancelled: daemon is shutting down");
        }
        let trimmed_name = payload.name.trim().to_string();
        openvmms_core::models::validate_vm_name(&trimmed_name)
            .map_err(|e| err_bad_request(format!("{e:#}")))?;

        {
            let guard = self.vms.read().unwrap();
            if guard
                .values()
                .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
            {
                return Err(err_conflict(format!(
                    "a VM named '{trimmed_name}' already exists"
                )));
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
        if let Some(tpm) = payload.tpm_enabled {
            cfg.tpm_enabled = tpm;
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
            return Err(err_bad_request(format!(
                "ISO image not found: {}",
                iso_path.display()
            )));
        }

        let host_cfg = self.host_config();
        let paths = resolve_host_paths(&host_cfg);

        for mut disk_att in payload.disks {
            if disk_att.kind == DiskKind::Dvd
                || disk_att.format == DiskFormat::Iso
                || DiskFormat::from_path(&disk_att.path) == DiskFormat::Iso
            {
                disk_att.kind = DiskKind::Dvd;
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

        cfg.normalize();
        cfg.validate().map_err(|e| err_bad_request(format!("{e:#}")))?;
        for d in &cfg.disks {
            ensure_disk_format_safe(&d.path, d.format, false, true)?;
        }
        let cfg_disk_paths: Vec<PathBuf> = cfg.disks.iter().map(|d| d.path.clone()).collect();
        let all_paths = {
            let guard = self.vms.read().unwrap();
            collect_all_vm_disk_paths(&guard, &cfg_disk_paths)
        };
        let mut path_keys = precompute_disk_path_keys(all_paths);
        for attempt in 0..=MAX_PATH_KEY_RETRIES {
            let missing = {
                let guard = self.vms.read().unwrap();
                if guard
                    .values()
                    .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
                {
                    return Err(err_conflict(format!(
                        "a VM named '{trimmed_name}' already exists"
                    )));
                }
                let missing = collect_missing_disk_path_keys(&guard, None, &cfg.disks, &path_keys);
                if missing.is_empty() {
                    ensure_disks_not_conflicting_with_keys(&guard, None, &cfg.disks, &path_keys)?;
                    break;
                }
                missing
            };
            if attempt == MAX_PATH_KEY_RETRIES {
                return Err(err_conflict(
                    "concurrent disk configuration changes while creating VM; please retry",
                ));
            }
            path_keys.extend(precompute_disk_path_keys(missing));
        }

        if let Some(ref disk_req) = pending_disk_req {
            disk::validate_disk_path_for_format(&disk_req.path, disk_req.format)
                .map_err(|e| err_bad_request(format!("{e:#}")))?;
            if disk_req.format == DiskFormat::Vhdx {
                validate_explicit_path(
                    host_cfg.openvmm_img_bin.as_deref(),
                    paths.openvmm_img_bin.as_deref(),
                    "openvmm-img binary",
                )?;
            }
        }

        let created_disk_path = if let Some(disk_req) = pending_disk_req {
            if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
                bail!("cancelled: daemon is shutting down");
            }
            let info = disk::create_disk_cancellable(
                &disk_req,
                paths.openvmm_img_bin.as_deref(),
                cancelled,
            )
            .map_err(|e| err_bad_request(format!("{e:#}")))?;
            Some(info.path)
        } else {
            None
        };

        let persist_res: anyhow::Result<VmSummary> = (|| {
            if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
                return Err(err_bad_request("VM creation cancelled"));
            }
            let post_create_paths = {
                let guard = self.vms.read().unwrap();
                if guard
                    .values()
                    .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
                {
                    return Err(err_conflict(format!(
                        "a VM named '{trimmed_name}' already exists"
                    )));
                }
                collect_all_vm_disk_paths(&guard, &cfg_disk_paths)
            };
            let mut post_keys = precompute_disk_path_keys(post_create_paths);
            for attempt in 0..=MAX_PATH_KEY_RETRIES {
                let missing = {
                    let guard = self.vms.read().unwrap();
                    let missing =
                        collect_missing_disk_path_keys(&guard, None, &cfg.disks, &post_keys);
                    if missing.is_empty() {
                        ensure_disks_not_conflicting_with_keys(
                            &guard,
                            None,
                            &cfg.disks,
                            &post_keys,
                        )?;
                        break;
                    }
                    missing
                };
                if attempt == MAX_PATH_KEY_RETRIES {
                    return Err(err_conflict(
                        "concurrent disk configuration changes while creating VM; please retry",
                    ));
                }
                post_keys.extend(precompute_disk_path_keys(missing));
            }

            // Persist vm.toml outside the global `vms` lock.
            self.store.save_vm(&cfg)?;

            for attempt in 0..=MAX_PATH_KEY_RETRIES {
                let missing = {
                    let mut guard = self.vms.write().unwrap();
                    if guard
                        .values()
                        .any(|m| m.config.name.eq_ignore_ascii_case(&trimmed_name))
                    {
                        drop(guard);
                        let _ = self.store.delete_vm_dir(probe_id);
                        return Err(err_conflict(format!(
                            "a VM named '{trimmed_name}' already exists"
                        )));
                    }
                    let missing =
                        collect_missing_disk_path_keys(&guard, None, &cfg.disks, &post_keys);
                    if missing.is_empty() {
                        if let Err(err) = ensure_disks_not_conflicting_with_keys(
                            &guard,
                            None,
                            &cfg.disks,
                            &post_keys,
                        ) {
                            drop(guard);
                            let _ = self.store.delete_vm_dir(probe_id);
                            return Err(err);
                        }

                        let managed = ManagedVm {
                            config: cfg,
                            runtime: VmRuntimeState {
                                revision: self.next_revision(),
                                ..Default::default()
                            },
                            child: None,
                            adopted_proc: None,
                            monitor_cancel: None,
                            monitor_handle: None,
                            generation: 0,
                            start_cancel: None,
                            stop_wait: None,
                            prepare_lock: Arc::new(tokio::sync::Mutex::new(())),
                            power_op_lock: Arc::new(tokio::sync::Mutex::new(())),
                            hotplug_lock: Arc::new(tokio::sync::Mutex::new(())),
                            pending_disks: Vec::new(),
                            pending_port_forwards: Vec::new(),
                            orphaned_disks: Vec::new(),
                            orphaned_port_forwards: Vec::new(),
                            detach_on_drop: false,
                            deleting: false,
                        };
                        let summary = managed.summary();
                        guard.insert(summary.config.id, managed);
                        return Ok(summary);
                    }
                    missing
                };
                if attempt == MAX_PATH_KEY_RETRIES {
                    let _ = self.store.delete_vm_dir(probe_id);
                    return Err(err_conflict(
                        "concurrent disk configuration changes while creating VM; please retry",
                    ));
                }
                post_keys.extend(precompute_disk_path_keys(missing));
            }
            unreachable!()
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

    pub async fn update_vm(
        self: &Arc<Self>,
        id_or_name: &str,
        payload: UpdateVmPayload,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move { sup.update_vm_inner(&id_or_name, payload).await })
            .await
            .map_err(|e| anyhow::anyhow!("update_vm task failed: {e}"))?
    }

    async fn update_vm_inner(
        &self,
        id_or_name: &str,
        payload: UpdateVmPayload,
    ) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;

        let hardware_change = payload.vcpus.is_some()
            || payload.memory_mb.is_some()
            || payload.nested_virt.is_some()
            || payload.uefi_firmware.is_some()
            || payload.secure_boot_enabled.is_some()
            || payload.secure_boot_template.is_some()
            || payload.tpm_enabled.is_some()
            || payload.fastdesk.is_some()
            || payload.disks.is_some()
            || payload.nics.is_some();

        if let Some(new_name) = &payload.name {
            let trimmed = new_name.trim();
            openvmms_core::models::validate_vm_name(trimmed)
                .map_err(|e| err_bad_request(format!("{e:#}")))?;
        }

        let disks_updated = payload.disks.is_some();
        let validation_opts = VmValidationOptions::for_update(&payload);
        let (prev_cfg, updated, snap_gen, all_disk_paths) = {
            let guard = self.vms.read().unwrap();
            let vm = guard
                .get(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            if vm.deleting {
                return Err(err_conflict(format!(
                    "cannot update VM '{}' while it is being deleted",
                    vm.config.name
                )));
            }
            if hardware_change && !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                return Err(err_conflict(format!(
                    "cannot edit hardware configuration while VM '{}' is {}",
                    vm.config.name, vm.runtime.status
                )));
            }
            if let Some(new_name) = &payload.name {
                let trimmed = new_name.trim();
                if !trimmed.eq_ignore_ascii_case(&vm.config.name)
                    && guard
                        .values()
                        .any(|m| m.config.id != id && m.config.name.eq_ignore_ascii_case(trimmed))
                {
                    return Err(err_conflict(format!(
                        "another VM is already named '{trimmed}'"
                    )));
                }
            }

            let prev_cfg = vm.config.clone();
            let mut updated = prev_cfg.clone();
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
            if let Some(tpm) = payload.tpm_enabled {
                updated.tpm_enabled = tpm;
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
            updated.normalize();
            updated.updated_at = Utc::now();

            let extra: Vec<PathBuf> = if disks_updated {
                updated.disks.iter().map(|d| d.path.clone()).collect()
            } else {
                Vec::new()
            };
            let all_paths = if disks_updated {
                collect_all_vm_disk_paths(&guard, &extra)
            } else {
                Vec::new()
            };
            (prev_cfg, updated, vm.generation, all_paths)
        };

        let updated_to_validate = updated.clone();
        let mut path_keys = tokio::task::spawn_blocking(move || {
            updated_to_validate
                .validate_with_field_options(validation_opts)
                .map_err(|e| err_bad_request(format!("{e:#}")))?;
            if disks_updated {
                for d in &updated_to_validate.disks {
                    ensure_disk_format_safe(&d.path, d.format, true, true)?;
                }
                Ok::<_, anyhow::Error>(precompute_disk_path_keys(all_disk_paths))
            } else {
                Ok(HashMap::new())
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("update_vm validation task failed: {e}"))??;

        if disks_updated {
            for attempt in 0..=MAX_PATH_KEY_RETRIES {
                let missing = {
                    let guard = self.vms.read().unwrap();
                    let missing = collect_missing_disk_path_keys(
                        &guard,
                        Some(id),
                        &updated.disks,
                        &path_keys,
                    );
                    if missing.is_empty() {
                        ensure_disks_not_conflicting_with_keys(
                            &guard,
                            Some(id),
                            &updated.disks,
                            &path_keys,
                        )?;
                        break;
                    }
                    missing
                };
                if attempt == MAX_PATH_KEY_RETRIES {
                    return Err(err_conflict(
                        "concurrent disk configuration changes while updating VM; please retry",
                    ));
                }
                extend_disk_path_keys(&mut path_keys, missing, "update_vm").await?;
            }
        }

        let prev_name = prev_cfg.name.clone();
        let keys_opt = if disks_updated {
            Some(&mut path_keys)
        } else {
            None
        };
        let summary = self
            .commit_offline_vm_mutation(
                id,
                "update_vm",
                "update VM",
                prev_cfg,
                updated,
                keys_opt,
                |guard, vm, updated_cfg, keys| {
                    let name_changed = !updated_cfg.name.eq_ignore_ascii_case(&prev_name);
                    if name_changed
                        && guard.values().any(|m| {
                            m.config.id != id && m.config.name.eq_ignore_ascii_case(&updated_cfg.name)
                        })
                    {
                        return Err(err_conflict(format!(
                            "another VM is already named '{}'",
                            updated_cfg.name
                        )));
                    }
                    if hardware_change
                        && (vm.generation != snap_gen
                            || !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error))
                    {
                        return Err(err_conflict(format!(
                            "cannot edit hardware configuration while VM '{}' is {}",
                            vm.config.name, vm.runtime.status
                        )));
                    }
                    if disks_updated {
                        let missing = collect_missing_disk_path_keys(
                            guard,
                            Some(id),
                            &updated_cfg.disks,
                            keys,
                        );
                        if !missing.is_empty() {
                            return Ok(CommitCheckStep::NeedDiskKeys(missing));
                        }
                        ensure_disks_not_conflicting_with_keys(
                            guard,
                            Some(id),
                            &updated_cfg.disks,
                            keys,
                        )?;
                    }
                    Ok(CommitCheckStep::Ready)
                },
                |vm, updated_cfg| vm.config = updated_cfg,
            )
            .await?;

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn delete_vm(
        self: &Arc<Self>,
        id_or_name: &str,
        delete_disks: bool,
    ) -> anyhow::Result<()> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(
            async move { sup.delete_vm_inner(&id_or_name, delete_disks).await },
        )
        .await
        .map_err(|e| anyhow::anyhow!("delete_vm task failed: {e}"))?
    }

    async fn delete_vm_inner(
        &self,
        id_or_name: &str,
        delete_disks: bool,
    ) -> anyhow::Result<()> {
        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;

        let (vm_disks, all_paths) = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard
                .get(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            if vm.deleting {
                return Err(err_conflict(format!(
                    "VM '{}' is already being deleted",
                    vm.config.name
                )));
            }
            if !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                return Err(err_conflict(format!(
                    "cannot delete VM '{}' while status is {}; stop it first",
                    vm.config.name, vm.runtime.status
                )));
            }
            let vm_disks = if delete_disks {
                vm.config.disks.clone()
            } else {
                Vec::new()
            };
            let all_paths = if delete_disks {
                collect_all_vm_disk_paths(&guard, &[])
            } else {
                Vec::new()
            };
            guard.get_mut(&id).unwrap().deleting = true;
            (vm_disks, all_paths)
        };

        struct ClearDeletingGuard<'a> {
            vms: &'a RwLock<HashMap<Uuid, ManagedVm>>,
            id: Uuid,
            armed: bool,
        }

        impl Drop for ClearDeletingGuard<'_> {
            fn drop(&mut self) {
                if self.armed
                    && let Ok(mut guard) = self.vms.write()
                    && let Some(vm) = guard.get_mut(&self.id)
                {
                    vm.deleting = false;
                }
            }
        }

        let mut deleting_guard = ClearDeletingGuard {
            vms: &self.vms,
            id,
            armed: true,
        };

        // Remove persisted VM metadata outside the global `vms` lock so a store
        // failure preserves `vm.toml` and does not block unrelated API calls.
        let store = self.store.clone();
        let join_res = tokio::task::spawn_blocking(move || {
            let res = store.delete_vm_dir(id);
            let toml_absent = vm_toml_definitely_absent(&store.vm_config_path(id));
            (res, toml_absent)
        })
        .await;
        let (del_res, vm_toml_absent) = match join_res {
            Ok(pair) => pair,
            Err(join_err) => {
                let store = self.store.clone();
                let toml_absent = tokio::task::spawn_blocking(move || {
                    vm_toml_definitely_absent(&store.vm_config_path(id))
                })
                .await
                .unwrap_or_else(|_| vm_toml_definitely_absent(&self.store.vm_config_path(id)));
                (
                    Err(anyhow::anyhow!("delete_vm task failed: {join_err}")),
                    toml_absent,
                )
            }
        };
        let mut post_toml_err: Option<anyhow::Error> = None;
        if let Err(err) = del_res {
            if !vm_toml_absent {
                return Err(err);
            }
            // `vm.toml` was already removed before a post-`vm.toml` cleanup step failed
            // (`state.vmgs` removal is best-effort). Still run disk cleanup, drop the
            // in-memory ghost entry, and emit `VmDeleted` instead of stranding an
            // unpersisted VM or leaking owned disks.
            post_toml_err = Some(err);
        }

        // `vm.toml` is gone: keep the entry in `self.vms` with `deleting = true`
        // until disk file deletion completes so disk reservations remain intact.
        deleting_guard.armed = false;

        let mut disk_errors: Vec<String> = Vec::new();
        let disks_to_delete = if delete_disks && !vm_disks.is_empty() {
            match tokio::task::spawn_blocking(move || precompute_disk_path_keys(all_paths)).await {
                Ok(mut path_keys) => {
                    let mut candidates = Vec::new();
                    for attempt in 0..=MAX_PATH_KEY_RETRIES {
                        let missing = {
                            let guard = self.vms.read().unwrap();
                            let missing = collect_missing_disk_path_keys(
                                &guard,
                                Some(id),
                                &vm_disks,
                                &path_keys,
                            );
                            if missing.is_empty() {
                                let referenced_by_others: HashSet<PathBuf> = guard
                                    .values()
                                    .filter(|other| other.config.id != id)
                                    .flat_map(|other| {
                                        other
                                            .config
                                            .disks
                                            .iter()
                                            .chain(other.pending_disks.iter())
                                    })
                                    .filter_map(|d| path_keys.get(&d.path).cloned())
                                    .collect();
                                let mut seen = HashSet::new();
                                for d in &vm_disks {
                                    if d.kind == DiskKind::HardDisk
                                        && !d.read_only
                                        && let Some(key) = path_keys.get(&d.path)
                                    {
                                        if referenced_by_others.contains(key) {
                                            warn!(
                                                vm_id = %id,
                                                path = %d.path.display(),
                                                "refusing to delete disk referenced by another VM"
                                            );
                                        } else if seen.insert(key.clone()) {
                                            candidates.push(d.path.clone());
                                        }
                                    }
                                }
                                break;
                            }
                            missing
                        };
                        if attempt == MAX_PATH_KEY_RETRIES {
                            warn!(
                                vm_id = %id,
                                "exhausted retries resolving canonical disk keys during delete_vm; skipping disk file deletion"
                            );
                            disk_errors.push(
                                "exhausted retries resolving canonical disk keys during delete_vm"
                                    .to_string(),
                            );
                            break;
                        }
                        if let Err(err) =
                            extend_disk_path_keys(&mut path_keys, missing, "delete_vm").await
                        {
                            warn!(
                                vm_id = %id,
                                error = %err,
                                "delete_vm disk key task failed; skipping disk file deletion"
                            );
                            disk_errors.push(format!("resolving canonical disk keys: {err}"));
                            break;
                        }
                    }
                    candidates
                }
                Err(err) => {
                    warn!(
                        vm_id = %id,
                        error = %err,
                        "delete_vm disk key task failed; skipping disk file deletion"
                    );
                    disk_errors.push(format!("resolving canonical disk keys: {err}"));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        if !disks_to_delete.is_empty() {
            match tokio::task::spawn_blocking(move || {
                let mut errors = Vec::new();
                for path in disks_to_delete {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "failed to delete VM disk");
                            errors.push(format!("{}: {err}", path.display()));
                        }
                    }
                }
                errors
            })
            .await
            {
                Ok(errors) => disk_errors.extend(errors),
                Err(err) => {
                    warn!(vm_id = %id, error = %err, "delete_vm disk removal task failed");
                    disk_errors.push(format!("disk removal task failed: {err}"));
                }
            }
        }

        let revision = {
            let mut guard = self.vms.write().unwrap();
            guard.remove(&id);
            self.next_revision()
        };
        self.emit(DaemonEvent::VmDeleted { id, revision });
        match (post_toml_err, disk_errors.is_empty()) {
            (Some(err), true) => return Err(err),
            (Some(err), false) => {
                return Err(err_internal(format!(
                    "{err:#}; additionally failed to remove disk file(s): {}",
                    disk_errors.join("; ")
                )));
            }
            (None, false) => {
                return Err(err_internal(format!(
                    "VM deleted, but failed to remove disk file(s): {}",
                    disk_errors.join("; ")
                )));
            }
            (None, true) => {}
        }
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
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;
        let (cfg, host_cfg, paths, start_gen, mut cancel_rx, prepare_lock) = {
            let mut guard = self.vms.write().unwrap();
            if self.is_shutting_down() {
                return Err(err_conflict("cannot start VM while openvmmsd is shutting down"));
            }
            if self.runtime_dir_changing.load(Ordering::Acquire) {
                return Err(err_conflict(
                    "cannot start VM while host runtime_dir is being updated",
                ));
            }
            let vm_nics = {
                let vm = guard
                    .get(&id)
                    .ok_or_else(|| err_not_found("VM not found"))?;
                if vm.deleting {
                    return Err(err_conflict(format!(
                        "cannot start VM '{}' while it is being deleted",
                        vm.config.name
                    )));
                }
                if !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                    return Err(err_conflict(format!(
                        "VM '{}' is already {}",
                        vm.config.name, vm.runtime.status
                    )));
                }
                vm.config.nics.clone()
            };
            ensure_port_forwards_not_conflicting_with_active_vms(&guard, id, &vm_nics)?;
            let vm = guard.get_mut(&id).unwrap();
            vm.generation += 1;
            let start_gen = vm.generation;
            let (cancel_tx, cancel_rx) = watch::channel(false);
            vm.start_cancel = Some(cancel_tx);
            vm.detach_on_drop = false;
            vm.runtime.status = VmStatus::Starting;
            vm.runtime.last_error = None;
            vm.runtime.live_properties = None;
            vm.runtime.revision = self.next_revision();
            (
                vm.config.clone(),
                self.host_config(),
                self.resolved_paths(),
                start_gen,
                cancel_rx,
                Arc::clone(&vm.prepare_lock),
            )
        };
        drop(_hp_guard);

        self.emit_vm_snapshot(id);

        struct StartCleanupGuard {
            sup: Arc<Supervisor>,
            id: Uuid,
            start_gen: u64,
            armed: bool,
        }

        impl Drop for StartCleanupGuard {
            fn drop(&mut self) {
                if self.armed {
                    self.sup.cleanup_failed_start(
                        self.id,
                        self.start_gen,
                        "VM startup task aborted",
                    );
                }
            }
        }

        // Run startup in a detached task so an HTTP client disconnect (dropping the request future)
        // cannot orphan the VM in Starting state without cleanup.
        let sup = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut guard = StartCleanupGuard {
                sup: Arc::clone(&sup),
                id,
                start_gen,
                armed: true,
            };
            let result = sup
                .start_vm_inner(
                    id,
                    start_gen,
                    &mut cancel_rx,
                    prepare_lock,
                    cfg,
                    host_cfg,
                    paths,
                )
                .await;
            match result {
                Ok(summary) => {
                    guard.armed = false;
                    Ok(summary)
                }
                Err(err) => {
                    guard.armed = false;
                    sup.cleanup_failed_start(id, start_gen, &format!("{err:#}"));
                    Err(err)
                }
            }
        });

        match handle.await {
            Ok(res) => res,
            Err(join_err) => {
                self.cleanup_failed_start(
                    id,
                    start_gen,
                    &format!("VM startup task failed: {join_err}"),
                );
                Err(anyhow::anyhow!("VM startup task failed: {join_err}"))
            }
        }
    }

    async fn start_vm_inner(
        self: &Arc<Self>,
        id: Uuid,
        start_gen: u64,
        cancel_rx: &mut watch::Receiver<bool>,
        prepare_lock: Arc<tokio::sync::Mutex<()>>,
        cfg: VmConfig,
        host_cfg: HostConfig,
        paths: ResolvedHostPaths,
    ) -> anyhow::Result<VmSummary> {
        validate_explicit_path(
            host_cfg.openvmm_bin.as_deref(),
            paths.openvmm_bin.as_deref(),
            "openvmm binary",
        )?;
        let openvmm_bin = paths.openvmm_bin.ok_or_else(|| {
            err_bad_request(
                "openvmm binary not found; configure openvmm_bin in Host Settings or place openvmm next to openvmmsd",
            )
        })?;
        let vm_uefi_explicit = cfg
            .uefi_firmware
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty());
        let resolved_vm_uefi =
            vm_uefi_explicit.map(openvmms_core::models::resolve_explicit_file_path);
        if let Some(resolved) = &resolved_vm_uefi {
            validate_explicit_path(vm_uefi_explicit, resolved.as_deref(), "VM UEFI firmware")?;
        } else {
            validate_explicit_path(
                host_cfg.uefi_firmware_path.as_deref(),
                paths.uefi_firmware_path.as_deref(),
                "UEFI firmware",
            )?;
        }
        let uefi_path = resolved_vm_uefi
            .flatten()
            .or(paths.uefi_firmware_path)
            .ok_or_else(|| {
                err_bad_request(
                    "UEFI firmware (MSVM.fd) not found; configure uefi_firmware_path in Host Settings",
                )
            })?;
        openvmms_core::models::validate_runtime_dir(&host_cfg.runtime_dir)
            .map_err(|e| err_bad_request(format!("{e:#}")))?;

        let extra_paths: Vec<PathBuf> = cfg.disks.iter().map(|d| d.path.clone()).collect();
        let all_disk_paths = {
            let guard = self.vms.read().unwrap();
            collect_all_vm_disk_paths(&guard, &extra_paths)
        };
        let disks_to_check = cfg.disks.clone();
        let uefi_to_check = uefi_path.clone();
        let runtime_dir_to_prepare = host_cfg.runtime_dir.clone();
        let managed_default_runtime_dir =
            openvmms_core::models::default_runtime_dir(self.store.data_dir());

        let mut path_keys = tokio::task::spawn_blocking(move || {
            if !uefi_to_check.is_file() {
                return Err(err_bad_request(format!(
                    "UEFI firmware not found at {}",
                    uefi_to_check.display()
                )));
            }
            for d in &disks_to_check {
                if !d.path.is_file() {
                    return Err(err_bad_request(format!(
                        "attached disk/ISO not found: {}",
                        d.path.display()
                    )));
                }
                if !d.path.is_absolute() {
                    warn!(
                        vm_id = %id,
                        path = %d.path.display(),
                        "starting legacy VM with relative disk path"
                    );
                }
                ensure_disk_format_safe(&d.path, d.format, d.format != DiskFormat::Raw, false)?;
            }
            let path_keys = precompute_disk_path_keys(all_disk_paths);
            let existed_before = runtime_dir_to_prepare.exists();
            fs::create_dir_all(&runtime_dir_to_prepare).with_context(|| {
                format!("creating runtime dir {}", runtime_dir_to_prepare.display())
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let is_managed_default =
                    openvmms_core::models::normalize_path_key(&runtime_dir_to_prepare)
                        == openvmms_core::models::normalize_path_key(&managed_default_runtime_dir);
                if !existed_before || is_managed_default {
                    if let Err(err) = fs::set_permissions(
                        &runtime_dir_to_prepare,
                        fs::Permissions::from_mode(0o700),
                    ) {
                        warn!(
                            path = %runtime_dir_to_prepare.display(),
                            error = %err,
                            "failed to set 0700 permissions on runtime_dir"
                        );
                    }
                } else if let Ok(meta) = fs::metadata(&runtime_dir_to_prepare) {
                    let mode = meta.permissions().mode();
                    if (mode & 0o002) != 0 {
                        warn!(
                            path = %runtime_dir_to_prepare.display(),
                            mode = format!("{:04o}", mode & 0o7777),
                            "custom runtime_dir is world-writable; consider using a dedicated 0700 directory for VM sockets and SHM files"
                        );
                    }
                }
            }
            Ok(path_keys)
        })
        .await
        .map_err(|e| anyhow::anyhow!("VM startup preflight task failed: {e}"))??;

        for attempt in 0..=MAX_PATH_KEY_RETRIES {
            let missing = {
                let guard = self.vms.read().unwrap();
                let missing =
                    collect_missing_disk_path_keys(&guard, Some(id), &cfg.disks, &path_keys);
                if missing.is_empty() {
                    ensure_disks_not_conflicting_with_keys(
                        &guard,
                        Some(id),
                        &cfg.disks,
                        &path_keys,
                    )?;
                    break;
                }
                missing
            };
            if attempt == MAX_PATH_KEY_RETRIES {
                return Err(err_conflict(
                    "concurrent disk configuration changes while starting VM; please retry",
                ));
            }
            extend_disk_path_keys(&mut path_keys, missing, "start_vm").await?;
        }

        if *cancel_rx.borrow()
            || self.is_shutting_down()
            || !self.is_start_generation_active(id, start_gen)
        {
            bail!("VM startup cancelled by stop request");
        }

        let runtime_dir = &host_cfg.runtime_dir;

        let prepare_guard = prepare_lock.lock().await;
        if *cancel_rx.borrow()
            || self.is_shutting_down()
            || !self.is_start_generation_active(id, start_gen)
        {
            bail!("VM startup cancelled by stop request");
        }

        let sock_path = runtime_dir.join(format!("{id}.sock"));
        let pid_path = sock_path.with_extension("pid");
        if sock_path.exists() {
            let sock_check = sock_path.clone();
            let pid_check = pid_path.clone();
            let expected_bin = openvmm_bin.clone();
            let reclaim_cancel = Arc::new(AtomicBool::new(false));
            let reclaim_cancel_worker = Arc::clone(&reclaim_cancel);
            let mut reclaim_task = tokio::task::spawn_blocking(move || {
                reclaim_or_verify_inactive_socket(
                    &sock_check,
                    &pid_check,
                    Some(&expected_bin),
                    &reclaim_cancel_worker,
                )
            });
            let mut shutdown_rx = self.subscribe_shutdown();
            let still_alive = tokio::select! {
                res = &mut reclaim_task => {
                    res.context("checking existing OpenVMM RPC socket")?
                }
                _ = cancel_rx.changed() => {
                    reclaim_cancel.store(true, Ordering::Release);
                    let _ = reclaim_task.await;
                    bail!("VM startup cancelled by stop request");
                }
                _ = wait_until_true(&mut shutdown_rx) => {
                    reclaim_cancel.store(true, Ordering::Release);
                    let _ = reclaim_task.await;
                    bail!("VM startup cancelled by daemon shutdown");
                }
            };
            if still_alive {
                bail!(
                    "refusing to start VM '{}': existing openvmm instance on {} is still active",
                    cfg.name,
                    sock_path.display()
                );
            }
        }

        let log_path = self.store.vm_log_path(id);
        let stderr_log_path = log_path.with_extension("stderr.log");
        let (shm_path, stderr_file) = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM vanished")?;
            if *cancel_rx.borrow()
                || self.is_shutting_down()
                || vm.generation != start_gen
                || vm.runtime.status != VmStatus::Starting
            {
                bail!("VM startup cancelled by stop request");
            }
            let _ = fs::remove_file(&sock_path);
            let _ = fs::remove_file(&pid_path);

            let shm_path = if cfg.fastdesk.enabled && cfg.fastdesk.use_shm {
                let path = runtime_dir.join(format!("vram-{id}.shm"));
                prepare_shm_path(&path)?;
                Some(path)
            } else {
                None
            };

            if let Some(parent) = log_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let stderr_file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&stderr_log_path)
                .with_context(|| format!("opening {}", stderr_log_path.display()))?;

            vm.runtime.rpc_socket_path = Some(sock_path.clone());
            vm.runtime.shm_path = shm_path.as_ref().map(|p| p.to_string_lossy().into_owned());
            (shm_path, stderr_file)
        };

        let rpc_arg = format!("path={},transport=ttrpc", sock_path.display());
        let mut cmd = Command::new(&openvmm_bin);
        cmd.arg("--rpc")
            .arg(rpc_arg)
            .arg("--log-file")
            .arg(&log_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));

        #[cfg(unix)]
        {
            openvmms_core::process::configure_worker_command(&mut cmd);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", openvmm_bin.display()))?;
        let pid = child.id();

        {
            let mut guard = self.vms.write().unwrap();
            let vm = guard.get_mut(&id).context("VM vanished")?;
            if *cancel_rx.borrow()
                || self.is_shutting_down()
                || vm.generation != start_gen
                || vm.runtime.status != VmStatus::Starting
            {
                let _ = child.kill();
                let _ = child.wait();
                bail!("VM startup cancelled by stop request");
            }
            vm.child = Some(child);
            vm.adopted_proc = None;
            vm.runtime.pid = Some(pid);
            vm.runtime.rpc_socket_path = Some(sock_path.clone());
            vm.runtime.shm_path = shm_path.as_ref().map(|p| p.to_string_lossy().into_owned());
            openvmms_core::process::write_pid_file(&pid_path, pid)
                .with_context(|| format!("writing VM pid file {}", pid_path.display()))?;
        }
        drop(prepare_guard);

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
        if let Err(err) = run_cancellable_rpc(cancel_rx, client.create_vm(create_req)).await {
            let msg = format!("{err:#}");
            if cfg.tpm_enabled
                && (msg.contains("TpmDeviceHandle")
                    || msg.contains("no resolver")
                    || msg.to_ascii_lowercase().contains("tpm"))
            {
                return Err(err).context(
                    "OpenVMM CreateVM failed (vTPM 2.0 requires an openvmm binary built with --features tpm, or disable vTPM in VM settings)",
                );
            }
            return Err(err).context("OpenVMM CreateVM failed");
        }
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
            vm.runtime.revision = self.next_revision();

            let (mon_cancel_tx, mon_cancel_rx) = watch::channel(false);
            let sup = Arc::clone(self);
            let sock_clone = sock_path.clone();
            let monitor = tokio::spawn(async move {
                sup.monitor_vm(id, start_gen, sock_clone, mon_cancel_rx).await;
            });
            vm.monitor_cancel = Some(mon_cancel_tx);
            vm.monitor_handle = Some(monitor);
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
            if let Some(tx) = vm.monitor_cancel.take() {
                let _ = tx.send(true);
            }
            if let Some(handle) = vm.monitor_handle.take() {
                handle.abort();
            }
            if let Some(mut child) = vm.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            cleanup_runtime_files(vm.runtime.rpc_socket_path.take(), vm.runtime.shm_path.take());
            vm.adopted_proc = None;
            vm.runtime.pid = None;
            vm.runtime.started_at = None;
            vm.runtime.fastdesk_endpoint = None;
            vm.runtime.status = VmStatus::Error;
            vm.runtime.last_error = Some(err_msg.to_string());
            vm.clear_pending_reservations();
            vm.runtime.revision = self.next_revision();
            let summary = vm.summary();
            drop(guard);
            self.emit(DaemonEvent::VmUpdated { vm: summary });
        }
    }

    fn is_monitor_generation_active(&self, id: Uuid, run_gen: u64) -> bool {
        if *self.shutdown_watch.borrow() && !self.should_stop_vms_on_shutdown() {
            return false;
        }
        self.vms.read().unwrap().get(&id).is_some_and(|vm| {
            vm.generation == run_gen
                && !matches!(
                    vm.runtime.status,
                    VmStatus::Stopping | VmStatus::Off | VmStatus::Error
                )
        })
    }

    async fn is_vm_process_alive(&self, id: Uuid, run_gen: u64) -> bool {
        let adopted_proc = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                return false;
            };
            if vm.generation != run_gen {
                return false;
            }
            if let Some(child) = vm.child.as_mut() {
                return matches!(child.try_wait(), Ok(None));
            }
            vm.adopted_proc.clone()
        };
        if let Some(proc_handle) = adopted_proc {
            return tokio::task::spawn_blocking(move || proc_handle.is_alive())
                .await
                .unwrap_or(false);
        }
        false
    }

    fn sync_status_from_properties(
        &self,
        id: Uuid,
        run_gen: u64,
        props: &vmservice::PropertiesVmResponse,
    ) {
        let next_status = if props.state == vmservice::VmState::Paused as i32 {
            VmStatus::Paused
        } else if props.state == vmservice::VmState::Running as i32 {
            VmStatus::Running
        } else {
            return;
        };
        let next_endpoint = props
            .fastdesk_listen_addr
            .as_deref()
            .map(normalize_fastdesk_endpoint);

        let updated_summary = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                return;
            };
            if vm.generation != run_gen
                || matches!(vm.runtime.status, VmStatus::Stopping | VmStatus::Off | VmStatus::Error)
            {
                return;
            }
            let mut changed = false;
            if vm.runtime.status != next_status {
                vm.runtime.status = next_status;
                changed = true;
            }
            if vm.runtime.fastdesk_endpoint.is_none() && next_endpoint.is_some() {
                vm.runtime.fastdesk_endpoint = next_endpoint;
                changed = true;
            }
            if vm.runtime.shm_path.is_none()
                && let Some(shm) = &props.fastdesk_shm_path
            {
                vm.runtime.shm_path = Some(shm.clone());
                changed = true;
            }
            if changed {
                vm.runtime.revision = self.next_revision();
            }
            changed.then(|| vm.summary())
        };
        if let Some(summary) = updated_summary {
            self.emit(DaemonEvent::VmUpdated { vm: summary });
        }
    }

    async fn monitor_vm(
        self: Arc<Self>,
        id: Uuid,
        run_gen: u64,
        sock_path: PathBuf,
        mut cancel_rx: watch::Receiver<bool>,
    ) {
        let client = TtrpcClient::new(&sock_path);
        let (wait_res, mut props) = loop {
            if *cancel_rx.borrow() || !self.is_monitor_generation_active(id, run_gen) {
                return;
            }
            let needs_endpoint = self
                .vms
                .read()
                .unwrap()
                .get(&id)
                .is_some_and(|vm| vm.generation == run_gen && vm.runtime.fastdesk_endpoint.is_none());
            if needs_endpoint
                && let Ok(Ok(props)) = tokio::time::timeout(
                    Duration::from_secs(1),
                    client.properties_vm(vmservice::PropertiesVmRequest { types: vec![] }),
                )
                .await
            {
                if props.state == vmservice::VmState::Halted as i32 {
                    break (Ok(()), Some(props));
                }
                self.sync_status_from_properties(id, run_gen, &props);
            }

            let res = client.wait_vm_cancellable(cancel_rx.clone()).await;
            if *cancel_rx.borrow() || !self.is_monitor_generation_active(id, run_gen) {
                return;
            }
            match res {
                Ok(WaitVmOutcome::Cancelled) => return,
                Ok(WaitVmOutcome::Exited) => {
                    let props = tokio::time::timeout(
                        Duration::from_secs(2),
                        client.properties_vm(vmservice::PropertiesVmRequest { types: vec![] }),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok);
                    break (Ok(()), props);
                }
                Err(err) => {
                    // Distinguish transient RPC/EOF glitches on a live VM from actual VM exit.
                    let mut confirmed_alive = false;
                    let mut last_props = None;
                    for attempt in 0..3 {
                        if *cancel_rx.borrow() || !self.is_monitor_generation_active(id, run_gen) {
                            return;
                        }
                        if let Ok(Ok(p)) = tokio::time::timeout(
                            Duration::from_secs(2),
                            client.properties_vm(vmservice::PropertiesVmRequest { types: vec![] }),
                        )
                        .await
                        {
                            let is_live_state = p.state == vmservice::VmState::Running as i32
                                || p.state == vmservice::VmState::Paused as i32;
                            if is_live_state {
                                self.sync_status_from_properties(id, run_gen, &p);
                                confirmed_alive = true;
                                break;
                            }
                            last_props = Some(p);
                            break;
                        }
                        let proc_alive = self.is_vm_process_alive(id, run_gen).await;
                        let sock_client = client.clone();
                        let sock_alive = tokio::task::spawn_blocking(move || {
                            sock_client.is_socket_accepting()
                        })
                        .await
                        .unwrap_or(false);
                        if !proc_alive && !sock_alive {
                            break;
                        }
                        if attempt == 2 && proc_alive && sock_alive {
                            confirmed_alive = true;
                            break;
                        }
                        if sleep_or_cancel(Duration::from_millis(250), &mut cancel_rx).await {
                            return;
                        }
                    }
                    if confirmed_alive {
                        warn!(
                            vm_id = %id,
                            error = %err,
                            "WaitVM returned error while VM is still alive; re-attaching monitor"
                        );
                        if sleep_or_cancel(Duration::from_millis(250), &mut cancel_rx).await {
                            return;
                        }
                        continue;
                    }
                    break (Err(err), last_props);
                }
            }
        };

        let stop_done_tx = {
            let mut guard = self.vms.write().unwrap();
            if *self.shutdown_watch.borrow() && !self.should_stop_vms_on_shutdown() {
                return;
            }
            let Some(vm) = guard.get_mut(&id) else {
                return;
            };
            if vm.generation != run_gen || vm.runtime.status == VmStatus::Stopping {
                return;
            }
            let (stop_done_tx, stop_done_rx) = watch::channel(StopOutcome::Pending);
            vm.runtime.status = VmStatus::Stopping;
            vm.monitor_cancel = None;
            vm.monitor_handle = None;
            vm.stop_wait = Some(stop_done_rx);
            stop_done_tx
        };

        if props.is_none() {
            props = tokio::time::timeout(
                Duration::from_secs(2),
                client.properties_vm(vmservice::PropertiesVmRequest { types: vec![] }),
            )
            .await
            .ok()
            .and_then(Result::ok);
        }

        let _ = tokio::time::timeout(Duration::from_secs(3), client.teardown_vm()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), client.quit()).await;

        let (mut child_opt, adopted_opt, sock_opt) = {
            let mut guard = self.vms.write().unwrap();
            if let Some(vm) = guard.get_mut(&id).filter(|vm| vm.generation == run_gen) {
                (
                    vm.child.take(),
                    vm.adopted_proc.clone(),
                    vm.runtime.rpc_socket_path.clone(),
                )
            } else {
                (None, None, None)
            }
        };

        let expected_bin = self.resolved_paths().openvmm_bin;
        let (exit_status, stopped) = if let Some(mut child) = child_opt.take() {
            let status = tokio::task::spawn_blocking(move || {
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
            .flatten();
            (status, true)
        } else {
            let stopped = if adopted_opt.is_some() || sock_opt.is_some() {
                tokio::task::spawn_blocking(move || {
                    wait_or_terminate_adopted_proc(
                        sock_opt.as_deref(),
                        adopted_opt.as_deref(),
                        expected_bin.as_deref(),
                    )
                })
                .await
                .unwrap_or(false)
            } else {
                true
            };
            (None, stopped)
        };

        let summary = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                let _ = stop_done_tx.send(StopOutcome::Done(None));
                return;
            };
            if vm.generation != run_gen {
                let _ = stop_done_tx.send(StopOutcome::Done(None));
                return;
            }
            if !stopped {
                vm.runtime.status = VmStatus::Running;
                vm.runtime.last_error = Some(
                    "OpenVMM process is still running and could not be terminated".to_string(),
                );
                vm.monitor_cancel = None;
                vm.monitor_handle = None;
                vm.stop_wait = None;
            } else {
                cleanup_runtime_files(vm.runtime.rpc_socket_path.take(), vm.runtime.shm_path.take());
                vm.adopted_proc = None;
                vm.runtime.pid = None;
                vm.runtime.started_at = None;
                vm.runtime.fastdesk_endpoint = None;
                vm.runtime.live_properties = None;
                vm.monitor_cancel = None;
                vm.monitor_handle = None;
                vm.stop_wait = None;
                vm.clear_pending_reservations();

                let halt_reason = props.as_ref().and_then(|p| p.halt_reason.clone());
                let is_halted = props
                    .as_ref()
                    .is_some_and(|p| p.state == vmservice::VmState::Halted as i32);
                match wait_res {
                    Ok(()) => {
                        vm.runtime.status = VmStatus::Off;
                        vm.runtime.last_error = halt_reason.filter(|r| r != "PowerOff");
                    }
                    Err(err) => {
                        if exit_status.is_some_and(|st| st.success()) || is_halted {
                            vm.runtime.status = VmStatus::Off;
                            vm.runtime.last_error = None;
                        } else {
                            vm.runtime.status = VmStatus::Error;
                            vm.runtime.last_error = Some(format!("VM exited: {err:#}"));
                        }
                    }
                }
            }
            vm.runtime.revision = self.next_revision();
            vm.summary()
        };

        let done_summary = matches!(summary.runtime.status, VmStatus::Off | VmStatus::Error).then(|| {
            let mut stopped = summary.clone();
            stopped.runtime.status = VmStatus::Off;
            stopped.runtime.last_error = None;
            stopped
        });
        let _ = stop_done_tx.send(StopOutcome::Done(done_summary));
        self.emit(DaemonEvent::VmUpdated { vm: summary });
    }

    pub async fn stop_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let (wait_rx, stop_task_args) = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard
                .get_mut(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            if vm.deleting {
                return Err(err_conflict(format!(
                    "cannot stop VM '{}' while it is being deleted",
                    vm.config.name
                )));
            }
            if matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                let mut changed =
                    vm.runtime.status != VmStatus::Off || vm.runtime.last_error.is_some();
                vm.runtime.status = VmStatus::Off;
                vm.runtime.last_error = None;
                changed |= vm.clear_pending_reservations();
                if changed {
                    vm.runtime.revision = self.next_revision();
                }
                let summary = vm.summary();
                drop(guard);
                if changed {
                    self.emit(DaemonEvent::VmUpdated {
                        vm: summary.clone(),
                    });
                }
                return Ok(summary);
            }
            if vm.runtime.status == VmStatus::Stopping {
                if let Some(rx) = vm.stop_wait.clone() {
                    (Some(rx), None)
                } else {
                    return Err(err_conflict(format!(
                        "cannot stop VM '{}' while it is already Stopping",
                        vm.config.name
                    )));
                }
            } else {
                vm.generation += 1;
                let stop_gen = vm.generation;
                if let Some(cancel_tx) = vm.start_cancel.take() {
                    let _ = cancel_tx.send(true);
                }
                let (stop_done_tx, stop_done_rx) = watch::channel(StopOutcome::Pending);
                vm.stop_wait = Some(stop_done_rx.clone());
                vm.runtime.status = VmStatus::Stopping;
                vm.runtime.revision = self.next_revision();
                let adopted_proc = if vm.child.is_none() {
                    vm.adopted_proc.take()
                } else {
                    vm.adopted_proc = None;
                    None
                };
                (
                    Some(stop_done_rx),
                    Some((
                        stop_gen,
                        vm.runtime.rpc_socket_path.take(),
                        vm.runtime.shm_path.take(),
                        vm.child.take(),
                        adopted_proc,
                        vm.monitor_cancel.take(),
                        vm.monitor_handle.take(),
                        stop_done_tx,
                    )),
                )
            }
        };

        if let Some((
            stop_gen,
            sock_opt,
            shm_opt,
            child_opt,
            adopted_proc,
            mon_cancel_opt,
            mon_handle_opt,
            stop_done_tx,
        )) = stop_task_args
        {
            if let Some(tx) = mon_cancel_opt {
                let _ = tx.send(true);
            }
            self.emit_vm_snapshot(id);
            let sup = Arc::clone(self);
            tokio::spawn(async move {
                if let Some(mut mon_handle) = mon_handle_opt
                    && tokio::time::timeout(Duration::from_secs(2), &mut mon_handle)
                        .await
                        .is_err()
                {
                    mon_handle.abort();
                    let _ = mon_handle.await;
                }
                if let Some(sock) = &sock_opt {
                    let client = TtrpcClient::new(sock);
                    let _ = tokio::time::timeout(Duration::from_secs(4), client.teardown_vm()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), client.quit()).await;
                }

                let expected_bin = sup.resolved_paths().openvmm_bin;
                let stopped = if let Some(mut child) = child_opt {
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
                    true
                } else if adopted_proc.is_some() || sock_opt.is_some() {
                    let sock_wait = sock_opt.clone();
                    let proc_wait = adopted_proc.clone();
                    tokio::task::spawn_blocking(move || {
                        wait_or_terminate_adopted_proc(
                            sock_wait.as_deref(),
                            proc_wait.as_deref(),
                            expected_bin.as_deref(),
                        )
                    })
                    .await
                    .unwrap_or(false)
                } else {
                    true
                };

                if !stopped {
                    let summary = {
                        let mut guard = sup.vms.write().unwrap();
                        if let Some(vm) = guard.get_mut(&id) {
                            if vm.generation == stop_gen {
                                vm.runtime.rpc_socket_path = sock_opt.clone();
                                vm.runtime.shm_path = shm_opt;
                                vm.adopted_proc = adopted_proc;
                                vm.runtime.status = VmStatus::Running;
                                vm.runtime.last_error = Some(
                                    "failed to stop adopted VM: OpenVMM process is still running and could not be terminated".to_string(),
                                );
                                vm.stop_wait = None;
                                if let Some(sock) = sock_opt {
                                    let (mon_cancel_tx, mon_cancel_rx) = watch::channel(false);
                                    let mon_sup = Arc::clone(&sup);
                                    let handle = tokio::spawn(async move {
                                        mon_sup.monitor_vm(id, stop_gen, sock, mon_cancel_rx).await;
                                    });
                                    vm.monitor_cancel = Some(mon_cancel_tx);
                                    vm.monitor_handle = Some(handle);
                                }
                                vm.runtime.revision = sup.next_revision();
                                (!vm.deleting).then(|| vm.summary())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    let _ = stop_done_tx.send(StopOutcome::Done(None));
                    if let Some(summary) = summary {
                        sup.emit(DaemonEvent::VmUpdated { vm: summary });
                    }
                    return;
                }

                cleanup_runtime_files(sock_opt, shm_opt);
                let (done_summary, emit_summary) = {
                    let mut guard = sup.vms.write().unwrap();
                    if let Some(vm) = guard.get_mut(&id) {
                        if vm.generation == stop_gen {
                            vm.runtime.status = VmStatus::Off;
                            vm.adopted_proc = None;
                            vm.runtime.pid = None;
                            vm.runtime.started_at = None;
                            vm.runtime.fastdesk_endpoint = None;
                            vm.runtime.live_properties = None;
                            vm.runtime.last_error = None;
                            vm.stop_wait = None;
                            vm.clear_pending_reservations();
                            vm.runtime.revision = sup.next_revision();
                            let stopped_summary = vm.summary();
                            let emit_summary = (!vm.deleting).then(|| stopped_summary.clone());
                            (Some(stopped_summary), emit_summary)
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    }
                };
                let _ = stop_done_tx.send(StopOutcome::Done(done_summary));
                if let Some(summary) = emit_summary {
                    sup.emit(DaemonEvent::VmUpdated { vm: summary });
                }
            });
        }

        let mut done_summary = None;
        if let Some(mut rx) = wait_rx {
            loop {
                let outcome = rx.borrow_and_update().clone();
                match outcome {
                    StopOutcome::Pending => {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    StopOutcome::Done(s) => {
                        done_summary = s;
                        break;
                    }
                }
            }
        }

        let mut emit_normalized = None;
        let summary = {
            let mut guard = self.vms.write().unwrap();
            match guard.get_mut(&id) {
                Some(vm) => {
                    if matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) && vm.deleting {
                        Ok(done_summary.unwrap_or_else(|| {
                            let mut s = vm.summary();
                            s.runtime.status = VmStatus::Off;
                            s.runtime.last_error = None;
                            s
                        }))
                    } else if vm.deleting {
                        return Err(err_conflict(format!(
                            "cannot stop VM '{}' while it is being deleted",
                            vm.config.name
                        )));
                    } else if matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error) {
                        let mut changed = vm.runtime.status != VmStatus::Off
                            || vm.runtime.last_error.is_some();
                        vm.runtime.status = VmStatus::Off;
                        vm.runtime.last_error = None;
                        changed |= vm.clear_pending_reservations();
                        if changed {
                            vm.runtime.revision = self.next_revision();
                        }
                        let s = vm.summary();
                        if changed {
                            emit_normalized = Some(s.clone());
                        }
                        Ok(s)
                    } else if let Some(s) = done_summary {
                        Ok(s)
                    } else {
                        Err(anyhow::anyhow!(
                            "{}",
                            vm.runtime
                                .last_error
                                .as_deref()
                                .unwrap_or("failed to stop adopted VM")
                        ))
                    }
                }
                None => match done_summary {
                    Some(s) if s.runtime.status == VmStatus::Off => Ok(s),
                    _ => return Err(err_not_found("VM not found")),
                },
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

    fn power_op_lock(&self, id: Uuid) -> anyhow::Result<Arc<tokio::sync::Mutex<()>>> {
        let guard = self.vms.read().unwrap();
        let vm = guard
            .get(&id)
            .ok_or_else(|| err_not_found("VM not found"))?;
        Ok(Arc::clone(&vm.power_op_lock))
    }

    fn hotplug_lock(&self, id: Uuid) -> anyhow::Result<Arc<tokio::sync::Mutex<()>>> {
        let guard = self.vms.read().unwrap();
        let vm = guard
            .get(&id)
            .ok_or_else(|| err_not_found("VM not found"))?;
        Ok(Arc::clone(&vm.hotplug_lock))
    }

    pub async fn pause_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move {
            let id = sup.resolve_vm_id(&id_or_name)?;
            let power_lock = sup.power_op_lock(id)?;
            let _power_guard = power_lock.lock().await;
            let (sock, rpc_gen) = sup.require_socket_with_generation(id, &[VmStatus::Running])?;
            TtrpcClient::new(sock).pause_vm().await?;
            sup.commit_power_transition(id, rpc_gen, "pause", VmStatus::Paused)
        })
        .await
        .map_err(|e| anyhow::anyhow!("VM pause task failed: {e}"))?
    }

    pub async fn resume_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move {
            let id = sup.resolve_vm_id(&id_or_name)?;
            let power_lock = sup.power_op_lock(id)?;
            let _power_guard = power_lock.lock().await;
            let (sock, rpc_gen) =
                sup.require_socket_with_generation(id, &[VmStatus::Running, VmStatus::Paused])?;
            TtrpcClient::new(sock).resume_vm().await?;
            sup.commit_power_transition(id, rpc_gen, "resume", VmStatus::Running)
        })
        .await
        .map_err(|e| anyhow::anyhow!("VM resume task failed: {e}"))?
    }

    /// Applies the result of a pause/resume RPC only if the VM lifecycle did not move on
    /// while the RPC was in flight. A stop or restart bumps the generation and sets
    /// `Stopping`/`Starting`; overwriting that state would let a second stop or a premature
    /// restart race with the in-progress teardown.
    pub(crate) fn commit_power_transition(
        &self,
        id: Uuid,
        rpc_gen: u64,
        action: &str,
        new_status: VmStatus,
    ) -> anyhow::Result<VmSummary> {
        let summary = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard
                .get_mut(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            if vm.generation != rpc_gen
                || !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused)
            {
                return Err(err_conflict(format!(
                    "cannot complete {action} because VM '{}' changed state to {}",
                    vm.config.name, vm.runtime.status
                )));
            }
            vm.runtime.status = new_status;
            vm.runtime.revision = self.next_revision();
            vm.summary()
        };
        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn reset_vm(self: &Arc<Self>, id_or_name: &str) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move {
            let id = sup.resolve_vm_id(&id_or_name)?;
            let power_lock = sup.power_op_lock(id)?;
            let _power_guard = power_lock.lock().await;
            let sock = sup.require_active_socket(id)?;
            TtrpcClient::new(sock).reset_vm().await?;
            sup.get_vm(&id.to_string())
        })
        .await
        .map_err(|e| anyhow::anyhow!("VM reset task failed: {e}"))?
    }

    pub async fn attach_disk(
        self: &Arc<Self>,
        id_or_name: &str,
        req: AttachDiskPayload,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move { sup.attach_disk_inner(&id_or_name, req).await })
            .await
            .map_err(|e| anyhow::anyhow!("attach_disk task failed: {e}"))?
    }

    async fn attach_disk_inner(
        &self,
        id_or_name: &str,
        req: AttachDiskPayload,
    ) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;
        self.attach_disk_locked(id, req, true).await
    }

    async fn attach_disk_locked(
        &self,
        id: Uuid,
        req: AttachDiskPayload,
        require_absolute: bool,
    ) -> anyhow::Result<VmSummary> {
        if let Some(lun) = req.lun
            && lun > 255
        {
            return Err(err_bad_request(format!(
                "SCSI LUN must be 0..=255 (got {lun})"
            )));
        }
        let explicit_format = req.format.is_some();
        let format = req
            .format
            .unwrap_or_else(|| DiskFormat::from_path(&req.path));
        let kind = if format == DiskFormat::Iso {
            DiskKind::Dvd
        } else {
            DiskKind::HardDisk
        };
        let all_paths = {
            let guard = self.vms.read().unwrap();
            collect_all_vm_disk_paths(&guard, std::slice::from_ref(&req.path))
        };
        let req_path = req.path;
        let path_to_check = req_path.clone();
        let mut path_keys = tokio::task::spawn_blocking(move || {
            ensure_disk_format_safe(&path_to_check, format, explicit_format, require_absolute)?;
            if !path_to_check.is_file() {
                return Err(err_bad_request(format!(
                    "disk file not found: {}",
                    path_to_check.display()
                )));
            }
            Ok::<_, anyhow::Error>(precompute_disk_path_keys(all_paths))
        })
        .await
        .map_err(|e| anyhow::anyhow!("attach_disk preflight task failed: {e}"))??;

        let plan = 'plan: {
            for attempt in 0..=MAX_PATH_KEY_RETRIES {
                let missing = {
                    let mut guard = self.vms.write().unwrap();
                    let vm = guard
                        .get(&id)
                        .ok_or_else(|| err_not_found("VM not found"))?;
                    let active_sock = vm.active_rpc_socket("attach disk")?;
                    let lun = req.lun.unwrap_or_else(|| {
                        (0..=255)
                            .find(|&l| {
                                !vm.config
                                    .disks
                                    .iter()
                                    .chain(vm.pending_disks.iter())
                                    .any(|d| d.lun == l)
                            })
                            .unwrap_or(256)
                    });
                    if lun > 255 {
                        return Err(err_bad_request(format!(
                            "SCSI LUN must be 0..=255 (got {lun})"
                        )));
                    }
                    if vm
                        .config
                        .disks
                        .iter()
                        .chain(vm.pending_disks.iter())
                        .any(|d| d.lun == lun)
                    {
                        return Err(err_conflict(format!("SCSI LUN {lun} is already occupied")));
                    }
                    let attachment = DiskAttachment {
                        lun,
                        kind,
                        path: req_path.clone(),
                        read_only: req.read_only || kind == DiskKind::Dvd,
                        format,
                    };
                    let mut candidate_disks = vm.config.disks.clone();
                    candidate_disks.push(attachment.clone());
                    candidate_disks.sort_by_key(|d| d.lun);
                    let missing = collect_missing_disk_path_keys(
                        &guard,
                        Some(id),
                        &candidate_disks,
                        &path_keys,
                    );
                    if missing.is_empty() {
                        ensure_disks_not_conflicting_with_keys(
                            &guard,
                            Some(id),
                            &candidate_disks,
                            &path_keys,
                        )?;
                        if let Some(new_key) = path_keys.get(&attachment.path)
                            && let Some(pending) = vm.pending_disks.iter().find(|pd| {
                                path_keys.get(&pd.path) == Some(new_key)
                                    && (!attachment.read_only || !pd.read_only)
                            })
                        {
                            return Err(err_conflict(format!(
                                "disk image '{}' is already pending attachment on VM '{}' (LUN {})",
                                attachment.path.display(),
                                vm.config.name,
                                pending.lun
                            )));
                        }
                        let vm_gen = vm.generation;
                        break 'plan match active_sock {
                            Some(sock) => {
                                guard
                                    .get_mut(&id)
                                    .unwrap()
                                    .pending_disks
                                    .push(attachment.clone());
                                let pending_guard = PendingReservationGuard::new(&self.vms, id, {
                                    let lun = attachment.lun;
                                    let path = attachment.path.clone();
                                    move |vm| {
                                        if let Some(pos) = vm
                                            .pending_disks
                                            .iter()
                                            .position(|d| d.lun == lun && d.path == path)
                                        {
                                            vm.pending_disks.remove(pos);
                                        }
                                    }
                                });
                                Ok((attachment, sock, vm_gen, pending_guard))
                            }
                            None => {
                                let prev_cfg = vm.config.clone();
                                let mut updated = prev_cfg.clone();
                                updated.disks = candidate_disks;
                                updated.updated_at = Utc::now();
                                Err((attachment, prev_cfg, updated, vm_gen))
                            }
                        };
                    }
                    missing
                };
                if attempt == MAX_PATH_KEY_RETRIES {
                    return Err(err_conflict(
                        "concurrent disk configuration changes while attaching disk; please retry",
                    ));
                }
                extend_disk_path_keys(&mut path_keys, missing, "attach_disk").await?;
            }
            unreachable!()
        };

        let summary = match plan {
            Err((attachment, prev_cfg, updated, snap_gen)) => {
                let lun = attachment.lun;
                self.commit_offline_vm_mutation(
                    id,
                    "attach_disk",
                    "attach disk",
                    prev_cfg,
                    updated,
                    Some(&mut path_keys),
                    |guard, vm, updated_cfg, keys| {
                        if vm.generation != snap_gen
                            || vm.active_rpc_socket("attach disk")?.is_some()
                        {
                            return Err(err_conflict(format!(
                                "cannot complete offline disk attach because VM '{}' changed state",
                                vm.config.name
                            )));
                        }
                        if vm.config.disks.iter().any(|d| d.lun == lun) {
                            return Err(err_conflict(format!(
                                "SCSI LUN {lun} is already occupied"
                            )));
                        }
                        let missing = collect_missing_disk_path_keys(
                            guard,
                            Some(id),
                            &updated_cfg.disks,
                            keys,
                        );
                        if !missing.is_empty() {
                            return Ok(CommitCheckStep::NeedDiskKeys(missing));
                        }
                        ensure_disks_not_conflicting_with_keys(
                            guard,
                            Some(id),
                            &updated_cfg.disks,
                            keys,
                        )?;
                        Ok(CommitCheckStep::Ready)
                    },
                    |vm, updated_cfg| vm.config = updated_cfg,
                )
                .await?
            }
            Ok((attachment, sock, rpc_gen, mut pending_guard)) => {
                if let Err(err) = TtrpcClient::new(sock.clone())
                    .modify_resource(vmservice::ModifyResourceRequest {
                        r#type: vmservice::ModifyType::Add as i32,
                        resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                            to_proto_scsi_disk(&attachment),
                        )),
                    })
                    .await
                    .context("hot-adding SCSI disk to running VM")
                {
                    if !openvmms_core::ttrpc::is_ttrpc_definite_not_applied(&err)
                        && self.is_same_live_vm(id, rpc_gen)
                        && !rollback_hot_added_disk(&sock, id, attachment.lun).await
                    {
                        let lun = attachment.lun;
                        let disk_path = attachment.path.clone();
                        if self.retain_orphaned_disk(id, rpc_gen, attachment) {
                            warn!(
                                vm_id = %id,
                                lun,
                                path = %disk_path.display(),
                                error = %err,
                                "hot-add SCSI disk RPC failed ambiguously and rollback did not succeed; retaining pending disk reservation until VM stops"
                            );
                            pending_guard.disarm();
                        }
                    }
                    return Err(err);
                }

                let lun = attachment.lun;
                let res = self
                    .commit_online_vm_mutation(
                        id,
                        rpc_gen,
                        "attach_disk",
                        "attach disk",
                        "disk attach",
                        Some(&mut path_keys),
                        |guard, vm, keys| {
                            if vm.config.disks.iter().any(|d| d.lun == lun) {
                                return Err(err_conflict(format!(
                                    "SCSI LUN {lun} is already occupied"
                                )));
                            }
                            let mut candidate_disks = vm.config.disks.clone();
                            candidate_disks.push(attachment.clone());
                            candidate_disks.sort_by_key(|d| d.lun);
                            let missing = collect_missing_disk_path_keys(
                                guard,
                                Some(id),
                                &candidate_disks,
                                keys,
                            );
                            if !missing.is_empty() {
                                return Ok(OnlinePlanStep::NeedDiskKeys(missing));
                            }
                            ensure_disks_not_conflicting_with_keys(
                                guard,
                                Some(id),
                                &candidate_disks,
                                keys,
                            )?;
                            let prev_cfg = vm.config.clone();
                            let mut updated = prev_cfg.clone();
                            updated.disks = candidate_disks;
                            updated.updated_at = Utc::now();
                            Ok(OnlinePlanStep::Ready(prev_cfg, updated))
                        },
                        |guard, vm, updated_cfg, keys| {
                            if vm.config.disks.iter().any(|d| d.lun == lun) {
                                return Err(err_conflict(format!(
                                    "SCSI LUN {lun} is already occupied"
                                )));
                            }
                            let missing = collect_missing_disk_path_keys(
                                guard,
                                Some(id),
                                &updated_cfg.disks,
                                keys,
                            );
                            if !missing.is_empty() {
                                return Ok(CommitCheckStep::NeedDiskKeys(missing));
                            }
                            ensure_disks_not_conflicting_with_keys(
                                guard,
                                Some(id),
                                &updated_cfg.disks,
                                keys,
                            )?;
                            Ok(CommitCheckStep::Ready)
                        },
                        |vm, updated_cfg| {
                            vm.config = updated_cfg;
                            if let Some(pos) = vm
                                .pending_disks
                                .iter()
                                .position(|d| d.lun == lun && d.path == attachment.path)
                            {
                                vm.pending_disks.remove(pos);
                            }
                        },
                        || rollback_hot_added_disk(&sock, id, lun),
                    )
                    .await;

                match res {
                    Ok(summary) => {
                        pending_guard.disarm();
                        summary
                    }
                    Err((err, retain_reservation)) => {
                        if retain_reservation && self.retain_orphaned_disk(id, rpc_gen, attachment) {
                            pending_guard.disarm();
                        }
                        return Err(err);
                    }
                }
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn detach_disk(
        self: &Arc<Self>,
        id_or_name: &str,
        lun: u32,
    ) -> anyhow::Result<VmSummary> {
        self.detach_disk_matching(id_or_name, lun, None).await
    }

    pub async fn detach_disk_matching(
        self: &Arc<Self>,
        id_or_name: &str,
        lun: u32,
        expected_path: Option<&Path>,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        let expected_path = expected_path.map(Path::to_path_buf);
        self.spawn_tracked_mutation(async move {
            sup.detach_disk_inner(&id_or_name, lun, expected_path.as_deref())
                .await
        })
        .await
        .map_err(|e| anyhow::anyhow!("detach_disk task failed: {e}"))?
    }

    async fn detach_disk_inner(
        &self,
        id_or_name: &str,
        lun: u32,
        expected_path: Option<&Path>,
    ) -> anyhow::Result<VmSummary> {
        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;
        self.detach_disk_locked(id, lun, expected_path).await
    }

    async fn detach_disk_locked(
        &self,
        id: Uuid,
        lun: u32,
        expected_path: Option<&Path>,
    ) -> anyhow::Result<VmSummary> {
        let (existing, active_sock, snap_gen, prev_cfg) = {
            let guard = self.vms.read().unwrap();
            let vm = guard
                .get(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            let active_sock = vm.active_rpc_socket("detach disk")?;
            let disk = vm
                .config
                .disks
                .iter()
                .find(|d| d.lun == lun)
                .cloned()
                .ok_or_else(|| err_not_found(format!("no disk attached at SCSI LUN {lun}")))?;
            (disk, active_sock, vm.generation, vm.config.clone())
        };

        if let Some(exp) = expected_path {
            let disk_path = existing.path.clone();
            let exp_buf = exp.to_path_buf();
            let keys = tokio::task::spawn_blocking(move || {
                precompute_disk_path_keys(vec![disk_path, exp_buf])
            })
            .await
            .map_err(|e| anyhow::anyhow!("detach_disk path key task failed: {e}"))?;
            if keys.get(&existing.path) != keys.get(exp) {
                return Err(err_conflict(format!(
                    "disk at SCSI LUN {lun} changed (expected '{}', found '{}')",
                    exp.display(),
                    existing.path.display()
                )));
            }
        }

        let summary = match active_sock {
            None => {
                let mut updated = prev_cfg.clone();
                updated.disks.retain(|d| d.lun != lun);
                updated.updated_at = Utc::now();

                self.commit_offline_vm_mutation(
                    id,
                    "detach_disk",
                    "detach disk",
                    prev_cfg,
                    updated,
                    None,
                    |_guard, vm, _updated_cfg, _keys| {
                        if vm.generation != snap_gen
                            || vm.active_rpc_socket("detach disk")?.is_some()
                        {
                            return Err(err_conflict(format!(
                                "cannot complete offline disk detach because VM '{}' changed state",
                                vm.config.name
                            )));
                        }
                        let current_disk = vm.config.disks.iter().find(|d| d.lun == lun).ok_or_else(
                            || err_not_found(format!("no disk attached at SCSI LUN {lun}")),
                        )?;
                        if current_disk.path != existing.path {
                            return Err(err_conflict(format!(
                                "disk at SCSI LUN {lun} changed to '{}'",
                                current_disk.path.display()
                            )));
                        }
                        Ok(CommitCheckStep::Ready)
                    },
                    |vm, updated_cfg| vm.config = updated_cfg,
                )
                .await?
            }
            Some(sock) => {
                let rpc_gen = snap_gen;
                if expected_path.is_some() {
                    let guard = self.vms.read().unwrap();
                    let vm = guard
                        .get(&id)
                        .ok_or_else(|| err_not_found("VM not found"))?;
                    ensure_same_live_vm(vm, rpc_gen, "disk detach")?;
                }
                TtrpcClient::new(sock.clone())
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

                self.commit_online_vm_mutation(
                    id,
                    rpc_gen,
                    "detach_disk",
                    "detach disk",
                    "disk detach",
                    None,
                    |_guard, vm, _keys| {
                        let current_disk = vm.config.disks.iter().find(|d| d.lun == lun).ok_or_else(
                            || err_not_found(format!("no disk attached at SCSI LUN {lun}")),
                        )?;
                        if current_disk.path != existing.path {
                            return Err(err_conflict(format!(
                                "disk at SCSI LUN {lun} changed to '{}'",
                                current_disk.path.display()
                            )));
                        }
                        let prev = vm.config.clone();
                        let mut updated = prev.clone();
                        updated.disks.retain(|d| d.lun != lun);
                        updated.updated_at = Utc::now();
                        Ok(OnlinePlanStep::Ready(prev, updated))
                    },
                    |_guard, vm, _updated_cfg, _keys| {
                        let current_disk = vm.config.disks.iter().find(|d| d.lun == lun).ok_or_else(
                            || err_not_found(format!("no disk attached at SCSI LUN {lun}")),
                        )?;
                        if current_disk.path != existing.path {
                            return Err(err_conflict(format!(
                                "disk at SCSI LUN {lun} changed to '{}'",
                                current_disk.path.display()
                            )));
                        }
                        Ok(CommitCheckStep::Ready)
                    },
                    |vm, updated_cfg| vm.config = updated_cfg,
                    || async {
                        rollback_hot_removed_disk(&sock, id, lun, &existing).await;
                        true
                    },
                )
                .await
                .map_err(|(err, _)| err)?
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    pub async fn mount_iso(
        self: &Arc<Self>,
        id_or_name: &str,
        req: MountIsoPayload,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move { sup.mount_iso_inner(&id_or_name, req).await })
            .await
            .map_err(|e| anyhow::anyhow!("mount_iso task failed: {e}"))?
    }

    async fn mount_iso_inner(
        &self,
        id_or_name: &str,
        req: MountIsoPayload,
    ) -> anyhow::Result<VmSummary> {
        if let Some(lun) = req.lun
            && lun > 255
        {
            return Err(err_bad_request(format!(
                "SCSI LUN must be 0..=255 (got {lun})"
            )));
        }
        let iso_path = req.path.clone();
        tokio::task::spawn_blocking(move || {
            ensure_disk_format_safe(&iso_path, DiskFormat::Iso, true, true)?;
            if !iso_path.is_file() {
                return Err(err_bad_request(format!(
                    "ISO image not found: {}",
                    iso_path.display()
                )));
            }
            if let Err(err) = fs::File::open(&iso_path) {
                return Err(err_bad_request(format!(
                    "cannot open ISO image '{}': {err}",
                    iso_path.display()
                )));
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("mount_iso preflight task failed: {e}"))??;

        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;

        let all_paths = {
            let guard = self.vms.read().unwrap();
            collect_all_vm_disk_paths(&guard, std::slice::from_ref(&req.path))
        };
        let mut path_keys = tokio::task::spawn_blocking(move || precompute_disk_path_keys(all_paths))
            .await
            .map_err(|e| anyhow::anyhow!("mount_iso path key task failed: {e}"))?;

        let active_plan = 'plan: {
            for attempt in 0..=MAX_PATH_KEY_RETRIES {
                let missing = {
                    let guard = self.vms.read().unwrap();
                    let vm = guard
                        .get(&id)
                        .ok_or_else(|| err_not_found("VM not found"))?;
                    let active_sock = vm.active_rpc_socket("mount ISO")?;
                    let (target_lun, previous_dvd) = match req.lun {
                        Some(l) => match vm.config.disks.iter().find(|d| d.lun == l) {
                            Some(d) if d.kind != DiskKind::Dvd => {
                                return Err(err_conflict(format!(
                                    "SCSI LUN {l} is occupied by a non-DVD disk ({}); specify a different LUN or omit --lun",
                                    d.path.display()
                                )));
                            }
                            Some(d) => (d.lun, Some(d.clone())),
                            None => (l, None),
                        },
                        None => match vm.config.disks.iter().find(|d| d.kind == DiskKind::Dvd) {
                            Some(d) => (d.lun, Some(d.clone())),
                            None => (vm.config.next_free_lun(), None),
                        },
                    };
                    if target_lun > 255 {
                        return Err(err_bad_request(format!(
                            "SCSI LUN must be 0..=255 (got {target_lun})"
                        )));
                    }

                    let new_dvd = DiskAttachment {
                        lun: target_lun,
                        kind: DiskKind::Dvd,
                        path: req.path.clone(),
                        read_only: true,
                        format: DiskFormat::Iso,
                    };
                    let mut candidate_disks: Vec<DiskAttachment> = vm
                        .config
                        .disks
                        .iter()
                        .filter(|d| d.lun != target_lun)
                        .cloned()
                        .collect();
                    candidate_disks.push(new_dvd);
                    candidate_disks.sort_by_key(|d| d.lun);
                    let missing = collect_missing_disk_path_keys(
                        &guard,
                        Some(id),
                        &candidate_disks,
                        &path_keys,
                    );
                    if missing.is_empty() {
                        ensure_disks_not_conflicting_with_keys(
                            &guard,
                            Some(id),
                            &candidate_disks,
                            &path_keys,
                        )?;
                        break 'plan if active_sock.is_none() {
                            let prev_cfg = vm.config.clone();
                            let mut updated = prev_cfg.clone();
                            updated.disks = candidate_disks;
                            updated.updated_at = Utc::now();
                            Err((prev_cfg, updated, vm.generation))
                        } else {
                            Ok((target_lun, previous_dvd))
                        };
                    }
                    missing
                };
                if attempt == MAX_PATH_KEY_RETRIES {
                    return Err(err_conflict(
                        "concurrent disk configuration changes while mounting ISO; please retry",
                    ));
                }
                extend_disk_path_keys(&mut path_keys, missing, "mount_iso").await?;
            }
            unreachable!()
        };

        let (target_lun, previous_dvd) = match active_plan {
            Err((prev_cfg, updated, snap_gen)) => {
                let summary = self
                    .commit_offline_vm_mutation(
                        id,
                        "mount_iso",
                        "mount ISO",
                        prev_cfg,
                        updated,
                        Some(&mut path_keys),
                        |guard, vm, updated_cfg, keys| {
                            if vm.generation != snap_gen
                                || vm.active_rpc_socket("mount ISO")?.is_some()
                            {
                                return Err(err_conflict(format!(
                                    "cannot complete offline ISO mount because VM '{}' changed state",
                                    vm.config.name
                                )));
                            }
                            let missing = collect_missing_disk_path_keys(
                                guard,
                                Some(id),
                                &updated_cfg.disks,
                                keys,
                            );
                            if !missing.is_empty() {
                                return Ok(CommitCheckStep::NeedDiskKeys(missing));
                            }
                            ensure_disks_not_conflicting_with_keys(
                                guard,
                                Some(id),
                                &updated_cfg.disks,
                                keys,
                            )?;
                            Ok(CommitCheckStep::Ready)
                        },
                        |vm, updated_cfg| vm.config = updated_cfg,
                    )
                    .await?;
                self.emit(DaemonEvent::VmUpdated {
                    vm: summary.clone(),
                });
                return Ok(summary);
            }
            Ok(plan) => plan,
        };

        if previous_dvd.is_some() {
            let _ = self.detach_disk_locked(id, target_lun, None).await?;
        }

        match self
            .attach_disk_locked(
                id,
                AttachDiskPayload {
                    path: req.path,
                    lun: Some(target_lun),
                    read_only: true,
                    format: Some(DiskFormat::Iso),
                },
                true,
            )
            .await
        {
            Ok(summary) => Ok(summary),
            Err(attach_err) => {
                if let Some(prev) = previous_dvd
                    && let Err(restore_err) = self
                        .attach_disk_locked(
                            id,
                            AttachDiskPayload {
                                path: prev.path,
                                lun: Some(prev.lun),
                                read_only: prev.read_only,
                                format: Some(prev.format),
                            },
                            false,
                        )
                        .await
                {
                    warn!(
                        vm_id = %id,
                        lun = target_lun,
                        error = %restore_err,
                        "failed to restore previous ISO after mount failure"
                    );
                }
                Err(attach_err)
            }
        }
    }

    pub async fn eject_iso(
        self: &Arc<Self>,
        id_or_name: &str,
        lun: Option<u32>,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move {
            let id = sup.resolve_vm_id(&id_or_name)?;
            let hp_lock = sup.hotplug_lock(id)?;
            let _hp_guard = hp_lock.lock().await;
            let target_lun = {
                let guard = sup.vms.read().unwrap();
                let vm = guard
                    .get(&id)
                    .ok_or_else(|| err_not_found("VM not found"))?;
                let _ = vm.active_rpc_socket("eject ISO")?;
                match lun {
                    Some(l) => vm
                        .config
                        .disks
                        .iter()
                        .find(|d| d.lun == l && d.kind == DiskKind::Dvd)
                        .map(|d| d.lun)
                        .ok_or_else(|| err_not_found(format!("no ISO/DVD attached at LUN {l}")))?,
                    None => vm
                        .config
                        .disks
                        .iter()
                        .find(|d| d.kind == DiskKind::Dvd)
                        .map(|d| d.lun)
                        .ok_or_else(|| err_not_found("no ISO/DVD drive attached to VM"))?,
                }
            };
            sup.detach_disk_locked(id, target_lun, None).await
        })
        .await
        .map_err(|e| anyhow::anyhow!("eject_iso task failed: {e}"))?
    }

    pub async fn add_port_forward(
        self: &Arc<Self>,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        self.modify_port_forward(id_or_name, nic_id, port, true)
            .await
    }

    pub async fn remove_port_forward(
        self: &Arc<Self>,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        self.modify_port_forward(id_or_name, nic_id, port, false)
            .await
    }

    async fn modify_port_forward(
        self: &Arc<Self>,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
        add: bool,
    ) -> anyhow::Result<VmSummary> {
        let sup = Arc::clone(self);
        let id_or_name = id_or_name.to_string();
        self.spawn_tracked_mutation(async move {
            sup.modify_port_forward_inner(&id_or_name, nic_id, port, add)
                .await
        })
        .await
        .map_err(|e| anyhow::anyhow!("modify_port_forward task failed: {e}"))?
    }

    async fn modify_port_forward_inner(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        mut port: PortForward,
        add: bool,
    ) -> anyhow::Result<VmSummary> {
        port.normalize();
        if add {
            port.validate()
                .map_err(|e| err_bad_request(format!("{e:#}")))?;
        }
        let id = self.resolve_vm_id(id_or_name)?;
        let hp_lock = self.hotplug_lock(id)?;
        let _hp_guard = hp_lock.lock().await;

        let plan = {
            let mut guard = self.vms.write().unwrap();
            let vm = guard
                .get(&id)
                .ok_or_else(|| err_not_found("VM not found"))?;
            let sock_opt = vm.active_rpc_socket("modify port forwarding")?;
            validate_port_forward_change(&vm.config, nic_id, &port, add)?;
            if add && sock_opt.is_some() {
                ensure_no_pending_port_forward_conflict(&vm.pending_port_forwards, nic_id, &port)?;
                ensure_port_forward_not_conflicting_with_active_vms(&guard, id, &port)?;
            }
            let vm_gen = vm.generation;
            match sock_opt {
                Some(sock) => {
                    let pending_guard = if add {
                        guard
                            .get_mut(&id)
                            .unwrap()
                            .pending_port_forwards
                            .push((nic_id, port.clone()));
                        Some(PendingReservationGuard::new(&self.vms, id, {
                            let port = port.clone();
                            move |vm| {
                                if let Some(pos) = vm
                                    .pending_port_forwards
                                    .iter()
                                    .position(|(n, p)| *n == nic_id && *p == port)
                                {
                                    vm.pending_port_forwards.remove(pos);
                                }
                            }
                        }))
                    } else {
                        None
                    };
                    Ok((sock, vm_gen, pending_guard))
                }
                None => {
                    let prev_cfg = vm.config.clone();
                    let updated = apply_port_forward_change(&prev_cfg, nic_id, &port, add)?;
                    Err((prev_cfg, updated, vm_gen))
                }
            }
        };

        let summary = match plan {
            Err((prev_cfg, updated, snap_gen)) => {
                self.commit_offline_vm_mutation(
                    id,
                    "modify_port_forward",
                    "modify port forwarding",
                    prev_cfg,
                    updated,
                    None,
                    |_guard, vm, _updated_cfg, _keys| {
                        if vm.generation != snap_gen
                            || vm.active_rpc_socket("modify port forwarding")?.is_some()
                        {
                            return Err(err_conflict(format!(
                                "cannot complete offline port forward update because VM '{}' changed state",
                                vm.config.name
                            )));
                        }
                        validate_port_forward_change(&vm.config, nic_id, &port, add)?;
                        Ok(CommitCheckStep::Ready)
                    },
                    |vm, updated_cfg| vm.config = updated_cfg,
                )
                .await?
            }
            Ok((sock, rpc_gen, mut pending_guard)) => {
                let modify_type = if add {
                    vmservice::ModifyType::Update as i32
                } else {
                    vmservice::ModifyType::Remove as i32
                };
                let proto_port = to_proto_port_config(&port);
                if let Err(err) = TtrpcClient::new(sock.clone())
                    .modify_resource(vmservice::ModifyResourceRequest {
                        r#type: modify_type,
                        resource: Some(vmservice::modify_resource_request::Resource::NicConfig(
                            vmservice::NicConfig {
                                nic_id: nic_id.to_string(),
                                backend: Some(vmservice::nic_config::Backend::Consomme(
                                    vmservice::ConsommeBackend {
                                        ports: vec![proto_port.clone()],
                                        ..Default::default()
                                    },
                                )),
                                ..Default::default()
                            },
                        )),
                    })
                    .await
                    .context("updating Consomme port forward on running VM")
                {
                    if add
                        && !openvmms_core::ttrpc::is_ttrpc_definite_not_applied(&err)
                        && self.is_same_live_vm(id, rpc_gen)
                        && !rollback_port_forward(&sock, id, nic_id, add, proto_port).await
                        && let Some(pg) = pending_guard.as_mut()
                        && self.retain_orphaned_port_forward(id, rpc_gen, nic_id, port.clone())
                    {
                        warn!(
                            vm_id = %id,
                            nic_id = %nic_id,
                            host_port = port.host_port,
                            error = %err,
                            "hot-add Consomme port forward RPC failed ambiguously and rollback did not succeed; retaining pending port reservation until VM stops"
                        );
                        pg.disarm();
                    }
                    return Err(err);
                }

                let res = self
                    .commit_online_vm_mutation(
                        id,
                        rpc_gen,
                        "modify_port_forward",
                        "modify port forwarding",
                        "port forward update",
                        None,
                        |guard, vm, _keys| {
                            validate_port_forward_change(&vm.config, nic_id, &port, add)?;
                            if add {
                                ensure_port_forward_not_conflicting_with_active_vms(
                                    guard, id, &port,
                                )?;
                            }
                            let prev = vm.config.clone();
                            let updated = apply_port_forward_change(&prev, nic_id, &port, add)?;
                            Ok(OnlinePlanStep::Ready(prev, updated))
                        },
                        |guard, vm, _updated_cfg, _keys| {
                            validate_port_forward_change(&vm.config, nic_id, &port, add)?;
                            if add {
                                ensure_port_forward_not_conflicting_with_active_vms(
                                    guard, id, &port,
                                )?;
                            }
                            Ok(CommitCheckStep::Ready)
                        },
                        |vm, updated_cfg| {
                            vm.config = updated_cfg;
                            if add
                                && let Some(pos) = vm
                                    .pending_port_forwards
                                    .iter()
                                    .position(|(n, p)| *n == nic_id && *p == port)
                            {
                                vm.pending_port_forwards.remove(pos);
                            }
                        },
                        || rollback_port_forward(&sock, id, nic_id, add, proto_port),
                    )
                    .await;

                match res {
                    Ok(summary) => {
                        if let Some(pg) = pending_guard.as_mut() {
                            pg.disarm();
                        }
                        summary
                    }
                    Err((err, retain_reservation)) => {
                        if retain_reservation
                            && add
                            && let Some(pg) = pending_guard.as_mut()
                            && self.retain_orphaned_port_forward(id, rpc_gen, nic_id, port)
                        {
                            pg.disarm();
                        }
                        return Err(err);
                    }
                }
            }
        };

        self.emit(DaemonEvent::VmUpdated {
            vm: summary.clone(),
        });
        Ok(summary)
    }

    fn retain_orphaned_disk(&self, id: Uuid, rpc_gen: u64, disk: DiskAttachment) -> bool {
        let summary = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                return false;
            };
            if ensure_same_live_vm(vm, rpc_gen, "disk attach").is_err() {
                return false;
            }
            if !vm
                .orphaned_disks
                .iter()
                .any(|d| d.lun == disk.lun && d.path == disk.path)
            {
                vm.orphaned_disks.push(disk);
            }
            vm.runtime.revision = self.next_revision();
            vm.summary()
        };
        self.emit(DaemonEvent::VmUpdated { vm: summary });
        true
    }

    fn retain_orphaned_port_forward(
        &self,
        id: Uuid,
        rpc_gen: u64,
        nic_id: Uuid,
        forward: PortForward,
    ) -> bool {
        let summary = {
            let mut guard = self.vms.write().unwrap();
            let Some(vm) = guard.get_mut(&id) else {
                return false;
            };
            if ensure_same_live_vm(vm, rpc_gen, "port forward update").is_err() {
                return false;
            }
            let entry = OrphanedPortForward { nic_id, forward };
            if !vm.orphaned_port_forwards.contains(&entry) {
                vm.orphaned_port_forwards.push(entry);
            }
            vm.runtime.revision = self.next_revision();
            vm.summary()
        };
        self.emit(DaemonEvent::VmUpdated { vm: summary });
        true
    }

    #[cfg(test)]
    pub fn set_vm_status_for_test(&self, id: Uuid, status: VmStatus) {
        if let Some(vm) = self.vms.write().unwrap().get_mut(&id) {
            vm.runtime.status = status;
        }
    }

    #[cfg(test)]
    pub fn set_vm_running_socket_for_test(&self, id: Uuid, sock_path: PathBuf) {
        if let Some(vm) = self.vms.write().unwrap().get_mut(&id) {
            vm.runtime.status = VmStatus::Running;
            vm.runtime.rpc_socket_path = Some(sock_path);
            vm.generation += 1;
        }
    }

    #[cfg(test)]
    pub fn generation_for_test(&self, id: Uuid) -> u64 {
        self.vms.read().unwrap()[&id].generation
    }

    /// Simulates a lifecycle transition (stop/start) that bumps the generation.
    #[cfg(test)]
    pub fn advance_lifecycle_for_test(&self, id: Uuid, status: VmStatus) {
        if let Some(vm) = self.vms.write().unwrap().get_mut(&id) {
            vm.generation += 1;
            vm.runtime.status = status;
        }
    }

    #[cfg(test)]
    pub fn set_runtime_dir_changing_for_test(&self, changing: bool) {
        self.runtime_dir_changing.store(changing, Ordering::Release);
    }

    pub fn create_disk(&self, req: CreateDiskPayload) -> anyhow::Result<DiskInfo> {
        self.create_disk_cancellable(req, &AtomicBool::new(false))
    }

    pub fn create_disk_cancellable(
        &self,
        req: CreateDiskPayload,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<DiskInfo> {
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            return Err(err_bad_request("cancelled: daemon is shutting down"));
        }
        let host_cfg = self.host_config();
        let paths = resolve_host_paths(&host_cfg);
        if req.format == DiskFormat::Vhdx {
            validate_explicit_path(
                host_cfg.openvmm_img_bin.as_deref(),
                paths.openvmm_img_bin.as_deref(),
                "openvmm-img binary",
            )?;
        }
        disk::create_disk_cancellable(&req, paths.openvmm_img_bin.as_deref(), cancelled)
            .map_err(|e| err_bad_request(format!("{e:#}")))
    }

    pub fn inspect_disk(&self, path: &Path) -> anyhow::Result<DiskInfo> {
        let host_cfg = self.host_config();
        let paths = resolve_host_paths(&host_cfg);
        disk::inspect_disk_with_explicit_tool(
            path,
            host_cfg.openvmm_img_bin.as_deref(),
            paths.openvmm_img_bin.as_deref(),
        )
        .map_err(|e| err_bad_request(format!("{e:#}")))
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

    pub async fn detach_all_vms(self: &Arc<Self>) {
        self.mark_shutting_down();
        let (starting_ids, stopping_waits, monitor_handles) = {
            let mut guard = self.vms.write().unwrap();
            let mut starting_ids = Vec::new();
            let mut stopping_waits = Vec::new();
            let mut monitor_handles = Vec::new();
            for vm in guard.values_mut() {
                vm.detach_on_drop = true;
                match vm.runtime.status {
                    VmStatus::Starting => {
                        starting_ids.push(vm.config.id);
                    }
                    VmStatus::Stopping => {
                        if let Some(rx) = vm.stop_wait.clone() {
                            stopping_waits.push(rx);
                        }
                    }
                    VmStatus::Running | VmStatus::Paused => {
                        if let Some(tx) = vm.monitor_cancel.take() {
                            let _ = tx.send(true);
                        }
                        if let Some(handle) = vm.monitor_handle.take() {
                            monitor_handles.push(handle);
                        }
                        // Drop Child without killing/waiting and preserve runtime files so the next daemon re-adopts it.
                        let _ = vm.child.take();
                        vm.runtime.rpc_socket_path = None;
                        vm.runtime.shm_path = None;
                    }
                    VmStatus::Off | VmStatus::Error => {}
                }
            }
            (starting_ids, stopping_waits, monitor_handles)
        };

        for mut handle in monitor_handles {
            if tokio::time::timeout(Duration::from_secs(3), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        for id in starting_ids {
            if let Err(err) = self.stop_vm(&id.to_string()).await {
                warn!(vm_id = %id, error = %err, "failed to cancel starting VM during daemon detach");
            }
        }
        for mut rx in stopping_waits {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while matches!(*rx.borrow_and_update(), StopOutcome::Pending) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        let mut guard = self.vms.write().unwrap();
        for vm in guard.values_mut() {
            vm.detach_on_drop = true;
            if matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused) {
                let _ = vm.child.take();
                vm.adopted_proc = None;
                vm.runtime.rpc_socket_path = None;
                vm.runtime.shm_path = None;
            }
        }
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

    fn is_same_live_vm(&self, id: Uuid, rpc_gen: u64) -> bool {
        self.vms.read().unwrap().get(&id).is_some_and(|vm| {
            !vm.deleting
                && vm.generation == rpc_gen
                && matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused)
        })
    }

    fn require_running_socket(&self, id: Uuid) -> anyhow::Result<PathBuf> {
        self.require_socket_with_generation(id, &[VmStatus::Running])
            .map(|(sock, _)| sock)
    }

    fn require_socket_with_generation(
        &self,
        id: Uuid,
        allowed: &[VmStatus],
    ) -> anyhow::Result<(PathBuf, u64)> {
        let guard = self.vms.read().unwrap();
        let vm = guard
            .get(&id)
            .ok_or_else(|| err_not_found("VM not found"))?;
        if !allowed.contains(&vm.runtime.status) {
            let names: Vec<String> = allowed.iter().map(ToString::to_string).collect();
            return Err(err_conflict(format!(
                "VM '{}' is {} (must be {})",
                vm.config.name,
                vm.runtime.status,
                names.join(" or ")
            )));
        }
        let sock = vm
            .runtime
            .rpc_socket_path
            .clone()
            .ok_or_else(|| err_internal("VM has no active RPC socket"))?;
        Ok((sock, vm.generation))
    }

    fn require_active_socket(&self, id: Uuid) -> anyhow::Result<PathBuf> {
        self.require_socket_with_generation(id, &[VmStatus::Running, VmStatus::Paused])
            .map(|(sock, _)| sock)
    }

    fn emit_vm_snapshot(&self, id: Uuid) {
        let guard = self.vms.read().unwrap();
        if let Some(vm) = guard.get(&id) {
            self.emit(DaemonEvent::VmUpdated { vm: vm.summary() });
        }
    }

    async fn rollback_vm_config(&self, vm_id: Uuid, action: &str, prev_cfg: VmConfig) {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.save_vm_unchecked(&prev_cfg)).await {
            Ok(Ok(())) => {}
            Ok(Err(rb_err)) => warn!(
                vm_id = %vm_id,
                action,
                error = %rb_err,
                "failed to restore previous vm.toml after commit failure"
            ),
            Err(join_err) => warn!(
                vm_id = %vm_id,
                action,
                error = %join_err,
                "vm.toml rollback task failed after commit failure"
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_offline_vm_mutation<FCheck, FMutate>(
        &self,
        id: Uuid,
        task_name: &str,
        action_label: &str,
        prev_cfg: VmConfig,
        updated: VmConfig,
        mut path_keys: Option<&mut HashMap<PathBuf, PathBuf>>,
        mut check_and_prepare: FCheck,
        mut mutate_vm: FMutate,
    ) -> anyhow::Result<VmSummary>
    where
        FCheck: FnMut(
            &HashMap<Uuid, ManagedVm>,
            &ManagedVm,
            &VmConfig,
            &HashMap<PathBuf, PathBuf>,
        ) -> anyhow::Result<CommitCheckStep>,
        FMutate: FnMut(&mut ManagedVm, VmConfig),
    {
        let store = self.store.clone();
        let updated_to_save = updated.clone();
        let save_task = task_name.to_string();
        tokio::task::spawn_blocking(move || store.save_vm_unchecked(&updated_to_save))
            .await
            .map_err(|e| anyhow::anyhow!("{save_task} save task failed: {e}"))??;

        let empty_keys = HashMap::new();
        let mut should_rollback_file;
        let max_attempts = if path_keys.is_some() {
            MAX_PATH_KEY_RETRIES
        } else {
            0
        };
        let commit_res: anyhow::Result<VmSummary> = 'commit: {
            for attempt in 0..=max_attempts {
                let missing = {
                    let mut guard = self.vms.write().unwrap();
                    let Some(vm) = guard.get(&id) else {
                        should_rollback_file = false;
                        break 'commit Err(err_not_found("VM not found"));
                    };
                    if vm.deleting {
                        should_rollback_file = false;
                        break 'commit Err(err_conflict(format!(
                            "cannot {action_label} while VM '{}' is being deleted",
                            vm.config.name
                        )));
                    }
                    should_rollback_file = true;
                    let keys_ref = path_keys.as_deref().unwrap_or(&empty_keys);
                    match check_and_prepare(&guard, vm, &updated, keys_ref) {
                        Ok(CommitCheckStep::NeedDiskKeys(missing)) => missing,
                        Ok(CommitCheckStep::Ready) => {
                            let vm = guard.get_mut(&id).unwrap();
                            mutate_vm(vm, updated);
                            vm.runtime.revision = self.next_revision();
                            break 'commit Ok(vm.summary());
                        }
                        Err(err) => break 'commit Err(err),
                    }
                };
                if attempt == MAX_PATH_KEY_RETRIES || path_keys.is_none() {
                    break 'commit Err(err_conflict(format!(
                        "concurrent disk configuration changes while trying to {action_label}; please retry"
                    )));
                }
                if let Some(keys) = path_keys.as_deref_mut()
                    && let Err(err) = extend_disk_path_keys(keys, missing, task_name).await
                {
                    break 'commit Err(err);
                }
            }
            unreachable!()
        };

        match commit_res {
            Ok(summary) => Ok(summary),
            Err(err) => {
                if should_rollback_file {
                    self.rollback_vm_config(id, action_label, prev_cfg).await;
                }
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_online_vm_mutation<FPlan, FCheck, FMutate, FLiveRb, Fut>(
        &self,
        id: Uuid,
        rpc_gen: u64,
        task_name: &str,
        action_label: &str,
        same_live_action: &str,
        mut path_keys: Option<&mut HashMap<PathBuf, PathBuf>>,
        mut plan_fn: FPlan,
        mut commit_check: FCheck,
        mut mutate_vm: FMutate,
        live_rollback: FLiveRb,
    ) -> Result<VmSummary, (anyhow::Error, bool)>
    where
        FPlan: FnMut(
            &HashMap<Uuid, ManagedVm>,
            &ManagedVm,
            &HashMap<PathBuf, PathBuf>,
        ) -> anyhow::Result<OnlinePlanStep>,
        FCheck: FnMut(
            &HashMap<Uuid, ManagedVm>,
            &ManagedVm,
            &VmConfig,
            &HashMap<PathBuf, PathBuf>,
        ) -> anyhow::Result<CommitCheckStep>,
        FMutate: FnMut(&mut ManagedVm, VmConfig),
        FLiveRb: FnOnce() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let empty_keys = HashMap::new();
        let max_attempts = if path_keys.is_some() {
            MAX_PATH_KEY_RETRIES
        } else {
            0
        };

        let plan_res: anyhow::Result<(VmConfig, VmConfig)> = 'plan: {
            for attempt in 0..=max_attempts {
                let missing = {
                    let guard = self.vms.read().unwrap();
                    let Some(vm) = guard.get(&id) else {
                        break 'plan Err(err_not_found("VM not found"));
                    };
                    if let Err(err) = ensure_same_live_vm(vm, rpc_gen, same_live_action) {
                        break 'plan Err(err);
                    }
                    let keys_ref = path_keys.as_deref().unwrap_or(&empty_keys);
                    match plan_fn(&guard, vm, keys_ref) {
                        Ok(OnlinePlanStep::NeedDiskKeys(missing)) => missing,
                        Ok(OnlinePlanStep::Ready(prev_cfg, updated)) => {
                            break 'plan Ok((prev_cfg, updated));
                        }
                        Err(err) => break 'plan Err(err),
                    }
                };
                if attempt == MAX_PATH_KEY_RETRIES || path_keys.is_none() {
                    break 'plan Err(err_conflict(format!(
                        "concurrent disk configuration changes while trying to {action_label}; please retry"
                    )));
                }
                if let Some(keys) = path_keys.as_deref_mut()
                    && let Err(err) = extend_disk_path_keys(keys, missing, task_name).await
                {
                    break 'plan Err(err);
                }
            }
            unreachable!()
        };

        let (prev_cfg, updated) = match plan_res {
            Ok(pair) => pair,
            Err(err) => {
                let retain = self.is_same_live_vm(id, rpc_gen) && !live_rollback().await;
                return Err((err, retain));
            }
        };

        let store = self.store.clone();
        let updated_to_save = updated.clone();
        let save_task = task_name.to_string();
        if let Err(err) = tokio::task::spawn_blocking(move || store.save_vm_unchecked(&updated_to_save))
            .await
            .map_err(|e| anyhow::anyhow!("{save_task} save task failed: {e}"))
            .and_then(|r| r)
        {
            let retain = self.is_same_live_vm(id, rpc_gen) && !live_rollback().await;
            return Err((err, retain));
        }

        let mut rollback_on_same_vm;
        let mut should_rollback_file;
        let commit_res: anyhow::Result<VmSummary> = 'commit: {
            for attempt in 0..=max_attempts {
                let missing = {
                    let mut guard = self.vms.write().unwrap();
                    let Some(vm) = guard.get(&id) else {
                        should_rollback_file = false;
                        rollback_on_same_vm = false;
                        break 'commit Err(err_not_found("VM not found"));
                    };
                    if let Err(err) = ensure_same_live_vm(vm, rpc_gen, same_live_action) {
                        should_rollback_file = !vm.deleting;
                        rollback_on_same_vm = false;
                        break 'commit Err(err);
                    }
                    rollback_on_same_vm = true;
                    should_rollback_file = true;
                    let keys_ref = path_keys.as_deref().unwrap_or(&empty_keys);
                    match commit_check(&guard, vm, &updated, keys_ref) {
                        Ok(CommitCheckStep::NeedDiskKeys(missing)) => missing,
                        Ok(CommitCheckStep::Ready) => {
                            let vm = guard.get_mut(&id).unwrap();
                            mutate_vm(vm, updated);
                            vm.runtime.revision = self.next_revision();
                            break 'commit Ok(vm.summary());
                        }
                        Err(err) => break 'commit Err(err),
                    }
                };
                if attempt == MAX_PATH_KEY_RETRIES || path_keys.is_none() {
                    break 'commit Err(err_conflict(format!(
                        "concurrent disk configuration changes while trying to {action_label}; please retry"
                    )));
                }
                if let Some(keys) = path_keys.as_deref_mut()
                    && let Err(err) = extend_disk_path_keys(keys, missing, task_name).await
                {
                    break 'commit Err(err);
                }
            }
            unreachable!()
        };

        match commit_res {
            Ok(summary) => Ok(summary),
            Err(err) => {
                if should_rollback_file {
                    self.rollback_vm_config(id, action_label, prev_cfg).await;
                }
                let retain = rollback_on_same_vm
                    && self.is_same_live_vm(id, rpc_gen)
                    && !live_rollback().await;
                Err((err, retain))
            }
        }
    }

    async fn rollback_host_config(&self, prev_cfg: HostConfig) {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.save_host_config(&prev_cfg)).await {
            Ok(Ok(())) => {}
            Ok(Err(rb_err)) => warn!(
                error = %rb_err,
                "failed to restore previous config.toml after listener switch failure"
            ),
            Err(join_err) => warn!(
                error = %join_err,
                "config.toml rollback task failed after listener switch failure"
            ),
        }
    }
}

fn validate_explicit_path(
    explicit: Option<&Path>,
    resolved: Option<&Path>,
    label: &str,
) -> anyhow::Result<()> {
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty())
        && resolved.is_none()
    {
        return Err(err_bad_request(format!(
            "configured {label} not found at '{}'",
            p.display()
        )));
    }
    Ok(())
}

fn ensure_disk_format_safe(
    path: &Path,
    format: DiskFormat,
    explicit_format: bool,
    require_absolute: bool,
) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Err(err_bad_request("disk path must specify a file name"));
    }
    if require_absolute && !path.is_absolute() {
        return Err(err_bad_request(format!(
            "disk path must be absolute: '{}'",
            path.display()
        )));
    }
    if format == DiskFormat::Raw {
        let lower = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(
            lower.as_deref(),
            Some("vhdx" | "avhdx" | "vhd" | "avhd" | "vmdk" | "vmgs" | "iso")
        ) {
            return Err(err_bad_request(format!(
                "disk path '{}' is incompatible with raw format: extension is reserved for another disk format (specify the matching format such as vhdx, vhd, or iso)",
                path.display()
            )));
        }
        if !explicit_format && disk::file_has_vhdx_signature(path) {
            return Err(err_bad_request(format!(
                "disk image '{}' starts with a VHDX signature ('vhdxfile') but its format was inferred as raw; rename it to .vhdx or specify --format vhdx explicitly",
                path.display()
            )));
        }
    }
    Ok(())
}

const MAX_PATH_KEY_RETRIES: usize = 8;

enum CommitCheckStep {
    Ready,
    NeedDiskKeys(Vec<PathBuf>),
}

enum OnlinePlanStep {
    Ready(VmConfig, VmConfig),
    NeedDiskKeys(Vec<PathBuf>),
}

struct PendingReservationGuard<'a, F: FnOnce(&mut ManagedVm)> {
    vms: &'a RwLock<HashMap<Uuid, ManagedVm>>,
    vm_id: Uuid,
    cleanup: Option<F>,
}

impl<'a, F: FnOnce(&mut ManagedVm)> PendingReservationGuard<'a, F> {
    fn new(vms: &'a RwLock<HashMap<Uuid, ManagedVm>>, vm_id: Uuid, cleanup: F) -> Self {
        Self {
            vms,
            vm_id,
            cleanup: Some(cleanup),
        }
    }

    fn disarm(&mut self) {
        self.cleanup = None;
    }
}

impl<F: FnOnce(&mut ManagedVm)> Drop for PendingReservationGuard<'_, F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take()
            && let Ok(mut guard) = self.vms.write()
            && let Some(vm) = guard.get_mut(&self.vm_id)
        {
            cleanup(vm);
        }
    }
}

fn ensure_same_live_vm(vm: &ManagedVm, rpc_gen: u64, action: &str) -> anyhow::Result<()> {
    if vm.deleting
        || vm.generation != rpc_gen
        || !matches!(vm.runtime.status, VmStatus::Running | VmStatus::Paused)
    {
        return Err(err_conflict(format!(
            "cannot complete {action} because VM '{}' changed state to {}",
            vm.config.name, vm.runtime.status
        )));
    }
    Ok(())
}

fn collect_all_vm_disk_paths(vms: &HashMap<Uuid, ManagedVm>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(
        vms.values()
            .map(|v| v.config.disks.len() + v.pending_disks.len())
            .sum::<usize>()
            + extra.len(),
    );
    for vm in vms.values() {
        for disk in vm.config.disks.iter().chain(vm.pending_disks.iter()) {
            out.push(disk.path.clone());
        }
    }
    out.extend(extra.iter().cloned());
    out
}

fn precompute_disk_path_keys(paths: Vec<PathBuf>) -> HashMap<PathBuf, PathBuf> {
    let mut keys = HashMap::with_capacity(paths.len());
    for path in paths {
        if !keys.contains_key(&path) {
            let key = openvmms_core::models::normalize_path_key(&path);
            keys.insert(path, key);
        }
    }
    keys
}

fn collect_missing_disk_path_keys(
    vms: &HashMap<Uuid, ManagedVm>,
    self_id: Option<Uuid>,
    extra_disks: &[DiskAttachment],
    path_keys: &HashMap<PathBuf, PathBuf>,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for d in extra_disks {
        if !path_keys.contains_key(&d.path) && seen.insert(d.path.clone()) {
            missing.push(d.path.clone());
        }
    }
    for other in vms.values() {
        if Some(other.config.id) == self_id {
            for d in &other.pending_disks {
                if !path_keys.contains_key(&d.path) && seen.insert(d.path.clone()) {
                    missing.push(d.path.clone());
                }
            }
            continue;
        }
        for d in other.config.disks.iter().chain(other.pending_disks.iter()) {
            if !path_keys.contains_key(&d.path) && seen.insert(d.path.clone()) {
                missing.push(d.path.clone());
            }
        }
    }
    missing
}

async fn extend_disk_path_keys(
    path_keys: &mut HashMap<PathBuf, PathBuf>,
    missing: Vec<PathBuf>,
    label: &str,
) -> anyhow::Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let computed = tokio::task::spawn_blocking(move || precompute_disk_path_keys(missing))
        .await
        .map_err(|e| anyhow::anyhow!("{label} path key task failed: {e}"))?;
    path_keys.extend(computed);
    Ok(())
}

fn ensure_disks_not_conflicting_with_keys(
    vms: &HashMap<Uuid, ManagedVm>,
    self_id: Option<Uuid>,
    disks: &[DiskAttachment],
    path_keys: &HashMap<PathBuf, PathBuf>,
) -> anyhow::Result<()> {
    let mut seen_paths: HashMap<PathBuf, u32> = HashMap::new();
    for d in disks {
        let key = path_keys.get(&d.path).ok_or_else(|| {
            err_internal(format!(
                "missing canonical disk path key for '{}'",
                d.path.display()
            ))
        })?;
        if let Some(prev_lun) = seen_paths.insert(key.clone(), d.lun) {
            return Err(err_conflict(format!(
                "disk image '{}' is attached multiple times to the same VM (LUNs {prev_lun} and {})",
                d.path.display(),
                d.lun
            )));
        }
        for other in vms.values() {
            if Some(other.config.id) == self_id {
                continue;
            }
            for other_disk in other.config.disks.iter().chain(other.pending_disks.iter()) {
                let other_key = path_keys.get(&other_disk.path).ok_or_else(|| {
                    err_internal(format!(
                        "missing canonical disk path key for '{}'",
                        other_disk.path.display()
                    ))
                })?;
                if other_key == key && (!d.read_only || !other_disk.read_only) {
                    return Err(err_conflict(format!(
                        "disk image '{}' is already attached to VM '{}' (LUN {}); detach it first or attach both read-only",
                        d.path.display(),
                        other.config.name,
                        other_disk.lun
                    )));
                }
            }
        }
    }
    Ok(())
}

fn ensure_port_forwards_not_conflicting_with_active_vms(
    vms: &HashMap<Uuid, ManagedVm>,
    self_id: Uuid,
    nics: &[NicAttachment],
) -> anyhow::Result<()> {
    let mut candidate_forwards: Vec<(&str, &PortForward)> = Vec::new();
    for nic in nics {
        if let NicBackendConfig::Consomme { port_forwards, .. } = &nic.backend {
            for (idx, pf) in port_forwards.iter().enumerate() {
                if let Some(existing) = port_forwards[..idx]
                    .iter()
                    .find(|existing| existing.same_nic_guest_conflict(pf))
                {
                    return Err(err_conflict(format!(
                        "port forward {pf} conflicts with existing rule {existing} on NIC {} (duplicate guest port for protocol and IP family)",
                        nic.name
                    )));
                }
                if let Some((existing_nic, existing)) = candidate_forwards
                    .iter()
                    .find(|(_, existing)| existing.overlaps(pf))
                {
                    return Err(err_conflict(format!(
                        "port forward {pf} on NIC {} overlaps existing rule {existing} on NIC {existing_nic}",
                        nic.name
                    )));
                }
                candidate_forwards.push((nic.name.as_str(), pf));
            }
        }
    }
    if candidate_forwards.is_empty() {
        return Ok(());
    }
    for other in vms.values() {
        if other.config.id == self_id || matches!(other.runtime.status, VmStatus::Off | VmStatus::Error) {
            continue;
        }
        for other_nic in &other.config.nics {
            if let NicBackendConfig::Consomme { port_forwards, .. } = &other_nic.backend {
                for existing in port_forwards {
                    for (_, pf) in &candidate_forwards {
                        if existing.overlaps(pf) {
                            return Err(err_conflict(format!(
                                "port forward {pf} overlaps existing rule {existing} on active VM '{}' (NIC {})",
                                other.config.name, other_nic.name
                            )));
                        }
                    }
                }
            }
        }
        for (_, existing) in &other.pending_port_forwards {
            for (_, pf) in &candidate_forwards {
                if existing.overlaps(pf) {
                    return Err(err_conflict(format!(
                        "port forward {pf} overlaps pending rule {existing} on active VM '{}'",
                        other.config.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn ensure_no_pending_port_forward_conflict(
    pending: &[(Uuid, PortForward)],
    nic_id: Uuid,
    port: &PortForward,
) -> anyhow::Result<()> {
    for (pending_nic_id, existing) in pending {
        if existing == port {
            return Err(err_conflict(format!(
                "port forward {port} is already pending on this VM"
            )));
        }
        if *pending_nic_id == nic_id && existing.same_nic_guest_conflict(port) {
            return Err(err_conflict(format!(
                "port forward {port} conflicts with pending rule {existing} (duplicate guest port for protocol and IP family)"
            )));
        }
        if existing.overlaps(port) {
            return Err(err_conflict(format!(
                "port forward {port} overlaps pending rule {existing} on this VM"
            )));
        }
    }
    Ok(())
}

fn ensure_port_forward_not_conflicting_with_active_vms(
    vms: &HashMap<Uuid, ManagedVm>,
    self_id: Uuid,
    port: &PortForward,
) -> anyhow::Result<()> {
    for other in vms.values() {
        if other.config.id == self_id || matches!(other.runtime.status, VmStatus::Off | VmStatus::Error) {
            continue;
        }
        for other_nic in &other.config.nics {
            if let NicBackendConfig::Consomme { port_forwards, .. } = &other_nic.backend {
                for existing in port_forwards {
                    if existing.overlaps(port) {
                        return Err(err_conflict(format!(
                            "port forward {port} overlaps existing rule {existing} on active VM '{}' (NIC {})",
                            other.config.name, other_nic.name
                        )));
                    }
                }
            }
        }
        for (_, existing) in &other.pending_port_forwards {
            if existing.overlaps(port) {
                return Err(err_conflict(format!(
                    "port forward {port} overlaps pending rule {existing} on active VM '{}'",
                    other.config.name
                )));
            }
        }
    }
    Ok(())
}

fn validate_port_forward_change(
    cfg: &VmConfig,
    nic_id: Uuid,
    port: &PortForward,
    add: bool,
) -> anyhow::Result<()> {
    let nic = cfg
        .nics
        .iter()
        .find(|n| n.id == nic_id)
        .ok_or_else(|| err_not_found(format!("NIC {nic_id} not found")))?;
    let NicBackendConfig::Consomme { port_forwards, .. } = &nic.backend else {
        return Err(err_bad_request(
            "port forwarding is only supported on Consomme NAT NICs",
        ));
    };
    if add {
        if port_forwards.contains(port) {
            return Err(err_conflict(format!("port forward {port} already exists")));
        }
        for existing in port_forwards {
            if existing.same_nic_guest_conflict(port) {
                return Err(err_conflict(format!(
                    "port forward {port} conflicts with existing rule {existing} on NIC {} (duplicate guest port for protocol and IP family)",
                    nic.name
                )));
            }
        }
        for other_nic in &cfg.nics {
            if let NicBackendConfig::Consomme { port_forwards, .. } = &other_nic.backend {
                for existing in port_forwards {
                    if existing.overlaps(port) {
                        return Err(err_conflict(format!(
                            "port forward {port} overlaps existing rule {existing} on NIC {}",
                            other_nic.name
                        )));
                    }
                }
            }
        }
    } else if !port_forwards.contains(port) {
        return Err(err_not_found(format!("port forward {port} not found")));
    }
    Ok(())
}

fn apply_port_forward_change(
    cfg: &VmConfig,
    nic_id: Uuid,
    port: &PortForward,
    add: bool,
) -> anyhow::Result<VmConfig> {
    let mut updated = cfg.clone();
    let nic = updated
        .nics
        .iter_mut()
        .find(|n| n.id == nic_id)
        .ok_or_else(|| err_not_found("NIC vanished"))?;
    if let NicBackendConfig::Consomme { port_forwards, .. } = &mut nic.backend {
        if add {
            if !port_forwards.contains(port) {
                port_forwards.push(port.clone());
            }
        } else {
            port_forwards.retain(|p| p != port);
        }
    }
    updated.updated_at = Utc::now();
    Ok(updated)
}

fn vm_toml_definitely_absent(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

async fn rollback_hot_added_disk(sock: &Path, id: Uuid, lun: u32) -> bool {
    if let Err(rb_err) = TtrpcClient::new(sock.to_path_buf())
        .modify_resource(vmservice::ModifyResourceRequest {
            r#type: vmservice::ModifyType::Remove as i32,
            resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                vmservice::ScsiDisk {
                    controller: 0,
                    lun,
                    ..Default::default()
                },
            )),
        })
        .await
    {
        if rb_err
            .chain()
            .filter_map(|c| c.downcast_ref::<openvmms_core::ttrpc::TtrpcRpcError>())
            .any(|rpc_err| rpc_err.code == 5)
        {
            return true;
        }
        warn!(
            vm_id = %id,
            lun,
            error = %rb_err,
            "failed to roll back hot-added SCSI disk after commit error"
        );
        return false;
    }
    true
}

async fn rollback_hot_removed_disk(sock: &Path, id: Uuid, lun: u32, existing: &DiskAttachment) {
    if let Err(rb_err) = TtrpcClient::new(sock.to_path_buf())
        .modify_resource(vmservice::ModifyResourceRequest {
            r#type: vmservice::ModifyType::Add as i32,
            resource: Some(vmservice::modify_resource_request::Resource::ScsiDisk(
                to_proto_scsi_disk(existing),
            )),
        })
        .await
    {
        warn!(
            vm_id = %id,
            lun,
            error = %rb_err,
            "failed to roll back hot-removed SCSI disk after commit error"
        );
    }
}

async fn rollback_port_forward(
    sock: &Path,
    id: Uuid,
    nic_id: Uuid,
    added: bool,
    proto_port: vmservice::PortConfig,
) -> bool {
    let rollback_type = if added {
        vmservice::ModifyType::Remove as i32
    } else {
        vmservice::ModifyType::Update as i32
    };
    if let Err(rb_err) = TtrpcClient::new(sock.to_path_buf())
        .modify_resource(vmservice::ModifyResourceRequest {
            r#type: rollback_type,
            resource: Some(vmservice::modify_resource_request::Resource::NicConfig(
                vmservice::NicConfig {
                    nic_id: nic_id.to_string(),
                    backend: Some(vmservice::nic_config::Backend::Consomme(
                        vmservice::ConsommeBackend {
                            ports: vec![proto_port],
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )),
        })
        .await
    {
        if added
            && rb_err
                .chain()
                .filter_map(|c| c.downcast_ref::<openvmms_core::ttrpc::TtrpcRpcError>())
                .any(|rpc_err| rpc_err.code == 5)
        {
            return true;
        }
        warn!(
            vm_id = %id,
            nic_id = %nic_id,
            error = %rb_err,
            "failed to roll back Consomme port forward after commit error"
        );
        return false;
    }
    true
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

fn prepare_shm_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                bail!(
                    "refusing to use non-regular or symlinked FastDesk SHM path {}",
                    path.display()
                );
            }
            fs::remove_file(path)
                .with_context(|| format!("removing stale FastDesk SHM file {}", path.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspecting FastDesk SHM path {}", path.display()));
        }
    }
    Ok(())
}

fn wait_or_terminate_adopted_proc(
    sock_path: Option<&Path>,
    proc_handle: Option<&openvmms_core::process::AdoptedProcess>,
    expected_bin: Option<&Path>,
) -> bool {
    let client = sock_path.map(TtrpcClient::new);
    let resolved_handle;
    let effective_handle = if proc_handle.is_some() {
        proc_handle
    } else {
        resolved_handle = sock_path.and_then(|s| {
            openvmms_core::process::open_verified_pid_or_socket_peer(
                &s.with_extension("pid"),
                Some(s),
                expected_bin,
            )
        });
        resolved_handle.as_deref()
    };
    let start = std::time::Instant::now();
    loop {
        let sock_alive = client.as_ref().is_some_and(TtrpcClient::is_socket_accepting);
        let proc_alive =
            effective_handle.is_some_and(openvmms_core::process::AdoptedProcess::is_alive);
        if !sock_alive && !proc_alive {
            return true;
        }
        if start.elapsed() >= Duration::from_secs(5) {
            if let Some(proc_handle) = effective_handle {
                proc_handle.terminate();
                let kill_deadline = std::time::Instant::now() + Duration::from_secs(1);
                while std::time::Instant::now() < kill_deadline {
                    let still_sock = client.as_ref().is_some_and(TtrpcClient::is_socket_accepting);
                    if !still_sock && !proc_handle.is_alive() {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let still_sock = client.as_ref().is_some_and(TtrpcClient::is_socket_accepting);
                return !still_sock && !proc_handle.is_alive();
            }
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn reclaim_or_verify_inactive_socket(
    sock_path: &Path,
    pid_path: &Path,
    expected_bin: Option<&Path>,
    cancelled: &AtomicBool,
) -> bool {
    if cancelled.load(Ordering::Acquire) {
        return false;
    }
    let client = TtrpcClient::new(sock_path);
    let verified_proc = openvmms_core::process::open_verified_pid_or_socket_peer(
        pid_path,
        Some(sock_path),
        expected_bin,
    );
    let responded = client
        .properties_vm_blocking_cancellable(Duration::from_secs(1), cancelled)
        .is_ok();
    if cancelled.load(Ordering::Acquire) {
        return false;
    }
    if responded || client.is_socket_accepting_cancellable(Duration::from_secs(1), cancelled) {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        let _ = client.teardown_and_quit_blocking_cancellable(Duration::from_secs(2), cancelled);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            if !client.is_socket_accepting_cancellable(Duration::from_millis(200), cancelled) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(proc_handle) = verified_proc {
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            proc_handle.terminate();
            let kill_deadline = std::time::Instant::now() + Duration::from_secs(1);
            while std::time::Instant::now() < kill_deadline {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                if !client.is_socket_accepting_cancellable(Duration::from_millis(200), cancelled) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        return !cancelled.load(Ordering::Acquire)
            && client.is_socket_accepting_cancellable(Duration::from_millis(200), cancelled);
    }
    false
}

fn cleanup_runtime_files(sock_path: Option<PathBuf>, shm_path: Option<String>) {
    if let Some(sock) = sock_path {
        let _ = fs::remove_file(sock.with_extension("pid"));
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

pub(crate) fn build_create_vm_request(
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
                FastDeskTransport::HvSock => Some(cfg.fastdesk.vsock_port.unwrap_or(0)),
                FastDeskTransport::Tcp => None,
            },
            shm_path: shm_path.map(|p| p.to_string_lossy().into_owned()),
            max_clients: 16,
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
            tpm_enabled: cfg.tpm_enabled,
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
            DiskFormat::Vhd => vmservice::DiskType::ScsiDiskTypeVhd1 as i32,
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
                switch_id: switch_id.trim().to_string(),
                port_id: String::new(),
            })
        }
        NicBackendConfig::Tap { name } => {
            vmservice::nic_config::Backend::Tap(vmservice::TapBackend {
                source: Some(vmservice::tap_backend::Source::Name(
                    name.trim().to_string(),
                )),
            })
        }
    };
    vmservice::NicConfig {
        nic_id: nic.id.to_string(),
        mac_address: nic.mac_address.trim().to_string(),
        nic_name: nic.name.clone(),
        frontend: vmservice::NicFrontend::Vmbus as i32,
        backend: Some(backend),
        ..Default::default()
    }
}

fn read_log_tail(path: &Path, tail: usize) -> anyhow::Result<String> {
    const CHUNK_SIZE: u64 = 64 * 1024;
    const MAX_TAIL_BYTES: usize = 4 * 1024 * 1024;

    let tail = tail.clamp(1, 10_000);
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(String::new());
    }

    let mut pos = file_len;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total_bytes = 0usize;
    let mut newline_count = 0usize;

    while pos > 0 && total_bytes < MAX_TAIL_BYTES {
        let to_read = pos.min(CHUNK_SIZE).min((MAX_TAIL_BYTES - total_bytes) as u64);
        pos -= to_read;
        file.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; to_read as usize];
        file.read_exact(&mut buf)?;
        newline_count += buf.iter().filter(|&&b| b == b'\n').count();
        total_bytes += buf.len();
        chunks.push(buf);
        if newline_count > tail {
            break;
        }
    }

    let mut combined = Vec::with_capacity(total_bytes);
    for chunk in chunks.into_iter().rev() {
        combined.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&combined);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].join("\n"))
}

#[cfg(windows)]
fn windows_hyperv_module_available() -> bool {
    let mut roots: Vec<PathBuf> = std::env::var_os("PSModulePath")
        .map(|val| std::env::split_paths(&val).collect())
        .unwrap_or_default();
    if let Some(sys_root) = std::env::var_os("SystemRoot") {
        roots.push(
            PathBuf::from(sys_root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("Modules"),
        );
    }
    roots.into_iter().any(|dir| dir.join("Hyper-V").is_dir())
}

#[cfg(windows)]
fn discover_windows_switches() -> Vec<VirtualSwitchInfo> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::sync::OnceLock;
    use std::time::Instant;

    const CACHE_TTL: Duration = Duration::from_secs(10);
    const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    static CACHE: OnceLock<std::sync::Mutex<Option<(Instant, Vec<VirtualSwitchInfo>)>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some((captured_at, cached)) = guard.as_ref()
        && captured_at.elapsed() < CACHE_TTL
    {
        return cached.clone();
    }

    let mut out = vec![VirtualSwitchInfo {
        id: "c08cb7b8-9b3c-408e-8e30-5e16a3aeb444".to_string(),
        name: "Default Switch".to_string(),
        kind: "vmswitch".to_string(),
        description: "Windows Hyper-V Default Switch (HCN)".to_string(),
    }];

    if windows_hyperv_module_available()
        && let Ok(mut child) = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-VMSwitch | Select-Object @{n='Id';e={$_.Id.ToString()}},Name,SwitchType | ConvertTo-Json -Compress",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    {
        let reader_handle = child.stdout.take().map(|mut stdout| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                buf
            })
        });
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let mut status = None;
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    status = Some(st);
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        if status.is_some_and(|st| st.success())
            && let Some(stdout_bytes) = reader_handle.and_then(|h| h.join().ok())
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&stdout_bytes)
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
    }

    *guard = Some((Instant::now(), out.clone()));
    out
}

#[cfg(target_os = "linux")]
fn discover_linux_tap_interfaces() -> Vec<VirtualSwitchInfo> {
    discover_linux_tap_interfaces_in(Path::new("/sys/class/net"))
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

async fn wait_until_true(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

async fn sleep_or_cancel(duration: Duration, cancel_rx: &mut watch::Receiver<bool>) -> bool {
    if *cancel_rx.borrow_and_update() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        res = cancel_rx.changed() => res.is_err() || *cancel_rx.borrow_and_update(),
    }
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

    #[test]
    fn read_log_tail_reads_exact_last_lines_from_large_file() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("large.log");
        {
            let mut f = fs::File::create(&log_path).unwrap();
            let filler = "x".repeat(120);
            for i in 0..25_000 {
                writeln!(f, "line-{i:05}:{filler}").unwrap();
            }
        }
        assert!(fs::metadata(&log_path).unwrap().len() > 3 * 1024 * 1024);

        let tail = read_log_tail(&log_path, 5).unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("line-24995:"));
        assert!(lines[4].starts_with("line-24999:"));
    }

    #[tokio::test]
    async fn start_vm_is_rejected_while_runtime_dir_is_changing() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "RuntimeDirRace-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();

        sup.set_runtime_dir_changing_for_test(true);
        let err = sup.start_vm(&created.config.id.to_string()).await.unwrap_err();
        let kind = err
            .chain()
            .find_map(|c| c.downcast_ref::<ApiErrorKind>());
        assert!(matches!(kind, Some(ApiErrorKind::Conflict(_))), "{err:#}");
        assert_eq!(
            sup.get_vm(&created.config.id.to_string())
                .unwrap()
                .runtime
                .status,
            VmStatus::Off
        );

        sup.set_runtime_dir_changing_for_test(false);
    }

    #[test]
    fn build_create_vm_request_translates_all_compute_boot_disk_nic_and_fastdesk_fields() {
        let vm_dir = tempfile::tempdir().unwrap();
        let disk_vhdx = vm_dir.path().join("os.vhdx");
        let iso_path = vm_dir.path().join("install.iso");
        let uefi_path = vm_dir.path().join("MSVM.fd");
        let shm_path = vm_dir.path().join("vram.shm");
        let vmgs_path = vm_dir.path().join("state.vmgs");

        let mut cfg = VmConfig::from_preset(
            "TranslateTest",
            openvmms_core::models::VmPreset::Windows11,
            vm_dir.path(),
        );
        cfg.vcpus = 6;
        cfg.memory_mb = 6144;
        cfg.nested_virt = true;
        cfg.secure_boot_enabled = true;
        cfg.secure_boot_template = SecureBootTemplateConfig::MicrosoftUefiCa;
        cfg.tpm_enabled = true;
        cfg.vmgs_path = Some(vmgs_path);
        cfg.disks = vec![
            DiskAttachment {
                lun: 0,
                kind: DiskKind::HardDisk,
                path: disk_vhdx.clone(),
                read_only: false,
                format: DiskFormat::Vhdx,
            },
            DiskAttachment {
                lun: 1,
                kind: DiskKind::Dvd,
                path: iso_path.clone(),
                read_only: true,
                format: DiskFormat::Iso,
            },
        ];
        cfg.nics = vec![
            NicAttachment {
                id: Uuid::new_v4(),
                name: "nat0".into(),
                mac_address: "52:54:00:11:22:33".into(),
                backend: NicBackendConfig::Consomme {
                    cidr: Some("10.0.2.0/24".into()),
                    port_forwards: vec![PortForward {
                        host_addr: "127.0.0.1".into(),
                        host_port: 2222,
                        guest_port: 22,
                        protocol: IpProto::Tcp,
                    }],
                    internal_dns: true,
                    allow_host_local_access: true,
                },
            },
            NicAttachment {
                id: Uuid::new_v4(),
                name: "tap0".into(),
                mac_address: "52:54:00:44:55:66".into(),
                backend: NicBackendConfig::Tap {
                    name: "tap-test".into(),
                },
            },
        ];
        cfg.fastdesk.enabled = true;
        cfg.fastdesk.vram_mb = 64;
        cfg.fastdesk.listen_addr = "127.0.0.1:5905".into();
        cfg.fastdesk.transport = FastDeskTransport::HvSock;
        cfg.fastdesk.vsock_port = Some(6005);

        let req = build_create_vm_request(&cfg, &uefi_path, Some(&shm_path)).unwrap();
        assert_eq!(req.log_id, cfg.id.to_string());
        let vm_cfg = req.config.unwrap();
        assert_eq!(vm_cfg.memory_config.unwrap().memory_mb, 6144);
        assert_eq!(vm_cfg.processor_config.unwrap().processor_count, 6);
        assert!(vm_cfg.nested_virt);
        assert!(vm_cfg.tpm_enabled);

        match vm_cfg.boot_config.unwrap() {
            vmservice::vm_config::BootConfig::Uefi(u) => {
                assert_eq!(u.firmware_path, uefi_path.to_string_lossy());
                assert!(u.secure_boot_enabled);
                assert_eq!(
                    u.initial_variables.unwrap().secure_boot_template,
                    vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftUefiCertificateAuthority as i32
                );
            }
            other => panic!("unexpected boot config: {other:?}"),
        }

        let devices = vm_cfg.devices_config.unwrap();
        assert!(devices.enable_scsi_controller);
        assert_eq!(devices.scsi_disks.len(), 2);
        assert_eq!(devices.scsi_disks[0].lun, 0);
        assert_eq!(
            devices.scsi_disks[0].r#type,
            vmservice::DiskType::ScsiDiskTypeVhdx as i32
        );
        assert!(!devices.scsi_disks[0].read_only);
        assert!(!devices.scsi_disks[0].is_dvd);
        assert_eq!(devices.scsi_disks[1].lun, 1);
        assert_eq!(
            devices.scsi_disks[1].r#type,
            vmservice::DiskType::ScsiDiskTypePhysical as i32
        );
        assert!(devices.scsi_disks[1].read_only);
        assert!(devices.scsi_disks[1].is_dvd);

        assert_eq!(devices.nic_config.len(), 2);
        assert_eq!(devices.nic_config[0].mac_address, "52:54:00:11:22:33");
        match devices.nic_config[0].backend.as_ref().unwrap() {
            vmservice::nic_config::Backend::Consomme(c) => {
                assert_eq!(c.cidr, "10.0.2.0/24");
                assert_eq!(c.internal_dns, Some(true));
                assert!(c.allow_host_local_access);
                assert_eq!(c.ports.len(), 1);
                assert_eq!(c.ports[0].host_port, 2222);
                assert_eq!(c.ports[0].guest_port, 22);
            }
            other => panic!("expected Consomme backend, got {other:?}"),
        }
        match devices.nic_config[1].backend.as_ref().unwrap() {
            vmservice::nic_config::Backend::Tap(t) => {
                assert!(matches!(
                    t.source,
                    Some(vmservice::tap_backend::Source::Name(ref n)) if n == "tap-test"
                ));
            }
            other => panic!("expected Tap backend, got {other:?}"),
        }

        let fd = vm_cfg.fastdesk_config.unwrap();
        assert_eq!(fd.vram_mb, 64);
        assert_eq!(fd.listen_addr, "127.0.0.1:5905");
        assert_eq!(fd.vsock_port, Some(6005));
        assert_eq!(fd.shm_path.as_deref(), Some(shm_path.to_string_lossy().as_ref()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_disk_conflict_and_delete_protection_never_mix_canonical_and_lexical_keys() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        let link_dir = dir.path().join("link");
        fs::create_dir_all(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let real_disk = real_dir.join("shared.img");
        let link_disk = link_dir.join("shared.img");
        fs::write(&real_disk, vec![0u8; 4096]).unwrap();

        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let vm1 = sup
            .create_vm(CreateVmPayload {
                name: "RealPath-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                disks: vec![DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: real_disk.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .unwrap();

        // Creating or updating another VM with a symlinked path to the same RW disk must conflict.
        assert!(
            sup.create_vm(CreateVmPayload {
                name: "SymlinkPath-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                disks: vec![DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: link_disk.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .is_err()
        );

        let vm2 = sup
            .create_vm(CreateVmPayload {
                name: "LinkTarget-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        assert!(
            sup.attach_disk(
                &vm2.config.id.to_string(),
                AttachDiskPayload {
                    path: link_disk.clone(),
                    lun: Some(0),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .is_err()
        );

        // If `path_keys` is missing an entry, `ensure_disks_not_conflicting_with_keys` refuses
        // to compare against a lexical key (and `collect_missing_disk_path_keys` flags it).
        {
            let guard = sup.vms.read().unwrap();
            let empty_keys = HashMap::new();
            let missing =
                collect_missing_disk_path_keys(&guard, Some(vm2.config.id), &vm1.config.disks, &empty_keys);
            assert!(missing.contains(&real_disk));
            assert!(
                ensure_disks_not_conflicting_with_keys(
                    &guard,
                    Some(vm2.config.id),
                    &vm1.config.disks,
                    &empty_keys,
                )
                .is_err()
            );
        }

        // Seed a second supervisor where one VM holds `real_disk` and another holds `link_disk`;
        // deleting the first with `delete_disks=true` must NOT delete `real_disk`.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = RegistryStore::new(dir2.path()).unwrap();
        let mut seed_a = VmConfig::from_preset(
            "Seed-Real",
            openvmms_core::models::VmPreset::Custom,
            &store2.vm_dir(Uuid::new_v4()),
        );
        seed_a.disks = vec![DiskAttachment {
            lun: 0,
            kind: DiskKind::HardDisk,
            path: real_disk.clone(),
            read_only: false,
            format: DiskFormat::Raw,
        }];
        store2.save_vm(&seed_a).unwrap();

        let mut seed_b = VmConfig::from_preset(
            "Seed-Link",
            openvmms_core::models::VmPreset::Custom,
            &store2.vm_dir(Uuid::new_v4()),
        );
        seed_b.disks = vec![DiskAttachment {
            lun: 0,
            kind: DiskKind::HardDisk,
            path: link_disk,
            read_only: false,
            format: DiskFormat::Raw,
        }];
        store2.save_vm(&seed_b).unwrap();

        let sup2 = Supervisor::new(dir2.path(), Some("127.0.0.1:0")).unwrap();
        sup2.delete_vm(&seed_a.id.to_string(), true).await.unwrap();
        assert!(
            real_disk.exists(),
            "disk referenced via symlink by another VM must not be deleted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_vm_clears_deleting_on_failure_and_drops_ghost_when_toml_gone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();

        // 1. Failing delete while `vm.toml` remains intact must clear `deleting = false`.
        let keep_vm = sup
            .create_vm(CreateVmPayload {
                name: "KeepOnErr-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let keep_logs = sup.store.vm_dir(keep_vm.config.id).join("logs");
        let keep_log_file = keep_logs.join("openvmm.log");
        fs::write(&keep_log_file, b"lock").unwrap();
        fs::set_permissions(&keep_logs, fs::Permissions::from_mode(0o555)).unwrap();

        struct RestorePerm(PathBuf);
        impl Drop for RestorePerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore_keep = RestorePerm(keep_logs.clone());

        if fs::remove_file(&keep_log_file).is_err() {
            assert!(sup.delete_vm(&keep_vm.config.id.to_string(), false).await.is_err());
            assert!(
                !sup.vms.read().unwrap()[&keep_vm.config.id].deleting,
                "failed delete must reset vm.deleting to false"
            );
            drop(restore_keep);
            sup.delete_vm(&keep_vm.config.id.to_string(), false)
                .await
                .unwrap();
        } else {
            drop(restore_keep);
        }

        // 2. Partial failure where `vm.toml` is already removed must drop the ghost VM and emit VmDeleted.
        let ghost_vm = sup
            .create_vm(CreateVmPayload {
                name: "GhostDrop-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let ghost_logs = sup.store.vm_dir(ghost_vm.config.id).join("logs");
        let ghost_log_file = ghost_logs.join("openvmm.log");
        fs::write(&ghost_log_file, b"lock").unwrap();
        fs::set_permissions(&ghost_logs, fs::Permissions::from_mode(0o555)).unwrap();
        let _restore_ghost = RestorePerm(ghost_logs.clone());

        if fs::remove_file(&ghost_log_file).is_err() {
            fs::remove_file(sup.store.vm_config_path(ghost_vm.config.id)).unwrap();
            let mut rx = sup.subscribe();
            assert!(
                sup.delete_vm(&ghost_vm.config.id.to_string(), false)
                    .await
                    .is_err()
            );
            assert!(
                sup.get_vm(&ghost_vm.config.id.to_string()).is_err(),
                "partial delete after vm.toml removal must not leave a ghost VM in memory"
            );
            let evt = rx.try_recv().unwrap();
            assert!(matches!(
                evt,
                DaemonEvent::VmDeleted { id, revision } if id == ghost_vm.config.id && revision > 0
            ));
        }
    }

    #[tokio::test]
    async fn delete_vm_orders_vm_deleted_revision_after_disk_removal_and_is_same_live_vm_guards() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let disk_path = dir.path().join("owned.img");
        fs::write(&disk_path, vec![0u8; 4096]).unwrap();

        let vm = sup
            .create_vm(CreateVmPayload {
                name: "OrderAndGuard-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                disks: vec![DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: disk_path.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .unwrap();
        let id = vm.config.id;

        // Verify `is_same_live_vm` rejects wrong status, wrong generation, and `deleting = true`.
        let gen0 = sup.generation_for_test(id);
        assert!(!sup.is_same_live_vm(id, gen0));
        sup.set_vm_status_for_test(id, VmStatus::Running);
        assert!(sup.is_same_live_vm(id, gen0));
        sup.set_vm_status_for_test(id, VmStatus::Paused);
        assert!(sup.is_same_live_vm(id, gen0));
        sup.vms.write().unwrap().get_mut(&id).unwrap().deleting = true;
        assert!(!sup.is_same_live_vm(id, gen0));
        sup.vms.write().unwrap().get_mut(&id).unwrap().deleting = false;
        sup.advance_lifecycle_for_test(id, VmStatus::Running);
        assert!(!sup.is_same_live_vm(id, gen0));
        sup.set_vm_status_for_test(id, VmStatus::Off);

        let mut rx = sup.subscribe();
        sup.delete_vm(&id.to_string(), true).await.unwrap();
        assert!(!disk_path.exists());
        let evt = rx.try_recv().unwrap();
        match evt {
            DaemonEvent::VmDeleted {
                id: del_id,
                revision,
            } => {
                assert_eq!(del_id, id);
                assert_eq!(revision, sup.host_info().event_revision);
                assert!(revision > vm.runtime.revision);
            }
            other => panic!("expected VmDeleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn offline_hotplug_conflict_on_starting_always_rolls_back_vm_toml_and_deleting_update_never_resurrects() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Arc::new(Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap());
        let disk_path = dir.path().join("extra.img");
        fs::write(&disk_path, vec![0u8; 4096]).unwrap();

        let created = sup
            .create_vm(CreateVmPayload {
                name: "RollbackGuard-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;

        // 1. Race offline attach_disk against a transition to VmStatus::Starting across several
        //    microsecond offsets to cover both plan-time and commit-time active_rpc_socket errors.
        for delay_us in [0u64, 50, 150, 300, 600, 1200] {
            sup.set_vm_status_for_test(id, VmStatus::Off);
            let sup_a = Arc::clone(&sup);
            let disk = disk_path.clone();
            let attach_fut = tokio::spawn(async move {
                sup_a
                    .attach_disk(
                        &id.to_string(),
                        AttachDiskPayload {
                            path: disk,
                            lun: Some(0),
                            read_only: false,
                            format: Some(DiskFormat::Raw),
                        },
                    )
                    .await
            });
            if delay_us > 0 {
                tokio::time::sleep(Duration::from_micros(delay_us)).await;
            }
            sup.set_vm_status_for_test(id, VmStatus::Starting);
            let res = attach_fut.await.unwrap();
            sup.set_vm_status_for_test(id, VmStatus::Off);

            if res.is_ok() {
                sup.detach_disk(&id.to_string(), 0).await.unwrap();
            } else {
                let on_disk = sup
                    .store
                    .load_all_vms()
                    .unwrap()
                    .into_iter()
                    .find(|v| v.id == id)
                    .unwrap();
                assert!(
                    on_disk.disks.is_empty(),
                    "failed attach_disk at delay {delay_us}us left unattached disk in vm.toml"
                );
            }
        }

        // 2. update_vm while deleting = true must reject and never resurrect a removed vm.toml.
        sup.vms.write().unwrap().get_mut(&id).unwrap().deleting = true;
        fs::remove_file(sup.store.vm_config_path(id)).unwrap();
        let upd_err = sup
            .update_vm(
                &id.to_string(),
                UpdateVmPayload {
                    description: Some("should-not-resurrect".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{upd_err:#}").contains("being deleted"));
        assert!(
            !sup.store.vm_config_path(id).exists(),
            "update_vm on deleting VM must not write or resurrect vm.toml"
        );
        sup.vms.write().unwrap().get_mut(&id).unwrap().deleting = false;
    }

    #[tokio::test]
    async fn legacy_vm_with_out_of_range_memory_supports_rename_attach_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let disk_path = dir.path().join("legacy-attach.img");
        fs::write(&disk_path, vec![0u8; 4096]).unwrap();

        let legacy_id;
        {
            let store = RegistryStore::new(dir.path()).unwrap();
            let mut legacy = VmConfig::from_preset(
                "LegacyMem",
                openvmms_core::models::VmPreset::Custom,
                dir.path(),
            );
            legacy.memory_mb = 64; // below current minimum of 128 MiB
            legacy_id = legacy.id;
            store.save_vm_unchecked(&legacy).unwrap();
        }

        let sup = Arc::new(Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap());

        // Rename succeeds without touching memory_mb.
        let renamed = sup
            .update_vm(
                &legacy_id.to_string(),
                UpdateVmPayload {
                    name: Some("LegacyRenamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.config.name, "LegacyRenamed");
        assert_eq!(renamed.config.memory_mb, 64);

        // Attaching a disk succeeds even though memory_mb = 64.
        let attached = sup
            .attach_disk(
                &legacy_id.to_string(),
                AttachDiskPayload {
                    path: disk_path,
                    lun: Some(0),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap();
        assert_eq!(attached.config.disks.len(), 1);
        assert_eq!(attached.config.memory_mb, 64);

        // Explicitly updating memory_mb to an out-of-range value is still rejected.
        assert!(
            sup.update_vm(
                &legacy_id.to_string(),
                UpdateVmPayload {
                    memory_mb: Some(32),
                    ..Default::default()
                },
            )
            .await
            .is_err()
        );

        // Rollback restoring prev_cfg with memory_mb = 64 must succeed so vm.toml and memory stay in sync.
        let mut modified = attached.config.clone();
        modified.memory_mb = 512;
        sup.store.save_vm_unchecked(&modified).unwrap();
        sup.rollback_vm_config(legacy_id, "test rollback", attached.config.clone())
            .await;
        let on_disk = sup
            .store
            .load_all_vms()
            .unwrap()
            .into_iter()
            .find(|v| v.id == legacy_id)
            .unwrap();
        assert_eq!(on_disk.memory_mb, 64);

        // Racing an update that fixes memory_mb=512 against a concurrent name collision
        // rolls vm.toml back to memory_mb=64 instead of leaving vm.toml and memory out of sync.
        for delay_us in [0u64, 50, 150, 400, 900] {
            let clash_name = format!("Clash-{delay_us}");
            let sup_u = Arc::clone(&sup);
            let name_u = clash_name.clone();
            let upd_fut = tokio::spawn(async move {
                sup_u
                    .update_vm(
                        &legacy_id.to_string(),
                        UpdateVmPayload {
                            name: Some(name_u),
                            memory_mb: Some(512),
                            ..Default::default()
                        },
                    )
                    .await
            });
            if delay_us > 0 {
                tokio::time::sleep(Duration::from_micros(delay_us)).await;
            }
            let clash_vm = sup.create_vm(CreateVmPayload {
                name: clash_name,
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            });
            let upd_res = upd_fut.await.unwrap();
            if let Ok(created) = clash_vm {
                let _ = sup.delete_vm(&created.config.id.to_string(), false).await;
            }
            if upd_res.is_ok() {
                // Revert back to 64 for next iteration.
                sup.store.save_vm_unchecked(&attached.config).unwrap();
                sup.vms.write().unwrap().get_mut(&legacy_id).unwrap().config =
                    attached.config.clone();
            } else {
                let reloaded = sup
                    .store
                    .load_all_vms()
                    .unwrap()
                    .into_iter()
                    .find(|v| v.id == legacy_id)
                    .unwrap();
                let in_mem = sup.get_vm(&legacy_id.to_string()).unwrap().config;
                assert_eq!(reloaded.memory_mb, in_mem.memory_mb);
                assert_eq!(reloaded.memory_mb, 64);
            }
        }
    }

    #[tokio::test]
    async fn avhdx_inspect_and_attach_without_explicit_format_agree() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();

        let avhdx_path = dir.path().join("differencing.avhdx");
        let mut fake_vhdx = b"vhdxfile".to_vec();
        fake_vhdx.resize(4096, 0);
        fs::write(&avhdx_path, &fake_vhdx).unwrap();

        let vm = sup
            .create_vm(CreateVmPayload {
                name: "Avhdx-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .unwrap();

        // Attaching `.avhdx` without explicit format must map it to Vhdx, not Raw.
        let attached = sup
            .attach_disk(
                &vm.config.id.to_string(),
                AttachDiskPayload {
                    path: avhdx_path.clone(),
                    lun: Some(0),
                    read_only: false,
                    format: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(attached.config.disks[0].format, DiskFormat::Vhdx);
        assert_eq!(attached.config.disks[0].kind, DiskKind::HardDisk);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let img_script = dir.path().join("fake-openvmm-img.sh");
            fs::write(
                &img_script,
                "#!/bin/sh\necho '{\"disk_size\":65536,\"type\":\"differencing\",\"block_size\":2097152,\"logical_sector_size\":512}'\n",
            )
            .unwrap();
            fs::set_permissions(&img_script, fs::Permissions::from_mode(0o755)).unwrap();
            let mut host_cfg = sup.host_config();
            host_cfg.openvmm_img_bin = Some(img_script);
            sup.update_host_config(host_cfg).await.unwrap();

            let info = sup.inspect_disk(&avhdx_path).unwrap();
            assert_eq!(info.format, "vhdx");
            assert_eq!(info.disk_type, "differencing");
            assert_eq!(info.virtual_size_bytes, 65536);
        }
    }

    #[tokio::test]
    async fn cross_vm_port_forward_conflicts_are_rejected_only_against_active_vms() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();

        let mut nic_a = NicAttachment::default_consomme();
        nic_a.backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 8080,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        let vm_a = sup
            .create_vm(CreateVmPayload {
                name: "PortA-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                nics: Some(vec![nic_a]),
                ..Default::default()
            })
            .unwrap();

        // Stopped VMs may share overlapping rules via create_vm and unrelated NIC edits.
        let mut nic_b = NicAttachment::default_consomme();
        nic_b.backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "0.0.0.0".into(),
                host_port: 8080,
                guest_port: 81,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        let vm_b = sup
            .create_vm(CreateVmPayload {
                name: "PortB-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                nics: Some(vec![nic_b]),
                ..Default::default()
            })
            .unwrap();

        let mut edited_b_nics = vm_b.config.nics.clone();
        if let NicBackendConfig::Consomme { internal_dns, .. } = &mut edited_b_nics[0].backend {
            *internal_dns = false;
        }
        sup.update_vm(
            &vm_b.config.id.to_string(),
            UpdateVmPayload {
                nics: Some(edited_b_nics),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Stopped VM may also add an overlapping rule via offline add_port_forward and update_vm.
        let vm_c = sup
            .create_vm(CreateVmPayload {
                name: "PortC-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .unwrap();
        let nic_c_id = vm_c.config.nics[0].id;
        let exact_pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 8080,
            guest_port: 82,
        };
        sup.add_port_forward(&vm_c.config.id.to_string(), nic_c_id, exact_pf.clone())
            .await
            .unwrap();

        let mut wild_nics = vm_c.config.nics.clone();
        wild_nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "0.0.0.0".into(),
                host_port: 8080,
                guest_port: 82,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        sup.update_vm(
            &vm_c.config.id.to_string(),
            UpdateVmPayload {
                nics: Some(wild_nics),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Once PortA-VM is active, starting a VM with an overlapping wildcard or exact rule fails.
        sup.vms
            .write()
            .unwrap()
            .get_mut(&vm_a.config.id)
            .unwrap()
            .runtime
            .status = VmStatus::Running;

        let err_wild = sup
            .start_vm(&vm_c.config.id.to_string())
            .await
            .unwrap_err();
        assert!(
            format!("{err_wild:#}").contains("overlaps existing rule"),
            "unexpected error for wildcard start: {err_wild:#}"
        );

        let mut exact_nics = vm_c.config.nics.clone();
        exact_nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![exact_pf],
            internal_dns: true,
            allow_host_local_access: false,
        };
        sup.update_vm(
            &vm_c.config.id.to_string(),
            UpdateVmPayload {
                nics: Some(exact_nics),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let err_exact = sup
            .start_vm(&vm_c.config.id.to_string())
            .await
            .unwrap_err();
        assert!(
            format!("{err_exact:#}").contains("overlaps existing rule"),
            "unexpected error for exact start: {err_exact:#}"
        );
        assert_eq!(
            sup.get_vm(&vm_c.config.id.to_string())
                .unwrap()
                .runtime
                .status,
            VmStatus::Off
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_vm_with_partial_disk_deletion_failure_returns_error_after_deleting_vm() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();

        let deletable_disk = dir.path().join("deletable.img");
        fs::write(&deletable_disk, vec![0u8; 4096]).unwrap();

        let locked_dir = dir.path().join("locked_disks");
        fs::create_dir_all(&locked_dir).unwrap();
        let stuck_disk = locked_dir.join("stuck.img");
        fs::write(&stuck_disk, vec![0u8; 4096]).unwrap();

        let vm = sup
            .create_vm(CreateVmPayload {
                name: "PartialDiskDel-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![
                    DiskAttachment {
                        lun: 0,
                        kind: DiskKind::HardDisk,
                        path: deletable_disk.clone(),
                        read_only: false,
                        format: DiskFormat::Raw,
                    },
                    DiskAttachment {
                        lun: 1,
                        kind: DiskKind::HardDisk,
                        path: stuck_disk.clone(),
                        read_only: false,
                        format: DiskFormat::Raw,
                    },
                ],
                ..Default::default()
            })
            .unwrap();
        let id = vm.config.id;

        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();
        struct RestorePerm(PathBuf);
        impl Drop for RestorePerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let _restore = RestorePerm(locked_dir.clone());

        if fs::remove_file(&stuck_disk).is_err() {
            let mut rx = sup.subscribe();
            let err = sup.delete_vm(&id.to_string(), true).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("failed to remove disk file"),
                "unexpected error: {err:#}"
            );
            assert!(!deletable_disk.exists());
            assert!(stuck_disk.exists());
            assert!(sup.get_vm(&id.to_string()).is_err());
            let evt = rx.try_recv().unwrap();
            assert!(matches!(
                evt,
                DaemonEvent::VmDeleted { id: del_id, .. } if del_id == id
            ));
        }
    }

    #[tokio::test]
    async fn start_vm_rejects_legacy_vm_with_duplicate_same_nic_guest_port_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let mut legacy = openvmms_core::models::VmConfig::from_preset(
            "LegacySameNicPort-VM",
            openvmms_core::models::VmPreset::Custom,
            &store.vm_dir(Uuid::new_v4()),
        );
        legacy.nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![
                PortForward {
                    protocol: IpProto::Tcp,
                    host_addr: "127.0.0.1".into(),
                    host_port: 8080,
                    guest_port: 80,
                },
                PortForward {
                    protocol: IpProto::Tcp,
                    host_addr: "127.0.0.2".into(),
                    host_port: 8081,
                    guest_port: 80,
                },
            ],
            internal_dns: true,
            allow_host_local_access: false,
        };
        store.save_vm_unchecked(&legacy).unwrap();

        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let err = sup.start_vm(&legacy.id.to_string()).await.unwrap_err();
        assert!(
            matches!(err.downcast_ref::<ApiErrorKind>(), Some(ApiErrorKind::Conflict(_))),
            "expected HTTP 409 conflict for duplicate same-NIC guest port, got: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("duplicate guest port for protocol and IP family"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            sup.get_vm(&legacy.id.to_string()).unwrap().runtime.status,
            VmStatus::Off
        );

        // Two different adapters with the same guest port still pass the port-forward check.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = openvmms_core::store::RegistryStore::new(dir2.path()).unwrap();
        let mut multi_nic = openvmms_core::models::VmConfig::from_preset(
            "LegacyMultiNicPort-VM",
            openvmms_core::models::VmPreset::Custom,
            &store2.vm_dir(Uuid::new_v4()),
        );
        multi_nic.nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 8082,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        let mut nic2 = NicAttachment::default_consomme();
        nic2.name = "NAT-2".into();
        nic2.backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 8083,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        multi_nic.nics.push(nic2);
        store2.save_vm_unchecked(&multi_nic).unwrap();

        let sup2 = Supervisor::new(dir2.path(), Some("127.0.0.1:0")).unwrap();
        let err2 = sup2.start_vm(&multi_nic.id.to_string()).await.unwrap_err();
        assert!(
            !format!("{err2:#}").contains("duplicate guest port"),
            "cross-NIC guest ports must not trigger same-NIC conflict: {err2:#}"
        );
    }

    #[tokio::test]
    async fn start_vm_rejects_intra_vm_overlapping_and_localhost_port_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let mut legacy = openvmms_core::models::VmConfig::from_preset(
            "IntraVmOverlap-VM",
            openvmms_core::models::VmPreset::Custom,
            &store.vm_dir(Uuid::new_v4()),
        );
        legacy.nics[0].name = "nat0".into();
        legacy.nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 8080,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        let mut nat1 = NicAttachment::default_consomme();
        nat1.name = "nat1".into();
        nat1.backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "0.0.0.0".into(),
                host_port: 8080,
                guest_port: 81,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        legacy.nics.push(nat1);
        store.save_vm_unchecked(&legacy).unwrap();

        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let err = sup.start_vm(&legacy.id.to_string()).await.unwrap_err();
        assert!(
            matches!(err.downcast_ref::<ApiErrorKind>(), Some(ApiErrorKind::Conflict(_))),
            "expected HTTP 409 conflict for intra-VM overlapping host port, got: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("overlaps existing rule"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            sup.get_vm(&legacy.id.to_string()).unwrap().runtime.status,
            VmStatus::Off
        );

        // Legacy "localhost" duplicates on the same NIC are also caught as 409 Conflict.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = openvmms_core::store::RegistryStore::new(dir2.path()).unwrap();
        let mut localhost_dup = openvmms_core::models::VmConfig::from_preset(
            "LocalhostDup-VM",
            openvmms_core::models::VmPreset::Custom,
            &store2.vm_dir(Uuid::new_v4()),
        );
        localhost_dup.nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![
                PortForward {
                    protocol: IpProto::Tcp,
                    host_addr: "localhost".into(),
                    host_port: 8080,
                    guest_port: 80,
                },
                PortForward {
                    protocol: IpProto::Tcp,
                    host_addr: "localhost".into(),
                    host_port: 8081,
                    guest_port: 80,
                },
            ],
            internal_dns: true,
            allow_host_local_access: false,
        };
        store2.save_vm_unchecked(&localhost_dup).unwrap();

        let sup2 = Supervisor::new(dir2.path(), Some("127.0.0.1:0")).unwrap();
        let err2 = sup2.start_vm(&localhost_dup.id.to_string()).await.unwrap_err();
        assert!(
            matches!(err2.downcast_ref::<ApiErrorKind>(), Some(ApiErrorKind::Conflict(_))),
            "expected HTTP 409 conflict for duplicate localhost guest port, got: {err2:#}"
        );
    }

    #[tokio::test]
    async fn stop_vm_in_error_state_bumps_revision_and_emits_vm_updated() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let cfg = openvmms_core::models::VmConfig::from_preset(
            "ErrorStop-VM",
            openvmms_core::models::VmPreset::Custom,
            &store.vm_dir(Uuid::new_v4()),
        );
        store.save_vm(&cfg).unwrap();

        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let initial_rev = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&cfg.id).unwrap();
            vm.runtime.status = VmStatus::Error;
            vm.runtime.last_error = Some("boot failure".into());
            vm.runtime.revision = sup.next_revision();
            vm.runtime.revision
        };

        let mut rx = sup.subscribe();
        let stopped = sup.stop_vm(&cfg.id.to_string()).await.unwrap();
        assert_eq!(stopped.runtime.status, VmStatus::Off);
        assert_eq!(stopped.runtime.last_error, None);
        assert!(stopped.runtime.revision > initial_rev);

        let evt = rx.try_recv().unwrap();
        match evt {
            DaemonEvent::VmUpdated { vm } => {
                assert_eq!(vm.config.id, cfg.id);
                assert_eq!(vm.runtime.status, VmStatus::Off);
                assert_eq!(vm.runtime.last_error, None);
                assert_eq!(vm.runtime.revision, stopped.runtime.revision);
            }
            other => panic!("expected VmUpdated event, got {other:?}"),
        }

        // Idempotent stop when already Off with no error does not emit another event.
        let again = sup.stop_vm(&cfg.id.to_string()).await.unwrap();
        assert_eq!(again.runtime.revision, stopped.runtime.revision);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stop_vm_rejects_when_vm_is_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let cfg = openvmms_core::models::VmConfig::from_preset(
            "DeletingStop-VM",
            openvmms_core::models::VmPreset::Custom,
            &store.vm_dir(Uuid::new_v4()),
        );
        store.save_vm(&cfg).unwrap();

        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let initial_rev = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&cfg.id).unwrap();
            vm.runtime.status = VmStatus::Error;
            vm.runtime.last_error = Some("boot failure".into());
            vm.runtime.revision = sup.next_revision();
            vm.deleting = true;
            vm.runtime.revision
        };

        let mut rx = sup.subscribe();
        let err = sup.stop_vm(&cfg.id.to_string()).await.unwrap_err();
        assert!(
            matches!(err.downcast_ref::<ApiErrorKind>(), Some(ApiErrorKind::Conflict(_))),
            "expected Conflict error, got: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("being deleted"),
            "unexpected error message: {err:#}"
        );
        assert!(rx.try_recv().is_err());

        let vm = sup.get_vm(&cfg.id.to_string()).unwrap();
        assert_eq!(vm.runtime.status, VmStatus::Error);
        assert_eq!(vm.runtime.last_error.as_deref(), Some("boot failure"));
        assert_eq!(vm.runtime.revision, initial_rev);
    }

    #[tokio::test]
    async fn stop_vm_returns_ok_when_deleting_starts_after_stop_reaches_off() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "StopThenDeleting-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;
        let (stop_tx, stop_rx) = watch::channel(StopOutcome::Pending);
        {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Stopping;
            vm.stop_wait = Some(stop_rx);
        }

        let sup_clone = Arc::clone(&sup);
        let handle = tokio::spawn(async move { sup_clone.stop_vm(&id.to_string()).await });
        tokio::task::yield_now().await;

        let stopped_summary = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Off;
            vm.stop_wait = None;
            vm.deleting = true;
            vm.summary()
        };
        let _ = stop_tx.send(StopOutcome::Done(Some(stopped_summary)));

        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.runtime.status, VmStatus::Off);
    }

    #[tokio::test]
    async fn stop_vm_returns_ok_when_vm_deleted_after_stop_completes() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "StopThenRemoved-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;
        let (stop_tx, stop_rx) = watch::channel(StopOutcome::Pending);
        {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Stopping;
            vm.stop_wait = Some(stop_rx);
        }

        let sup_clone = Arc::clone(&sup);
        let handle = tokio::spawn(async move { sup_clone.stop_vm(&id.to_string()).await });
        tokio::task::yield_now().await;

        let stopped_summary = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Off;
            vm.stop_wait = None;
            let s = vm.summary();
            guard.remove(&id);
            s
        };
        let _ = stop_tx.send(StopOutcome::Done(Some(stopped_summary)));

        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.runtime.status, VmStatus::Off);
    }

    #[tokio::test]
    async fn patch_listen_addr_compares_with_live_port_and_preserves_port_zero_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();

        let probe_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = probe_a.local_addr().unwrap();
        let probe_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = probe_b.local_addr().unwrap();
        drop(probe_a);
        drop(probe_b);

        let initial_cfg = HostConfig {
            listen_addr: addr_b.to_string(),
            data_dir: dir.path().to_path_buf(),
            runtime_dir: openvmms_core::models::default_runtime_dir(dir.path()),
            ..Default::default()
        };
        store.save_host_config(&initial_cfg).unwrap();

        // Start daemon with --listen addr_a while config.toml has addr_b.
        let sup = Supervisor::new(dir.path(), Some(&addr_a.to_string())).unwrap();
        let listener_a = tokio::net::TcpListener::bind(addr_a).await.unwrap();
        sup.serve_on_listener(listener_a).await.unwrap();
        assert_eq!(sup.listen_addr(), addr_a.to_string());

        // PATCH listen_addr to addr_b (matching config.toml, differing from live port addr_a) must rebind.
        let patched_b = sup
            .patch_host_config(HostConfigPatch {
                listen_addr: Some(addr_b.to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(patched_b.listen_addr, addr_b.to_string());
        assert_eq!(sup.listen_addr(), addr_b.to_string());
        assert_eq!(
            store.read_runtime_listen_addr().unwrap(),
            addr_b.to_string()
        );

        // PATCH listen_addr to 127.0.0.1:0 must rebind to a live ephemeral port while keeping 127.0.0.1:0 in config.toml.
        let patched_zero = sup
            .patch_host_config(HostConfigPatch {
                listen_addr: Some("127.0.0.1:0".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(patched_zero.listen_addr, "127.0.0.1:0");
        assert_eq!(
            store.load_host_config_or_default().unwrap().listen_addr,
            "127.0.0.1:0"
        );
        let live_after_zero: SocketAddr = sup.listen_addr().parse().unwrap();
        assert_ne!(live_after_zero.port(), 0);
        assert_eq!(
            store.read_runtime_listen_addr().unwrap(),
            live_after_zero.to_string()
        );

        sup.mark_shutting_down();
        sup.join_all_listeners().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_vm_with_unreadable_vm_dir_keeps_vm_and_skips_vm_deleted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let vm = sup
            .create_vm(CreateVmPayload {
                name: "LockedDir-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm_dir = sup.store.vm_dir(vm.config.id);
        let vm_toml = sup.store.vm_config_path(vm.config.id);

        struct RestoreDirPerm(PathBuf);
        impl Drop for RestoreDirPerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore = RestoreDirPerm(vm_dir.clone());
        fs::set_permissions(&vm_dir, fs::Permissions::from_mode(0o000)).unwrap();

        if fs::metadata(&vm_toml).is_err() {
            let mut rx = sup.subscribe();
            let err = sup.delete_vm(&vm.config.id.to_string(), false).await.unwrap_err();
            assert!(!format!("{err:#}").is_empty());
            assert!(
                sup.get_vm(&vm.config.id.to_string()).is_ok(),
                "VM must remain in memory when vm.toml cannot be removed"
            );
            assert!(
                !sup.vms.read().unwrap()[&vm.config.id].deleting,
                "failed delete must reset deleting=false"
            );
            assert!(
                rx.try_recv().is_err(),
                "VmDeleted must not be emitted when vm.toml status is unknown/unreadable"
            );
            drop(restore);
            assert!(vm_toml.is_file(), "vm.toml must still exist on disk");
            sup.delete_vm(&vm.config.id.to_string(), false).await.unwrap();
        } else {
            drop(restore);
        }
    }

    #[tokio::test]
    async fn wait_for_in_flight_mutations_blocks_until_tracked_task_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let started_tx = Arc::clone(&started);
        let release = Arc::new(tokio::sync::Notify::new());
        let release_rx = Arc::clone(&release);
        let finished = Arc::new(AtomicBool::new(false));
        let finished_flag = Arc::clone(&finished);

        let task = sup.spawn_tracked_mutation(async move {
            started_tx.notify_one();
            release_rx.notified().await;
            finished_flag.store(true, Ordering::Release);
        });

        started.notified().await;
        let sup_wait = Arc::clone(&sup);
        let wait_handle = tokio::spawn(async move {
            sup_wait
                .wait_for_in_flight_mutations(Duration::from_secs(5))
                .await;
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!wait_handle.is_finished());
        assert!(!finished.load(Ordering::Acquire));

        release.notify_one();
        wait_handle.await.unwrap();
        assert!(finished.load(Ordering::Acquire));
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_rollback_retains_pending_disk_and_port_reservations_until_vm_stops() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        fn spawn_hotplug_ok_rollback_fail_server(sock_path: &Path) -> std::thread::JoinHandle<()> {
            let listener = UnixListener::bind(sock_path).unwrap();
            std::thread::spawn(move || {
                // Handle 1st RPC (hot-add -> Ok) and 2nd RPC (rollback -> Err) for disk,
                // then 3rd RPC (hot-add -> Ok) and 4th RPC (rollback -> Err) for port.
                for idx in 0..4 {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    let Ok((stream_id, _, _)) = read_frame(&mut stream) else {
                        break;
                    };
                    let resp = if idx % 2 == 0 {
                        TestWireResponse {
                            result: Some(test_wire_response::Result::Payload(Vec::new())),
                        }
                    } else {
                        TestWireResponse {
                            result: Some(test_wire_response::Result::Status(TtrpcStatus {
                                code: 13,
                                message: "injected rollback failure".into(),
                                details: Vec::new(),
                            })),
                        }
                    };
                    if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                        let _ = stream.write_all(&frame);
                        let _ = stream.flush();
                    }
                }
            })
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let shared_disk = dir.path().join("shared.img");
        fs::write(&shared_disk, vec![0u8; 4096]).unwrap();

        let vm1 = sup
            .create_vm(CreateVmPayload {
                name: "VM1".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm2 = sup
            .create_vm(CreateVmPayload {
                name: "VM2".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm1_id = vm1.config.id;
        let vm1_nic = vm1.config.nics[0].id;
        let vm2_id = vm2.config.id;
        let vm2_nic = vm2.config.nics[0].id;

        let sock_path = dir.path().join("vm1.sock");
        let server = spawn_hotplug_ok_rollback_fail_server(&sock_path);

        {
            let mut guard = sup.vms.write().unwrap();
            let m1 = guard.get_mut(&vm1_id).unwrap();
            m1.runtime.status = VmStatus::Running;
            m1.runtime.rpc_socket_path = Some(sock_path);
            m1.generation = 1;
        }

        let vm1_dir = sup.store.vm_dir(vm1_id);
        struct RestorePerm(PathBuf);
        impl Drop for RestorePerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore = RestorePerm(vm1_dir.clone());
        fs::set_permissions(&vm1_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let can_write_vm1_dir = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(vm1_dir.join(".probe"))
            .is_ok();
        if !can_write_vm1_dir {
            let mut events_rx = sup.subscribe();
            // 1. Hot-add disk succeeds, save fails, rollback fails -> reservation must remain active.
            let disk_err = sup
                .attach_disk(
                    &vm1_id.to_string(),
                    AttachDiskPayload {
                        lun: Some(1),
                        path: shared_disk.clone(),
                        read_only: false,
                        format: Some(DiskFormat::Raw),
                    },
                )
                .await
                .unwrap_err();
            assert!(!format!("{disk_err:#}").is_empty());

            let summary_after_disk = sup.get_vm(&vm1_id.to_string()).unwrap();
            assert_eq!(summary_after_disk.runtime.orphaned_disks.len(), 1);
            assert_eq!(
                summary_after_disk.runtime.orphaned_disks[0].path,
                shared_disk
            );
            assert!(matches!(
                events_rx.try_recv(),
                Ok(DaemonEvent::VmUpdated { vm }) if vm.runtime.orphaned_disks.len() == 1
            ));

            // Conflicting attach on VM2 must be rejected with 409 Conflict.
            let conflict_disk = sup
                .attach_disk(
                    &vm2_id.to_string(),
                    AttachDiskPayload {
                        lun: Some(1),
                        path: shared_disk.clone(),
                        read_only: false,
                        format: Some(DiskFormat::Raw),
                    },
                )
                .await
                .unwrap_err();
            assert!(
                matches!(
                    conflict_disk.downcast_ref::<ApiErrorKind>(),
                    Some(ApiErrorKind::Conflict(_))
                ),
                "expected 409 conflict for un-rolled-back pending disk, got: {conflict_disk:#}"
            );

            // 2. Hot-add port forward succeeds, save fails, rollback fails -> reservation must remain active.
            let pf = PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 19022,
                guest_port: 22,
            };
            let pf_err = sup
                .add_port_forward(&vm1_id.to_string(), vm1_nic, pf.clone())
                .await
                .unwrap_err();
            assert!(!format!("{pf_err:#}").is_empty());

            let summary_after_pf = sup.get_vm(&vm1_id.to_string()).unwrap();
            assert_eq!(summary_after_pf.runtime.orphaned_port_forwards.len(), 1);
            assert_eq!(
                summary_after_pf.runtime.orphaned_port_forwards[0].forward,
                pf
            );
            assert!(matches!(
                events_rx.try_recv(),
                Ok(DaemonEvent::VmUpdated { vm }) if vm.runtime.orphaned_port_forwards.len() == 1
            ));

            // Starting or adding overlapping port on active VM2 must be rejected with 409 Conflict.
            drop(restore);
            sup.add_port_forward(&vm2_id.to_string(), vm2_nic, pf.clone())
                .await
                .unwrap();
            let conflict_start = sup.start_vm(&vm2_id.to_string()).await.unwrap_err();
            assert!(
                matches!(
                    conflict_start.downcast_ref::<ApiErrorKind>(),
                    Some(ApiErrorKind::Conflict(_))
                ),
                "expected 409 conflict for un-rolled-back pending port, got: {conflict_start:#}"
            );

            // Once VM1 stops, pending & orphaned reservations clear and VM2 can attach the shared disk.
            let stopped_vm1 = sup.stop_vm(&vm1_id.to_string()).await.unwrap();
            assert!(stopped_vm1.runtime.orphaned_disks.is_empty());
            assert!(stopped_vm1.runtime.orphaned_port_forwards.is_empty());
            let fetched_vm1 = sup.get_vm(&vm1_id.to_string()).unwrap();
            assert!(fetched_vm1.runtime.orphaned_disks.is_empty());
            assert!(fetched_vm1.runtime.orphaned_port_forwards.is_empty());
            sup.attach_disk(
                &vm2_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk,
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap();
        } else {
            drop(restore);
        }
        drop(server);
    }

    #[tokio::test]
    async fn stop_vm_returns_not_found_when_vm_removed_while_waiting_for_stop() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let vm = sup
            .create_vm(CreateVmPayload {
                name: "stop-race-delete".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm_id = vm.config.id;
        let (stop_tx, stop_rx) = watch::channel(StopOutcome::Pending);
        {
            let mut guard = sup.vms.write().unwrap();
            let entry = guard.get_mut(&vm_id).unwrap();
            entry.runtime.status = VmStatus::Stopping;
            entry.stop_wait = Some(stop_rx);
        }

        let sup_clone = Arc::clone(&sup);
        let handle = tokio::spawn(async move { sup_clone.stop_vm(&vm_id.to_string()).await });
        tokio::task::yield_now().await;
        {
            let mut guard = sup.vms.write().unwrap();
            guard.remove(&vm_id);
        }
        let _ = stop_tx.send(StopOutcome::Done(None));

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            matches!(err.downcast_ref::<ApiErrorKind>(), Some(ApiErrorKind::NotFound(_))),
            "expected NotFound (404) when VM is removed while stop_vm waits, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn stop_vm_returns_ok_when_monitor_exits_to_error_before_concurrent_delete() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "MonitorErrThenDelete-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;
        let (stop_tx, stop_rx) = watch::channel(StopOutcome::Pending);
        {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Stopping;
            vm.stop_wait = Some(stop_rx);
        }

        let sup_clone = Arc::clone(&sup);
        let stop_handle = tokio::spawn(async move { sup_clone.stop_vm(&id.to_string()).await });
        tokio::task::yield_now().await;

        let done_summary = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Error;
            vm.runtime.last_error = Some("crashed".into());
            vm.stop_wait = None;
            vm.deleting = true;
            let mut s = vm.summary();
            s.runtime.status = VmStatus::Off;
            s.runtime.last_error = None;
            s
        };
        let _ = stop_tx.send(StopOutcome::Done(Some(done_summary)));

        let res = stop_handle.await.unwrap().unwrap();
        assert_eq!(res.runtime.status, VmStatus::Off);
        assert!(res.runtime.last_error.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_vm_error_exit_sends_normalized_off_done_summary() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::net::UnixListener;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "MonitorErrDoneSummary-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;
        let run_gen = 1u64;
        let sock_path = sup.host_config().runtime_dir.join(format!("{id}.mon.sock"));
        fs::create_dir_all(sock_path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (allow_finish_tx, allow_finish_rx) = std::sync::mpsc::channel::<()>();

        let srv_sock = sock_path.clone();
        let srv = std::thread::spawn(move || {
            // 1. Initial PropertiesVm -> Running
            if let Ok((mut s, _)) = listener.accept()
                && let Ok((stream_id, _, _)) = read_frame(&mut s)
            {
                let payload = vmservice::PropertiesVmResponse {
                    state: vmservice::VmState::Running as i32,
                    ..Default::default()
                }
                .encode_to_vec();
                let resp = TestWireResponse {
                    result: Some(test_wire_response::Result::Payload(payload)),
                };
                if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                    let _ = s.write_all(&frame);
                    let _ = s.flush();
                }
            }
            // 2. WaitVM -> fail with RPC error
            if let Ok((mut s, _)) = listener.accept()
                && let Ok((stream_id, _, _)) = read_frame(&mut s)
            {
                let resp = TestWireResponse {
                    result: Some(test_wire_response::Result::Status(TtrpcStatus {
                        code: 13,
                        message: "unexpected vm crash".into(),
                        details: Vec::new(),
                    })),
                };
                if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                    let _ = s.write_all(&frame);
                    let _ = s.flush();
                }
            }
            // 3. PropertiesVm -> return non-live Uninitialized state (not Halted) so monitor transitions to Error
            if let Ok((mut s, _)) = listener.accept()
                && let Ok((stream_id, _, _)) = read_frame(&mut s)
            {
                let payload = vmservice::PropertiesVmResponse {
                    state: vmservice::VmState::Uninitialized as i32,
                    ..Default::default()
                }
                .encode_to_vec();
                let resp = TestWireResponse {
                    result: Some(test_wire_response::Result::Payload(payload)),
                };
                if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                    let _ = s.write_all(&frame);
                    let _ = s.flush();
                }
            }
            // 4. TeardownVm -> hold connection until test reads vm.stop_wait, then drop listener & socket
            if let Ok((mut s, _)) = listener.accept() {
                let _ = read_frame(&mut s);
                let _ = allow_finish_rx.recv_timeout(Duration::from_secs(5));
                drop(s);
            }
            drop(listener);
            let _ = fs::remove_file(&srv_sock);
        });

        {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.generation = run_gen;
            vm.runtime.status = VmStatus::Running;
            vm.runtime.rpc_socket_path = Some(sock_path.clone());
        }

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mon_sup = Arc::clone(&sup);
        let mon_handle = tokio::spawn(async move {
            mon_sup.monitor_vm(id, run_gen, sock_path, cancel_rx).await;
        });

        let mut mon_stop_rx = None;
        for _ in 0..200 {
            if let Some(rx) = sup.vms.read().unwrap().get(&id).and_then(|vm| vm.stop_wait.clone()) {
                mon_stop_rx = Some(rx);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut mon_stop_rx =
            mon_stop_rx.expect("monitor_vm should install stop_wait before teardown");
        let _ = allow_finish_tx.send(());
        mon_handle.await.unwrap();
        srv.join().unwrap();

        match mon_stop_rx.borrow_and_update().clone() {
            StopOutcome::Done(Some(s)) => {
                assert_eq!(s.runtime.status, VmStatus::Off);
                assert!(s.runtime.last_error.is_none());
            }
            _ => panic!("expected monitor exit to Error to emit normalized Off done_summary"),
        }
        assert_eq!(
            sup.vms.read().unwrap().get(&id).unwrap().runtime.status,
            VmStatus::Error
        );
    }

    #[tokio::test]
    async fn stop_vm_returns_ok_when_restarted_before_post_wait_relock() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let created = sup
            .create_vm(CreateVmPayload {
                name: "StopThenRestart-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let id = created.config.id;
        let (stop_tx, stop_rx) = watch::channel(StopOutcome::Pending);
        {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Stopping;
            vm.stop_wait = Some(stop_rx);
        }

        let sup_clone = Arc::clone(&sup);
        let handle = tokio::spawn(async move { sup_clone.stop_vm(&id.to_string()).await });
        tokio::task::yield_now().await;

        let stopped_summary = {
            let mut guard = sup.vms.write().unwrap();
            let vm = guard.get_mut(&id).unwrap();
            vm.runtime.status = VmStatus::Off;
            vm.stop_wait = None;
            let s = vm.summary();
            vm.runtime.status = VmStatus::Running;
            s
        };
        let _ = stop_tx.send(StopOutcome::Done(Some(stopped_summary)));

        let res = handle.await.unwrap().unwrap();
        assert_eq!(res.runtime.status, VmStatus::Off);
    }

    #[tokio::test]
    async fn update_host_config_locked_clears_empty_optional_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let mut cfg = sup.host_config();
        cfg.openvmm_bin = Some(PathBuf::from(""));
        cfg.fastdesk_viewer_bin = Some(PathBuf::from(""));
        cfg.openvmm_img_bin = Some(PathBuf::from(""));
        cfg.uefi_firmware_path = Some(PathBuf::from(""));

        let updated = sup.update_host_config_locked(cfg, false).await.unwrap();
        assert_eq!(updated.openvmm_bin, None);
        assert_eq!(updated.fastdesk_viewer_bin, None);
        assert_eq!(updated.openvmm_img_bin, None);
        assert_eq!(updated.uefi_firmware_path, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ambiguous_hotadd_drop_attempts_rollback_and_retains_reservation_when_rollback_fails() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::AtomicUsize;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let shared_disk = dir.path().join("ambiguous-shared.img");
        fs::write(&shared_disk, vec![0u8; 4096]).unwrap();

        let vm1 = sup
            .create_vm(CreateVmPayload {
                name: "VM1-Ambiguous".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm2 = sup
            .create_vm(CreateVmPayload {
                name: "VM2-Ambiguous".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm1_id = vm1.config.id;
        let vm1_nic = vm1.config.nics[0].id;
        let vm2_id = vm2.config.id;
        let vm2_nic = vm2.config.nics[0].id;

        let sock_path = dir.path().join("vm1-ambiguous.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_srv = Arc::clone(&call_count);
        let server = std::thread::spawn(move || {
            for idx in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Ok((stream_id, _, _)) = read_frame(&mut stream) else {
                    break;
                };
                call_count_srv.fetch_add(1, Ordering::SeqCst);
                if idx % 2 == 0 {
                    // Initial hot-add: drop connection without replying (ambiguous EOF).
                    drop(stream);
                } else {
                    // Rollback attempt: return explicit failure.
                    let resp = TestWireResponse {
                        result: Some(test_wire_response::Result::Status(TtrpcStatus {
                            code: 13,
                            message: "injected rollback failure".into(),
                            details: Vec::new(),
                        })),
                    };
                    if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                        let _ = stream.write_all(&frame);
                        let _ = stream.flush();
                    }
                }
            }
        });

        {
            let mut guard = sup.vms.write().unwrap();
            let m1 = guard.get_mut(&vm1_id).unwrap();
            m1.runtime.status = VmStatus::Running;
            m1.runtime.rpc_socket_path = Some(sock_path);
            m1.generation = 1;
        }

        // 1. Disk hot-add drops without replying -> rollback fails -> pending disk reservation retained.
        let disk_err = sup
            .attach_disk(
                &vm1_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap_err();
        assert!(!format!("{disk_err:#}").is_empty());

        let conflict_disk = sup
            .attach_disk(
                &vm2_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            conflict_disk.downcast_ref::<ApiErrorKind>(),
            Some(ApiErrorKind::Conflict(_))
        ));

        // 2. Port-forward hot-add drops without replying -> rollback fails -> pending port retained.
        let pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 19023,
            guest_port: 23,
        };
        let pf_err = sup
            .add_port_forward(&vm1_id.to_string(), vm1_nic, pf.clone())
            .await
            .unwrap_err();
        assert!(!format!("{pf_err:#}").is_empty());

        sup.add_port_forward(&vm2_id.to_string(), vm2_nic, pf.clone())
            .await
            .unwrap();
        let conflict_start = sup.start_vm(&vm2_id.to_string()).await.unwrap_err();
        assert!(matches!(
            conflict_start.downcast_ref::<ApiErrorKind>(),
            Some(ApiErrorKind::Conflict(_))
        ));
        assert_eq!(call_count.load(Ordering::SeqCst), 4);

        // Both ambiguous-RPC failed-rollback reservations are exposed in VmSummary while running.
        let summary_while_running = sup.get_vm(&vm1_id.to_string()).unwrap();
        assert_eq!(summary_while_running.runtime.orphaned_disks.len(), 1);
        assert_eq!(
            summary_while_running.runtime.orphaned_disks[0].path,
            shared_disk
        );
        assert_eq!(
            summary_while_running.runtime.orphaned_port_forwards,
            vec![OrphanedPortForward {
                nic_id: vm1_nic,
                forward: pf.clone(),
            }]
        );

        sup.stop_vm(&vm1_id.to_string()).await.unwrap();
        let summary_after_stop = sup.get_vm(&vm1_id.to_string()).unwrap();
        assert!(summary_after_stop.runtime.orphaned_disks.is_empty());
        assert!(summary_after_stop.runtime.orphaned_port_forwards.is_empty());
        sup.attach_disk(
            &vm2_id.to_string(),
            AttachDiskPayload {
                lun: Some(1),
                path: shared_disk,
                read_only: false,
                format: Some(DiskFormat::Raw),
            },
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn definite_hotadd_rpc_error_releases_pending_reservation() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::net::UnixListener;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let shared_disk = dir.path().join("definite-shared.img");
        fs::write(&shared_disk, vec![0u8; 4096]).unwrap();

        let vm1 = sup
            .create_vm(CreateVmPayload {
                name: "VM1-Definite".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm2 = sup
            .create_vm(CreateVmPayload {
                name: "VM2-Definite".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm1_id = vm1.config.id;
        let vm1_nic = vm1.config.nics[0].id;
        let vm2_id = vm2.config.id;
        let vm2_nic = vm2.config.nics[0].id;

        let sock_path = dir.path().join("vm1-definite.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Ok((stream_id, _, _)) = read_frame(&mut stream) else {
                    break;
                };
                let resp = TestWireResponse {
                    result: Some(test_wire_response::Result::Status(TtrpcStatus {
                        code: 3,
                        message: "explicitly rejected by vm".into(),
                        details: Vec::new(),
                    })),
                };
                if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                    let _ = stream.write_all(&frame);
                    let _ = stream.flush();
                }
            }
        });

        {
            let mut guard = sup.vms.write().unwrap();
            let m1 = guard.get_mut(&vm1_id).unwrap();
            m1.runtime.status = VmStatus::Running;
            m1.runtime.rpc_socket_path = Some(sock_path);
            m1.generation = 1;
        }

        let _ = sup
            .attach_disk(
                &vm1_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap_err();
        let pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 19024,
            guest_port: 24,
        };
        let _ = sup
            .add_port_forward(&vm1_id.to_string(), vm1_nic, pf.clone())
            .await
            .unwrap_err();

        {
            let guard = sup.vms.read().unwrap();
            let m1 = guard.get(&vm1_id).unwrap();
            assert!(m1.pending_disks.is_empty());
            assert!(m1.pending_port_forwards.is_empty());
        }

        // VM2 can immediately attach the shared disk and add the port forward.
        sup.attach_disk(
            &vm2_id.to_string(),
            AttachDiskPayload {
                lun: Some(1),
                path: shared_disk,
                read_only: false,
                format: Some(DiskFormat::Raw),
            },
        )
        .await
        .unwrap();
        sup.add_port_forward(&vm2_id.to_string(), vm2_nic, pf)
            .await
            .unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ambiguous_hotadd_rollback_not_found_and_presend_connect_error_release_reservations() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::AtomicUsize;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let shared_disk = dir.path().join("rollback-nf-shared.img");
        fs::write(&shared_disk, vec![0u8; 4096]).unwrap();

        let vm1 = sup
            .create_vm(CreateVmPayload {
                name: "VM1-RollbackNF".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm2 = sup
            .create_vm(CreateVmPayload {
                name: "VM2-RollbackNF".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm1_id = vm1.config.id;
        let vm1_nic = vm1.config.nics[0].id;
        let vm2_id = vm2.config.id;
        let vm2_nic = vm2.config.nics[0].id;

        // Missing socket: pre-send TtrpcConnectError releases reservation immediately without rollback.
        let missing_sock = dir.path().join("missing.sock");
        {
            let mut guard = sup.vms.write().unwrap();
            let m1 = guard.get_mut(&vm1_id).unwrap();
            m1.runtime.status = VmStatus::Running;
            m1.runtime.rpc_socket_path = Some(missing_sock);
            m1.generation = 1;
        }
        let _ = sup
            .attach_disk(
                &vm1_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap_err();
        let pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 19025,
            guest_port: 25,
        };
        let _ = sup
            .add_port_forward(&vm1_id.to_string(), vm1_nic, pf.clone())
            .await
            .unwrap_err();
        {
            let guard = sup.vms.read().unwrap();
            let m1 = guard.get(&vm1_id).unwrap();
            assert!(m1.pending_disks.is_empty());
            assert!(m1.pending_port_forwards.is_empty());
        }

        // Mock server: hot-add drops connection (ambiguous), rollback returns NOT_FOUND (code 5).
        let sock_path = dir.path().join("vm1-rollback-nf.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let conn_count = Arc::new(AtomicUsize::new(0));
        let conn_count_srv = Arc::clone(&conn_count);
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let idx = conn_count_srv.fetch_add(1, Ordering::Relaxed);
                let (stream_id, _, _) = read_frame(&mut stream).unwrap();
                if idx % 2 == 0 {
                    // Ambiguous EOF on Add
                    drop(stream);
                } else {
                    // Rollback Remove returns NOT_FOUND (code 5) -> treated as cleaned up
                    let resp = TestWireResponse {
                        result: Some(test_wire_response::Result::Status(TtrpcStatus {
                            code: 5,
                            message: "not found".into(),
                            details: Vec::new(),
                        })),
                    };
                    let frame = encode_frame(stream_id, 2, &resp.encode_to_vec()).unwrap();
                    stream.write_all(&frame).unwrap();
                    stream.flush().unwrap();
                }
            }
        });

        {
            let mut guard = sup.vms.write().unwrap();
            let m1 = guard.get_mut(&vm1_id).unwrap();
            m1.runtime.rpc_socket_path = Some(sock_path);
        }

        let _ = sup
            .attach_disk(
                &vm1_id.to_string(),
                AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                },
            )
            .await
            .unwrap_err();
        let _ = sup
            .add_port_forward(&vm1_id.to_string(), vm1_nic, pf.clone())
            .await
            .unwrap_err();

        assert_eq!(conn_count.load(Ordering::Relaxed), 4);
        {
            let guard = sup.vms.read().unwrap();
            let m1 = guard.get(&vm1_id).unwrap();
            assert!(m1.pending_disks.is_empty());
            assert!(m1.pending_port_forwards.is_empty());
        }

        // VM2 can attach the shared disk and add the port forward immediately.
        sup.attach_disk(
            &vm2_id.to_string(),
            AttachDiskPayload {
                lun: Some(1),
                path: shared_disk,
                read_only: false,
                format: Some(DiskFormat::Raw),
            },
        )
        .await
        .unwrap();
        sup.add_port_forward(&vm2_id.to_string(), vm2_nic, pf)
            .await
            .unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retain_orphaned_skips_when_vm_stops_restarts_or_deletes_during_rollback() {
        use openvmms_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(openvmms_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(dir.path(), Some("127.0.0.1:0")).unwrap();
        let disk_path = dir.path().join("toctou-orphan.img");
        fs::write(&disk_path, vec![0u8; 4096]).unwrap();

        let vm = sup
            .create_vm(CreateVmPayload {
                name: "ToctouOrphan-VM".into(),
                preset: openvmms_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .unwrap();
        let vm_id = vm.config.id;
        let nic_id = vm.config.nics[0].id;
        let vm_dir = openvmms_core::store::RegistryStore::open(dir.path()).vm_dir(vm_id);
        struct RestorePerm(std::path::PathBuf);
        impl Drop for RestorePerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore = RestorePerm(vm_dir.clone());
        fs::set_permissions(&vm_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let can_write_vm_dir = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(vm_dir.join(".probe"))
            .is_ok();
        if !can_write_vm_dir {
            let sock_path = dir.path().join("toctou-orphan.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let (rb_entered_tx, rb_entered_rx) = std::sync::mpsc::channel::<()>();
            let (rb_release_tx, rb_release_rx) = std::sync::mpsc::channel::<()>();
            let srv = std::thread::spawn(move || {
                // 1. Hot-add disk succeeds
                if let Ok((mut s, _)) = listener.accept()
                    && let Ok((stream_id, _, _)) = read_frame(&mut s)
                {
                    let resp = TestWireResponse {
                        result: Some(test_wire_response::Result::Payload(Vec::new())),
                    };
                    if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                        let _ = s.write_all(&frame);
                        let _ = s.flush();
                    }
                }
                // 2. Rollback hot-add disk pauses until test transitions VM state, then fails
                if let Ok((mut s, _)) = listener.accept()
                    && let Ok((stream_id, _, _)) = read_frame(&mut s)
                {
                    let _ = rb_entered_tx.send(());
                    let _ = rb_release_rx.recv_timeout(Duration::from_secs(5));
                    let resp = TestWireResponse {
                        result: Some(test_wire_response::Result::Status(TtrpcStatus {
                            code: 13,
                            message: "rollback failed".into(),
                            details: Vec::new(),
                        })),
                    };
                    if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                        let _ = s.write_all(&frame);
                        let _ = s.flush();
                    }
                }
            });

            sup.set_vm_running_socket_for_test(vm_id, sock_path);
            let mut events_rx = sup.subscribe();
            let sup_attach = Arc::clone(&sup);
            let disk_for_attach = disk_path.clone();
            let handle = tokio::spawn(async move {
                sup_attach
                    .attach_disk(
                        &vm_id.to_string(),
                        AttachDiskPayload {
                            lun: Some(1),
                            path: disk_for_attach,
                            read_only: false,
                            format: Some(DiskFormat::Raw),
                        },
                    )
                    .await
            });

            // Wait until rollback_hot_added_disk is in flight, then simulate VM restart/stop/deleting.
            while rb_entered_rx.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            {
                let mut guard = sup.vms.write().unwrap();
                let entry = guard.get_mut(&vm_id).unwrap();
                entry.generation += 1;
                entry.runtime.status = VmStatus::Off;
                entry.clear_pending_reservations();
            }
            let _ = rb_release_tx.send(());
            assert!(handle.await.unwrap().is_err());

            let summary = sup.get_vm(&vm_id.to_string()).unwrap();
            assert!(
                summary.runtime.orphaned_disks.is_empty(),
                "stale generation/stopped VM must not retain orphaned_disks"
            );
            assert!(
                matches!(events_rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
                "must not emit VmUpdated when retain_orphaned_disk skips due to state/generation change"
            );
            let _ = rb_release_tx.send(());
            srv.join().unwrap();
        }
        drop(restore);

        // Also verify direct guard behavior for deleting / Off / Error / generation-mismatch states.
        let rpc_gen = sup.generation_for_test(vm_id);
        let disk = DiskAttachment {
            lun: 2,
            path: disk_path,
            read_only: false,
            kind: DiskKind::HardDisk,
            format: DiskFormat::Raw,
        };
        let pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 19222,
            guest_port: 22,
        };
        {
            let mut guard = sup.vms.write().unwrap();
            let entry = guard.get_mut(&vm_id).unwrap();
            entry.runtime.status = VmStatus::Running;
            entry.generation = rpc_gen + 10;
            entry.deleting = true;
        }
        assert!(!sup.retain_orphaned_disk(vm_id, rpc_gen + 10, disk.clone()));
        assert!(!sup.retain_orphaned_port_forward(vm_id, rpc_gen + 10, nic_id, pf.clone()));
        {
            let mut guard = sup.vms.write().unwrap();
            let entry = guard.get_mut(&vm_id).unwrap();
            entry.deleting = false;
            entry.runtime.status = VmStatus::Error;
        }
        assert!(!sup.retain_orphaned_disk(vm_id, rpc_gen + 10, disk.clone()));
        assert!(!sup.retain_orphaned_port_forward(vm_id, rpc_gen + 10, nic_id, pf.clone()));
        {
            let mut guard = sup.vms.write().unwrap();
            let entry = guard.get_mut(&vm_id).unwrap();
            entry.runtime.status = VmStatus::Running;
        }
        assert!(!sup.retain_orphaned_disk(vm_id, rpc_gen + 9, disk));
        assert!(!sup.retain_orphaned_port_forward(vm_id, rpc_gen + 9, nic_id, pf));
        let final_summary = sup.get_vm(&vm_id.to_string()).unwrap();
        assert!(final_summary.runtime.orphaned_disks.is_empty());
        assert!(final_summary.runtime.orphaned_port_forwards.is_empty());
    }
}
