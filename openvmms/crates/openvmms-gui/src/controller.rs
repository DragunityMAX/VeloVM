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

#[derive(Default)]
struct SnapshotSyncState {
    epoch: u64,
    synced: Option<(String, String, u64)>,
}

struct SnapshotSyncOutcome {
    daemon_instance_id: Option<Uuid>,
    sent: bool,
    fetched: bool,
}

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
    snapshot_sync_lock: Mutex<()>,
    snapshot_sync: Mutex<SnapshotSyncState>,
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
            snapshot_sync_lock: Mutex::new(()),
            snapshot_sync: Mutex::new(SnapshotSyncState::default()),
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
            snapshot_sync_lock: Mutex::new(()),
            snapshot_sync: Mutex::new(SnapshotSyncState::default()),
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
                self.invalidate_snapshot_sync();
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

    fn invalidate_snapshot_sync(&self) {
        let mut s = self.snapshot_sync.lock().unwrap();
        s.epoch = s.epoch.wrapping_add(1);
        s.synced = None;
    }

    /// Fetches a full snapshot and emits `DaemonConnected` at most once per
    /// `(base_url, token, epoch)` unless `force` is `true` (used by manual `RefreshAll`
    /// while already connected, and by `Resync` / `HostConfigUpdated` SSE events).
    fn sync_snapshot_once(
        &self,
        client: &ApiClient,
        force: bool,
    ) -> anyhow::Result<SnapshotSyncOutcome> {
        let _sync_guard = self.snapshot_sync_lock.lock().unwrap();
        let base_url = client.base_url().to_string();
        let token = client.token().to_string();
        if !force {
            let s = self.snapshot_sync.lock().unwrap();
            if s.synced
                .as_ref()
                .is_some_and(|(u, tok, ep)| *u == base_url && *tok == token && *ep == s.epoch)
                && self.notified_connected.load(Ordering::Acquire)
            {
                *self.client.write().unwrap() = Some(client.clone());
                return Ok(SnapshotSyncOutcome {
                    daemon_instance_id: self.daemon_instance_id.lock().unwrap().1,
                    sent: true,
                    fetched: false,
                });
            }
        }
        let epoch_before = self.snapshot_sync.lock().unwrap().epoch;
        let snapshot_seq = self.next_snapshot_seq();
        let (host_info, host_config, switches, vms) = fetch_full_snapshot(client)?;
        let inst_id = host_info.daemon_instance_id;
        *self.client.write().unwrap() = Some(client.clone());
        {
            let mut s = self.snapshot_sync.lock().unwrap();
            if s.epoch == epoch_before {
                s.synced = Some((base_url.clone(), token, s.epoch));
            }
        }
        let sent = self.emit_checked(GuiUpdate::DaemonConnected {
            base_url,
            host_info,
            host_config,
            switches,
            vms,
            snapshot_seq,
        });
        Ok(SnapshotSyncOutcome {
            daemon_instance_id: inst_id,
            sent,
            fetched: true,
        })
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
                        *self.client.write().unwrap() = Some(c);
                        if !self.ensure_sse() {
                            self.wake_sse();
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
                            if !matches!(other, GuiRequest::RefreshAll) {
                                let _ = self.sync_snapshot_once(&reloaded, false);
                            }
                            if !self.ensure_sse() {
                                self.wake_sse();
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
                    let synced_matches = {
                        let s = self.snapshot_sync.lock().unwrap();
                        s.synced.as_ref().is_some_and(|(u, tok, ep)| {
                            *u == c.base_url() && *tok == c.token() && *ep == s.epoch
                        })
                    };
                    if !synced_matches {
                        self.invalidate_snapshot_sync();
                        if !matches!(other, GuiRequest::RefreshAll) {
                            let _ = self.sync_snapshot_once(&c, false);
                        }
                        self.wake_sse();
                    }
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
                            self.invalidate_snapshot_sync();
                            c = reloaded;
                            set_shared_endpoint(&self.shared_endpoint, &c);
                            *self.client.write().unwrap() = Some(c.clone());
                            if !matches!(other, GuiRequest::RefreshAll) {
                                let _ = self.sync_snapshot_once(&c, false);
                            }
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
                let was_disconnected = !self.notified_connected.load(Ordering::Acquire);
                let _ = self.sync_snapshot_once(client, !was_disconnected)?;
                if was_disconnected {
                    self.wake_sse();
                }
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

const MAX_SSE_LINE_BUFFER_BYTES: usize = 4 * 1024 * 1024;

struct SseLineBuffer {
    buf: Vec<u8>,
    max_bytes: usize,
}

impl SseLineBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_bytes,
        }
    }

    fn decode_line(mut line_bytes: Vec<u8>) -> String {
        if line_bytes.ends_with(b"\n") {
            line_bytes.pop();
        }
        if line_bytes.ends_with(b"\r") {
            line_bytes.pop();
        }
        String::from_utf8_lossy(&line_bytes).into_owned()
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> bool {
        if self
            .buf
            .len()
            .checked_add(chunk.len())
            .is_none_or(|n| n > self.max_bytes)
        {
            self.buf.clear();
            return false;
        }
        self.buf.extend_from_slice(chunk);
        true
    }

    fn take_line(&mut self) -> Option<String> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
        Some(Self::decode_line(line_bytes))
    }

    fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let tail = std::mem::take(&mut self.buf);
        Some(Self::decode_line(tail))
    }
}

