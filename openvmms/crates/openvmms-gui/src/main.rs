mod controller;
mod dialogs;

use clap::Parser;
use controller::BackendHandle;
use controller::GuiRequest;
use controller::GuiUpdate;
use dialogs::DeleteVmDialog;
use dialogs::DiskManagerDialog;
use dialogs::FilePickerModal;
use dialogs::FilePickerTarget;
use dialogs::HostSettingsDialog;
use dialogs::NewVmWizard;
use dialogs::VmSettingsDialog;
use dialogs::render_delete_vm_dialog;
use dialogs::render_disk_manager_dialog;
use dialogs::render_file_picker;
use dialogs::render_host_settings_dialog;
use dialogs::render_new_vm_wizard;
use dialogs::render_vm_settings_dialog;
use egui::Color32;
use egui::RichText;
use openvmms_core::models::AttachDiskPayload;
use openvmms_core::models::DiskKind;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::IpProto;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::NicBackendConfig;
use openvmms_core::models::PortForward;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmStatus;
use openvmms_core::models::VmSummary;
use openvmms_core::models::default_data_dir;
use openvmms_core::models::format_bytes;
use std::path::PathBuf;
use std::time::Instant;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "openvmms-gui", about = "Hyper-V-style desktop manager for OpenVMM and FastDesk")]
struct Args {
    /// Directory for VM configs, logs, and host configuration
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Override daemon HTTP endpoint (e.g. http://127.0.0.1:47620)
    #[arg(long)]
    daemon_url: Option<String>,

    /// Run headless end-to-end verification of the GUI controller against an ephemeral daemon
    #[arg(long)]
    self_test: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavFilter {
    All,
    Running,
    Paused,
    Off,
    Switches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Summary,
    Storage,
    Networking,
    ConsoleLogs,
}

struct OpenVmmsApp {
    data_dir: PathBuf,
    backend: BackendHandle,
    connected: bool,
    connection_status: String,
    host_info: Option<HostInfo>,
    host_config: HostConfig,
    switches: Vec<VirtualSwitchInfo>,
    vms: Vec<VmSummary>,
    selected_vm: Option<Uuid>,
    nav_filter: NavFilter,
    detail_tab: DetailTab,
    logs_cache: Option<(Uuid, String)>,
    last_log_fetch: Instant,
    status_line: String,
    error_banner: Option<String>,

    // Inline inputs in bottom inspector tabs
    attach_disk_input: String,
    mount_iso_input: String,
    pf_proto: IpProto,
    pf_host_addr: String,
    pf_host_port: u16,
    pf_guest_port: u16,

    // Modals & wizards
    new_vm_wizard: Option<NewVmWizard>,
    vm_settings_dlg: Option<VmSettingsDialog>,
    disk_mgr_dlg: Option<DiskManagerDialog>,
    host_settings_dlg: Option<HostSettingsDialog>,
    delete_vm_dlg: Option<DeleteVmDialog>,
    file_picker: Option<FilePickerModal>,
}

impl OpenVmmsApp {
    fn new(cc: &eframe::CreationContext<'_>, data_dir: PathBuf, daemon_url: Option<String>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let backend = BackendHandle::spawn(Some(data_dir.clone()), daemon_url, cc.egui_ctx.clone());
        backend.send(GuiRequest::ConnectOrStartDaemon);

        Self {
            data_dir,
            backend,
            connected: false,
            connection_status: "Connecting to openvmmsd…".to_string(),
            host_info: None,
            host_config: HostConfig::default(),
            switches: Vec::new(),
            vms: Vec::new(),
            selected_vm: None,
            nav_filter: NavFilter::All,
            detail_tab: DetailTab::Summary,
            logs_cache: None,
            last_log_fetch: Instant::now(),
            status_line: "Starting OpenVMMS…".to_string(),
            error_banner: None,
            attach_disk_input: String::new(),
            mount_iso_input: String::new(),
            pf_proto: IpProto::Tcp,
            pf_host_addr: "127.0.0.1".to_string(),
            pf_host_port: 3389,
            pf_guest_port: 3389,
            new_vm_wizard: None,
            vm_settings_dlg: None,
            disk_mgr_dlg: None,
            host_settings_dlg: None,
            delete_vm_dlg: None,
            file_picker: None,
        }
    }

    fn process_updates(&mut self) {
        while let Some(upd) = self.backend.try_recv() {
            match upd {
                GuiUpdate::DaemonConnected {
                    base_url,
                    host_info,
                    host_config,
                    switches,
                    vms,
                } => {
                    self.connected = true;
                    self.connection_status = format!("Connected ({base_url})");
                    self.host_info = Some(host_info);
                    self.host_config = host_config;
                    self.switches = switches;
                    self.vms = vms;
                    self.error_banner = None;
                    self.status_line = format!("{} virtual machine(s) loaded", self.vms.len());
                    if self.selected_vm.is_none() {
                        self.selected_vm = self.vms.first().map(|v| v.config.id);
                    }
                }
                GuiUpdate::DaemonDisconnected(reason) => {
                    self.connected = false;
                    self.connection_status = "Disconnected".to_string();
                    self.error_banner = Some(reason);
                }
                GuiUpdate::VmUpdated(vm) => {
                    let id = vm.config.id;
                    if let Some(slot) = self.vms.iter_mut().find(|v| v.config.id == id) {
                        *slot = vm;
                    } else {
                        self.vms.push(vm);
                        self.vms.sort_by(|a, b| {
                            a.config
                                .name
                                .to_lowercase()
                                .cmp(&b.config.name.to_lowercase())
                        });
                        self.selected_vm = Some(id);
                    }
                }
                GuiUpdate::VmDeleted(id) => {
                    self.vms.retain(|v| v.config.id != id);
                    if self.selected_vm == Some(id) {
                        self.selected_vm = self.vms.first().map(|v| v.config.id);
                    }
                }
                GuiUpdate::VmLogs { id, content } => {
                    self.logs_cache = Some((id, content));
                }
                GuiUpdate::DiskCreated(info) | GuiUpdate::DiskInspected(info) => {
                    if let Some(dlg) = self.disk_mgr_dlg.as_mut() {
                        dlg.last_info = Some(info);
                    }
                }
                GuiUpdate::HostConfigUpdated {
                    host_info,
                    host_config,
                } => {
                    let base_url =
                        openvmms_core::api_client::normalize_base_url(&host_config.listen_addr);
                    self.connected = true;
                    self.connection_status = format!("Connected ({base_url})");
                    self.host_info = Some(host_info);
                    self.host_config = host_config;
                }
                GuiUpdate::StatusMessage(msg) => {
                    self.status_line = msg;
                    self.error_banner = None;
                }
                GuiUpdate::ErrorMessage(err) => {
                    self.error_banner = Some(err);
                }
            }
        }
    }

    fn selected_vm_summary(&self) -> Option<&VmSummary> {
        let id = self.selected_vm?;
        self.vms.iter().find(|v| v.config.id == id)
    }

    fn filtered_vms(&self) -> Vec<&VmSummary> {
        self.vms
            .iter()
            .filter(|v| match self.nav_filter {
                NavFilter::All | NavFilter::Switches => true,
                NavFilter::Running => v.runtime.status == VmStatus::Running,
                NavFilter::Paused => v.runtime.status == VmStatus::Paused,
                NavFilter::Off => matches!(v.runtime.status, VmStatus::Off | VmStatus::Error),
            })
            .collect()
    }

    fn apply_picked_file(&mut self, target: FilePickerTarget, path: PathBuf) {
        let path_str = path.to_string_lossy().into_owned();
        match target {
            FilePickerTarget::WizardExistingDisk => {
                if let Some(w) = self.new_vm_wizard.as_mut() {
                    w.existing_disk_path = path_str;
                }
            }
            FilePickerTarget::WizardNewDiskPath => {
                if let Some(w) = self.new_vm_wizard.as_mut() {
                    w.new_disk_path = path_str;
                }
            }
            FilePickerTarget::WizardIso => {
                if let Some(w) = self.new_vm_wizard.as_mut() {
                    w.iso_path = path_str;
                }
            }
            FilePickerTarget::AttachDiskPath => self.attach_disk_input = path_str,
            FilePickerTarget::MountIsoPath => self.mount_iso_input = path_str,
            FilePickerTarget::DiskMgrCreatePath => {
                if let Some(d) = self.disk_mgr_dlg.as_mut() {
                    d.create_path = path_str;
                }
            }
            FilePickerTarget::DiskMgrInspectPath => {
                if let Some(d) = self.disk_mgr_dlg.as_mut() {
                    d.inspect_path = path_str;
                }
            }
            FilePickerTarget::SettingsFirmwarePath => {
                if let Some(d) = self.vm_settings_dlg.as_mut() {
                    d.uefi_firmware = path_str;
                }
            }
            FilePickerTarget::HostOpenVmmBin => {
                if let Some(d) = self.host_settings_dlg.as_mut() {
                    d.openvmm_bin = path_str;
                }
            }
            FilePickerTarget::HostViewerBin => {
                if let Some(d) = self.host_settings_dlg.as_mut() {
                    d.fastdesk_viewer_bin = path_str;
                }
            }
            FilePickerTarget::HostImgBin => {
                if let Some(d) = self.host_settings_dlg.as_mut() {
                    d.openvmm_img_bin = path_str;
                }
            }
            FilePickerTarget::HostUefiFirmware => {
                if let Some(d) = self.host_settings_dlg.as_mut() {
                    d.uefi_firmware_path = path_str;
                }
            }
        }
    }
}

impl eframe::App for OpenVmmsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_updates();

        if self.detail_tab == DetailTab::ConsoleLogs
            && let Some(sel) = self.selected_vm
            && self.last_log_fetch.elapsed().as_secs() >= 2
        {
            self.last_log_fetch = Instant::now();
            self.backend.send(GuiRequest::FetchVmLogs { id: sel });
        }

        // Top toolbar
        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("OpenVMM Manager").strong().size(16.0));
                ui.separator();

