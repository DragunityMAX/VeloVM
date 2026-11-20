use anyhow::Context;
use openvmms_core::api_client::ApiClient;
use openvmms_core::api_client::ensure_daemon_running;
use openvmms_core::api_client::launch_fastdesk_viewer;
use openvmms_core::api_client::normalize_base_url;
use openvmms_core::api_client::resolve_local_viewer_binary;
use openvmms_core::models::AttachDiskPayload;
use openvmms_core::models::CreateDiskPayload;
use openvmms_core::models::CreateVmPayload;
use openvmms_core::models::DaemonEvent;
use openvmms_core::models::DiskFormat;
use openvmms_core::models::DiskInfo;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::IpProto;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::NicBackendConfig;
use openvmms_core::models::PortForward;
use openvmms_core::models::ShutdownVmPayload;
use openvmms_core::models::UpdateVmPayload;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmPreset;
use openvmms_core::models::VmStatus;
use openvmms_core::models::VmSummary;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

type SharedEndpoint = Arc<RwLock<Option<(String, String)>>>;

#[derive(Debug, Clone)]
pub enum GuiRequest {
    ConnectOrStartDaemon,
    RefreshAll,
    FetchVmDetails { id: Uuid },
    FetchVmLogs { id: Uuid },
    CreateVm(CreateVmPayload),
    UpdateVm { id: Uuid, payload: UpdateVmPayload },
    DeleteVm { id: Uuid, delete_disks: bool },
    StartVm { id: Uuid },
    StopVm { id: Uuid },
    ShutdownVm { id: Uuid, acpi: bool, force: bool },
    PauseVm { id: Uuid },
    ResumeVm { id: Uuid },
    ResetVm { id: Uuid },
    ConnectViewer { id: Uuid, no_shm: bool },
    AttachDisk { id: Uuid, payload: AttachDiskPayload },
    DetachDisk { id: Uuid, lun: u32 },
    MountIso { id: Uuid, payload: MountIsoPayload },
    EjectIso { id: Uuid, lun: Option<u32> },
    AddPortForward { id: Uuid, nic_id: Uuid, port: PortForward },
    RemovePortForward { id: Uuid, nic_id: Uuid, port: PortForward },
    CreateDisk(CreateDiskPayload),
    InspectDisk(PathBuf),
    SaveHostConfig(HostConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    NewVmWizard,
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
    },
    DaemonDisconnected(String),
    VmUpdated(VmSummary),
    VmDeleted(Uuid),
    VmLogs { id: Uuid, content: String },
    DiskInspected(DiskInfo),
    DiskCreated(DiskInfo),
    HostConfigUpdated {
        host_info: HostInfo,
        host_config: HostConfig,
    },
    DialogFinished {
        kind: DialogKind,
        error: Option<String>,
    },
    StatusMessage(String),
    ErrorMessage(String),
}

