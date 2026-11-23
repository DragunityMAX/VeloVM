use anyhow::Context;
use openvmms_core::api_client::ApiClient;
use openvmms_core::api_client::ensure_daemon_running;
use openvmms_core::api_client::launch_fastdesk_viewer;
use openvmms_core::api_client::resolve_local_viewer_binary;
use openvmms_core::models::AttachDiskPayload;
use openvmms_core::models::CreateDiskPayload;
use openvmms_core::models::CreateVmPayload;
use openvmms_core::models::DaemonEvent;
use openvmms_core::models::DiskInfo;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::PortForward;
use openvmms_core::models::ShutdownVmPayload;
use openvmms_core::models::UpdateVmPayload;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmSummary;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

type SharedEndpoint = Arc<RwLock<Option<(String, String)>>>;

pub use openvmms_core::models::HostConfigPatch;

#[derive(Debug, Clone)]
pub enum GuiRequest {
    ConnectOrStartDaemon,
    RefreshAll,
    FetchVmDetails { id: Uuid },
    FetchVmLogs { id: Uuid },
    CreateVm { wizard_id: Uuid, payload: CreateVmPayload },
    UpdateVm { id: Uuid, payload: UpdateVmPayload },
    DeleteVm { id: Uuid, delete_disks: bool },
    StartVm { id: Uuid },
    StopVm { id: Uuid },
    ShutdownVm { id: Uuid, acpi: bool, force: bool },
    PauseVm { id: Uuid },
    ResumeVm { id: Uuid },
    ResetVm { id: Uuid },
    ConnectViewer { id: Uuid, no_shm: bool },
    AttachDisk { id: Uuid, op_id: u64, payload: AttachDiskPayload },
    DetachDisk {
        id: Uuid,
        lun: u32,
        expected_path: Option<PathBuf>,
    },
    MountIso { id: Uuid, op_id: u64, payload: MountIsoPayload },
    EjectIso { id: Uuid, lun: Option<u32> },
    AddPortForward { id: Uuid, nic_id: Uuid, port: PortForward },
    RemovePortForward { id: Uuid, nic_id: Uuid, port: PortForward },
    CreateDisk(CreateDiskPayload),
    InspectDisk(PathBuf),
    SaveHostConfig(HostConfigPatch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageOpKind {
    AttachDisk,
    MountIso,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    NewVmWizard(Uuid),
    VmSettings(Uuid),
    HostSettings,
    DeleteVm(Uuid),
}

#[derive(Debug)]
pub enum GuiUpdate {
    DaemonConnected {
        base_url: String,
        host_info: HostInfo,
        host_config: HostConfig,
        switches: Vec<VirtualSwitchInfo>,
        vms: Vec<VmSummary>,
        snapshot_seq: u64,
    },
    DaemonDisconnected(String),
    VmUpdated {
        vm: VmSummary,
        select: bool,
        is_details_fetch: bool,
        daemon_instance_id: Option<Uuid>,
    },
    VmDeleted {
        id: Uuid,
        revision: u64,
        daemon_instance_id: Option<Uuid>,
    },
    VmLogs { id: Uuid, content: String },
    DiskInspected(DiskInfo),
    DiskCreated(DiskInfo),
    HostConfigUpdated {
        base_url: String,
        host_info: Option<HostInfo>,
        host_config: HostConfig,
        snapshot_seq: u64,
    },
    DialogFinished {
        kind: DialogKind,
        error: Option<String>,
    },
    StorageOpFinished {
        vm_id: Uuid,
        op: StorageOpKind,
        op_id: u64,
    },
    StatusMessage(String),
    ErrorMessage(String),
}

pub struct BackendHandle {
    rt: Option<tokio::runtime::Runtime>,
    state: Arc<BackendState>,
    upd_rx: mpsc::Receiver<GuiUpdate>,
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        self.state.wake_sse();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

const SSE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const SSE_BACKOFF_MAX: Duration = Duration::from_secs(15);
const SSE_BACKOFF_STABLE_AFTER: Duration = Duration::from_secs(5);

struct BackendState {
    data_dir: Option<PathBuf>,
    daemon_url: Option<String>,
    client: RwLock<Option<ApiClient>>,
    shared_endpoint: SharedEndpoint,
    sse_active: AtomicBool,
    notified_connected: AtomicBool,
    shutdown: AtomicBool,
    sse_wake: (Mutex<u64>, Condvar),
    connect_in_flight: AtomicBool,
    snapshot_seq: std::sync::atomic::AtomicU64,
    daemon_instance_id: Mutex<(u64, Option<Uuid>)>,
    host_cfg_lock: Mutex<()>,
    logs_in_flight: Mutex<HashSet<Uuid>>,
    details_in_flight: Mutex<HashSet<Uuid>>,
    attach_disk_in_flight: Mutex<HashSet<(Option<Uuid>, Uuid)>>,
    mount_iso_in_flight: Mutex<HashSet<(Option<Uuid>, Uuid)>>,
    sse_read_timeout: Duration,
    sse_backoff_initial: Duration,
    sse_backoff_max: Duration,
    sse_backoff_stable_after: Duration,
    upd_tx: mpsc::Sender<GuiUpdate>,
    repaint_ctx: egui::Context,
}

fn storage_op_from_request(req: &GuiRequest) -> Option<(Uuid, StorageOpKind, u64)> {
    match *req {
        GuiRequest::AttachDisk { id, op_id, .. } => {
            Some((id, StorageOpKind::AttachDisk, op_id))
        }
        GuiRequest::MountIso { id, op_id, .. } => Some((id, StorageOpKind::MountIso, op_id)),
        _ => None,
    }
}

struct DroppedStorageOpGuard {
    state: Arc<BackendState>,
    op: Option<(Uuid, StorageOpKind, u64)>,
}

impl DroppedStorageOpGuard {
    fn new(state: Arc<BackendState>, req: &GuiRequest) -> Self {
        Self {
            state,
            op: storage_op_from_request(req),
        }
    }

    fn disarm(&mut self) {
        self.op = None;
    }
}

impl Drop for DroppedStorageOpGuard {
    fn drop(&mut self) {
        if let Some((vm_id, op, op_id)) = self.op.take() {
            self.state.emit(GuiUpdate::StorageOpFinished {
                vm_id,
                op,
                op_id,
            });
        }
    }
}

fn is_idempotent_read(req: &GuiRequest) -> bool {
    matches!(
        req,
        GuiRequest::RefreshAll
            | GuiRequest::FetchVmDetails { .. }
            | GuiRequest::FetchVmLogs { .. }
            | GuiRequest::InspectDisk(_)
    )
}

enum InFlightGuard<'a> {
    Uuid {
        set: &'a Mutex<HashSet<Uuid>>,
        id: Uuid,
    },
    StorageOp {
        set: &'a Mutex<HashSet<(Option<Uuid>, Uuid)>>,
        key: (Option<Uuid>, Uuid),
        backend: &'a BackendState,
        op: StorageOpKind,
        op_id: u64,
    },
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        match *self {
            InFlightGuard::Uuid { set, id } => {
                set.lock().unwrap().remove(&id);
            }
            InFlightGuard::StorageOp {
                set,
                key,
                backend,
                op,
                op_id,
            } => {
                set.lock().unwrap().remove(&key);
                backend.emit(GuiUpdate::StorageOpFinished {
                    vm_id: key.1,
                    op,
                    op_id,
                });
            }
        }
    }
}

struct ResetBoolGuard<'a>(&'a AtomicBool);

impl Drop for ResetBoolGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn request_dialog_kind(req: &GuiRequest) -> Option<DialogKind> {
    match req {
        GuiRequest::CreateVm { wizard_id, .. } => Some(DialogKind::NewVmWizard(*wizard_id)),
        GuiRequest::UpdateVm { id, .. } => Some(DialogKind::VmSettings(*id)),
        GuiRequest::SaveHostConfig(_) => Some(DialogKind::HostSettings),
        GuiRequest::DeleteVm { id, .. } => Some(DialogKind::DeleteVm(*id)),
        _ => None,
    }
}

impl BackendHandle {
    pub fn spawn(
        data_dir: Option<PathBuf>,
        daemon_url: Option<String>,
        repaint_ctx: egui::Context,
    ) -> Self {
        Self::spawn_with_sse_timeout(data_dir, daemon_url, repaint_ctx, SSE_READ_TIMEOUT)
    }

    fn spawn_with_sse_timeout(
        data_dir: Option<PathBuf>,
        daemon_url: Option<String>,
        repaint_ctx: egui::Context,
        sse_read_timeout: Duration,
    ) -> Self {
        Self::spawn_with_sse_timing(
            data_dir,
            daemon_url,
            repaint_ctx,
            sse_read_timeout,
            SSE_BACKOFF_INITIAL,
            SSE_BACKOFF_MAX,
        )
    }

    fn spawn_with_sse_timing(
        data_dir: Option<PathBuf>,
        daemon_url: Option<String>,
        repaint_ctx: egui::Context,
        sse_read_timeout: Duration,
        sse_backoff_initial: Duration,
        sse_backoff_max: Duration,
    ) -> Self {
        Self::spawn_with_sse_timing_and_stable(
            data_dir,
            daemon_url,
            repaint_ctx,
            sse_read_timeout,
            sse_backoff_initial,
            sse_backoff_max,
            SSE_BACKOFF_STABLE_AFTER,
        )
    }

    fn spawn_with_sse_timing_and_stable(
        data_dir: Option<PathBuf>,
        daemon_url: Option<String>,
        repaint_ctx: egui::Context,
        sse_read_timeout: Duration,
        sse_backoff_initial: Duration,
        sse_backoff_max: Duration,
        sse_backoff_stable_after: Duration,
    ) -> Self {
        let (upd_tx, upd_rx) = mpsc::channel::<GuiUpdate>();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(32)
            .thread_name("openvmms-gui-worker")
            .enable_all()
            .build()
            .expect("failed to build GUI backend runtime");

        let state = Arc::new(BackendState {
            data_dir,
            daemon_url,
            client: RwLock::new(None),
            shared_endpoint: Arc::new(RwLock::new(None)),
            sse_active: AtomicBool::new(false),
            notified_connected: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            sse_wake: (Mutex::new(0), Condvar::new()),
            connect_in_flight: AtomicBool::new(false),
            snapshot_seq: std::sync::atomic::AtomicU64::new(0),
            daemon_instance_id: Mutex::new((0, None)),
            host_cfg_lock: Mutex::new(()),
            logs_in_flight: Mutex::new(HashSet::new()),
            details_in_flight: Mutex::new(HashSet::new()),
            attach_disk_in_flight: Mutex::new(HashSet::new()),
            mount_iso_in_flight: Mutex::new(HashSet::new()),
            sse_read_timeout,
            sse_backoff_initial,
            sse_backoff_max,
            sse_backoff_stable_after,
            upd_tx,
            repaint_ctx,
        });

        Self {
            rt: Some(rt),
            state,
            upd_rx,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(repaint_ctx: egui::Context) -> Self {
        let (upd_tx, upd_rx) = mpsc::channel::<GuiUpdate>();
        let state = Arc::new(BackendState {
            data_dir: None,
            daemon_url: None,
            client: RwLock::new(None),
            shared_endpoint: Arc::new(RwLock::new(None)),
            sse_active: AtomicBool::new(false),
            notified_connected: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            sse_wake: (Mutex::new(0), Condvar::new()),
            connect_in_flight: AtomicBool::new(false),
            snapshot_seq: std::sync::atomic::AtomicU64::new(0),
            daemon_instance_id: Mutex::new((0, None)),
            host_cfg_lock: Mutex::new(()),
            logs_in_flight: Mutex::new(HashSet::new()),
            details_in_flight: Mutex::new(HashSet::new()),
            attach_disk_in_flight: Mutex::new(HashSet::new()),
            mount_iso_in_flight: Mutex::new(HashSet::new()),
            sse_read_timeout: SSE_READ_TIMEOUT,
            sse_backoff_initial: SSE_BACKOFF_INITIAL,
            sse_backoff_max: SSE_BACKOFF_MAX,
            sse_backoff_stable_after: SSE_BACKOFF_STABLE_AFTER,
            upd_tx,
            repaint_ctx,
        });
        Self {
            rt: None,
            state,
            upd_rx,
        }
    }

    pub fn send(&self, req: GuiRequest) {
        let mut drop_guard = DroppedStorageOpGuard::new(Arc::clone(&self.state), &req);
        if self.state.shutdown.load(Ordering::Acquire) {
            return;
        }
        if let Some(rt) = &self.rt {
            let state = Arc::clone(&self.state);
            rt.spawn_blocking(move || {
                drop_guard.disarm();
                state.process_request(req);
            });
        }
    }

    pub fn try_recv(&self) -> Option<GuiUpdate> {
        self.upd_rx.try_recv().ok()
    }
}

impl BackendState {
    fn emit_checked(&self, msg: GuiUpdate) -> bool {
        match &msg {
            GuiUpdate::DaemonConnected {
                host_info,
                snapshot_seq,
                ..
            } => {
                self.notified_connected.store(true, Ordering::Release);
                let mut guard = self.daemon_instance_id.lock().unwrap();
                if *snapshot_seq >= guard.0 {
                    guard.0 = *snapshot_seq;
                    guard.1 = host_info.daemon_instance_id;
                }
            }
            GuiUpdate::HostConfigUpdated {
                host_info,
                snapshot_seq,
                ..
            } => {
                if let Some(info) = host_info {
                    let mut guard = self.daemon_instance_id.lock().unwrap();
                    if *snapshot_seq >= guard.0 {
                        guard.0 = *snapshot_seq;
                        guard.1 = info.daemon_instance_id;
                    }
                }
            }
            GuiUpdate::DaemonDisconnected(_) => {
                self.notified_connected.store(false, Ordering::Release);
            }
            _ => {}
        }
        if self.upd_tx.send(msg).is_err() {
            return false;
        }
        self.repaint_ctx.request_repaint();
        true
    }

    fn emit(&self, msg: GuiUpdate) {
        let _ = self.emit_checked(msg);
    }

    fn next_snapshot_seq(&self) -> u64 {
        self.snapshot_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn wake_sse(&self) {
        let mut guard = self.sse_wake.0.lock().unwrap();
        *guard = guard.wrapping_add(1);
        self.sse_wake.1.notify_all();
    }

    fn acquire_storage_op_guard<'a>(
        self: &'a Arc<Self>,
        set: &'a Mutex<HashSet<(Option<Uuid>, Uuid)>>,
        vm_id: Uuid,
        op: StorageOpKind,
        op_id: u64,
        busy_msg: &str,
    ) -> Option<InFlightGuard<'a>> {
        let key = (self.daemon_instance_id.lock().unwrap().1, vm_id);
        if !set.lock().unwrap().insert(key) {
            self.emit(GuiUpdate::StatusMessage(busy_msg.to_string()));
            self.emit(GuiUpdate::StorageOpFinished {
                vm_id,
                op,
                op_id,
            });
            return None;
        }
        Some(InFlightGuard::StorageOp {
            set,
            key,
            backend: self.as_ref(),
            op,
            op_id,
        })
    }

    fn ensure_sse(self: &Arc<Self>) -> bool {
        if self
            .sse_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            spawn_sse_listener(Arc::clone(self));
            true
        } else {
            false
        }
    }

    fn emit_storage_op_finished_for_request(&self, req: &GuiRequest) {
        if let Some((vm_id, op, op_id)) = storage_op_from_request(req) {
            self.emit(GuiUpdate::StorageOpFinished {
                vm_id,
                op,
                op_id,
            });
        }
    }