                let pill_color = if self.connected {
                    Color32::from_rgb(65, 195, 110)
                } else {
                    Color32::from_rgb(225, 95, 85)
                };
                ui.colored_label(pill_color, format!("● {}", self.connection_status));
                if !self.connected && ui.button("Reconnect / Start Daemon").clicked() {
                    self.backend.send(GuiRequest::ConnectOrStartDaemon);
                }

                ui.separator();
                if ui.button("➕ New VM…").clicked() {
                    self.new_vm_wizard = Some(NewVmWizard::new(&self.data_dir));
                }
                if ui.button("💾 Virtual Disk Manager…").clicked() {
                    self.disk_mgr_dlg = Some(DiskManagerDialog::new(&self.data_dir));
                }
                if ui.button("⚙ Host Settings…").clicked() {
                    self.host_settings_dlg =
                        Some(HostSettingsDialog::from_config(&self.host_config));
                }
                if ui.button("↻ Refresh").clicked() {
                    self.backend.send(GuiRequest::RefreshAll);
                }
            });
            ui.add_space(4.0);
        });

        // Bottom status bar
        egui::TopBottomPanel::bottom("bottom_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(err) = &self.error_banner {
                    ui.colored_label(Color32::LIGHT_RED, format!("Error: {err}"));
                    if ui.small_button("Dismiss").clicked() {
                        self.error_banner = None;
                    }
                } else {
                    ui.label(&self.status_line);
                }
            });
        });

        // Left navigation tree
        egui::SidePanel::left("left_nav")
            .default_width(210.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                let host_name = self
                    .host_info
                    .as_ref()
                    .map(|h| h.hostname.as_str())
                    .unwrap_or("localhost");
                ui.label(RichText::new(format!("🖥 {host_name}")).strong());
                ui.separator();

                let total = self.vms.len();
                let running = self
                    .vms
                    .iter()
                    .filter(|v| v.runtime.status == VmStatus::Running)
                    .count();
                let paused = self
                    .vms
                    .iter()
                    .filter(|v| v.runtime.status == VmStatus::Paused)
                    .count();
                let off = self
                    .vms
                    .iter()
                    .filter(|v| matches!(v.runtime.status, VmStatus::Off | VmStatus::Error))
                    .count();

                ui.selectable_value(
                    &mut self.nav_filter,
                    NavFilter::All,
                    format!("All Virtual Machines ({total})"),
                );
                ui.selectable_value(
                    &mut self.nav_filter,
                    NavFilter::Running,
                    format!("Running ({running})"),
                );
                ui.selectable_value(
                    &mut self.nav_filter,
                    NavFilter::Paused,
                    format!("Paused ({paused})"),
                );
                ui.selectable_value(
                    &mut self.nav_filter,
                    NavFilter::Off,
                    format!("Off / Error ({off})"),
                );

                ui.separator();
                ui.selectable_value(
                    &mut self.nav_filter,
                    NavFilter::Switches,
                    format!("Virtual Switches ({})", self.switches.len()),
                );
            });

        // Right Hyper-V style Actions pane
        egui::SidePanel::right("right_actions")
            .default_width(220.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Actions").strong().size(14.0));
                ui.separator();

                ui.label(RichText::new("Host").strong());
                if ui.button("New Virtual Machine…").clicked() {
                    self.new_vm_wizard = Some(NewVmWizard::new(&self.data_dir));
                }
                if ui.button("Virtual Hard Disk Manager…").clicked() {
                    self.disk_mgr_dlg = Some(DiskManagerDialog::new(&self.data_dir));
                }
                if ui.button("Host Settings…").clicked() {
                    self.host_settings_dlg =
                        Some(HostSettingsDialog::from_config(&self.host_config));
                }

                ui.add_space(10.0);
                ui.separator();

                if let Some(vm) = self.selected_vm_summary().cloned() {
                    let id = vm.config.id;
                    let status = vm.runtime.status;
                    let can_start = matches!(status, VmStatus::Off | VmStatus::Error);
                    let is_running = status == VmStatus::Running;
                    let is_paused = status == VmStatus::Paused;
                    let is_active = is_running || is_paused;
                    let has_fastdesk = vm.runtime.fastdesk_endpoint.is_some();

                    ui.label(RichText::new(&vm.config.name).strong());
                    ui.add_space(4.0);

                    ui.add_enabled_ui(has_fastdesk, |ui| {
                        if ui
                            .button(RichText::new("🖥 Connect (FastDesk)…").strong())
                            .clicked()
                        {
                            self.backend.send(GuiRequest::ConnectViewer { id, no_shm: false });
                        }
                    });
                    if ui.add_enabled(can_start, egui::Button::new("▶ Start")).clicked() {
                        self.backend.send(GuiRequest::StartVm { id });
                    }
                    if ui
                        .add_enabled(is_running, egui::Button::new("⏻ Shut Down"))
                        .clicked()
                    {
                        self.backend.send(GuiRequest::ShutdownVm {
                            id,
                            acpi: false,
                            force: false,
                        });
                    }
                    if ui
                        .add_enabled(is_active, egui::Button::new("⏹ Turn Off"))
                        .clicked()
                    {
                        self.backend.send(GuiRequest::StopVm { id });
                    }
                    if is_paused {
                        if ui.button("▶ Resume").clicked() {
                            self.backend.send(GuiRequest::ResumeVm { id });
                        }
                    } else if ui
                        .add_enabled(is_running, egui::Button::new("⏸ Pause"))
                        .clicked()
                    {
                        self.backend.send(GuiRequest::PauseVm { id });
                    }
                    if ui
                        .add_enabled(is_active, egui::Button::new("↻ Reset"))
                        .clicked()
                    {
                        self.backend.send(GuiRequest::ResetVm { id });
                    }
                    ui.separator();
                    if ui.button("⚙ Settings…").clicked() {
                        self.vm_settings_dlg =
                            Some(VmSettingsDialog::from_config(&vm.config, status));
                    }
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(RichText::new("🗑 Delete…").color(Color32::LIGHT_RED)),
                        )
                        .clicked()
                    {
                        self.delete_vm_dlg = Some(DeleteVmDialog {
                            vm_id: id,
                            vm_name: vm.config.name.clone(),
                            delete_disks: false,
                        });
                    }
                } else {
                    ui.label("Select a VM to see actions.");
                }
            });

        // Center Workspace
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.nav_filter == NavFilter::Switches {
                ui.heading("Virtual Switches & Network Backends");
                ui.separator();
                egui::Grid::new("switches_table")
                    .striped(true)
                    .num_columns(4)
                    .spacing([18.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Name").strong());
                        ui.label(RichText::new("Kind").strong());
                        ui.label(RichText::new("ID").strong());
                        ui.label(RichText::new("Description").strong());
                        ui.end_row();
                        for sw in &self.switches {
                            ui.label(&sw.name);
                            ui.label(&sw.kind);
                            ui.monospace(&sw.id);
                            ui.label(&sw.description);
                            ui.end_row();
                        }
                    });
                return;
            }

            // Center-top: Virtual Machines Table
            ui.label(RichText::new("Virtual Machines").strong().size(14.0));
            ui.separator();

            let filtered: Vec<VmSummary> = self.filtered_vms().into_iter().cloned().collect();
            egui::ScrollArea::vertical()
                .id_salt("vm_table_scroll")
                .max_height(260.0)
                .show(ui, |ui| {
                    if filtered.is_empty() {
                        ui.add_space(16.0);
                        ui.label("No virtual machines match this filter. Click 'New VM…' to create one.");
                    } else {
                        egui::Grid::new("vm_list_grid")
                            .striped(true)
                            .num_columns(6)
                            .spacing([16.0, 6.0])
                            .show(ui, |ui| {
                                ui.label(RichText::new("Name").strong());
                                ui.label(RichText::new("State").strong());
                                ui.label(RichText::new("CPU").strong());
                                ui.label(RichText::new("Memory").strong());
                                ui.label(RichText::new("FastDesk").strong());
                                ui.label(RichText::new("Storage").strong());
                                ui.end_row();

                                for vm in &filtered {
                                    let selected = self.selected_vm == Some(vm.config.id);
                                    let row_resp =
                                        ui.selectable_label(selected, &vm.config.name);
                                    if row_resp.clicked() {
                                        self.selected_vm = Some(vm.config.id);
                                        self.backend
                                            .send(GuiRequest::FetchVmDetails { id: vm.config.id });
                                        if self.detail_tab == DetailTab::ConsoleLogs {
                                            self.backend
                                                .send(GuiRequest::FetchVmLogs { id: vm.config.id });
                                        }
                                    }
                                    if row_resp.double_clicked()
                                        && vm.runtime.fastdesk_endpoint.is_some()
                                    {
                                        self.backend.send(GuiRequest::ConnectViewer {
                                            id: vm.config.id,
                                            no_shm: false,
                                        });
                                    }

                                    let (state_color, state_text) = match vm.runtime.status {
                                        VmStatus::Running => (Color32::from_rgb(70, 200, 115), "Running"),
                                        VmStatus::Paused => (Color32::from_rgb(240, 190, 70), "Paused"),
                                        VmStatus::Starting | VmStatus::Stopping => {
                                            (Color32::from_rgb(100, 180, 245), "Transitioning")
                                        }
                                        VmStatus::Off => (Color32::GRAY, "Off"),
                                        VmStatus::Error => (Color32::LIGHT_RED, "Error"),
                                    };
                                    ui.colored_label(state_color, state_text);
                                    ui.label(vm.config.vcpus.to_string());
                                    ui.label(format_bytes(vm.config.memory_mb * 1024 * 1024));
                                    ui.label(
                                        vm.runtime
                                            .fastdesk_endpoint
                                            .as_deref()
                                            .unwrap_or(if vm.config.fastdesk.enabled {
                                                "Ready"
                                            } else {
                                                "Off"
                                            }),
                                    );
                                    ui.label(format!("{} LUN(s)", vm.config.disks.len()));
                                    ui.end_row();
                                }
                            });
                    }
                });

            ui.add_space(10.0);
            ui.separator();

            // Center-bottom: Details Inspector Tabs
            if let Some(vm) = self.selected_vm_summary().cloned() {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.detail_tab, DetailTab::Summary, "Summary");
                    ui.selectable_value(
                        &mut self.detail_tab,
                        DetailTab::Storage,
                        format!("Storage & ISO ({})", vm.config.disks.len()),
                    );
                    ui.selectable_value(
                        &mut self.detail_tab,
                        DetailTab::Networking,
                        format!("Networking ({})", vm.config.nics.len()),
                    );
                    if ui
                        .selectable_value(
                            &mut self.detail_tab,
                            DetailTab::ConsoleLogs,
                            "Console & Logs",
                        )
                        .clicked()
                    {
                        self.backend
                            .send(GuiRequest::FetchVmLogs { id: vm.config.id });
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt("details_scroll")
                    .show(ui, |ui| match self.detail_tab {
                        DetailTab::Summary => self.render_summary_tab(ui, &vm),
                        DetailTab::Storage => self.render_storage_tab(ui, &vm),
                        DetailTab::Networking => self.render_networking_tab(ui, &vm),
                        DetailTab::ConsoleLogs => self.render_logs_tab(ui, &vm),
                    });
            } else {
                ui.label("Select a virtual machine above to inspect its summary, storage, networking, and logs.");
            }
        });

        // Render modal dialogs
        render_new_vm_wizard(
            ctx,
            &mut self.new_vm_wizard,
            &self.switches,
            &self.backend,
            &mut self.file_picker,
        );
        render_vm_settings_dialog(
            ctx,
            &mut self.vm_settings_dlg,
            &self.switches,
            &self.backend,
            &mut self.file_picker,
        );
        render_disk_manager_dialog(
            ctx,
            &mut self.disk_mgr_dlg,
            &self.backend,
            &mut self.file_picker,
        );
        render_host_settings_dialog(
            ctx,
            &mut self.host_settings_dlg,
            self.host_info.as_ref(),
            &self.backend,
            &mut self.file_picker,
        );
        render_delete_vm_dialog(ctx, &mut self.delete_vm_dlg, &self.backend);
        if let Some((target, path)) = render_file_picker(ctx, &mut self.file_picker) {
            self.apply_picked_file(target, path);
        }
    }
}