struct SseLineReader {
    resp: reqwest::Response,
    line_buf: SseLineBuffer,
}

impl SseLineReader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            line_buf: SseLineBuffer::new(MAX_SSE_LINE_BUFFER_BYTES),
        }
    }

    fn next_line(&mut self, rt: &tokio::runtime::Runtime) -> Option<String> {
        loop {
            if let Some(line) = self.line_buf.take_line() {
                return Some(line);
            }
            match rt.block_on(self.resp.chunk()) {
                Ok(Some(chunk)) => {
                    if !self.line_buf.push_chunk(&chunk) {
                        return None;
                    }
                }
                Ok(None) => return self.line_buf.finish(),
                Err(_) => return None,
            }
        }
    }
}

fn spawn_sse_listener(state: Arc<BackendState>) {
    thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            state.sse_active.store(false, Ordering::SeqCst);
            return;
        };
        let http = match reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(state.sse_read_timeout)
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
            match rt.block_on(async { http.get(&url).bearer_auth(&token).send().await }) {
                Ok(resp) if resp.status().is_success() => {
                    let sync_res = ApiClient::new(&base_url, &token)
                        .and_then(|c| state.sync_snapshot_once(&c, false));
                    let (mut sse_instance_id, mut skip_leading_resync) = match sync_res {
                        Ok(sync) => {
                            if !sync.sent {
                                break;
                            }
                            reported_initial_error = false;
                            reported_token_security_error = false;
                            (sync.daemon_instance_id, sync.fetched)
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
                    let mut sse_reader = SseLineReader::new(resp);
                    let mut sse_send_closed = false;
                    while let Some(line) = sse_reader.next_line(&rt) {
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
                                let reload_res = ApiClient::new(&base_url, &token)
                                    .and_then(|c| state.sync_snapshot_once(&c, true));
                                match reload_res {
                                    Ok(sync) => {
                                        if !sync.sent {
                                            sse_send_closed = true;
                                            break;
                                        }
                                        sse_instance_id = sync.daemon_instance_id;
                                    }
                                    Err(_) => {
                                        // Drop the stream so the outer loop runs the retryable reconnect sync path.
                                        break;
                                    }
                                }
                            }
                            DaemonEvent::HostConfigUpdated => {
                                let reload_res = resolve_updated_client(
                                    &base_url,
                                    &token,
                                    state.data_dir.as_deref(),
                                    state.daemon_url.as_deref(),
                                    &state.shared_endpoint,
                                )
                                .ok_or_else(|| anyhow::anyhow!("failed to resolve daemon client"))
                                .and_then(|c| {
                                    let new_base = c.base_url().to_string();
                                    state
                                        .sync_snapshot_once(&c, true)
                                        .map(|sync| (sync, new_base))
                                });
                                match reload_res {
                                    Ok((sync, new_base)) => {
                                        if !sync.sent {
                                            sse_send_closed = true;
                                            break;
                                        }
                                        sse_instance_id = sync.daemon_instance_id;
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
                    let disconnect_wake_gen = *state.sse_wake.0.lock().unwrap();
                    state.invalidate_snapshot_sync();
                    if switched_endpoint {
                        retry_delay = state.sse_backoff_initial;
                    } else if !state.shutdown.load(Ordering::Acquire) {
                        if saw_daemon_stopping
                            || stream_connected_at.elapsed() >= state.sse_backoff_stable_after
                        {
                            retry_delay = state.sse_backoff_initial;
                        }
                        if saw_daemon_stopping {
                            if sleep_backoff(&state, disconnect_wake_gen, retry_delay) {
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
                        if sleep_backoff(&state, disconnect_wake_gen, retry_delay) {
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
#[path = "controller_tests.rs"]
mod tests;