    fn process_request(self: &Arc<Self>, req: GuiRequest) {
        if self.shutdown.load(Ordering::Acquire) {
            self.emit_storage_op_finished_for_request(&req);
            return;
        }

        let _dedup_guard = match &req {
            GuiRequest::FetchVmLogs { id } => {
                if !self.logs_in_flight.lock().unwrap().insert(*id) {
                    return;
                }
                Some(InFlightGuard::Uuid {
                    set: &self.logs_in_flight,
                    id: *id,
                })
            }
            GuiRequest::FetchVmDetails { id } => {
                if !self.details_in_flight.lock().unwrap().insert(*id) {
                    return;
                }
                Some(InFlightGuard::Uuid {
                    set: &self.details_in_flight,
                    id: *id,
                })
            }
            GuiRequest::AttachDisk { id, op_id, .. } => {
                let Some(guard) = self.acquire_storage_op_guard(
                    &self.attach_disk_in_flight,
                    *id,
                    StorageOpKind::AttachDisk,
                    *op_id,
                    "Disk attach is already in progress for this VM",
                ) else {
                    return;
                };
                Some(guard)
            }
            GuiRequest::MountIso { id, op_id, .. } => {
                let Some(guard) = self.acquire_storage_op_guard(
                    &self.mount_iso_in_flight,
                    *id,
                    StorageOpKind::MountIso,
                    *op_id,
                    "ISO mount is already in progress for this VM",
                ) else {
                    return;
                };
                Some(guard)
            }
            _ => None,
        };

        match req {
            GuiRequest::ConnectOrStartDaemon => {
                if self
                    .connect_in_flight
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return;
                }
                let _connect_guard = ResetBoolGuard(&self.connect_in_flight);
                match ensure_daemon_running(self.data_dir.as_deref(), self.daemon_url.as_deref()) {
                    Ok(c) => {
                        set_shared_endpoint(&self.shared_endpoint, &c);
                        *self.client.write().unwrap() = Some(c.clone());
                        if !self.ensure_sse() {
                            self.wake_sse();
                            let snapshot_seq = self.next_snapshot_seq();
                            match fetch_full_snapshot(&c) {
                                Ok((host_info, host_config, switches, vms)) => {
                                    let base_url = c.base_url().to_string();
                                    *self.client.write().unwrap() = Some(c);
                                    self.emit(GuiUpdate::DaemonConnected {
                                        base_url,
                                        host_info,
                                        host_config,
                                        switches,
                                        vms,
                                        snapshot_seq,
                                    });
                                }
                                Err(err) => {
                                    self.emit(GuiUpdate::DaemonDisconnected(format!("{err:#}")));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        self.emit(GuiUpdate::DaemonDisconnected(format!("{err:#}")));
                    }
                }
            }
            other => {
                let dialog_kind = request_dialog_kind(&other);
                let mut current_client = self.client.read().unwrap().clone();
                if current_client.is_none() {
                    match reload_client_from_runtime_addr_result(
                        self.data_dir.as_deref(),
                        self.daemon_url.as_deref(),
                        &self.shared_endpoint,
                    ) {
                        Ok(Some(reloaded)) => {
                            set_shared_endpoint(&self.shared_endpoint, &reloaded);
                            if !self.ensure_sse() {
                                self.wake_sse();
                            }
                            if !matches!(other, GuiRequest::RefreshAll) {
                                let snapshot_seq = self.next_snapshot_seq();
                                if let Ok((host_info, host_config, switches, vms)) =
                                    fetch_full_snapshot(&reloaded)
                                {
                                    self.emit(GuiUpdate::DaemonConnected {
                                        base_url: reloaded.base_url().to_string(),
                                        host_info,
                                        host_config,
                                        switches,
                                        vms,
                                        snapshot_seq,
                                    });
                                }
                            }
                            *self.client.write().unwrap() = Some(reloaded.clone());
                            current_client = Some(reloaded);
                        }
                        Ok(None) => {}
                        Err(err) => {
                            let err_msg = format!("{err:#}");
                            self.emit(GuiUpdate::DaemonDisconnected(err_msg.clone()));
                            self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                            if let Some(kind) = dialog_kind {
                                self.emit(GuiUpdate::DialogFinished {
                                    kind,
                                    error: Some(err_msg),
                                });
                            }
                            return;
                        }
                    }
                }

                let Some(mut c) = current_client else {
                    let err_msg = "Not connected to openvmmsd".to_string();
                    self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                    if let Some(kind) = dialog_kind {
                        self.emit(GuiUpdate::DialogFinished {
                            kind,
                            error: Some(err_msg),
                        });
                    }
                    return;
                };

                self.ensure_sse();
                if sync_client_before_request(
                    &mut c,
                    self.data_dir.as_deref(),
                    self.daemon_url.as_deref(),
                    &self.shared_endpoint,
                ) {
                    *self.client.write().unwrap() = Some(c.clone());
                }

                match self.execute_client_request(&mut c, other.clone()) {
                    Ok(()) => {
                        if let Some(kind) = dialog_kind {
                            self.emit(GuiUpdate::DialogFinished { kind, error: None });
                        }
                    }
                    Err(err) => {
                        let reloaded = match reload_client_from_runtime_addr_result(
                            self.data_dir.as_deref(),
                            self.daemon_url.as_deref(),
                            &self.shared_endpoint,
                        ) {
                            Ok(opt) => opt,
                            Err(reload_err) => {
                                let err_msg = format!("{reload_err:#}");
                                *self.client.write().unwrap() = None;
                                self.notified_connected.store(false, Ordering::Release);
                                self.emit(GuiUpdate::DaemonDisconnected(err_msg.clone()));
                                self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                                if let Some(kind) = dialog_kind {
                                    self.emit(GuiUpdate::DialogFinished {
                                        kind,
                                        error: Some(err_msg),
                                    });
                                }
                                return;
                            }
                        };
                        if let Some(reloaded) = reloaded
                            && (reloaded.base_url() != c.base_url()
                                || reloaded.token() != c.token())
                        {
                            c = reloaded;
                            set_shared_endpoint(&self.shared_endpoint, &c);
                            *self.client.write().unwrap() = Some(c.clone());
                            if !self.ensure_sse() {
                                self.wake_sse();
                            }
                            if is_idempotent_read(&other)
                                || openvmms_core::api_client::is_pre_request_connect_error(&err)
                            {
                                match self.execute_client_request(&mut c, other) {
                                    Ok(()) => {
                                        if let Some(kind) = dialog_kind {
                                            self.emit(GuiUpdate::DialogFinished {
                                                kind,
                                                error: None,
                                            });
                                        }
                                    }
                                    Err(retry_err) => {
                                        let err_msg = format!("{retry_err:#}");
                                        self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                                        if let Some(kind) = dialog_kind {
                                            self.emit(GuiUpdate::DialogFinished {
                                                kind,
                                                error: Some(err_msg),
                                            });
                                        }
                                    }
                                }
                            } else {
                                let snapshot_seq = self.next_snapshot_seq();
                                if let Ok((host_info, host_config, switches, vms)) =
                                    fetch_full_snapshot(&c)
                                {
                                    self.emit(GuiUpdate::DaemonConnected {
                                        base_url: c.base_url().to_string(),
                                        host_info,
                                        host_config,
                                        switches,
                                        vms,
                                        snapshot_seq,
                                    });
                                }
                                let err_msg = format!(
                                    "{err:#} (not retried after daemon endpoint changed)"
                                );
                                self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                                if let Some(kind) = dialog_kind {
                                    self.emit(GuiUpdate::DialogFinished {
                                        kind,
                                        error: Some(err_msg),
                                    });
                                }
                            }
                        } else if c.health().is_err() {
                            let mut guard = self.client.write().unwrap();
                            if guard
                                .as_ref()
                                .is_some_and(|cur| cur.base_url() == c.base_url())
                            {
                                *guard = None;
                            }
                            drop(guard);
                            let err_msg = format!("Lost connection to openvmmsd: {err:#}");
                            self.emit(GuiUpdate::DaemonDisconnected(err_msg.clone()));
                            if let Some(kind) = dialog_kind {
                                self.emit(GuiUpdate::DialogFinished {
                                    kind,
                                    error: Some(err_msg),
                                });
                            }
                        } else {
                            let err_msg = format!("{err:#}");
                            self.emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                            if let Some(kind) = dialog_kind {
                                self.emit(GuiUpdate::DialogFinished {
                                    kind,
                                    error: Some(err_msg),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

fn set_shared_endpoint(shared_endpoint: &SharedEndpoint, client: &ApiClient) {
    *shared_endpoint.write().unwrap() =
        Some((client.base_url().to_string(), client.token().to_string()));
}

fn reload_client_from_runtime_addr_result(
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) -> anyhow::Result<Option<ApiClient>> {
    let current = shared_endpoint.read().unwrap().clone();
    let runtime_err = match ApiClient::from_runtime_addr(data_dir) {
        Ok(Some(c)) => {
            let token_matches = current
                .as_ref()
                .is_none_or(|(_, cur_token)| c.token() == cur_token);
            if (daemon_url.is_none() || token_matches) && c.health().is_ok() {
                set_shared_endpoint(shared_endpoint, &c);
                return Ok(Some(c));
            }
            None
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "failed to load daemon client from runtime address"
            );
            Some(err)
        }
    };
    if let Some((base_url, token)) = current
        && let Ok(c) = ApiClient::new(base_url, token)
        && c.health().is_ok()
    {
        return Ok(Some(c));
    }
    match runtime_err {
        Some(err) => Err(err),
        None => Ok(None),
    }
}

fn reload_client_from_runtime_addr(
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) -> Option<ApiClient> {
    reload_client_from_runtime_addr_result(data_dir, daemon_url, shared_endpoint)
        .ok()
        .flatten()
}

fn sync_client_before_request(
    client: &mut ApiClient,
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) -> bool {
    if let Some((shared_url, shared_token)) = shared_endpoint.read().unwrap().clone()
        && (shared_url != client.base_url() || shared_token != client.token())
        && let Ok(updated) = ApiClient::new(shared_url, shared_token)
        && updated.health().is_ok()
    {
        *client = updated;
        return true;
    }

    match ApiClient::from_runtime_addr(data_dir) {
        Ok(Some(runtime_client)) => {
            let token_matches = runtime_client.token() == client.token();
            let changed = runtime_client.base_url() != client.base_url()
                || runtime_client.token() != client.token();
            if changed && (daemon_url.is_none() || token_matches) && runtime_client.health().is_ok()
            {
                set_shared_endpoint(shared_endpoint, &runtime_client);
                *client = runtime_client;
                return true;
            }
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "failed to refresh daemon runtime endpoint/token before request"
            );
        }
    }
    false
}

pub fn fetch_full_snapshot(
    client: &ApiClient,
) -> anyhow::Result<(HostInfo, HostConfig, Vec<VirtualSwitchInfo>, Vec<VmSummary>)> {
    let mut host_info = client.host_info()?;
    let host_config = client.host_config()?;
    let switches = client.list_switches()?;
    let (vms, vms_rev) = client.list_vms_with_revision()?;
    if let Some(rev) = vms_rev {
        host_info.event_revision = rev;
    }
    Ok((host_info, host_config, switches, vms))
}

impl BackendState {
    fn execute_client_request(
        &self,
        client: &mut ApiClient,
        req: GuiRequest,
    ) -> anyhow::Result<()> {
        let req_instance_id = self.daemon_instance_id.lock().unwrap().1;
        match req {
            GuiRequest::ConnectOrStartDaemon => Ok(()),
            GuiRequest::RefreshAll => {
                if !self.notified_connected.load(Ordering::Acquire) {
                    self.wake_sse();
                }
                let snapshot_seq = self.next_snapshot_seq();
                let (host_info, host_config, switches, vms) = fetch_full_snapshot(client)?;
                self.emit(GuiUpdate::DaemonConnected {
                    base_url: client.base_url().to_string(),
                    host_info,
                    host_config,
                    switches,
                    vms,
                    snapshot_seq,
                });
                Ok(())
            }
            GuiRequest::FetchVmDetails { id } => {
                let vm = client.get_vm(&id.to_string())?;
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: true,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::FetchVmLogs { id } => {
                let content = client.vm_logs(&id.to_string(), 250)?;
                self.emit(GuiUpdate::VmLogs { id, content });
                Ok(())
            }
            GuiRequest::CreateVm { payload, .. } => {
                let vm = client.create_vm(&payload)?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Created VM '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: true,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::UpdateVm { id, payload } => {
                let vm = client.update_vm(&id.to_string(), &payload)?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Updated settings for '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::DeleteVm { id, delete_disks } => {
                client.delete_vm(&id.to_string(), delete_disks)?;
                self.emit(GuiUpdate::StatusMessage("Deleted virtual machine".to_string()));
                self.emit(GuiUpdate::VmDeleted {
                    id,
                    revision: 0,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::StartVm { id } => {
                let vm = client.start_vm(&id.to_string())?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Started '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::StopVm { id } => {
                let vm = client.stop_vm(&id.to_string())?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Turned off '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::ShutdownVm { id, acpi, force } => {
                let vm =
                    client.shutdown_vm(&id.to_string(), &ShutdownVmPayload { force, acpi })?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Sent shutdown signal to '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::PauseVm { id } => {
                let vm = client.pause_vm(&id.to_string())?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Paused '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::ResumeVm { id } => {
                let vm = client.resume_vm(&id.to_string())?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Resumed '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::ResetVm { id } => {
                let vm = client.reset_vm(&id.to_string())?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Reset '{}'",
                    vm.config.name
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::ConnectViewer { id, no_shm } => {
                let vm = client.get_vm(&id.to_string())?;
                let endpoint = vm
                    .runtime
                    .fastdesk_endpoint
                    .as_deref()
                    .context("VM does not have an active FastDesk endpoint")?;
                let viewer_bin = resolve_local_viewer_binary(self.data_dir.as_deref())?;
                let mut child =
                    launch_fastdesk_viewer(&viewer_bin, endpoint, &vm.config.name, no_shm)?;
                let _ = thread::Builder::new()
                    .name("fastdesk-viewer-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Launched FastDesk viewer for '{}' ({endpoint})",
                    vm.config.name
                )));
                Ok(())
            }
            GuiRequest::AttachDisk { id, payload, .. } => {
                let vm = client.attach_disk(&id.to_string(), &payload)?;
                self.emit(GuiUpdate::StatusMessage("Attached disk".to_string()));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::DetachDisk {
                id,
                lun,
                expected_path,
            } => {
                let vm = client.detach_disk_matching(
                    &id.to_string(),
                    lun,
                    expected_path.as_deref(),
                )?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Detached SCSI LUN {lun}"
                )));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::MountIso { id, payload, .. } => {
                let vm = client.mount_iso(&id.to_string(), &payload)?;
                self.emit(GuiUpdate::StatusMessage("Mounted ISO image".to_string()));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::EjectIso { id, lun } => {
                let vm = client.eject_iso(&id.to_string(), lun)?;
                self.emit(GuiUpdate::StatusMessage("Ejected ISO image".to_string()));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::AddPortForward { id, nic_id, port } => {
                let vm = client.add_port_forward(&id.to_string(), nic_id, port)?;
                self.emit(GuiUpdate::StatusMessage("Added port forward".to_string()));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::RemovePortForward { id, nic_id, port } => {
                let vm = client.remove_port_forward(&id.to_string(), nic_id, port)?;
                self.emit(GuiUpdate::StatusMessage("Removed port forward".to_string()));
                self.emit(GuiUpdate::VmUpdated {
                    vm,
                    select: false,
                    is_details_fetch: false,
                    daemon_instance_id: req_instance_id,
                });
                Ok(())
            }
            GuiRequest::CreateDisk(payload) => {
                let info = client.create_disk(&payload)?;
                self.emit(GuiUpdate::StatusMessage(format!(
                    "Created disk {}",
                    info.path.display()
                )));
                self.emit(GuiUpdate::DiskCreated(info));
                Ok(())
            }
            GuiRequest::InspectDisk(path) => {
                let info = client.inspect_disk(&path)?;
                self.emit(GuiUpdate::DiskInspected(info));
                Ok(())
            }
            GuiRequest::SaveHostConfig(patch) => {
                let _guard = self.host_cfg_lock.lock().unwrap();
                let pre_patch_seq = self.next_snapshot_seq();
                let updated = client.patch_host_config(&patch)?;
                // Follow only `daemon.addr` when the daemon rebinds; never connect to `updated.listen_addr`.
                if let Some(next_client) = reload_client_from_runtime_addr(
                    self.data_dir.as_deref(),
                    self.daemon_url.as_deref(),
                    &self.shared_endpoint,
                ) && next_client.base_url() != client.base_url()
                {
                    *client = next_client;
                    *self.client.write().unwrap() = Some(client.clone());
                    self.wake_sse();
                }
                let refresh_seq = self.next_snapshot_seq();
                let (host_config, snapshot_seq) = match client.host_config() {
                    Ok(fresh) => (fresh, refresh_seq),
                    Err(err) => {
                        tracing::warn!(
                            error = %format!("{err:#}"),
                            "Host settings saved, but follow-up host_config refresh failed"
                        );
                        (updated, pre_patch_seq)
                    }
                };
                let (host_info, status_msg) = match client.host_info() {
                    Ok(info) => (Some(info), "Saved host settings".to_string()),
                    Err(err) => {
                        tracing::warn!(
                            error = %format!("{err:#}"),
                            "Host settings saved, but follow-up host_info refresh failed"
                        );
                        (
                            None,
                            "Saved host settings (host info refresh pending)".to_string(),
                        )
                    }
                };
                let base_url = client.base_url().to_string();
                self.emit(GuiUpdate::StatusMessage(status_msg));
                self.emit(GuiUpdate::HostConfigUpdated {
                    base_url,
                    host_info,
                    host_config,
                    snapshot_seq,
                });
                Ok(())
            }
        }
    }
}

fn resolve_updated_client(
    current_base_url: &str,
    current_token: &str,
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) -> Option<ApiClient> {
    if let Some(c) = reload_client_from_runtime_addr(data_dir, daemon_url, shared_endpoint) {
        return Some(c);
    }
    ApiClient::new(current_base_url, current_token).ok()
}

fn maybe_switch_endpoint_on_disconnect(
    current_base_url: &str,
    state: &BackendState,
) -> anyhow::Result<bool> {
    if let Some(c) = reload_client_from_runtime_addr_result(
        state.data_dir.as_deref(),
        state.daemon_url.as_deref(),
        &state.shared_endpoint,
    )? && c.base_url() != current_base_url
    {
        *state.client.write().unwrap() = Some(c);
        return Ok(true);
    }
    Ok(false)
}

fn sleep_backoff(state: &BackendState, start_wake_gen: u64, dur: Duration) -> bool {
    let deadline = std::time::Instant::now() + dur;
    let mut guard = state.sse_wake.0.lock().unwrap();
    loop {
        if state.shutdown.load(Ordering::Acquire) {
            return false;
        }
        if *guard != start_wake_gen {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let (next_guard, wait_res) = state.sse_wake.1.wait_timeout(guard, remaining).unwrap();
        guard = next_guard;
        if wait_res.timed_out() {
            return !state.shutdown.load(Ordering::Acquire) && *guard != start_wake_gen;
        }
    }
}

fn spawn_sse_listener(state: Arc<BackendState>) {
    thread::spawn(move || {
        let http = match reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Some(state.sse_read_timeout))
            .build()
        {
            Ok(c) => c,
            Err(_) => {
                state.sse_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        let mut reported_initial_error = false;
        let mut reported_token_security_error = false;
        let mut retry_delay = state.sse_backoff_initial;
        let next_delay = |d: Duration| d.saturating_mul(2).min(state.sse_backoff_max);
        while !state.shutdown.load(Ordering::Acquire) {
            let wake_gen = *state.sse_wake.0.lock().unwrap();
            let endpoint = match state.shared_endpoint.read().unwrap().clone() {
                Some(ep) => Ok(Some(ep)),
                None => reload_client_from_runtime_addr_result(
                    state.data_dir.as_deref(),
                    state.daemon_url.as_deref(),
                    &state.shared_endpoint,
                )
                .map(|opt| opt.map(|c| (c.base_url().to_string(), c.token().to_string()))),
            };
            let (mut base_url, mut token) = match endpoint {
                Ok(Some(ep)) => ep,
                Ok(None) => {
                    if sleep_backoff(&state, wake_gen, retry_delay) {
                        retry_delay = state.sse_backoff_initial;
                    } else {
                        retry_delay = next_delay(retry_delay);
                    }
                    continue;
                }
                Err(err) => {
                    let is_token_sec = openvmms_core::store::is_token_security_error(&err);
                    if state.notified_connected.swap(false, Ordering::AcqRel)
                        || !reported_initial_error
                        || (is_token_sec && !reported_token_security_error)
                    {
                        reported_initial_error = true;
                        if is_token_sec {
                            reported_token_security_error = true;
                        }
                        *state.client.write().unwrap() = None;
                        if !state.emit_checked(GuiUpdate::DaemonDisconnected(format!("{err:#}"))) {
                            break;
                        }
                    }
                    if sleep_backoff(&state, wake_gen, retry_delay) {
                        retry_delay = state.sse_backoff_initial;
                    } else {
                        retry_delay = next_delay(retry_delay);
                    }
                    continue;
                }
            };
            // Verify health authentication challenge before transmitting token on the SSE request.
            if !ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok()) {
                match reload_client_from_runtime_addr_result(
                    state.data_dir.as_deref(),
                    state.daemon_url.as_deref(),
                    &state.shared_endpoint,
                ) {
                    Ok(Some(reloaded)) => {
                        if reloaded.base_url() != base_url {
                            retry_delay = state.sse_backoff_initial;
                        }
                        base_url = reloaded.base_url().to_string();
                        token = reloaded.token().to_string();
                    }
                    Ok(None) => {
                        if state.notified_connected.swap(false, Ordering::AcqRel)
                            || !reported_initial_error
                        {
                            reported_initial_error = true;
                            *state.client.write().unwrap() = None;
                            if !state.emit_checked(GuiUpdate::DaemonDisconnected(
                                "Lost connection or failed daemon verification".to_string(),
                            )) {
                                break;
                            }
                        }
                        if sleep_backoff(&state, wake_gen, retry_delay) {
                            retry_delay = state.sse_backoff_initial;
                        } else {
                            retry_delay = next_delay(retry_delay);
                        }
                        continue;
                    }
                    Err(err) => {
                        let is_token_sec = openvmms_core::store::is_token_security_error(&err);
                        if state.notified_connected.swap(false, Ordering::AcqRel)
                            || !reported_initial_error
                            || (is_token_sec && !reported_token_security_error)
                        {
                            reported_initial_error = true;
                            if is_token_sec {
                                reported_token_security_error = true;
                            }
                            *state.client.write().unwrap() = None;
                            if !state.emit_checked(GuiUpdate::DaemonDisconnected(format!(
                                "{err:#}"
                            ))) {
                                break;
                            }
                        }
                        if sleep_backoff(&state, wake_gen, retry_delay) {
                            retry_delay = state.sse_backoff_initial;
                        } else {
                            retry_delay = next_delay(retry_delay);
                        }
                        continue;
                    }
                }
            }
            let url = format!("{base_url}/api/v1/events");
            let mut switched_endpoint = false;
            let mut saw_daemon_stopping = false;
            match http.get(&url).bearer_auth(&token).send() {
                Ok(resp) if resp.status().is_success() => {
                    let snapshot_seq = state.next_snapshot_seq();
                    let sync_res = ApiClient::new(&base_url, &token)
                        .and_then(|c| fetch_full_snapshot(&c).map(|snap| (c, snap)));
                    let mut sse_instance_id = match sync_res {
                        Ok((c, (host_info, host_config, switches, vms))) => {
                            let inst_id = host_info.daemon_instance_id;
                            reported_initial_error = false;
                            reported_token_security_error = false;
                            *state.client.write().unwrap() = Some(c);
                            if !state.emit_checked(GuiUpdate::DaemonConnected {
                                base_url: base_url.clone(),
                                host_info,
                                host_config,
                                switches,
                                vms,
                                snapshot_seq,
                            }) {
                                break;
                            }
                            inst_id
                        }
                        Err(_) => {
                            drop(resp);
                            if state.notified_connected.swap(false, Ordering::AcqRel)
                                || !reported_initial_error
                            {
                                reported_initial_error = true;
                                *state.client.write().unwrap() = None;
                                if !state.emit_checked(GuiUpdate::DaemonDisconnected(
                                    "Failed to reload daemon state; retrying…".to_string(),
                                )) {
                                    break;
                                }
                            }
                            if sleep_backoff(&state, wake_gen, retry_delay) {
                                retry_delay = state.sse_backoff_initial;
                            } else {
                                retry_delay = next_delay(retry_delay);
                            }
                            continue;
                        }
                    };
                    let stream_connected_at = std::time::Instant::now();
                    let reader = std::io::BufReader::new(resp);
                    let mut skip_leading_resync = true;
                    let mut sse_send_closed = false;
                    for line in reader.lines().map_while(Result::ok) {
                        if state.shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        let Some(json_str) = line.strip_prefix("data: ") else {
                            continue;
                        };
                        let was_leading = std::mem::replace(&mut skip_leading_resync, false);
                        let Ok(event) = serde_json::from_str::<DaemonEvent>(json_str) else {
                            continue;
                        };
                        if was_leading && matches!(event, DaemonEvent::Resync) {
                            continue;
                        }
                        match event {
                            DaemonEvent::VmUpdated { vm } => {
                                if !state.emit_checked(GuiUpdate::VmUpdated {
                                    vm,
                                    select: false,
                                    is_details_fetch: false,
                                    daemon_instance_id: sse_instance_id,
                                }) {
                                    sse_send_closed = true;
                                    break;
                                }
                            }
                            DaemonEvent::VmDeleted { id, revision } => {
                                if !state.emit_checked(GuiUpdate::VmDeleted {
                                    id,
                                    revision,
                                    daemon_instance_id: sse_instance_id,
                                }) {
                                    sse_send_closed = true;
                                    break;
                                }
                            }
                            DaemonEvent::Resync => {
                                let snapshot_seq = state.next_snapshot_seq();
                                let reload_res = ApiClient::new(&base_url, &token)
                                    .and_then(|c| fetch_full_snapshot(&c).map(|snap| (c, snap)));
                                match reload_res {
                                    Ok((c, (host_info, host_config, switches, vms))) => {
                                        sse_instance_id = host_info.daemon_instance_id;
                                        *state.client.write().unwrap() = Some(c);
                                        if !state.emit_checked(GuiUpdate::DaemonConnected {
                                            base_url: base_url.clone(),
                                            host_info,
                                            host_config,
                                            switches,
                                            vms,
                                            snapshot_seq,
                                        }) {
                                            sse_send_closed = true;
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        // Drop the stream so the outer loop runs the retryable reconnect sync path.
                                        break;
                                    }
                                }
                            }
                            DaemonEvent::HostConfigUpdated => {
                                let snapshot_seq = state.next_snapshot_seq();
                                let reload_res = resolve_updated_client(
                                    &base_url,
                                    &token,
                                    state.data_dir.as_deref(),
                                    state.daemon_url.as_deref(),
                                    &state.shared_endpoint,
                                )
                                .ok_or_else(|| anyhow::anyhow!("failed to resolve daemon client"))
                                .and_then(|c| fetch_full_snapshot(&c).map(|snap| (c, snap)));
                                match reload_res {
                                    Ok((c, (host_info, host_config, switches, vms))) => {
                                        sse_instance_id = host_info.daemon_instance_id;
                                        let new_base = c.base_url().to_string();
                                        *state.client.write().unwrap() = Some(c);
                                        if !state.emit_checked(GuiUpdate::DaemonConnected {
                                            base_url: new_base.clone(),
                                            host_info,
                                            host_config,
                                            switches,
                                            vms,
                                            snapshot_seq,
                                        }) {
                                            sse_send_closed = true;
                                            break;
                                        }
                                        if new_base != base_url {
                                            switched_endpoint = true;
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        // Drop the stream so the outer loop runs the retryable reconnect sync path.
                                        break;
                                    }
                                }
                            }
                            DaemonEvent::DaemonStopping => {
                                saw_daemon_stopping = true;
                                *state.client.write().unwrap() = None;
                                if !state.emit_checked(GuiUpdate::DaemonDisconnected(
                                    "Daemon stopped".to_string(),
                                )) {
                                    sse_send_closed = true;
                                }
                                break;
                            }
                        }
                    }
                    if sse_send_closed {
                        break;
                    }
                    if switched_endpoint {
                        retry_delay = state.sse_backoff_initial;
                    } else if !state.shutdown.load(Ordering::Acquire) {
                        if saw_daemon_stopping
                            || stream_connected_at.elapsed() >= state.sse_backoff_stable_after
                        {
                            retry_delay = state.sse_backoff_initial;
                        }
                        if saw_daemon_stopping {
                            if sleep_backoff(&state, wake_gen, retry_delay) {
                                retry_delay = state.sse_backoff_initial;
                            } else {
                                retry_delay = next_delay(retry_delay);
                            }
                            continue;
                        }
                        let switch_err = match maybe_switch_endpoint_on_disconnect(&base_url, &state)
                        {
                            Ok(true) => {
                                retry_delay = state.sse_backoff_initial;
                                continue;
                            }
                            Ok(false) => None,
                            Err(err) => Some(err),
                        };
                        let healthy =
                            ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok());
                        if !healthy || switch_err.is_some() {
                            *state.client.write().unwrap() = None;
                            let is_token_sec = switch_err
                                .as_ref()
                                .is_some_and(openvmms_core::store::is_token_security_error);
                            if state.notified_connected.swap(false, Ordering::AcqRel)
                                || !reported_initial_error
                                || (is_token_sec && !reported_token_security_error)
                            {
                                reported_initial_error = true;
                                if is_token_sec {
                                    reported_token_security_error = true;
                                }
                                let reason = switch_err
                                    .map(|e| format!("{e:#}"))
                                    .unwrap_or_else(|| "Lost connection to openvmmsd".to_string());
                                if !state.emit_checked(GuiUpdate::DaemonDisconnected(reason)) {
                                    break;
                                }
                            }
                        }
                        if sleep_backoff(&state, wake_gen, retry_delay) {
                            retry_delay = state.sse_backoff_initial;
                        } else {
                            retry_delay = next_delay(retry_delay);
                        }
                    }
                }
                sse_res => {
                    let switch_err = match maybe_switch_endpoint_on_disconnect(&base_url, &state) {
                        Ok(true) => {
                            retry_delay = state.sse_backoff_initial;
                            continue;
                        }
                        Ok(false) => None,
                        Err(err) => Some(err),
                    };
                    let healthy =
                        ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok());
                    if !healthy || switch_err.is_some() {
                        *state.client.write().unwrap() = None;
                    }
                    let is_token_sec = switch_err
                        .as_ref()
                        .is_some_and(openvmms_core::store::is_token_security_error);
                    if state.notified_connected.swap(false, Ordering::AcqRel)
                        || !reported_initial_error
                        || (is_token_sec && !reported_token_security_error)
                    {
                        reported_initial_error = true;
                        if is_token_sec {
                            reported_token_security_error = true;
                        }
                        let reason = if let Some(err) = switch_err {
                            format!("{err:#}")
                        } else if !healthy {
                            "Lost connection to openvmmsd".to_string()
                        } else {
                            match sse_res {
                                Ok(resp) => format!(
                                    "Event stream returned HTTP {}; retrying…",
                                    resp.status()
                                ),
                                Err(err) => format!("Event stream error ({err}); retrying…"),
                            }
                        };
                        if !state.emit_checked(GuiUpdate::DaemonDisconnected(reason)) {
                            break;
                        }
                    }
                    if sleep_backoff(&state, wake_gen, retry_delay) {
                        retry_delay = state.sse_backoff_initial;
                    } else {
                        retry_delay = next_delay(retry_delay);
                    }
                }
            }
        }
        state.sse_active.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use openvmms_core::models::VmPreset;
    use openvmms_core::models::VmStatus;

    fn recv_until<F>(handle: &BackendHandle, timeout: Duration, mut pred: F) -> GuiUpdate
    where
        F: FnMut(&GuiUpdate) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            while let Some(upd) = handle.try_recv() {
                if let GuiUpdate::ErrorMessage(err) = &upd {
                    panic!("unexpected GuiUpdate::ErrorMessage: {err}");
                }
                if pred(&upd) {
                    return upd;
                }
                seen.push(format!("{upd:?}"));
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for expected GuiUpdate after {timeout:?} (seen: {seen:?})"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn accept_blocking_stream(
        listener: &std::net::TcpListener,
    ) -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
        let (stream, peer) = listener.accept()?;
        let _ = stream.set_nonblocking(false);
        Ok((stream, peer))
    }

    #[test]
    fn backend_handle_follows_external_daemon_port_change_for_requests_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let daemon = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();
        let old_base_url = daemon.base_url.clone();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => {
                assert_eq!(base_url, old_base_url);
            }
            _ => unreachable!(),
        }

        // Another client updates listen_addr to a fresh ephemeral port, closing the old listener.
        let external_client = ApiClient::new(&old_base_url, &daemon.token).unwrap();
        let mut cfg = external_client.host_config().unwrap();
        cfg.listen_addr = "127.0.0.1:0".to_string();
        let updated_cfg = external_client.update_host_config(&cfg).unwrap();
        assert_eq!(updated_cfg.listen_addr, "127.0.0.1:0");
        let new_base_url = ApiClient::from_data_dir(Some(dir.path()), None)
            .unwrap()
            .base_url()
            .to_string();
        assert_ne!(new_base_url, old_base_url);

        // Wait until the GUI's SSE listener emits the reconnected snapshot on the new URL.
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { base_url, .. } if base_url == &new_base_url
            )
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => {
                assert_eq!(base_url, new_base_url);
            }
            _ => unreachable!(),
        }

        // Ensure the old listener has completely stopped accepting connections before testing requests.
        let old_addr: std::net::SocketAddr = old_base_url
            .trim_start_matches("http://")
            .parse()
            .unwrap();
        let close_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::net::TcpStream::connect(old_addr).is_ok() {
            assert!(
                std::time::Instant::now() < close_deadline,
                "old listener {old_addr} stayed open"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // GUI RefreshAll request must succeed using the new endpoint.
        backend.send(GuiRequest::RefreshAll);
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => {
                assert_eq!(base_url, new_base_url);
            }
            _ => unreachable!(),
        }

        // An external client creating a VM on the new port must arrive over the reconnected SSE stream.
        let new_external = ApiClient::new(&new_base_url, &daemon.token).unwrap();
        let created = new_external
            .create_vm(&CreateVmPayload {
                name: "PortSync-SSE-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: None,
                disks: vec![],
                create_disk: None,
                iso_path: None,
                nics: None,
            })
            .unwrap();

        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::VmUpdated { vm, .. } if vm.config.id == created.config.id)
        }) {
            GuiUpdate::VmUpdated { vm, .. } => {
                assert_eq!(vm.config.name, "PortSync-SSE-VM");
            }
            _ => unreachable!(),
        }

        // GUI mutation request after port change must also succeed on the new endpoint.
        backend.send(GuiRequest::UpdateVm {
            id: created.config.id,
            payload: UpdateVmPayload {
                description: Some("updated via GUI after port change".into()),
                ..Default::default()
            },
        });
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::VmUpdated { vm, .. }
                    if vm.config.id == created.config.id
                        && vm.config.description == "updated via GUI after port change"
            )
        }) {
            GuiUpdate::VmUpdated { vm, .. } => {
                assert_eq!(vm.config.description, "updated via GUI after port change");
            }
            _ => unreachable!(),
        }

        rt.block_on(daemon.shutdown());
    }

    #[test]
    fn backend_handle_recovers_requests_and_sse_after_external_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let daemon1 = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();
        let url1 = daemon1.base_url.clone();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => assert_eq!(base_url, url1),
            _ => unreachable!(),
        }

        // Daemon stops; GUI must report DaemonDisconnected and wait for restart.
        rt.block_on(daemon1.shutdown());
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        }) {
            GuiUpdate::DaemonDisconnected(_) => {}
            _ => unreachable!(),
        }

        // External daemon starts on the same data_dir with a fresh ephemeral port.
        let daemon2 = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();
        let url2 = daemon2.base_url.clone();
        assert_ne!(url2, url1);

        match recv_until(&backend, Duration::from_secs(6), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { base_url, .. } if base_url == &url2
            )
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => assert_eq!(base_url, url2),
            _ => unreachable!(),
        }

        // Requests sent through BackendHandle after external restart must succeed (never "Not connected").
        let wizard_id = Uuid::new_v4();
        backend.send(GuiRequest::CreateVm {
            wizard_id,
            payload: CreateVmPayload {
                name: "Gui-After-Restart".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            },
        });
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DialogFinished {
                    kind: DialogKind::NewVmWizard(wid),
                    error: None
                } if *wid == wizard_id
            )
        }) {
            GuiUpdate::DialogFinished { error, .. } => assert!(error.is_none()),
            _ => unreachable!(),
        }

        // External mutations on daemon2 must also stream over the revived SSE connection.
        let ext2 = ApiClient::new(&url2, &daemon2.token).unwrap();
        let ext_vm = ext2
            .create_vm(&CreateVmPayload {
                name: "External-After-Restart".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .unwrap();
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::VmUpdated { vm, .. } if vm.config.id == ext_vm.config.id)
        }) {
            GuiUpdate::VmUpdated { vm, .. } => assert_eq!(vm.config.name, "External-After-Restart"),
            _ => unreachable!(),
        }