impl OpenVmmsApp {
    fn render_summary_tab(&self, ui: &mut egui::Ui, vm: &VmSummary) {
        egui::Grid::new("vm_summary_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                ui.label("UUID:");
                ui.monospace(vm.config.id.to_string());
                ui.end_row();
                ui.label("Preset:");
                ui.label(vm.config.preset.to_string());
                ui.end_row();
                ui.label("Compute:");
                ui.label(format!(
                    "{} vCPU, {} RAM (Nested Virt: {})",
                    vm.config.vcpus,
                    format_bytes(vm.config.memory_mb * 1024 * 1024),
                    if vm.config.nested_virt { "On" } else { "Off" }
                ));
                ui.end_row();
                ui.label("UEFI & Security:");
                ui.label(format!(
                    "Secure Boot: {} | Template: {}",
                    if vm.config.secure_boot_enabled {
                        "On"
                    } else {
                        "Off"
                    },
                    vm.config.secure_boot_template
                ));
                ui.end_row();
                ui.label("FastDesk Display:");
                if let Some(ep) = &vm.runtime.fastdesk_endpoint {
                    ui.monospace(format!(
                        "{ep} ({} MiB VRAM, SHM: {})",
                        vm.config.fastdesk.vram_mb,
                        vm.runtime.shm_path.as_deref().unwrap_or("disabled")
                    ));
                } else {
                    ui.label(format!(
                        "{} ({} MiB VRAM, SHM: {})",
                        if vm.config.fastdesk.enabled {
                            "Configured (offline)"
                        } else {
                            "Disabled"
                        },
                        vm.config.fastdesk.vram_mb,
                        if vm.config.fastdesk.use_shm { "On" } else { "Off" }
                    ));
                }
                ui.end_row();
                if let Some(pid) = vm.runtime.pid {
                    ui.label("Worker PID:");
                    ui.monospace(pid.to_string());
                    ui.end_row();
                }
                if let Some(err) = &vm.runtime.last_error {
                    ui.label("Last Error:");
                    ui.colored_label(Color32::LIGHT_RED, err);
                    ui.end_row();
                }
            });
    }

    fn render_storage_tab(&mut self, ui: &mut egui::Ui, vm: &VmSummary) {
        let id = vm.config.id;
        egui::Grid::new("storage_tab_grid")
            .striped(true)
            .num_columns(5)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("LUN").strong());
                ui.label(RichText::new("Kind").strong());
                ui.label(RichText::new("Path").strong());
                ui.label(RichText::new("Access").strong());
                ui.label(RichText::new("Action").strong());
                ui.end_row();

                for d in &vm.config.disks {
                    ui.label(d.lun.to_string());
                    ui.label(if d.kind == DiskKind::Dvd {
                        "DVD/ISO"
                    } else {
                        "Hard Disk"
                    });
                    ui.monospace(d.path.display().to_string());
                    ui.label(if d.read_only { "Read-Only" } else { "Read/Write" });
                    if d.kind == DiskKind::Dvd {
                        if ui.small_button("Eject").clicked() {
                            self.backend.send(GuiRequest::EjectIso {
                                id,
                                lun: Some(d.lun),
                            });
                        }
                    } else if ui.small_button("Detach").clicked() {
                        self.backend.send(GuiRequest::DetachDisk { id, lun: d.lun });
                    }
                    ui.end_row();
                }
            });

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Attach Disk:");
            ui.add(egui::TextEdit::singleline(&mut self.attach_disk_input).desired_width(300.0));
            if ui.button("Browse…").clicked() {
                self.file_picker = Some(FilePickerModal::open(
                    FilePickerTarget::AttachDiskPath,
                    &self.attach_disk_input,
                    false,
                    vec!["vhdx", "vhd", "img", "raw"],
                ));
            }
            if ui.button("Attach").clicked() && !self.attach_disk_input.trim().is_empty() {
                self.backend.send(GuiRequest::AttachDisk {
                    id,
                    payload: AttachDiskPayload {
                        path: PathBuf::from(self.attach_disk_input.trim()),
                        lun: None,
                        read_only: false,
                        format: None,
                    },
                });
            }
        });

        ui.horizontal(|ui| {
            ui.label("Mount ISO:  ");
            ui.add(egui::TextEdit::singleline(&mut self.mount_iso_input).desired_width(300.0));
            if ui.button("Browse…").clicked() {
                self.file_picker = Some(FilePickerModal::open(
                    FilePickerTarget::MountIsoPath,
                    &self.mount_iso_input,
                    false,
                    vec!["iso"],
                ));
            }
            if ui.button("Mount ISO").clicked() && !self.mount_iso_input.trim().is_empty() {
                self.backend.send(GuiRequest::MountIso {
                    id,
                    payload: MountIsoPayload {
                        path: PathBuf::from(self.mount_iso_input.trim()),
                        lun: None,
                    },
                });
            }
        });
    }

    fn render_networking_tab(&mut self, ui: &mut egui::Ui, vm: &VmSummary) {
        let id = vm.config.id;
        for nic in &vm.config.nics {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&nic.name).strong());
                    ui.monospace(format!("MAC: {}", nic.mac_address));
                    ui.monospace(format!("ID: {}", nic.id));
                });
                match &nic.backend {
                    NicBackendConfig::Consomme { port_forwards, .. } => {
                        ui.label("Backend: Consomme User-Mode NAT");
                        for pf in port_forwards {
                            ui.horizontal(|ui| {
                                ui.monospace(pf.to_string());
                                if ui.small_button("Remove").clicked() {
                                    self.backend.send(GuiRequest::RemovePortForward {
                                        id,
                                        nic_id: nic.id,
                                        port: pf.clone(),
                                    });
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt(("pf_proto", nic.id))
                                .selected_text(self.pf_proto.to_string())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.pf_proto, IpProto::Tcp, "tcp");
                                    ui.selectable_value(&mut self.pf_proto, IpProto::Udp, "udp");
                                });
                            ui.add(
                                egui::TextEdit::singleline(&mut self.pf_host_addr)
                                    .desired_width(110.0),
                            );
                            ui.label("Host Port:");
                            ui.add(egui::DragValue::new(&mut self.pf_host_port).range(1..=65535));
                            ui.label("-> Guest Port:");
                            ui.add(egui::DragValue::new(&mut self.pf_guest_port).range(1..=65535));
                            if ui.button("Add Port Forward").clicked() {
                                self.backend.send(GuiRequest::AddPortForward {
                                    id,
                                    nic_id: nic.id,
                                    port: PortForward {
                                        protocol: self.pf_proto,
                                        host_addr: self.pf_host_addr.clone(),
                                        host_port: self.pf_host_port,
                                        guest_port: self.pf_guest_port,
                                    },
                                });
                            }
                        });
                    }
                    NicBackendConfig::VmSwitch {
                        switch_id,
                        switch_name,
                    } => {
                        ui.label(format!(
                            "Backend: Windows Hyper-V VMSwitch ({} / {switch_id})",
                            switch_name.as_deref().unwrap_or("Switch")
                        ));
                    }
                    NicBackendConfig::Tap { name } => {
                        ui.label(format!("Backend: Linux TAP ({name})"));
                    }
                }
            });
        }
        if vm.config.nics.is_empty() {
            ui.label("No network adapters attached. Edit VM Settings while off to add one.");
        }
    }

    fn render_logs_tab(&mut self, ui: &mut egui::Ui, vm: &VmSummary) {
        ui.horizontal(|ui| {
            let can_connect = vm.runtime.fastdesk_endpoint.is_some();
            if ui
                .add_enabled(can_connect, egui::Button::new("🖥 Launch FastDesk Viewer"))
                .clicked()
            {
                self.backend.send(GuiRequest::ConnectViewer {
                    id: vm.config.id,
                    no_shm: false,
                });
            }
            if ui
                .add_enabled(
                    can_connect,
                    egui::Button::new("Launch Viewer (LZ4 TCP / --no-shm)"),
                )
                .clicked()
            {
                self.backend.send(GuiRequest::ConnectViewer {
                    id: vm.config.id,
                    no_shm: true,
                });
            }
            if ui.button("Refresh Logs").clicked() {
                self.backend
                    .send(GuiRequest::FetchVmLogs { id: vm.config.id });
            }
        });
        ui.separator();
        let text = match &self.logs_cache {
            Some((id, content)) if *id == vm.config.id && !content.trim().is_empty() => {
                content.as_str()
            }
            _ => "(No OpenVMM log output yet)",
        };
        ui.add(
            egui::TextEdit::multiline(&mut text.to_string())
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .desired_rows(12),
        );
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    if args.self_test {
        return controller::run_self_test();
    }

    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    let daemon_url = args.daemon_url;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "OpenVMM Manager (OpenVMMS)",
        options,
        Box::new(move |cc| Ok(Box::new(OpenVmmsApp::new(cc, data_dir, daemon_url)))),
    )
    .map_err(|err| anyhow::anyhow!("eframe error: {err}"))
}
