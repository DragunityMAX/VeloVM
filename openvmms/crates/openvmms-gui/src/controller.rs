use anyhow::Context;
use openvmms_core::api_client::ApiClient;
use openvmms_core::api_client::ensure_daemon_running;
use openvmms_core::api_client::launch_fastdesk_viewer;
use openvmms_core::api_client::normalize_base_url;
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

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
    StatusMessage(String),
    ErrorMessage(String),
}

pub struct BackendHandle {
    req_tx: mpsc::Sender<GuiRequest>,
    upd_rx: mpsc::Receiver<GuiUpdate>,
}

impl BackendHandle {
    pub fn spawn(
        data_dir: Option<PathBuf>,
        daemon_url: Option<String>,
        repaint_ctx: egui::Context,
    ) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<GuiRequest>();
        let (upd_tx, upd_rx) = mpsc::channel::<GuiUpdate>();

        thread::spawn(move || {
            let mut client: Option<ApiClient> = None;
            let sse_active = Arc::new(AtomicBool::new(false));

            let emit = |msg: GuiUpdate| {
                let _ = upd_tx.send(msg);
                repaint_ctx.request_repaint();
            };

            while let Ok(req) = req_rx.recv() {
                match req {
                    GuiRequest::ConnectOrStartDaemon => {
                        match ensure_daemon_running(data_dir.as_deref(), daemon_url.as_deref()) {
                            Ok(c) => {
                                if sse_active
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    spawn_sse_listener(
                                        c.base_url().to_string(),
                                        c.token().to_string(),
                                        upd_tx.clone(),
                                        repaint_ctx.clone(),
                                        Arc::clone(&sse_active),
                                    );
                                }
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
                        let Some(c) = client.as_mut() else {
                            emit(GuiUpdate::ErrorMessage(
                                "Not connected to openvmmsd".to_string(),
                            ));
                            continue;
                        };
                        if let Err(err) = handle_client_request(c, other, &emit) {
                            emit(GuiUpdate::ErrorMessage(format!("{err:#}")));
                        }
                    }
                }
            }
        });

        Self { req_tx, upd_rx }
    }

    pub fn send(&self, req: GuiRequest) {
        let _ = self.req_tx.send(req);
    }

    pub fn try_recv(&self) -> Option<GuiUpdate> {
        self.upd_rx.try_recv().ok()
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

fn handle_client_request<F>(client: &mut ApiClient, req: GuiRequest, emit: &F) -> anyhow::Result<()>
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
            let info = client.host_info()?;
            let viewer_bin = info.resolved_paths.fastdesk_viewer_bin.context(
                "fastdesk-viewer binary not found; configure it in Host Settings",
            )?;
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

fn spawn_sse_listener(
    base_url: String,
    token: String,
    upd_tx: mpsc::Sender<GuiUpdate>,
    repaint_ctx: egui::Context,
    sse_active: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let http = match reqwest::blocking::Client::builder().timeout(None).build() {
            Ok(c) => c,
            Err(_) => {
                sse_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        let url = format!("{base_url}/api/v1/events?token={token}");
        let mut stopping = false;
        while !stopping {
            match http.get(&url).send() {
                Ok(resp) if resp.status().is_success() => {
                    let reader = std::io::BufReader::new(resp);
                    for line in reader.lines().map_while(Result::ok) {
                        if let Some(json_str) = line.strip_prefix("data: ")
                            && let Ok(event) = serde_json::from_str::<DaemonEvent>(json_str)
                        {
                            match event {
                                DaemonEvent::VmUpdated { vm } => {
                                    let _ = upd_tx.send(GuiUpdate::VmUpdated(vm));
                                    repaint_ctx.request_repaint();
                                }
                                DaemonEvent::VmDeleted { id } => {
                                    let _ = upd_tx.send(GuiUpdate::VmDeleted(id));
                                    repaint_ctx.request_repaint();
                                }
                                DaemonEvent::HostConfigUpdated => {
                                    if let Ok(c) = ApiClient::new(&base_url, &token)
                                        && let Ok((host_info, host_config, switches, vms)) =
                                            fetch_full_snapshot(&c)
                                    {
                                        let _ = upd_tx.send(GuiUpdate::DaemonConnected {
                                            base_url: base_url.clone(),
                                            host_info,
                                            host_config,
                                            switches,
                                            vms,
                                        });
                                        repaint_ctx.request_repaint();
                                    }
                                }
                                DaemonEvent::DaemonStopping => {
                                    let _ = upd_tx.send(GuiUpdate::DaemonDisconnected(
                                        "Daemon stopped".to_string(),
                                    ));
                                    repaint_ctx.request_repaint();
                                    stopping = true;
                                    break;
                                }
                            }
                        }
                    }
                    if !stopping {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                _ => {
                    thread::sleep(Duration::from_secs(2));
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