        rt.block_on(daemon2.shutdown());
    }

    #[test]
    fn rejected_create_vm_emits_dialog_finished_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let daemon = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        let wizard_id = Uuid::new_v4();
        backend.send(GuiRequest::CreateVm {
            wizard_id,
            payload: CreateVmPayload {
                name: "Invalid-Vcpu-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(0),
                memory_mb: Some(512),
                ..Default::default()
            },
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_dialog_err = false;
        while std::time::Instant::now() < deadline {
            if let Some(upd) = backend.try_recv()
                && let GuiUpdate::DialogFinished {
                    kind: DialogKind::NewVmWizard(wid),
                    error: Some(err),
                } = upd
                && wid == wizard_id
            {
                assert!(err.contains("vcpus"), "{err}");
                saw_dialog_err = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_dialog_err, "expected DialogFinished with error");

        rt.block_on(daemon.shutdown());
    }

    #[test]
    fn save_host_config_on_ephemeral_port_stays_on_daemon_addr_and_never_contacts_configured_listen_addr() {
        let dir = tempfile::tempdir().unwrap();
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let squatter_addr = squatter.local_addr().unwrap();
        let squatter_hit = Arc::new(AtomicBool::new(false));
        let squatter_hit_srv = Arc::clone(&squatter_hit);
        let squatter_stop = Arc::new(AtomicBool::new(false));
        let squatter_stop_srv = Arc::clone(&squatter_stop);
        let _ = squatter.set_nonblocking(true);

        let squatter_thread = thread::spawn(move || {
            while !squatter_stop_srv.load(Ordering::Acquire) {
                match accept_blocking_stream(&squatter) {
                    Ok((_stream, _)) => {
                        squatter_hit_srv.store(true, Ordering::SeqCst);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        // Pre-seed config.toml with the occupied squatter address before starting the ephemeral daemon.
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let mut init_cfg = store.load_or_init_host_config().unwrap();
        init_cfg.listen_addr = squatter_addr.to_string();
        store.save_host_config(&init_cfg).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let daemon = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();
        let daemon_url = daemon.base_url.clone();
        assert_ne!(daemon_url, format!("http://{squatter_addr}"));

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let host_cfg = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected {
                base_url,
                host_config,
                ..
            } => {
                assert_eq!(base_url, daemon_url);
                host_config
            }
            _ => unreachable!(),
        };

        // Save Host Settings without altering `listen_addr` (still the squatter address).
        let mut edited = host_cfg.clone();
        edited.openvmm_bin = Some(dir.path().join("custom-openvmm"));
        let patch = HostConfigPatch::from_diff(&host_cfg, &edited);
        backend.send(GuiRequest::SaveHostConfig(patch));

        let mut saw_updated = false;
        let mut saw_dialog_ok = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && (!saw_updated || !saw_dialog_ok) {
            while let Some(upd) = backend.try_recv() {
                match upd {
                    GuiUpdate::ErrorMessage(e) => panic!("unexpected ErrorMessage: {e}"),
                    GuiUpdate::HostConfigUpdated { base_url, .. } => {
                        assert_eq!(base_url, daemon_url);
                        saw_updated = true;
                    }
                    GuiUpdate::DialogFinished {
                        kind: DialogKind::HostSettings,
                        error: None,
                    } => {
                        saw_dialog_ok = true;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_updated && saw_dialog_ok, "SaveHostConfig did not complete cleanly");
        assert!(
            !squatter_hit.load(Ordering::SeqCst),
            "GUI contacted configured listen_addr instead of daemon.addr"
        );

        squatter_stop.store(true, Ordering::Release);
        squatter_thread.join().unwrap();
        rt.block_on(daemon.shutdown());
    }

    #[test]
    fn stop_vm_does_not_queue_behind_slow_start_vm() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let token = "concurrent-test-token";
        let vm_id = Uuid::new_v4();
        let vm_cfg = openvmms_core::models::VmConfig::from_preset(
            "Concurrent-UI-VM",
            VmPreset::Custom,
            Path::new("/tmp/vm"),
        );
        let mut vm_cfg = vm_cfg;
        vm_cfg.id = vm_id;

        let start_entered = Arc::new(AtomicBool::new(false));
        let release_start = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::new(AtomicBool::new(false));
        let _ = listener.set_nonblocking(true);

        let srv_start_entered = Arc::clone(&start_entered);
        let srv_release_start = Arc::clone(&release_start);
        let srv_stop = Arc::clone(&stop_server);
        let vm_cfg_srv = vm_cfg.clone();

        let srv = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !srv_stop.load(Ordering::Acquire) {
                match accept_blocking_stream(&listener) {
                    Ok((mut stream, _)) => {
                        let start_entered = Arc::clone(&srv_start_entered);
                        let release_start = Arc::clone(&srv_release_start);
                        let srv_stop_conn = Arc::clone(&srv_stop);
                        let vm_cfg_conn = vm_cfg_srv.clone();
                        handlers.push(thread::spawn(move || {
                            let mut buf = [0u8; 4096];
                            let n = stream.read(&mut buf).unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            let req = String::from_utf8_lossy(&buf[..n]);
                            let first_line = req.lines().next().unwrap_or("");
                            let target = first_line.split_whitespace().nth(1).unwrap_or("");
                            let method = first_line.split_whitespace().next().unwrap_or("");

                            let (status, body) = if target.starts_with("/api/v1/health?") {
                                let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
                                let challenge = query
                                    .split('&')
                                    .filter_map(|kv| kv.split_once('='))
                                    .find(|(k, _)| *k == "challenge")
                                    .map(|(_, v)| v)
                                    .unwrap_or("");
                                let proof = openvmms_core::models::compute_health_auth_proof(
                                    token, addr, challenge,
                                );
                                (
                                    "200 OK",
                                    format!(
                                        r#"{{"status":"ok","service":"openvmmsd","auth_proof":"{proof}"}}"#
                                    ),
                                )
                            } else if target == "/api/v1/host/info" {
                                let info = HostInfo {
                                    hostname: "test".into(),
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    cpu_count: 4,
                                    daemon_version: "0.1.0".into(),
                                    data_dir: PathBuf::from("/tmp"),
                                    runtime_dir: PathBuf::from("/tmp"),
                                    listen_addr: addr.to_string(),
                                    daemon_instance_id: None,
                                    event_revision: 0,
                                    resolved_paths: Default::default(),
                                };
                                ("200 OK", serde_json::to_string(&info).unwrap())
                            } else if target == "/api/v1/host/config" {
                                ("200 OK", serde_json::to_string(&HostConfig::default()).unwrap())
                            } else if target == "/api/v1/host/switches" {
                                ("200 OK", "[]".into())
                            } else if target == "/api/v1/vms" {
                                let summary = VmSummary {
                                    config: vm_cfg_conn.clone(),
                                    runtime: Default::default(),
                                };
                                ("200 OK", serde_json::to_string(&vec![summary]).unwrap())
                            } else if target == "/api/v1/events" {
                                let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
                                let _ = stream.write_all(hdr.as_bytes());
                                let _ = stream.flush();
                                while !srv_stop_conn.load(Ordering::Acquire) {
                                    thread::sleep(Duration::from_millis(25));
                                }
                                return;
                            } else if method == "POST" && target.ends_with("/start") {
                                start_entered.store(true, Ordering::Release);
                                while !release_start.load(Ordering::Acquire)
                                    && !srv_stop_conn.load(Ordering::Acquire)
                                {
                                    thread::sleep(Duration::from_millis(20));
                                }
                                let mut rt = openvmms_core::models::VmRuntimeState::default();
                                rt.status = VmStatus::Off;
                                let summary = VmSummary {
                                    config: vm_cfg_conn.clone(),
                                    runtime: rt,
                                };
                                ("200 OK", serde_json::to_string(&summary).unwrap())
                            } else if method == "POST" && target.ends_with("/stop") {
                                let mut rt = openvmms_core::models::VmRuntimeState::default();
                                rt.status = VmStatus::Off;
                                let summary = VmSummary {
                                    config: vm_cfg_conn.clone(),
                                    runtime: rt,
                                };
                                ("200 OK", serde_json::to_string(&summary).unwrap())
                            } else {
                                ("404 Not Found", r#"{"error":"not found"}"#.into())
                            };
                            let resp = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(_) => break,
                }
            }
            for h in handlers {
                let _ = h.join();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        openvmms_core::store::atomic_write(&store.token_path(), token.as_bytes(), true).unwrap();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            Some(format!("http://{addr}")),
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        backend.send(GuiRequest::StartVm { id: vm_id });
        let enter_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !start_entered.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < enter_deadline,
                "StartVm did not reach server handler"
            );
            thread::sleep(Duration::from_millis(15));
        }

        // While StartVm is still blocked in-flight, StopVm must complete concurrently!
        backend.send(GuiRequest::StopVm { id: vm_id });
        match recv_until(&backend, Duration::from_secs(2), |u| {
            matches!(u, GuiUpdate::StatusMessage(msg) if msg.contains("Turned off"))
        }) {
            GuiUpdate::StatusMessage(msg) => assert!(msg.contains("Turned off")),
            _ => unreachable!(),
        }

        release_start.store(true, Ordering::Release);
        stop_server.store(true, Ordering::Release);
        drop(backend);
        srv.join().unwrap();
    }

    #[test]
    fn non_idempotent_create_and_delete_are_not_retried_after_port_change() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        #[allow(clippy::too_many_arguments)]
        fn spawn_mock_endpoint(
            listener: std::net::TcpListener,
            token: &'static str,
            store_dir: PathBuf,
            vm_summary: VmSummary,
            post_count: Arc<AtomicUsize>,
            delete_count: Arc<AtomicUsize>,
            switch_on_post: Option<std::net::SocketAddr>,
            switch_on_delete: Option<std::net::SocketAddr>,
            stop: Arc<AtomicBool>,
        ) -> thread::JoinHandle<()> {
            let addr = listener.local_addr().unwrap();
            let _ = listener.set_nonblocking(true);
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                            let store_dir = store_dir.clone();
                            let vm_summary = vm_summary.clone();
                            let post_count = Arc::clone(&post_count);
                            let delete_count = Arc::clone(&delete_count);
                            let stop_conn = Arc::clone(&stop);
                            handlers.push(thread::spawn(move || {
                                let mut raw = Vec::with_capacity(1024);
                                let mut tmp = [0u8; 1024];
                                let mut header_end = None;
                                while header_end.is_none() {
                                    match stream.read(&mut tmp) {
                                        Ok(0) | Err(_) => return,
                                        Ok(n) => {
                                            raw.extend_from_slice(&tmp[..n]);
                                            header_end = raw
                                                .windows(4)
                                                .position(|w| w == b"\r\n\r\n")
                                                .map(|p| p + 4);
                                            if raw.len() > 64 * 1024 {
                                                break;
                                            }
                                        }
                                    }
                                }
                                let Some(hdr_end) = header_end else {
                                    return;
                                };
                                let headers_str = String::from_utf8_lossy(&raw[..hdr_end]).into_owned();
                                let content_len: usize = headers_str
                                    .lines()
                                    .find_map(|line| {
                                        let (k, v) = line.split_once(':')?;
                                        if k.eq_ignore_ascii_case("content-length") {
                                            v.trim().parse().ok()
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or(0);
                                while raw.len() < hdr_end + content_len {
                                    match stream.read(&mut tmp) {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => raw.extend_from_slice(&tmp[..n]),
                                    }
                                }
                                let first_line = headers_str.lines().next().unwrap_or("");
                                let method = first_line.split_whitespace().next().unwrap_or("");
                                let target = first_line.split_whitespace().nth(1).unwrap_or("");

                                if target == "/api/v1/events" {
                                    let hdrs = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                                    if stream.write_all(hdrs.as_bytes()).is_err() {
                                        return;
                                    }
                                    let _ = stream.flush();
                                    while !stop_conn.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }

                                if method == "POST" && target == "/api/v1/vms" {
                                    post_count.fetch_add(1, Ordering::SeqCst);
                                    if let Some(next_addr) = switch_on_post {
                                        let store =
                                            openvmms_core::store::RegistryStore::new(&store_dir)
                                                .unwrap();
                                        store
                                            .write_runtime_listen_addr(&next_addr.to_string())
                                            .unwrap();
                                        return;
                                    }
                                } else if method == "DELETE" && target.starts_with("/api/v1/vms/") {
                                    delete_count.fetch_add(1, Ordering::SeqCst);
                                    if let Some(next_addr) = switch_on_delete {
                                        let store =
                                            openvmms_core::store::RegistryStore::new(&store_dir)
                                                .unwrap();
                                        store
                                            .write_runtime_listen_addr(&next_addr.to_string())
                                            .unwrap();
                                        return;
                                    }
                                }

                                let (status, body) = if target.starts_with("/api/v1/health?") {
                                    let query =
                                        target.split_once('?').map(|(_, q)| q).unwrap_or("");
                                    let challenge = query
                                        .split('&')
                                        .filter_map(|kv| kv.split_once('='))
                                        .find(|(k, _)| *k == "challenge")
                                        .map(|(_, v)| v)
                                        .unwrap_or("");
                                    let proof = openvmms_core::models::compute_health_auth_proof(
                                        token, addr, challenge,
                                    );
                                    (
                                        "200 OK",
                                        format!(
                                            r#"{{"status":"ok","service":"openvmmsd","auth_proof":"{proof}"}}"#
                                        ),
                                    )
                                } else if target == "/api/v1/host/info" {
                                    let info = HostInfo {
                                        hostname: "test".into(),
                                        os: "linux".into(),
                                        arch: "x86_64".into(),
                                        cpu_count: 4,
                                        daemon_version: "0.1.0".into(),
                                        data_dir: store_dir.clone(),
                                        runtime_dir: store_dir.clone(),
                                        listen_addr: addr.to_string(),
                                        daemon_instance_id: None,
                                        event_revision: 0,
                                        resolved_paths: Default::default(),
                                    };
                                    ("200 OK", serde_json::to_string(&info).unwrap())
                                } else if target == "/api/v1/host/config" {
                                    (
                                        "200 OK",
                                        serde_json::to_string(&HostConfig::default()).unwrap(),
                                    )
                                } else if target == "/api/v1/host/switches" {
                                    ("200 OK", "[]".into())
                                } else if method == "GET" && target == "/api/v1/vms" {
                                    (
                                        "200 OK",
                                        serde_json::to_string(&vec![vm_summary.clone()]).unwrap(),
                                    )
                                } else {
                                    ("404 Not Found", "{}".into())
                                };
                                let resp = format!(
                                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(resp.as_bytes());
                                let _ = stream.flush();
                            }));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        }

        let listener1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr1 = listener1.local_addr().unwrap();
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let listener3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr3 = listener3.local_addr().unwrap();

        let token = "no-retry-token";
        let vm_id = Uuid::new_v4();
        let mut vm_cfg = openvmms_core::models::VmConfig::from_preset(
            "Existing-VM",
            VmPreset::Custom,
            Path::new("/tmp/vm"),
        );
        vm_cfg.id = vm_id;
        let vm_summary = VmSummary {
            config: vm_cfg,
            runtime: Default::default(),
        };

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        openvmms_core::store::atomic_write(&store.token_path(), token.as_bytes(), true).unwrap();
        store.write_runtime_listen_addr(&addr1.to_string()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let p1_posts = Arc::new(AtomicUsize::new(0));
        let p1_deletes = Arc::new(AtomicUsize::new(0));
        let p2_posts = Arc::new(AtomicUsize::new(0));
        let p2_deletes = Arc::new(AtomicUsize::new(0));
        let p3_posts = Arc::new(AtomicUsize::new(0));
        let p3_deletes = Arc::new(AtomicUsize::new(0));

        let srv1 = spawn_mock_endpoint(
            listener1,
            token,
            dir.path().to_path_buf(),
            vm_summary.clone(),
            Arc::clone(&p1_posts),
            Arc::clone(&p1_deletes),
            Some(addr2),
            None,
            Arc::clone(&stop),
        );
        let srv2 = spawn_mock_endpoint(
            listener2,
            token,
            dir.path().to_path_buf(),
            vm_summary.clone(),
            Arc::clone(&p2_posts),
            Arc::clone(&p2_deletes),
            None,
            Some(addr3),
            Arc::clone(&stop),
        );
        let srv3 = spawn_mock_endpoint(
            listener3,
            token,
            dir.path().to_path_buf(),
            vm_summary,
            Arc::clone(&p3_posts),
            Arc::clone(&p3_deletes),
            None,
            None,
            Arc::clone(&stop),
        );

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let url1 = format!("http://{addr1}");
        let url2 = format!("http://{addr2}");
        let url3 = format!("http://{addr3}");

        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { base_url, .. } => assert_eq!(base_url, url1),
            _ => unreachable!(),
        }

        // Send CreateVm while connected to port1; port1 switches daemon.addr to port2 and drops
        // the connection before replying. The GUI must refresh onto port2 without retrying POST.
        let wizard_id = Uuid::new_v4();
        backend.send(GuiRequest::CreateVm {
            wizard_id,
            payload: CreateVmPayload {
                name: "NoRetry-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            },
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_url2 = false;
        let mut saw_create_err = false;
        while std::time::Instant::now() < deadline && (!saw_url2 || !saw_create_err) {
            if let Some(upd) = backend.try_recv() {
                match upd {
                    GuiUpdate::DaemonConnected { base_url, .. } if base_url == url2 => {
                        saw_url2 = true;
                    }
                    GuiUpdate::DialogFinished {
                        kind: DialogKind::NewVmWizard(wid),
                        error: Some(_),
                    } if wid == wizard_id => {
                        saw_create_err = true;
                    }
                    _ => {}
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(saw_url2 && saw_create_err, "CreateVm did not switch to url2 and report failure");
        assert_eq!(p1_posts.load(Ordering::SeqCst), 1);
        assert_eq!(p2_posts.load(Ordering::SeqCst), 0);

        // Next send DeleteVm on port2; port2 switches daemon.addr to port3 and drops the
        // connection before replying. The GUI must refresh onto port3 without retrying DELETE.
        backend.send(GuiRequest::DeleteVm {
            id: vm_id,
            delete_disks: false,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_url3 = false;
        let mut saw_delete_err = false;
        while std::time::Instant::now() < deadline && (!saw_url3 || !saw_delete_err) {
            if let Some(upd) = backend.try_recv() {
                match upd {
                    GuiUpdate::DaemonConnected { base_url, .. } if base_url == url3 => {
                        saw_url3 = true;
                    }
                    GuiUpdate::DialogFinished {
                        kind: DialogKind::DeleteVm(id),
                        error: Some(_),
                    } if id == vm_id => {
                        saw_delete_err = true;
                    }
                    _ => {}
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(saw_url3 && saw_delete_err, "DeleteVm did not switch to url3 and report failure");
        assert_eq!(p2_deletes.load(Ordering::SeqCst), 1);
        assert_eq!(p3_deletes.load(Ordering::SeqCst), 0);

        drop(backend);
        stop.store(true, Ordering::Release);
        srv1.join().unwrap();
        srv2.join().unwrap();
        srv3.join().unwrap();
    }

    #[test]
    fn sse_read_timeout_and_resync_event_reconcile_state() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let sse_count = Arc::new(AtomicUsize::new(0));
        let vms_stage = Arc::new(AtomicUsize::new(0));
        let send_resync = Arc::new(AtomicBool::new(false));
        let health_wedged = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let vm_id = Uuid::new_v4();
        let mut base_cfg = openvmms_core::models::VmConfig::from_preset(
            "SseResync-VM",
            VmPreset::Custom,
            Path::new("/tmp/vm"),
        );
        base_cfg.id = vm_id;

        let server = {
            let token = token.clone();
            let sse_count = Arc::clone(&sse_count);
            let vms_stage = Arc::clone(&vms_stage);
            let send_resync = Arc::clone(&send_resync);
            let health_wedged = Arc::clone(&health_wedged);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let sse_count = Arc::clone(&sse_count);
                            let vms_stage = Arc::clone(&vms_stage);
                            let send_resync = Arc::clone(&send_resync);
                            let health_wedged = Arc::clone(&health_wedged);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let base_cfg = base_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first_line = req_text.lines().next().unwrap_or("");
                                let target = first_line.split_whitespace().nth(1).unwrap_or("");

                                if target == "/api/v1/events" {
                                    let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    // Match daemon subscribe behavior: emit an initial leading Resync frame.
                                    let leading_json =
                                        serde_json::to_string(&DaemonEvent::Resync).unwrap();
                                    let _ = stream.write_all(
                                        format!("data: {leading_json}\n\n").as_bytes(),
                                    );
                                    let _ = stream.flush();
                                    if idx == 1 {
                                        // Keep SSE stream alive with keep-alives until stage 1 Resync is requested.
                                        while !stop.load(Ordering::Acquire)
                                            && !send_resync.load(Ordering::Acquire)
                                        {
                                            if stream.write_all(b": keepalive\n\n").is_err() {
                                                return;
                                            }
                                            let _ = stream.flush();
                                            thread::sleep(Duration::from_millis(20));
                                        }
                                        if stop.load(Ordering::Acquire) {
                                            return;
                                        }
                                        let json = serde_json::to_string(&DaemonEvent::Resync).unwrap();
                                        let _ = stream.write_all(format!("data: {json}\n\n").as_bytes());
                                        let _ = stream.flush();
                                        // Now hold socket silent with no further bytes so SSE read timeout fires.
                                    }
                                    while !stop.load(Ordering::Acquire) {
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }

                                let (status, body) = if target.starts_with("/api/v1/health?") {
                                    if health_wedged.load(Ordering::Acquire) {
                                        ("503 Service Unavailable", r#"{"status":"wedged"}"#.to_string())
                                    } else {
                                        let query =
                                            target.split_once('?').map(|(_, q)| q).unwrap_or("");
                                        let challenge = query
                                            .split('&')
                                            .filter_map(|kv| kv.split_once('='))
                                            .find(|(k, _)| *k == "challenge")
                                            .map(|(_, v)| v)
                                            .unwrap_or("");
                                        let proof = openvmms_core::models::compute_health_auth_proof(
                                            &token, addr, challenge,
                                        );
                                        (
                                            "200 OK",
                                            format!(
                                                r#"{{"status":"ok","service":"openvmmsd","auth_proof":"{proof}"}}"#
                                            ),
                                        )
                                    }
                                } else if target == "/api/v1/host/info" {
                                    let info = HostInfo {
                                        hostname: "test".into(),
                                        os: "linux".into(),
                                        arch: "x86_64".into(),
                                        cpu_count: 4,
                                        daemon_version: "0.1.0".into(),
                                        data_dir: store_dir.clone(),
                                        runtime_dir: store_dir.clone(),
                                        listen_addr: addr.to_string(),
                                        daemon_instance_id: None,
                                        event_revision: 0,
                                        resolved_paths: Default::default(),
                                    };
                                    ("200 OK", serde_json::to_string(&info).unwrap())
                                } else if target == "/api/v1/host/config" {
                                    (
                                        "200 OK",
                                        serde_json::to_string(&HostConfig::default()).unwrap(),
                                    )
                                } else if target == "/api/v1/host/switches" {
                                    ("200 OK", "[]".into())
                                } else if target == "/api/v1/vms" {
                                    let stage = vms_stage.load(Ordering::Acquire) as u64;
                                    let vm_summary = VmSummary {
                                        config: base_cfg,
                                        runtime: openvmms_core::models::VmRuntimeState {
                                            status: if stage > 0 {
                                                VmStatus::Running
                                            } else {
                                                VmStatus::Off
                                            },
                                            revision: stage + 1,
                                            ..Default::default()
                                        },
                                    };
                                    ("200 OK", serde_json::to_string(&vec![vm_summary]).unwrap())
                                } else {
                                    ("404 Not Found", "{}".into())
                                };
                                let resp = format!(
                                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(resp.as_bytes());
                                let _ = stream.flush();
                            }));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_millis(300),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let seq_sse1 = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { snapshot_seq, .. } => snapshot_seq,
            _ => unreachable!(),
        };
        assert_eq!(sse_count.load(Ordering::SeqCst), 1);

        // 1. DaemonEvent::Resync triggers a snapshot refetch with updated VM state (stage 1).
        vms_stage.store(1, Ordering::Release);
        send_resync.store(true, Ordering::Release);
        let seq2 = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { snapshot_seq, vms, .. }
                    if *snapshot_seq > seq_sse1 && vms.iter().any(|v| v.runtime.revision == 2)
            )
        }) {
            GuiUpdate::DaemonConnected { snapshot_seq, .. } => snapshot_seq,
            _ => unreachable!(),
        };
        assert_eq!(
            sse_count.load(Ordering::SeqCst),
            1,
            "stage 1 should reconcile via DaemonEvent::Resync on the existing SSE stream, not via reconnect"
        );

        // 2. Stage 2 happens before the 300ms SSE read timeout fires; reconnect fetches stage 2 snapshot.
        vms_stage.store(2, Ordering::Release);
        let _seq3 = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { snapshot_seq, vms, .. }
                    if *snapshot_seq > seq2 && vms.iter().any(|v| v.runtime.revision == 3)
            )
        });
        assert!(
            sse_count.load(Ordering::SeqCst) >= 2,
            "stage 2 should reconcile via SSE read-timeout reconnect"
        );

        // 3. With health now wedged, the next SSE read timeout emits DaemonDisconnected.
        health_wedged.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    fn mock_http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn mock_common_endpoint(
        target: &str,
        token: &str,
        addr: std::net::SocketAddr,
        store_dir: &Path,
    ) -> Option<(&'static str, String)> {
        if target.starts_with("/api/v1/health?") {
            let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
            let challenge = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "challenge")
                .map(|(_, v)| v)
                .unwrap_or("");
            let proof = openvmms_core::models::compute_health_auth_proof(token, addr, challenge);
            Some((
                "200 OK",
                format!(r#"{{"status":"ok","service":"openvmmsd","auth_proof":"{proof}"}}"#),
            ))
        } else if target == "/api/v1/host/info" {
            let info = HostInfo {
                hostname: "test".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu_count: 4,
                daemon_version: "0.1.0".into(),
                data_dir: store_dir.to_path_buf(),
                runtime_dir: store_dir.to_path_buf(),
                listen_addr: addr.to_string(),
                daemon_instance_id: None,
                event_revision: 0,
                resolved_paths: Default::default(),
            };
            Some(("200 OK", serde_json::to_string(&info).unwrap()))
        } else if target == "/api/v1/host/config" {
            Some((
                "200 OK",
                serde_json::to_string(&HostConfig::default()).unwrap(),
            ))
        } else if target == "/api/v1/host/switches" {
            Some(("200 OK", "[]".into()))
        } else {
            None
        }
    }

    #[test]
    fn first_sse_connect_refreshes_missed_vm_changes() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let sse_connected = Arc::new(AtomicBool::new(false));
        let vms_reqs = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut vm_cfg = openvmms_core::models::VmConfig::from_preset(
            "GapWindow-VM",
            VmPreset::Custom,
            &dir.path().join("vm"),
        );
        vm_cfg.id = Uuid::new_v4();

        let server = {
            let token = token.clone();
            let sse_connected = Arc::clone(&sse_connected);
            let vms_reqs = Arc::clone(&vms_reqs);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let sse_connected = Arc::clone(&sse_connected);
                            let vms_reqs = Arc::clone(&vms_reqs);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let vm_cfg = vm_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    // Record SSE arrival; send no events yet so only initial connect sync can surface the VM.
                                    sse_connected.store(true, Ordering::Release);
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    let req_idx = vms_reqs.fetch_add(1, Ordering::SeqCst);
                                    if req_idx == 0 && !sse_connected.load(Ordering::Acquire) {
                                        ("200 OK", "[]".to_string())
                                    } else {
                                        let vm_summary = VmSummary {
                                            config: vm_cfg,
                                            runtime: openvmms_core::models::VmRuntimeState {
                                                status: VmStatus::Off,
                                                revision: 2,
                                                ..Default::default()
                                            },
                                        };
                                        (
                                            "200 OK",
                                            serde_json::to_string(&vec![vm_summary]).unwrap(),
                                        )
                                    }
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_millis(400),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { vms, .. }
                    if vms.iter().any(|v| v.config.name == "GapWindow-VM")
            )
        });

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn sse_reconnect_retries_snapshot_after_reload_failure() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let sse_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut recovered_cfg = openvmms_core::models::VmConfig::from_preset(
            "Recovered-VM",
            VmPreset::Custom,
            &dir.path().join("vm"),
        );
        recovered_cfg.id = Uuid::new_v4();

        let server = {
            let token = token.clone();
            let sse_count = Arc::clone(&sse_count);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let sse_count = Arc::clone(&sse_count);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let recovered_cfg = recovered_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    // Match daemon subscribe behavior: emit an initial leading Resync frame.
                                    let leading_json =
                                        serde_json::to_string(&DaemonEvent::Resync).unwrap();
                                    let _ = stream.write_all(
                                        format!("data: {leading_json}\n\n").as_bytes(),
                                    );
                                    let _ = stream.flush();
                                    if idx == 1 {
                                        // Force quick EOF/reconnect after initial sync.
                                        thread::sleep(Duration::from_millis(50));
                                    } else {
                                        while !stop.load(Ordering::Acquire) {
                                            if stream.write_all(b": keepalive\n\n").is_err() {
                                                break;
                                            }
                                            let _ = stream.flush();
                                            thread::sleep(Duration::from_millis(25));
                                        }
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    let sse_idx = sse_count.load(Ordering::Acquire);
                                    if sse_idx == 2 {
                                        (
                                            "500 Internal Server Error",
                                            r#"{"error":"transient failure"}"#.to_string(),
                                        )
                                    } else if sse_idx >= 3 {
                                        let vm_summary = VmSummary {
                                            config: recovered_cfg,
                                            runtime: openvmms_core::models::VmRuntimeState {
                                                status: VmStatus::Running,
                                                revision: 5,
                                                ..Default::default()
                                            },
                                        };
                                        (
                                            "200 OK",
                                            serde_json::to_string(&vec![vm_summary]).unwrap(),
                                        )
                                    } else {
                                        ("200 OK", "[]".to_string())
                                    }
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_millis(250),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        // Must emit DaemonDisconnected when reconnect reload fails at sse_count == 2,
        // and then automatically recover with DaemonConnected containing Recovered-VM.
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { vms, .. }
                    if vms.iter().any(|v| v.config.name == "Recovered-VM")
            )
        });

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn host_config_patch_preserves_concurrent_cli_changes() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let daemon = rt
            .block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
            .unwrap();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let opened_cfg = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { host_config, .. } => host_config,
            _ => unreachable!(),
        };

        // Simulate CLI concurrently changing uefi_firmware_path while GUI dialog is open.
        let cli_firmware = dir.path().join("cli-MSVM.fd");
        let client = ApiClient::new(&daemon.base_url, &daemon.token).unwrap();
        let mut cli_cfg = client.host_config().unwrap();
        cli_cfg.uefi_firmware_path = Some(cli_firmware.clone());
        client.update_host_config(&cli_cfg).unwrap();

        // GUI user only changes openvmm_bin from the stale `opened_cfg` baseline.
        let gui_openvmm = dir.path().join("gui-openvmm");
        let mut edited = opened_cfg.clone();
        edited.openvmm_bin = Some(gui_openvmm.clone());
        let patch = HostConfigPatch::from_diff(&opened_cfg, &edited);
        backend.send(GuiRequest::SaveHostConfig(patch));

        let updated_cfg = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::HostConfigUpdated { .. })
        }) {
            GuiUpdate::HostConfigUpdated { host_config, .. } => host_config,
            _ => unreachable!(),
        };
        assert_eq!(updated_cfg.openvmm_bin, Some(gui_openvmm));
        assert_eq!(updated_cfg.uefi_firmware_path, Some(cli_firmware));

        drop(backend);
        rt.block_on(daemon.shutdown());
    }

    #[test]
    fn sse_leading_resync_skips_duplicate_snapshot_while_later_resync_refreshes() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let vms_reqs = Arc::new(AtomicUsize::new(0));
        let send_second_resync = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let server = {
            let token = token.clone();
            let vms_reqs = Arc::clone(&vms_reqs);
            let send_second_resync = Arc::clone(&send_second_resync);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let vms_reqs = Arc::clone(&vms_reqs);
                            let send_second_resync = Arc::clone(&send_second_resync);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n: keep-alive\n\ndata: {\"event\":\"resync\"}\n\n",
                                    );
                                    let _ = stream.flush();
                                    let mut sent_second = false;
                                    while !stop.load(Ordering::Acquire) {
                                        if !sent_second
                                            && send_second_resync.load(Ordering::Acquire)
                                        {
                                            sent_second = true;
                                            if stream
                                                .write_all(b"data: {\"event\":\"resync\"}\n\n")
                                                .is_err()
                                            {
                                                break;
                                            }
                                            let _ = stream.flush();
                                        }
                                        thread::sleep(Duration::from_millis(20));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    vms_reqs.fetch_add(1, Ordering::SeqCst);
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(5),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        // Single initial SSE connect sync (1); wait briefly for leading Resync to arrive.
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { snapshot_seq: 1, .. })
        });
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            vms_reqs.load(Ordering::Acquire),
            1,
            "leading Resync right after initial SSE connect sync must not trigger a second snapshot fetch"
        );

        // Subsequent Resync on the same stream must still refresh.
        send_second_resync.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { snapshot_seq, .. } if *snapshot_seq >= 2)
        });
        assert_eq!(vms_reqs.load(Ordering::Acquire), 2);

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn midstream_resync_and_host_config_snapshot_failures_reconnect_and_recover() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let sse_count = Arc::new(AtomicUsize::new(0));
        let fail_vms = Arc::new(AtomicBool::new(false));
        let send_resync = Arc::new(AtomicBool::new(false));
        let send_host_cfg = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let mut recovered_cfg = openvmms_core::models::VmConfig::from_preset(
            "MidstreamRecovered-VM",
            VmPreset::Custom,
            &dir.path().join("vm"),
        );
        recovered_cfg.id = Uuid::new_v4();

        let server = {
            let token = token.clone();
            let sse_count = Arc::clone(&sse_count);
            let fail_vms = Arc::clone(&fail_vms);
            let send_resync = Arc::clone(&send_resync);
            let send_host_cfg = Arc::clone(&send_host_cfg);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let sse_count = Arc::clone(&sse_count);
                            let fail_vms = Arc::clone(&fail_vms);
                            let send_resync = Arc::clone(&send_resync);
                            let send_host_cfg = Arc::clone(&send_host_cfg);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let recovered_cfg = recovered_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: {\"event\":\"resync\"}\n\n",
                                    );
                                    let _ = stream.flush();
                                    let mut sent_mid = false;
                                    while !stop.load(Ordering::Acquire) {
                                        if idx == 1
                                            && !sent_mid
                                            && send_resync.load(Ordering::Acquire)
                                        {
                                            sent_mid = true;
                                            if stream
                                                .write_all(b"data: {\"event\":\"resync\"}\n\n")
                                                .is_err()
                                            {
                                                break;
                                            }
                                            let _ = stream.flush();
                                        } else if idx >= 2
                                            && !sent_mid
                                            && send_host_cfg.swap(false, Ordering::AcqRel)
                                        {
                                            sent_mid = true;
                                            if stream
                                                .write_all(
                                                    b"data: {\"event\":\"host_config_updated\"}\n\n",
                                                )
                                                .is_err()
                                            {
                                                break;
                                            }
                                            let _ = stream.flush();
                                        } else {
                                            if stream.write_all(b": keep-alive\n\n").is_err() {
                                                break;
                                            }
                                            let _ = stream.flush();
                                        }
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    if fail_vms.swap(false, Ordering::AcqRel) {
                                        (
                                            "500 Internal Server Error",
                                            r#"{"error":"transient vms failure"}"#.to_string(),
                                        )
                                    } else if sse_count.load(Ordering::Acquire) >= 3 {
                                        let vm_summary = VmSummary {
                                            config: recovered_cfg,
                                            runtime: openvmms_core::models::VmRuntimeState {
                                                status: VmStatus::Running,
                                                revision: 9,
                                                ..Default::default()
                                            },
                                        };
                                        (
                                            "200 OK",
                                            serde_json::to_string(&vec![vm_summary]).unwrap(),
                                        )
                                    } else {
                                        ("200 OK", "[]".to_string())
                                    }
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(5),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { snapshot_seq: 1, .. })
        });

        // 1. Mid-stream Resync fails snapshot -> must drop stream and reconnect (sse_count >= 2),
        //    emitting DaemonConnected on the new SSE stream.
        fail_vms.store(true, Ordering::Release);
        send_resync.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { snapshot_seq, .. } if *snapshot_seq >= 2)
        });
        assert!(sse_count.load(Ordering::Acquire) >= 2);

        // 2. On sse_count == 2, mid-stream HostConfigUpdated fails snapshot -> drops stream, reconnects at sse_count == 3,
        //    and loads MidstreamRecovered-VM.
        fail_vms.store(true, Ordering::Release);
        send_host_cfg.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { vms, .. }
                    if vms.iter().any(|v| v.config.name == "MidstreamRecovered-VM")
            )
        });
        assert!(sse_count.load(Ordering::Acquire) >= 3);

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn duplicate_storage_op_emits_finished_for_rejected_op_id_and_refresh_reconnect_resets_disconnect_tracking() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let health_fail = Arc::new(AtomicBool::new(false));
        let sse_fail = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let vm_id = Uuid::new_v4();
        let mut vm_cfg = openvmms_core::models::VmConfig::from_preset(
            "OpId-VM",
            VmPreset::Custom,
            &dir.path().join("vm"),
        );
        vm_cfg.id = vm_id;

        let server = {
            let token = token.clone();
            let health_fail = Arc::clone(&health_fail);
            let sse_fail = Arc::clone(&sse_fail);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let health_fail = Arc::clone(&health_fail);
                            let sse_fail = Arc::clone(&sse_fail);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let vm_cfg = vm_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first = req_text.lines().next().unwrap_or("");
                                let mut parts = first.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                if target == "/api/v1/events" {
                                    if sse_fail.load(Ordering::Acquire) {
                                        let resp = mock_http_response("503 Service Unavailable", "{}");
                                        let _ = stream.write_all(resp.as_bytes());
                                        return;
                                    }
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) && !sse_fail.load(Ordering::Acquire) {
                                        if stream.write_all(b": keep-alive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                if target.starts_with("/api/v1/health?") && health_fail.load(Ordering::Acquire) {
                                    let resp = mock_http_response("503 Service Unavailable", "{}");
                                    let _ = stream.write_all(resp.as_bytes());
                                    return;
                                }
                                if method == "POST" && target.ends_with("/disks") {
                                    thread::sleep(Duration::from_millis(250));
                                    let summary = VmSummary {
                                        config: vm_cfg,
                                        runtime: Default::default(),
                                    };
                                    let resp = mock_http_response(
                                        "200 OK",
                                        &serde_json::to_string(&summary).unwrap(),
                                    );
                                    let _ = stream.write_all(resp.as_bytes());
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_millis(300),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        // 1. Duplicate AttachDisk while op 11 is in flight must emit StorageOpFinished for op_id=12 immediately.
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 11,
            payload: AttachDiskPayload {
                path: dir.path().join("d.img"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        thread::sleep(Duration::from_millis(40));
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 12,
            payload: AttachDiskPayload {
                path: dir.path().join("d.img"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StatusMessage(msg) if msg == "Disk attach is already in progress for this VM"
            )
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 12
                } if *id == vm_id
            )
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 11
                } if *id == vm_id
            )
        });

        // 2. Drop health so SSE emits DaemonDisconnected, then keep SSE failing while
        //    RefreshAll succeeds and reconnects, then drop health again -> SSE must still emit DaemonDisconnected.
        health_fail.store(true, Ordering::Release);
        sse_fail.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });

        health_fail.store(false, Ordering::Release);
        backend.send(GuiRequest::RefreshAll);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        health_fail.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn storage_op_retry_succeeds_after_daemon_instance_switch_while_prior_op_blocked() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let daemon_a = Uuid::new_v4();
        let daemon_b = Uuid::new_v4();
        let current_instance_id = Arc::new(std::sync::Mutex::new(daemon_a));
        let attach_count = Arc::new(AtomicUsize::new(0));
        let first_attach_started = Arc::new(AtomicBool::new(false));
        let unblock_first_attach = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let vm_id = Uuid::new_v4();
        let mut vm_cfg = openvmms_core::models::VmConfig::from_preset(
            "DaemonSwitch-VM",
            VmPreset::Custom,
            &dir.path().join("vm"),
        );
        vm_cfg.id = vm_id;

        let server = {
            let token = token.clone();
            let current_instance_id = Arc::clone(&current_instance_id);
            let attach_count = Arc::clone(&attach_count);
            let first_attach_started = Arc::clone(&first_attach_started);
            let unblock_first_attach = Arc::clone(&unblock_first_attach);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let current_instance_id = Arc::clone(&current_instance_id);
                            let attach_count = Arc::clone(&attach_count);
                            let first_attach_started = Arc::clone(&first_attach_started);
                            let unblock_first_attach = Arc::clone(&unblock_first_attach);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let vm_cfg = vm_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first = req_text.lines().next().unwrap_or("");
                                let mut parts = first.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                if target == "/api/v1/events" {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keep-alive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                if method == "POST" && target.ends_with("/disks") {
                                    let idx = attach_count.fetch_add(1, Ordering::SeqCst);
                                    if idx == 0 {
                                        first_attach_started.store(true, Ordering::Release);
                                        let deadline = std::time::Instant::now() + Duration::from_secs(5);
                                        while !unblock_first_attach.load(Ordering::Acquire)
                                            && !stop.load(Ordering::Acquire)
                                            && std::time::Instant::now() < deadline
                                        {
                                            thread::sleep(Duration::from_millis(10));
                                        }
                                    }
                                    let summary = VmSummary {
                                        config: vm_cfg,
                                        runtime: Default::default(),
                                    };
                                    let resp = mock_http_response(
                                        "200 OK",
                                        &serde_json::to_string(&summary).unwrap(),
                                    );
                                    let _ = stream.write_all(resp.as_bytes());
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/host/info" {
                                    let info = HostInfo {
                                        hostname: "test".into(),
                                        os: "linux".into(),
                                        arch: "x86_64".into(),
                                        cpu_count: 4,
                                        daemon_version: "0.1.0".into(),
                                        data_dir: store_dir.clone(),
                                        runtime_dir: store_dir.clone(),
                                        listen_addr: addr.to_string(),
                                        daemon_instance_id: Some(*current_instance_id.lock().unwrap()),
                                        event_revision: 0,
                                        resolved_paths: Default::default(),
                                    };
                                    ("200 OK", serde_json::to_string(&info).unwrap())
                                } else if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timeout(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(5),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { host_info, .. }
                    if host_info.daemon_instance_id == Some(daemon_a)
            )
        });

        // Start op 1 on daemon A and wait until it blocks inside the HTTP POST handler.
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 1,
            payload: AttachDiskPayload {
                path: dir.path().join("d1.img"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        let wait_start = std::time::Instant::now();
        while !first_attach_started.load(Ordering::Acquire) {
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "timed out waiting for op 1 to hit mock server"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // Duplicate op 2 on the same daemon instance must be rejected with StatusMessage + StorageOpFinished.
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 2,
            payload: AttachDiskPayload {
                path: dir.path().join("d1.img"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StatusMessage(msg) if msg == "Disk attach is already in progress for this VM"
            )
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 2
                } if *id == vm_id
            )
        });

        // Switch to daemon B while op 1 on daemon A is still blocked.
        *current_instance_id.lock().unwrap() = daemon_b;
        backend.send(GuiRequest::RefreshAll);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { host_info, .. }
                    if host_info.daemon_instance_id == Some(daemon_b)
            )
        });

        // Retry op 3 on daemon B must proceed (not be dropped) and complete immediately.
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 3,
            payload: AttachDiskPayload {
                path: dir.path().join("d1.img"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 3
                } if *id == vm_id
            )
        });
        assert_eq!(attach_count.load(Ordering::Acquire), 2);

        // Unblock op 1 and verify it finishes cleanly as well.
        unblock_first_attach.store(true, Ordering::Release);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 1
                } if *id == vm_id
            )
        });

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn sse_backs_off_and_reports_error_when_events_fail_on_healthy_daemon() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let server = {
            let token = token.clone();
            let events_times = Arc::clone(&events_times);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let events_times = Arc::clone(&events_times);
                            let store_dir = store_dir.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                let (status, body) = if target == "/api/v1/events" {
                                    events_times.lock().unwrap().push(std::time::Instant::now());
                                    (
                                        "500 Internal Server Error",
                                        r#"{"error":"events broken"}"#.to_string(),
                                    )
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timing(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(2),
            Duration::from_millis(40),
            Duration::from_millis(320),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        }) {
            GuiUpdate::DaemonDisconnected(reason) => {
                assert!(
                    reason.contains("500"),
                    "expected HTTP 500 error in DaemonDisconnected when daemon is healthy, got: {reason}"
                );
            }
            _ => unreachable!(),
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if events_times.lock().unwrap().len() >= 4 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for 4 SSE retry attempts"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let times = events_times.lock().unwrap().clone();
        // Delays before attempts 1..4 are 40ms, 80ms, 160ms; verify backoff grows past initial 40ms.
        let gap1 = times[1].duration_since(times[0]);
        let gap3 = times[3].duration_since(times[2]);
        assert!(
            gap3 >= Duration::from_millis(120) && gap3 > gap1,
            "expected exponential SSE backoff (gap1={gap1:?}, gap3={gap3:?})"
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn save_host_config_succeeds_when_followup_host_info_fails() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let host_info_reqs = Arc::new(AtomicUsize::new(0));
        let fail_followup_info = Arc::new(AtomicBool::new(false));
        let patch_applied = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let new_bin = dir.path().join("updated-openvmm");

        let server = {
            let token = token.clone();
            let host_info_reqs = Arc::clone(&host_info_reqs);
            let fail_followup_info = Arc::clone(&fail_followup_info);
            let patch_applied = Arc::clone(&patch_applied);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            let new_bin = new_bin.clone();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let host_info_reqs = Arc::clone(&host_info_reqs);
                            let fail_followup_info = Arc::clone(&fail_followup_info);
                            let patch_applied = Arc::clone(&patch_applied);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let new_bin = new_bin.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first = req_text.lines().next().unwrap_or("");
                                let mut parts = first.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                if target == "/api/v1/events" {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/host/info" {
                                    host_info_reqs.fetch_add(1, Ordering::SeqCst);
                                    if fail_followup_info.load(Ordering::Acquire) {
                                        (
                                            "500 Internal Server Error",
                                            r#"{"error":"transient host_info error"}"#.to_string(),
                                        )
                                    } else {
                                        mock_common_endpoint(target, &token, addr, &store_dir)
                                            .unwrap()
                                    }
                                } else if method == "PATCH" && target == "/api/v1/host/config" {
                                    patch_applied.store(true, Ordering::Release);
                                    let mut cfg = HostConfig::default();
                                    cfg.openvmm_bin = Some(new_bin);
                                    ("200 OK", serde_json::to_string(&cfg).unwrap())
                                } else if method == "GET"
                                    && target == "/api/v1/host/config"
                                    && patch_applied.load(Ordering::Acquire)
                                {
                                    let mut cfg = HostConfig::default();
                                    cfg.openvmm_bin = Some(new_bin);
                                    ("200 OK", serde_json::to_string(&cfg).unwrap())
                                } else if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        fail_followup_info.store(true, Ordering::Release);
        let before_reqs = host_info_reqs.load(Ordering::Acquire);
        let patch = HostConfigPatch {
            openvmm_bin: Some(Some(new_bin.clone())),
            ..Default::default()
        };
        backend.send(GuiRequest::SaveHostConfig(patch));

        let mut saw_host_cfg = false;
        let mut saw_dialog_ok = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && (!saw_host_cfg || !saw_dialog_ok) {
            while let Some(upd) = backend.try_recv() {
                match upd {
                    GuiUpdate::ErrorMessage(err) => {
                        panic!("unexpected GuiUpdate::ErrorMessage after PATCH succeeded: {err}");
                    }
                    GuiUpdate::HostConfigUpdated {
                        host_info,
                        host_config,
                        ..
                    } => {
                        assert!(host_info.is_none());
                        assert_eq!(host_config.openvmm_bin, Some(new_bin.clone()));
                        saw_host_cfg = true;
                    }
                    GuiUpdate::DialogFinished {
                        kind: DialogKind::HostSettings,
                        error,
                    } => {
                        assert_eq!(error, None);
                        saw_dialog_ok = true;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_host_cfg && saw_dialog_ok);
        assert!(host_info_reqs.load(Ordering::Acquire) > before_reqs);

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn shutdown_emits_storage_op_finished_for_dropped_attach_and_mount() {
        let backend = BackendHandle::spawn(None, None, egui::Context::default());
        backend.state.shutdown.store(true, Ordering::Release);

        let vm_id = Uuid::new_v4();
        backend.send(GuiRequest::AttachDisk {
            id: vm_id,
            op_id: 41,
            payload: AttachDiskPayload {
                path: PathBuf::from("/tmp/disk.vhdx"),
                lun: None,
                read_only: false,
                format: None,
            },
        });
        backend.send(GuiRequest::MountIso {
            id: vm_id,
            op_id: 42,
            payload: MountIsoPayload {
                path: PathBuf::from("/tmp/install.iso"),
                lun: None,
            },
        });

        let _ = recv_until(&backend, Duration::from_secs(2), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::AttachDisk,
                    op_id: 41
                } if *id == vm_id
            )
        });
        let _ = recv_until(&backend, Duration::from_secs(2), |u| {
            matches!(
                u,
                GuiUpdate::StorageOpFinished {
                    vm_id: id,
                    op: StorageOpKind::MountIso,
                    op_id: 42
                } if *id == vm_id
            )
        });
    }

    #[test]
    fn sse_backoff_resets_after_successful_connect_and_daemon_stopping() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
        let sse_attempts = Arc::new(AtomicUsize::new(0));
        let send_stopping_1 = Arc::new(AtomicBool::new(false));
        let send_stopping_2 = Arc::new(AtomicBool::new(false));
        let stopping_1_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let stopping_2_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let stop = Arc::new(AtomicBool::new(false));

        let vm_4_cfg = openvmms_core::models::VmConfig::from_preset(
            "Attempt4",
            VmPreset::Custom,
            &dir.path().join("vm4"),
        );
        let vm_5_cfg = openvmms_core::models::VmConfig::from_preset(
            "Attempt5",
            VmPreset::Custom,
            &dir.path().join("vm5"),
        );

        let server = {
            let token = token.clone();
            let events_times = Arc::clone(&events_times);
            let sse_attempts = Arc::clone(&sse_attempts);
            let send_stopping_1 = Arc::clone(&send_stopping_1);
            let send_stopping_2 = Arc::clone(&send_stopping_2);
            let stopping_1_at = Arc::clone(&stopping_1_at);
            let stopping_2_at = Arc::clone(&stopping_2_at);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let events_times = Arc::clone(&events_times);
                            let sse_attempts = Arc::clone(&sse_attempts);
                            let send_stopping_1 = Arc::clone(&send_stopping_1);
                            let send_stopping_2 = Arc::clone(&send_stopping_2);
                            let stopping_1_at = Arc::clone(&stopping_1_at);
                            let stopping_2_at = Arc::clone(&stopping_2_at);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let vm_4_cfg = vm_4_cfg.clone();
                            let vm_5_cfg = vm_5_cfg.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    let idx = sse_attempts.fetch_add(1, Ordering::SeqCst);
                                    events_times.lock().unwrap().push(std::time::Instant::now());
                                    if idx < 4 {
                                        let resp = mock_http_response(
                                            "500 Internal Server Error",
                                            r#"{"error":"transient events error"}"#,
                                        );
                                        let _ = stream.write_all(resp.as_bytes());
                                        return;
                                    }
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    let (flag, sent_at) = if idx == 4 {
                                        (&send_stopping_1, &stopping_1_at)
                                    } else if idx == 5 {
                                        (&send_stopping_2, &stopping_2_at)
                                    } else {
                                        while !stop.load(Ordering::Acquire) {
                                            if stream.write_all(b": keepalive\n\n").is_err() {
                                                break;
                                            }
                                            let _ = stream.flush();
                                            thread::sleep(Duration::from_millis(20));
                                        }
                                        return;
                                    };
                                    while !stop.load(Ordering::Acquire) {
                                        if flag.load(Ordering::Acquire) {
                                            *sent_at.lock().unwrap() =
                                                Some(std::time::Instant::now());
                                            let _ = stream.write_all(
                                                b"data: {\"event\":\"daemon_stopping\"}\n\n",
                                            );
                                            let _ = stream.flush();
                                            break;
                                        }
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(10));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    let cur = sse_attempts.load(Ordering::Acquire);
                                    let vms = if cur == 5 {
                                        vec![VmSummary {
                                            config: vm_4_cfg,
                                            runtime: openvmms_core::models::VmRuntimeState {
                                                status: VmStatus::Off,
                                                revision: 1,
                                                ..Default::default()
                                            },
                                        }]
                                    } else if cur == 6 {
                                        vec![VmSummary {
                                            config: vm_5_cfg,
                                            runtime: openvmms_core::models::VmRuntimeState {
                                                status: VmStatus::Off,
                                                revision: 2,
                                                ..Default::default()
                                            },
                                        }]
                                    } else {
                                        Vec::new()
                                    };
                                    ("200 OK", serde_json::to_string(&vms).unwrap())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timing(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(2),
            Duration::from_millis(40),
            Duration::from_millis(640),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        // Wait until SSE succeeds after 4 backoff failures (40+80+160+320ms -> delay was 640ms).
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { vms, .. }
                    if vms.iter().any(|v| v.config.name == "Attempt4")
            )
        });
        send_stopping_1.store(true, Ordering::Release);

        // Next connect after DaemonStopping must also reset backoff before the second DaemonStopping.
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { vms, .. }
                    if vms.iter().any(|v| v.config.name == "Attempt5")
            )
        });
        send_stopping_2.store(true, Ordering::Release);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while events_times.lock().unwrap().len() < 7 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for reconnect after second DaemonStopping"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let times = events_times.lock().unwrap().clone();
        let stop1 = stopping_1_at.lock().unwrap().unwrap();
        let stop2 = stopping_2_at.lock().unwrap().unwrap();
        let gap_after_stop1 = times[5].duration_since(stop1);
        let gap_after_stop2 = times[6].duration_since(stop2);
        assert!(
            gap_after_stop1 < Duration::from_millis(300),
            "expected reconnect delay to reset to ~40ms after connect snapshot, got {gap_after_stop1:?}"
        );
        assert!(
            gap_after_stop2 < Duration::from_millis(300),
            "expected reconnect delay to remain reset across DaemonStopping restarts, got {gap_after_stop2:?}"
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn save_host_config_takes_snapshot_seq_after_patch() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let patch_started = Arc::new(AtomicBool::new(false));
        let unblock_patch = Arc::new(AtomicBool::new(false));
        let patch_applied = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let old_bin = dir.path().join("old-openvmm");
        let new_bin = dir.path().join("new-openvmm");

        let server = {
            let token = token.clone();
            let patch_started = Arc::clone(&patch_started);
            let unblock_patch = Arc::clone(&unblock_patch);
            let patch_applied = Arc::clone(&patch_applied);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            let old_bin = old_bin.clone();
            let new_bin = new_bin.clone();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let patch_started = Arc::clone(&patch_started);
                            let unblock_patch = Arc::clone(&unblock_patch);
                            let patch_applied = Arc::clone(&patch_applied);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let old_bin = old_bin.clone();
                            let new_bin = new_bin.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first = req_text.lines().next().unwrap_or("");
                                let mut parts = first.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                if target == "/api/v1/events" {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) =
                                    if method == "PATCH" && target == "/api/v1/host/config" {
                                        patch_started.store(true, Ordering::Release);
                                        while !unblock_patch.load(Ordering::Acquire)
                                            && !stop.load(Ordering::Acquire)
                                        {
                                            thread::sleep(Duration::from_millis(10));
                                        }
                                        patch_applied.store(true, Ordering::Release);
                                        let mut cfg = HostConfig::default();
                                        cfg.openvmm_bin = Some(new_bin);
                                        ("200 OK", serde_json::to_string(&cfg).unwrap())
                                    } else if method == "GET" && target == "/api/v1/host/config" {
                                        let mut cfg = HostConfig::default();
                                        cfg.openvmm_bin = Some(if patch_applied.load(Ordering::Acquire) {
                                            new_bin
                                        } else {
                                            old_bin
                                        });
                                        ("200 OK", serde_json::to_string(&cfg).unwrap())
                                    } else if target == "/api/v1/vms" {
                                        ("200 OK", "[]".to_string())
                                    } else {
                                        mock_common_endpoint(target, &token, addr, &store_dir)
                                            .unwrap_or(("404 Not Found", "{}".into()))
                                    };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let initial_seq = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        }) {
            GuiUpdate::DaemonConnected { snapshot_seq, .. } => snapshot_seq,
            _ => unreachable!(),
        };

        backend.send(GuiRequest::SaveHostConfig(HostConfigPatch {
            openvmm_bin: Some(Some(new_bin.clone())),
            ..Default::default()
        }));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !patch_started.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for PATCH to start"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // A RefreshAll starting while PATCH is in-flight reads the old config.
        backend.send(GuiRequest::RefreshAll);
        let refresh_seq = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DaemonConnected { snapshot_seq, .. } if *snapshot_seq > initial_seq
            )
        }) {
            GuiUpdate::DaemonConnected {
                snapshot_seq,
                host_config,
                ..
            } => {
                assert_eq!(host_config.openvmm_bin, Some(old_bin));
                snapshot_seq
            }
            _ => unreachable!(),
        };

        unblock_patch.store(true, Ordering::Release);
        let save_seq = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::HostConfigUpdated { .. })
        }) {
            GuiUpdate::HostConfigUpdated {
                snapshot_seq,
                host_config,
                ..
            } => {
                assert_eq!(host_config.openvmm_bin, Some(new_bin));
                snapshot_seq
            }
            _ => unreachable!(),
        };
        assert!(
            save_seq > refresh_seq,
            "SaveHostConfig seq ({save_seq}) must exceed RefreshAll seq ({refresh_seq}) started before PATCH returned"
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn host_config_updated_without_host_info_does_not_mark_notified_connected() {
        let backend = BackendHandle::new_for_test(egui::Context::default());
        assert!(!backend.state.notified_connected.load(Ordering::Acquire));
        assert!(backend.state.emit_checked(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:1".into(),
            host_info: None,
            host_config: HostConfig::default(),
            snapshot_seq: 1,
        }));
        assert!(!backend.state.notified_connected.load(Ordering::Acquire));
    }

    #[test]
    fn sse_backoff_does_not_reset_when_stream_drops_immediately_after_snapshot() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let server = {
            let token = token.clone();
            let events_times = Arc::clone(&events_times);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let events_times = Arc::clone(&events_times);
                            let store_dir = store_dir.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    events_times.lock().unwrap().push(std::time::Instant::now());
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    // Immediately drop the SSE stream right after serving 200 OK.
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timing(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(2),
            Duration::from_millis(40),
            Duration::from_millis(640),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while events_times.lock().unwrap().len() < 5 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for 5 SSE reconnect attempts"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let times = events_times.lock().unwrap().clone();
        // Delays before attempts 1..4 should grow exponentially (40ms -> 80ms -> 160ms -> 320ms),
        // rather than resetting to 40ms after each immediate drop.
        let gap3_4 = times[4].duration_since(times[3]);
        assert!(
            gap3_4 >= Duration::from_millis(240),
            "expected exponential backoff to preserve >= 240ms delay after immediate stream drops, got {gap3_4:?} (all gaps: {:?})",
            times.windows(2).map(|w| w[1].duration_since(w[0])).collect::<Vec<_>>()
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn sse_backoff_resets_after_stream_stays_stable() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let sse_attempts = Arc::new(AtomicUsize::new(0));
        let stable_closed_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let reconnected_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let stop = Arc::new(AtomicBool::new(false));

        let server = {
            let token = token.clone();
            let sse_attempts = Arc::clone(&sse_attempts);
            let stable_closed_at = Arc::clone(&stable_closed_at);
            let reconnected_at = Arc::clone(&reconnected_at);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let sse_attempts = Arc::clone(&sse_attempts);
                            let stable_closed_at = Arc::clone(&stable_closed_at);
                            let reconnected_at = Arc::clone(&reconnected_at);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    let idx = sse_attempts.fetch_add(1, Ordering::SeqCst);
                                    if idx < 5 {
                                        let resp = mock_http_response(
                                            "500 Internal Server Error",
                                            r#"{"error":"transient events error"}"#,
                                        );
                                        let _ = stream.write_all(resp.as_bytes());
                                        return;
                                    }
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    if idx == 5 {
                                        // Stay connected longer than sse_backoff_stable_after (120ms), then close without DaemonStopping.
                                        for _ in 0..8 {
                                            if stop.load(Ordering::Acquire) {
                                                return;
                                            }
                                            if stream.write_all(b": keepalive\n\n").is_err() {
                                                return;
                                            }
                                            let _ = stream.flush();
                                            thread::sleep(Duration::from_millis(30));
                                        }
                                        *stable_closed_at.lock().unwrap() =
                                            Some(std::time::Instant::now());
                                        return;
                                    }
                                    if idx == 6 {
                                        *reconnected_at.lock().unwrap() =
                                            Some(std::time::Instant::now());
                                    }
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(20));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        // After 5 failures (40+80+160+320+640ms), backoff reaches 1280ms (max 1500ms).
        // Stream #5 stays up for ~240ms (> stable_after=120ms) and drops; backoff should reset to 40ms.
        let backend = BackendHandle::spawn_with_sse_timing_and_stable(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(2),
            Duration::from_millis(40),
            Duration::from_millis(1500),
            Duration::from_millis(120),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while reconnected_at.lock().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for reconnect after stable SSE stream drop"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let closed = stable_closed_at.lock().unwrap().unwrap();
        let reconnected = reconnected_at.lock().unwrap().unwrap();
        let gap = reconnected.duration_since(closed);
        assert!(
            gap < Duration::from_millis(800),
            "expected reconnect delay to reset to ~40ms after stable stream (un-reset would be ~1280ms), got {gap:?}"
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn save_host_config_fetches_fresher_cli_change_after_patch() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let gui_bin = dir.path().join("gui-openvmm");
        let cli_fw = dir.path().join("cli-MSVM.fd");
        let patch_returned = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let server = {
            let token = token.clone();
            let gui_bin = gui_bin.clone();
            let cli_fw = cli_fw.clone();
            let patch_returned = Arc::clone(&patch_returned);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let gui_bin = gui_bin.clone();
                            let cli_fw = cli_fw.clone();
                            let patch_returned = Arc::clone(&patch_returned);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let first = req_text.lines().next().unwrap_or("");
                                let mut parts = first.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                if target == "/api/v1/events" {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                                    );
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) =
                                    if method == "PATCH" && target == "/api/v1/host/config" {
                                        patch_returned.store(true, Ordering::Release);
                                        let mut cfg = HostConfig::default();
                                        cfg.openvmm_bin = Some(gui_bin);
                                        ("200 OK", serde_json::to_string(&cfg).unwrap())
                                    } else if method == "GET" && target == "/api/v1/host/config" {
                                        let mut cfg = HostConfig::default();
                                        if patch_returned.load(Ordering::Acquire) {
                                            // Simulate a CLI change committed immediately after PATCH.
                                            cfg.openvmm_bin = Some(gui_bin);
                                            cfg.uefi_firmware_path = Some(cli_fw);
                                        }
                                        ("200 OK", serde_json::to_string(&cfg).unwrap())
                                    } else if target == "/api/v1/vms" {
                                        ("200 OK", "[]".to_string())
                                    } else {
                                        mock_common_endpoint(target, &token, addr, &store_dir)
                                            .unwrap_or(("404 Not Found", "{}".into()))
                                    };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonConnected { .. })
        });

        backend.send(GuiRequest::SaveHostConfig(HostConfigPatch {
            openvmm_bin: Some(Some(gui_bin.clone())),
            ..Default::default()
        }));

        let updated_cfg = match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::HostConfigUpdated { .. })
        }) {
            GuiUpdate::HostConfigUpdated { host_config, .. } => host_config,
            _ => unreachable!(),
        };
        assert_eq!(updated_cfg.openvmm_bin, Some(gui_bin));
        assert_eq!(updated_cfg.uefi_firmware_path, Some(cli_fw));

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn wake_sse_interrupts_long_backoff_immediately() {
        use openvmms_core::models::{VmConfig, VmPreset, VmRuntimeState};
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let token = store.load_or_create_token().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        store.write_runtime_listen_addr(&addr.to_string()).unwrap();

        let fail_events = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let vm_cfg = VmConfig::from_preset("Woken-VM", VmPreset::Custom, &store.vm_dir(Uuid::new_v4()));
        let vm_summary = VmSummary {
            config: vm_cfg,
            runtime: VmRuntimeState {
                revision: 42,
                ..Default::default()
            },
        };

        let server = {
            let token = token.clone();
            let fail_events = Arc::clone(&fail_events);
            let stop = Arc::clone(&stop);
            let store_dir = dir.path().to_path_buf();
            let vm_summary = vm_summary.clone();
            listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match accept_blocking_stream(&listener) {
                        Ok((mut stream, _)) => {
                            let token = token.clone();
                            let fail_events = Arc::clone(&fail_events);
                            let stop = Arc::clone(&stop);
                            let store_dir = store_dir.clone();
                            let vm_summary = vm_summary.clone();
                            handlers.push(thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                let mut buf = [0u8; 4096];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };
                                let req_text = String::from_utf8_lossy(&buf[..n]);
                                let target = req_text
                                    .lines()
                                    .next()
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .unwrap_or("");
                                if target == "/api/v1/events" {
                                    if fail_events.load(Ordering::Acquire) {
                                        let resp = mock_http_response(
                                            "500 Internal Server Error",
                                            r#"{"error":"events unavailable"}"#,
                                        );
                                        let _ = stream.write_all(resp.as_bytes());
                                        return;
                                    }
                                    let evt = DaemonEvent::VmUpdated { vm: vm_summary };
                                    let frame = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: {}\n\ndata: {}\n\n",
                                        serde_json::to_string(&DaemonEvent::Resync).unwrap(),
                                        serde_json::to_string(&evt).unwrap(),
                                    );
                                    let _ = stream.write_all(frame.as_bytes());
                                    let _ = stream.flush();
                                    while !stop.load(Ordering::Acquire) {
                                        if stream.write_all(b": keepalive\n\n").is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(25));
                                    }
                                    return;
                                }
                                let (status, body) = if target == "/api/v1/vms" {
                                    ("200 OK", "[]".to_string())
                                } else {
                                    mock_common_endpoint(target, &token, addr, &store_dir)
                                        .unwrap_or(("404 Not Found", "{}".into()))
                                };
                                let resp = mock_http_response(status, &body);
                                let _ = stream.write_all(resp.as_bytes());
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for h in handlers {
                    let _ = h.join();
                }
            })
        };

        let backend = BackendHandle::spawn_with_sse_timing(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
            Duration::from_secs(2),
            Duration::from_secs(10),
            Duration::from_secs(10),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        // Wait until SSE fails and enters the 10-second backoff sleep.
        let _ = recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });
        thread::sleep(Duration::from_millis(50));

        // Now bring SSE up and trigger ConnectOrStartDaemon, which must wake backoff immediately.
        fail_events.store(false, Ordering::Release);
        let started = std::time::Instant::now();
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let _ = recv_until(&backend, Duration::from_secs(2), |u| {
            matches!(
                u,
                GuiUpdate::VmUpdated { vm, .. } if vm.config.name == "Woken-VM"
            )
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SSE wakeup took too long: {:?}",
            started.elapsed()
        );

        drop(backend);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_token_surfaces_reason_in_daemon_disconnected() {
        let dir = tempfile::tempdir().unwrap();
        let store = openvmms_core::store::RegistryStore::new(dir.path()).unwrap();
        let _lock = store.try_acquire_daemon_lock().unwrap();
        let _token = store.load_or_create_token().unwrap();
        store.write_runtime_listen_addr("127.0.0.1:47621").unwrap();
        std::fs::hard_link(store.token_path(), dir.path().join("token-link")).unwrap();

        let backend = BackendHandle::spawn(
            Some(dir.path().to_path_buf()),
            None,
            egui::Context::default(),
        );
        backend.send(GuiRequest::ConnectOrStartDaemon);

        let update = recv_until(&backend, Duration::from_secs(3), |u| {
            matches!(u, GuiUpdate::DaemonDisconnected(_))
        });
        match update {
            GuiUpdate::DaemonDisconnected(reason) => {
                assert!(
                    reason.contains("hard-linked") && reason.contains("daemon.token"),
                    "expected hard-linked token error in DaemonDisconnected, got: {reason}"
                );
            }
            other => panic!("expected DaemonDisconnected, got {other:?}"),
        }
    }
}