pub struct BackendHandle {
    req_tx: mpsc::Sender<GuiRequest>,
    upd_rx: mpsc::Receiver<GuiUpdate>,
    shutdown: Arc<AtomicBool>,
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn request_dialog_kind(req: &GuiRequest) -> Option<DialogKind> {
    match req {
        GuiRequest::CreateVm(_) => Some(DialogKind::NewVmWizard),
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
        let (req_tx, req_rx) = mpsc::channel::<GuiRequest>();
        let (upd_tx, upd_rx) = mpsc::channel::<GuiUpdate>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_worker = Arc::clone(&shutdown);

        thread::spawn(move || {
            let mut client: Option<ApiClient> = None;
            let sse_active = Arc::new(AtomicBool::new(false));
            let shared_endpoint: SharedEndpoint = Arc::new(RwLock::new(None));

            let emit = |msg: GuiUpdate| {
                let _ = upd_tx.send(msg);
                repaint_ctx.request_repaint();
            };

            let ensure_sse = |shared: &SharedEndpoint| {
                if sse_active
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    spawn_sse_listener(
                        data_dir.clone(),
                        daemon_url.clone(),
                        Arc::clone(shared),
                        upd_tx.clone(),
                        repaint_ctx.clone(),
                        Arc::clone(&sse_active),
                        Arc::clone(&shutdown_worker),
                    );
                }
            };

            while let Ok(req) = req_rx.recv() {
                if shutdown_worker.load(Ordering::Acquire) {
                    break;
                }
                match req {
                    GuiRequest::ConnectOrStartDaemon => {
                        match ensure_daemon_running(data_dir.as_deref(), daemon_url.as_deref()) {
                            Ok(c) => {
                                set_shared_endpoint(&shared_endpoint, &c);
                                ensure_sse(&shared_endpoint);
                                match fetch_full_snapshot(&c) {
                                    Ok((host_info, host_config, switches, vms)) => {
                                        let base_url = c.base_url().to_string();
                                        client = Some(c);
                                        emit(GuiUpdate::DaemonConnected {
                                            base_url,
                                            host_info,
                                            host_config,
                                            switches,
                                            vms,
                                        });
                                    }
                                    Err(err) => {
                                        emit(GuiUpdate::DaemonDisconnected(format!("{err:#}")));
                                    }
                                }
                            }
                            Err(err) => {
                                emit(GuiUpdate::DaemonDisconnected(format!("{err:#}")));
                            }
                        }
                    }
                    other => {
                        let dialog_kind = request_dialog_kind(&other);
                        if client.is_none()
                            && let Some(reloaded) = reload_client_from_data_dir(
                                data_dir.as_deref(),
                                daemon_url.as_deref(),
                                &shared_endpoint,
                            )
                        {
                            set_shared_endpoint(&shared_endpoint, &reloaded);
                            ensure_sse(&shared_endpoint);
                            if !matches!(other, GuiRequest::RefreshAll)
                                && let Ok((host_info, host_config, switches, vms)) =
                                    fetch_full_snapshot(&reloaded)
                            {
                                emit(GuiUpdate::DaemonConnected {
                                    base_url: reloaded.base_url().to_string(),
                                    host_info,
                                    host_config,
                                    switches,
                                    vms,
                                });
                            }
                            client = Some(reloaded);
                        }

                        let Some(c) = client.as_mut() else {
                            let err_msg = "Not connected to openvmmsd".to_string();
                            emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                            if let Some(kind) = dialog_kind {
                                emit(GuiUpdate::DialogFinished {
                                    kind,
                                    error: Some(err_msg),
                                });
                            }
                            continue;
                        };
                        ensure_sse(&shared_endpoint);
                        sync_client_before_request(
                            c,
                            data_dir.as_deref(),
                            daemon_url.as_deref(),
                            &shared_endpoint,
                        );
                        match handle_client_request(
                            c,
                            data_dir.as_deref(),
                            other.clone(),
                            &shared_endpoint,
                            &emit,
                        ) {
                            Ok(()) => {
                                if let Some(kind) = dialog_kind {
                                    emit(GuiUpdate::DialogFinished { kind, error: None });
                                }
                            }
                            Err(err) => {
                                let reloaded = reload_client_from_data_dir(
                                    data_dir.as_deref(),
                                    daemon_url.as_deref(),
                                    &shared_endpoint,
                                );
                                if let Some(reloaded) = reloaded
                                    && (reloaded.base_url() != c.base_url()
                                        || reloaded.token() != c.token())
                                {
                                    *c = reloaded;
                                    ensure_sse(&shared_endpoint);
                                    match handle_client_request(
                                        c,
                                        data_dir.as_deref(),
                                        other,
                                        &shared_endpoint,
                                        &emit,
                                    ) {
                                        Ok(()) => {
                                            if let Some(kind) = dialog_kind {
                                                emit(GuiUpdate::DialogFinished {
                                                    kind,
                                                    error: None,
                                                });
                                            }
                                        }
                                        Err(retry_err) => {
                                            let err_msg = format!("{retry_err:#}");
                                            emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                                            if let Some(kind) = dialog_kind {
                                                emit(GuiUpdate::DialogFinished {
                                                    kind,
                                                    error: Some(err_msg),
                                                });
                                            }
                                        }
                                    }
                                } else if c.health().is_err() {
                                    client = None;
                                    let err_msg = format!("Lost connection to openvmmsd: {err:#}");
                                    emit(GuiUpdate::DaemonDisconnected(err_msg.clone()));
                                    if let Some(kind) = dialog_kind {
                                        emit(GuiUpdate::DialogFinished {
                                            kind,
                                            error: Some(err_msg),
                                        });
                                    }
                                } else {
                                    let err_msg = format!("{err:#}");
                                    emit(GuiUpdate::ErrorMessage(err_msg.clone()));
                                    if let Some(kind) = dialog_kind {
                                        emit(GuiUpdate::DialogFinished {
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
        });

        Self {
            req_tx,
            upd_rx,
            shutdown,
        }
    }

    pub fn send(&self, req: GuiRequest) {
        let _ = self.req_tx.send(req);
    }

    pub fn try_recv(&self) -> Option<GuiUpdate> {
        self.upd_rx.try_recv().ok()
    }
}

fn set_shared_endpoint(shared_endpoint: &SharedEndpoint, client: &ApiClient) {
    *shared_endpoint.write().unwrap() =
        Some((client.base_url().to_string(), client.token().to_string()));
}

fn reload_client_from_data_dir(
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) -> Option<ApiClient> {
    let current = shared_endpoint.read().unwrap().clone();
    if let Ok(c) = ApiClient::from_data_dir(data_dir, None) {
        let token_matches = current
            .as_ref()
            .is_none_or(|(_, cur_token)| c.token() == cur_token);
        if (daemon_url.is_none() || token_matches) && c.health().is_ok() {
            set_shared_endpoint(shared_endpoint, &c);
            return Some(c);
        }
    }
    if let Some((base_url, token)) = current
        && let Ok(c) = ApiClient::new(base_url, token)
        && c.health().is_ok()
    {
        return Some(c);
    }
    None
}

fn sync_client_before_request(
    client: &mut ApiClient,
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
) {
    if let Some((shared_url, shared_token)) = shared_endpoint.read().unwrap().clone()
        && (shared_url != client.base_url() || shared_token != client.token())
        && let Ok(updated) = ApiClient::new(shared_url, shared_token)
        && updated.health().is_ok()
    {
        *client = updated;
        return;
    }

    if let Ok(disk_client) = ApiClient::from_data_dir(data_dir, None) {
        let token_matches = disk_client.token() == client.token();
        let changed =
            disk_client.base_url() != client.base_url() || disk_client.token() != client.token();
        if changed && (daemon_url.is_none() || token_matches) && disk_client.health().is_ok() {
            set_shared_endpoint(shared_endpoint, &disk_client);
            *client = disk_client;
        }
    }
}

fn fetch_full_snapshot(
    client: &ApiClient,
) -> anyhow::Result<(HostInfo, HostConfig, Vec<VirtualSwitchInfo>, Vec<VmSummary>)> {
    let host_info = client.host_info()?;
    let host_config = client.host_config()?;
    let switches = client.list_switches()?;
    let vms = client.list_vms()?;
    Ok((host_info, host_config, switches, vms))
}

fn handle_client_request<F>(
    client: &mut ApiClient,
    data_dir: Option<&Path>,
    req: GuiRequest,
    shared_endpoint: &SharedEndpoint,
    emit: &F,
) -> anyhow::Result<()>
where
    F: Fn(GuiUpdate),
{
    match req {
        GuiRequest::ConnectOrStartDaemon => Ok(()),
        GuiRequest::RefreshAll => {
            let (host_info, host_config, switches, vms) = fetch_full_snapshot(client)?;
            emit(GuiUpdate::DaemonConnected {
                base_url: client.base_url().to_string(),
                host_info,
                host_config,
                switches,
                vms,
            });
            Ok(())
        }
        GuiRequest::FetchVmDetails { id } => {
            let vm = client.get_vm(&id.to_string())?;
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::FetchVmLogs { id } => {
            let content = client.vm_logs(&id.to_string(), 250)?;
            emit(GuiUpdate::VmLogs { id, content });
            Ok(())
        }
        GuiRequest::CreateVm(payload) => {
            let vm = client.create_vm(&payload)?;
            emit(GuiUpdate::StatusMessage(format!(
                "Created VM '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::UpdateVm { id, payload } => {
            let vm = client.update_vm(&id.to_string(), &payload)?;
            emit(GuiUpdate::StatusMessage(format!(
                "Updated settings for '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::DeleteVm { id, delete_disks } => {
            client.delete_vm(&id.to_string(), delete_disks)?;
            emit(GuiUpdate::StatusMessage("Deleted virtual machine".to_string()));
            emit(GuiUpdate::VmDeleted(id));
            Ok(())
        }
        GuiRequest::StartVm { id } => {
            let vm = client.start_vm(&id.to_string())?;
            emit(GuiUpdate::StatusMessage(format!(
                "Started '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::StopVm { id } => {
            let vm = client.stop_vm(&id.to_string())?;
            emit(GuiUpdate::StatusMessage(format!(
                "Turned off '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::ShutdownVm { id, acpi, force } => {
            let vm = client.shutdown_vm(&id.to_string(), &ShutdownVmPayload { force, acpi })?;
            emit(GuiUpdate::StatusMessage(format!(
                "Sent shutdown signal to '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::PauseVm { id } => {
            let vm = client.pause_vm(&id.to_string())?;
            emit(GuiUpdate::StatusMessage(format!(
                "Paused '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::ResumeVm { id } => {
            let vm = client.resume_vm(&id.to_string())?;
            emit(GuiUpdate::StatusMessage(format!(
                "Resumed '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::ResetVm { id } => {
            let vm = client.reset_vm(&id.to_string())?;
            emit(GuiUpdate::StatusMessage(format!(
                "Reset '{}'",
                vm.config.name
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::ConnectViewer { id, no_shm } => {
            let vm = client.get_vm(&id.to_string())?;
            let endpoint = vm
                .runtime
                .fastdesk_endpoint
                .as_deref()
                .context("VM does not have an active FastDesk endpoint")?;
            let viewer_bin = resolve_local_viewer_binary(data_dir)?;
            let _child = launch_fastdesk_viewer(&viewer_bin, endpoint, &vm.config.name, no_shm)?;
            emit(GuiUpdate::StatusMessage(format!(
                "Launched FastDesk viewer for '{}' ({endpoint})",
                vm.config.name
            )));
            Ok(())
        }
        GuiRequest::AttachDisk { id, payload } => {
            let vm = client.attach_disk(&id.to_string(), &payload)?;
            emit(GuiUpdate::StatusMessage("Attached disk".to_string()));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::DetachDisk { id, lun } => {
            let vm = client.detach_disk(&id.to_string(), lun)?;
            emit(GuiUpdate::StatusMessage(format!(
                "Detached SCSI LUN {lun}"
            )));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::MountIso { id, payload } => {
            let vm = client.mount_iso(&id.to_string(), &payload)?;
            emit(GuiUpdate::StatusMessage("Mounted ISO image".to_string()));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::EjectIso { id, lun } => {
            let vm = client.eject_iso(&id.to_string(), lun)?;
            emit(GuiUpdate::StatusMessage("Ejected ISO image".to_string()));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::AddPortForward { id, nic_id, port } => {
            let vm = client.add_port_forward(&id.to_string(), nic_id, port)?;
            emit(GuiUpdate::StatusMessage("Added port forward".to_string()));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::RemovePortForward { id, nic_id, port } => {
            let vm = client.remove_port_forward(&id.to_string(), nic_id, port)?;
            emit(GuiUpdate::StatusMessage("Removed port forward".to_string()));
            emit(GuiUpdate::VmUpdated(vm));
            Ok(())
        }
        GuiRequest::CreateDisk(payload) => {
            let info = client.create_disk(&payload)?;
            emit(GuiUpdate::StatusMessage(format!(
                "Created disk {}",
                info.path.display()
            )));
            emit(GuiUpdate::DiskCreated(info));
            Ok(())
        }
        GuiRequest::InspectDisk(path) => {
            let info = client.inspect_disk(&path)?;
            emit(GuiUpdate::DiskInspected(info));
            Ok(())
        }
        GuiRequest::SaveHostConfig(cfg) => {
            let updated = client.update_host_config(&cfg)?;
            let next_base = normalize_base_url(&updated.listen_addr);
            if next_base != client.base_url() {
                *client = ApiClient::new(next_base, client.token())?;
                set_shared_endpoint(shared_endpoint, client);
            }
            let host_info = client.host_info()?;
            emit(GuiUpdate::StatusMessage("Saved host settings".to_string()));
            emit(GuiUpdate::HostConfigUpdated {
                host_info,
                host_config: updated,
            });
            Ok(())
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
    if let Some(c) = reload_client_from_data_dir(data_dir, daemon_url, shared_endpoint) {
        if c.base_url() != current_base_url {
            return Some(c);
        }
        if let Ok(hc) = c.host_config() {
            let next_base = normalize_base_url(&hc.listen_addr);
            if next_base != current_base_url
                && let Ok(next_client) = ApiClient::new(next_base, c.token())
                && next_client.health().is_ok()
            {
                set_shared_endpoint(shared_endpoint, &next_client);
                return Some(next_client);
            }
        }
        return Some(c);
    }
    ApiClient::new(current_base_url, current_token).ok()
}

fn maybe_switch_endpoint_on_disconnect(
    current_base_url: &str,
    data_dir: Option<&Path>,
    daemon_url: Option<&str>,
    shared_endpoint: &SharedEndpoint,
    upd_tx: &mpsc::Sender<GuiUpdate>,
    repaint_ctx: &egui::Context,
) -> bool {
    if let Some(c) = reload_client_from_data_dir(data_dir, daemon_url, shared_endpoint)
        && c.base_url() != current_base_url
    {
        if let Ok((host_info, host_config, switches, vms)) = fetch_full_snapshot(&c) {
            let _ = upd_tx.send(GuiUpdate::DaemonConnected {
                base_url: c.base_url().to_string(),
                host_info,
                host_config,
                switches,
                vms,
            });
            repaint_ctx.request_repaint();
        }
        return true;
    }
    false
}

fn spawn_sse_listener(
    data_dir: Option<PathBuf>,
    daemon_url: Option<String>,
    shared_endpoint: SharedEndpoint,
    upd_tx: mpsc::Sender<GuiUpdate>,
    repaint_ctx: egui::Context,
    sse_active: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let http = match reqwest::blocking::Client::builder().timeout(None).build() {
            Ok(c) => c,
            Err(_) => {
                sse_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        let mut was_connected = true;
        while !shutdown.load(Ordering::Acquire) {
            let endpoint = shared_endpoint.read().unwrap().clone().or_else(|| {
                reload_client_from_data_dir(
                    data_dir.as_deref(),
                    daemon_url.as_deref(),
                    &shared_endpoint,
                )
                .map(|c| (c.base_url().to_string(), c.token().to_string()))
            });
            let Some((mut base_url, mut token)) = endpoint else {
                thread::sleep(Duration::from_millis(500));
                continue;
            };
            // Verify health authentication challenge before transmitting token on the SSE request.
            if !ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok()) {
                if let Some(reloaded) = reload_client_from_data_dir(
                    data_dir.as_deref(),
                    daemon_url.as_deref(),
                    &shared_endpoint,
                ) {
                    base_url = reloaded.base_url().to_string();
                    token = reloaded.token().to_string();
                } else {
                    if was_connected {
                        was_connected = false;
                        if upd_tx
                            .send(GuiUpdate::DaemonDisconnected(
                                "Lost connection or failed daemon verification".to_string(),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        repaint_ctx.request_repaint();
                    }
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
            let url = format!("{base_url}/api/v1/events");
            let mut switched_endpoint = false;
            let mut saw_daemon_stopping = false;
            match http.get(&url).bearer_auth(&token).send() {
                Ok(resp) if resp.status().is_success() => {
                    if !was_connected {
                        was_connected = true;
                        if let Ok(c) = ApiClient::new(&base_url, &token)
                            && let Ok((host_info, host_config, switches, vms)) =
                                fetch_full_snapshot(&c)
                        {
                            if upd_tx
                                .send(GuiUpdate::DaemonConnected {
                                    base_url: base_url.clone(),
                                    host_info,
                                    host_config,
                                    switches,
                                    vms,
                                })
                                .is_err()
                            {
                                break;
                            }
                            repaint_ctx.request_repaint();
                        }
                    }
                    let reader = std::io::BufReader::new(resp);
                    for line in reader.lines().map_while(Result::ok) {
                        if shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        if let Some(json_str) = line.strip_prefix("data: ")
                            && let Ok(event) = serde_json::from_str::<DaemonEvent>(json_str)
                        {
                            match event {
                                DaemonEvent::VmUpdated { vm } => {
                                    if upd_tx.send(GuiUpdate::VmUpdated(vm)).is_err() {
                                        break;
                                    }
                                    repaint_ctx.request_repaint();
                                }
                                DaemonEvent::VmDeleted { id } => {
                                    if upd_tx.send(GuiUpdate::VmDeleted(id)).is_err() {
                                        break;
                                    }
                                    repaint_ctx.request_repaint();
                                }
                                DaemonEvent::HostConfigUpdated => {
                                    if let Some(c) = resolve_updated_client(
                                        &base_url,
                                        &token,
                                        data_dir.as_deref(),
                                        daemon_url.as_deref(),
                                        &shared_endpoint,
                                    ) && let Ok((host_info, host_config, switches, vms)) =
                                        fetch_full_snapshot(&c)
                                    {
                                        let new_base = c.base_url().to_string();
                                        if upd_tx
                                            .send(GuiUpdate::DaemonConnected {
                                                base_url: new_base.clone(),
                                                host_info,
                                                host_config,
                                                switches,
                                                vms,
                                            })
                                            .is_err()
                                        {
                                            break;
                                        }
                                        repaint_ctx.request_repaint();
                                        if new_base != base_url {
                                            switched_endpoint = true;
                                            break;
                                        }
                                    }
                                }
                                DaemonEvent::DaemonStopping => {
                                    was_connected = false;
                                    saw_daemon_stopping = true;
                                    if upd_tx
                                        .send(GuiUpdate::DaemonDisconnected(
                                            "Daemon stopped".to_string(),
                                        ))
                                        .is_err()
                                    {
                                        break;
                                    }
                                    repaint_ctx.request_repaint();
                                    break;
                                }
                            }
                        }
                    }
                    if !shutdown.load(Ordering::Acquire) && !switched_endpoint {
                        if saw_daemon_stopping {
                            thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        if maybe_switch_endpoint_on_disconnect(
                            &base_url,
                            data_dir.as_deref(),
                            daemon_url.as_deref(),
                            &shared_endpoint,
                            &upd_tx,
                            &repaint_ctx,
                        ) {
                            was_connected = true;
                            continue;
                        }
                        let healthy =
                            ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok());
                        if !healthy && was_connected {
                            was_connected = false;
                            if upd_tx
                                .send(GuiUpdate::DaemonDisconnected(
                                    "Lost connection to openvmmsd".to_string(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                            repaint_ctx.request_repaint();
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                }
                _ => {
                    if maybe_switch_endpoint_on_disconnect(
                        &base_url,
                        data_dir.as_deref(),
                        daemon_url.as_deref(),
                        &shared_endpoint,
                        &upd_tx,
                        &repaint_ctx,
                    ) {
                        was_connected = true;
                        continue;
                    }
                    let healthy =
                        ApiClient::new(&base_url, &token).is_ok_and(|c| c.health().is_ok());
                    if !healthy && was_connected {
                        was_connected = false;
                        if upd_tx
                            .send(GuiUpdate::DaemonDisconnected(
                                "Lost connection to openvmmsd".to_string(),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        repaint_ctx.request_repaint();
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
        sse_active.store(false, Ordering::SeqCst);
    });
}

/// Runs an automated end-to-end verification of the GUI controller flows against
/// a real ephemeral `openvmmsd` instance using temporary disk and ISO files.
pub fn run_self_test() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let rt = tokio::runtime::Runtime::new()?;
    let daemon = rt.block_on(openvmmsd::TestDaemonHandle::spawn(dir.path().to_path_buf()))?;
    let client = ApiClient::new(&daemon.base_url, &daemon.token)?;

    let (host_info, _host_cfg, switches, initial_vms) = fetch_full_snapshot(&client)?;
    anyhow::ensure!(initial_vms.is_empty());
    anyhow::ensure!(!switches.is_empty());
    println!(
        "OpenVMMS GUI self-test: connected to {} (switches={}, cpu_count={})",
        daemon.base_url,
        switches.len(),
        host_info.cpu_count
    );

    let disk_path = dir.path().join("selftest-disk.img");
    let disk_info = client.create_disk(&CreateDiskPayload {
        path: disk_path.clone(),
        size_bytes: 32 * 1024 * 1024,
        format: DiskFormat::Raw,
        fixed: false,
    })?;
    anyhow::ensure!(disk_info.virtual_size_bytes == 32 * 1024 * 1024);

    let iso_path = dir.path().join("selftest.iso");
    std::fs::write(&iso_path, b"ISO_SELF_TEST")?;

    let created = client.create_vm(&CreateVmPayload {
        name: "GUI-SelfTest-VM".into(),
        description: "Created by openvmms-gui --self-test".into(),
        preset: VmPreset::Windows11,
        vcpus: Some(4),
        memory_mb: Some(8192),
        nested_virt: None,
        uefi_firmware: None,
        secure_boot_enabled: None,
        secure_boot_template: None,
        tpm_enabled: None,
        fastdesk: None,
        disks: vec![],
        create_disk: Some(CreateDiskPayload {
            path: dir.path().join("os-self-test.img"),
            size_bytes: 16 * 1024 * 1024,
            format: DiskFormat::Raw,
            fixed: false,
        }),
        iso_path: Some(iso_path.clone()),
        nics: None,
    })?;
    anyhow::ensure!(created.runtime.status == VmStatus::Off);
    anyhow::ensure!(created.config.disks.len() == 2);

    let edited = client.update_vm(
        &created.config.id.to_string(),
        &UpdateVmPayload {
            vcpus: Some(6),
            memory_mb: Some(12288),
            ..Default::default()
        },
    )?;
    anyhow::ensure!(edited.config.vcpus == 6);
    anyhow::ensure!(edited.config.memory_mb == 12288);

    let ejected = client.eject_iso(&created.config.id.to_string(), None)?;
    anyhow::ensure!(ejected.config.disks.len() == 1);

    let mounted = client.mount_iso(
        &created.config.id.to_string(),
        &MountIsoPayload {
            path: iso_path,
            lun: None,
        },
    )?;
    anyhow::ensure!(mounted.config.disks.len() == 2);

    let nic_id = mounted.config.nics[0].id;
    let pf = PortForward {
        protocol: IpProto::Tcp,
        host_addr: "127.0.0.1".into(),
        host_port: 3389,
        guest_port: 3389,
    };
    let with_pf = client.add_port_forward(&created.config.id.to_string(), nic_id, pf.clone())?;
    match &with_pf.config.nics[0].backend {
        NicBackendConfig::Consomme { port_forwards, .. } => {
            anyhow::ensure!(port_forwards.len() == 1);
        }
        other => anyhow::bail!("expected Consomme backend, got {other:?}"),
    }
    let without_pf = client.remove_port_forward(&created.config.id.to_string(), nic_id, pf)?;
    match &without_pf.config.nics[0].backend {
        NicBackendConfig::Consomme { port_forwards, .. } => {
            anyhow::ensure!(port_forwards.is_empty());
        }
        other => anyhow::bail!("expected Consomme backend, got {other:?}"),
    }

    client.delete_vm(&created.config.id.to_string(), true)?;
    anyhow::ensure!(client.list_vms()?.is_empty());

    rt.block_on(daemon.shutdown());
    println!("SELF-TEST OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv_until<F>(handle: &BackendHandle, timeout: Duration, mut pred: F) -> GuiUpdate
    where
        F: FnMut(&GuiUpdate) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            while let Some(upd) = handle.try_recv() {
                if let GuiUpdate::ErrorMessage(err) = &upd {
                    panic!("unexpected GuiUpdate::ErrorMessage: {err}");
                }
                if pred(&upd) {
                    return upd;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for expected GuiUpdate"
            );
            thread::sleep(Duration::from_millis(20));
        }
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
        let new_base_url = normalize_base_url(&updated_cfg.listen_addr);
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
            matches!(u, GuiUpdate::VmUpdated(vm) if vm.config.id == created.config.id)
        }) {
            GuiUpdate::VmUpdated(vm) => {
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
                GuiUpdate::VmUpdated(vm)
                    if vm.config.id == created.config.id
                        && vm.config.description == "updated via GUI after port change"
            )
        }) {
            GuiUpdate::VmUpdated(vm) => {
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
        backend.send(GuiRequest::CreateVm(CreateVmPayload {
            name: "Gui-After-Restart".into(),
            preset: VmPreset::Custom,
            vcpus: Some(2),
            memory_mb: Some(512),
            ..Default::default()
        }));
        match recv_until(&backend, Duration::from_secs(5), |u| {
            matches!(
                u,
                GuiUpdate::DialogFinished {
                    kind: DialogKind::NewVmWizard,
                    error: None
                }
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
            matches!(u, GuiUpdate::VmUpdated(vm) if vm.config.id == ext_vm.config.id)
        }) {
            GuiUpdate::VmUpdated(vm) => assert_eq!(vm.config.name, "External-After-Restart"),
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

        backend.send(GuiRequest::CreateVm(CreateVmPayload {
            name: "Invalid-Vcpu-VM".into(),
            preset: VmPreset::Custom,
            vcpus: Some(0),
            memory_mb: Some(512),
            ..Default::default()
        }));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_dialog_err = false;
        while std::time::Instant::now() < deadline {
            if let Some(upd) = backend.try_recv()
                && let GuiUpdate::DialogFinished {
                    kind: DialogKind::NewVmWizard,
                    error: Some(err),
                } = upd
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
}
