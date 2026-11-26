mod controller;
mod dialogs;

use clap::Parser;
use controller::BackendHandle;
use controller::DialogKind;
use controller::GuiRequest;
use controller::GuiUpdate;
use controller::HostConfigPatch;
use dialogs::ConfirmActionDialog;
use dialogs::DeleteVmDialog;
use dialogs::DiskManagerDialog;
use dialogs::FilePickerModal;
use dialogs::FilePickerState;
use dialogs::FilePickerTarget;
use dialogs::HostSettingsDialog;
use dialogs::NewVmWizard;
use dialogs::PendingConfirmAction;
use dialogs::VmSettingsDialog;
use dialogs::render_confirm_action_dialog;
use dialogs::render_delete_vm_dialog;
use dialogs::render_disk_manager_dialog;
use dialogs::render_file_picker_state;
use dialogs::render_host_settings_dialog;
use dialogs::render_new_vm_wizard;
use dialogs::render_vm_settings_dialog;
use egui::Color32;
use egui::RichText;
use velovm_core::models::AttachDiskPayload;
use velovm_core::models::DiskFormat;
use velovm_core::models::DiskKind;
use velovm_core::models::HostConfig;
use velovm_core::models::HostInfo;
use velovm_core::models::IpProto;
use velovm_core::models::MountIsoPayload;
use velovm_core::models::NicBackendConfig;
use velovm_core::models::PortForward;
use velovm_core::models::VirtualSwitchInfo;
use velovm_core::models::VmPreset;
use velovm_core::models::VmStatus;
use velovm_core::models::VmSummary;
use velovm_core::models::default_data_dir;
use velovm_core::models::format_memory_mb;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "velovm-gui", about = "Hyper-V-style desktop manager for VeloVM (OpenVMM and FastDesk)")]
struct Args {
    /// Directory for VM configs, logs, and host configuration
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Override daemon HTTP endpoint (e.g. http://127.0.0.1:47620)
    #[arg(long)]
    daemon_url: Option<String>,

    /// Run headless egui UI and controller verification against an ephemeral daemon
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

struct VeloVmApp {
    data_dir: PathBuf,
    backend: BackendHandle,
    connected: bool,
    host_config_loaded: bool,
    connection_status: String,
    host_info: Option<HostInfo>,
    host_config: HostConfig,
    switches: Vec<VirtualSwitchInfo>,
    vms: Vec<VmSummary>,
    selected_vm: Option<Uuid>,
    daemon_instance_id: Option<Uuid>,
    connected_base_url: Option<String>,
    last_vm_snapshot_seq: u64,
    last_host_config_seq: u64,
    vm_revisions: std::collections::HashMap<Uuid, u64>,
    deleted_vms: std::collections::HashMap<Uuid, u64>,
    nav_filter: NavFilter,
    detail_tab: DetailTab,
    logs_cache: Option<(Uuid, String)>,
    last_log_fetch: Instant,
    status_line: String,
    error_banner: Option<String>,

    // Inline inputs in bottom inspector tabs
    attach_disk_input: String,
    mount_iso_input: String,
    attach_disk_in_flight: std::collections::HashMap<Uuid, u64>,
    mount_iso_in_flight: std::collections::HashMap<Uuid, u64>,
    next_storage_op_id: u64,
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
    confirm_action_dlg: Option<ConfirmActionDialog>,
    file_picker: FilePickerState,
}

impl VeloVmApp {
    fn new(cc: &eframe::CreationContext<'_>, data_dir: PathBuf, daemon_url: Option<String>) -> Self {
        let mut app = Self::new_headless(&cc.egui_ctx, data_dir, daemon_url);
        app.file_picker.force_modal = false;
        app
    }

    fn new_headless(ctx: &egui::Context, data_dir: PathBuf, daemon_url: Option<String>) -> Self {
        ctx.set_visuals(egui::Visuals::dark());
        let backend = BackendHandle::spawn(Some(data_dir.clone()), daemon_url, ctx.clone());
        backend.send(GuiRequest::ConnectOrStartDaemon);
        let mut app = Self::new_with_backend(data_dir, backend);
        app.file_picker.force_modal = true;
        app
    }

    #[cfg(test)]
    fn new_for_test(ctx: &egui::Context, data_dir: PathBuf) -> Self {
        Self::new_with_backend(data_dir, BackendHandle::new_for_test(ctx.clone()))
    }

    fn new_with_backend(data_dir: PathBuf, backend: BackendHandle) -> Self {
        Self {
            data_dir,
            backend,
            connected: false,
            host_config_loaded: false,
            connection_status: "Connecting to velovmd…".to_string(),
            host_info: None,
            host_config: HostConfig::default(),
            switches: Vec::new(),
            vms: Vec::new(),
            selected_vm: None,
            daemon_instance_id: None,
            connected_base_url: None,
            last_vm_snapshot_seq: 0,
            last_host_config_seq: 0,
            vm_revisions: std::collections::HashMap::new(),
            deleted_vms: std::collections::HashMap::new(),
            nav_filter: NavFilter::All,
            detail_tab: DetailTab::Summary,
            logs_cache: None,
            last_log_fetch: Instant::now(),
            status_line: "Starting VeloVM…".to_string(),
            error_banner: None,
            attach_disk_input: String::new(),
            mount_iso_input: String::new(),
            attach_disk_in_flight: std::collections::HashMap::new(),
            mount_iso_in_flight: std::collections::HashMap::new(),
            next_storage_op_id: 1,
            pf_proto: IpProto::Tcp,
            pf_host_addr: "127.0.0.1".to_string(),
            pf_host_port: 3389,
            pf_guest_port: 3389,
            new_vm_wizard: None,
            vm_settings_dlg: None,
            disk_mgr_dlg: None,
            host_settings_dlg: None,
            delete_vm_dlg: None,
            confirm_action_dlg: None,
            file_picker: FilePickerState::default(),
        }
    }

    fn sync_vm_settings_from_vm(dlg: &mut Option<VmSettingsDialog>, vm: &VmSummary) {
        if let Some(dlg) = dlg.as_mut()
            && dlg.vm_id == vm.config.id
        {
            dlg.base_fastdesk = vm.config.fastdesk.clone();
            dlg.nics = vm.config.nics.clone();
            dlg.is_offline = matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error);
        }
    }

    fn open_host_settings_dialog(&mut self) {
        if self.host_settings_dlg.is_none() {
            self.host_settings_dlg = Some(HostSettingsDialog::from_config(&self.host_config));
        }
    }

    fn open_new_vm_wizard(&mut self) {
        if self.new_vm_wizard.is_none() {
            self.new_vm_wizard = Some(NewVmWizard::new(&self.data_dir));
        }
    }

    fn open_disk_manager_dialog(&mut self) {
        if self.disk_mgr_dlg.is_none() {
            self.disk_mgr_dlg = Some(DiskManagerDialog::new(&self.data_dir));
        }
    }

    fn process_updates(&mut self) {
        while let Some(upd) = self.backend.try_recv() {
            self.apply_update(upd);
        }
    }

    fn apply_update(&mut self, upd: GuiUpdate) {
        match upd {
            GuiUpdate::DaemonConnected {
                base_url,
                host_info,
                host_config,
                switches,
                vms,
                snapshot_seq,
            } => {
                if snapshot_seq > 0 && snapshot_seq < self.last_vm_snapshot_seq {
                    return;
                }
                if snapshot_seq > self.last_vm_snapshot_seq {
                    self.last_vm_snapshot_seq = snapshot_seq;
                }
                let was_connected = self.connected;
                let prev_id = self.daemon_instance_id;
                let prev_url = self.connected_base_url.clone();
                let same_daemon = self.daemon_instance_id.is_some()
                    && self.daemon_instance_id == host_info.daemon_instance_id;
                let daemon_changed = ((prev_id.is_some() || host_info.daemon_instance_id.is_some())
                    && prev_id != host_info.daemon_instance_id)
                    || !was_connected
                    || prev_url.as_deref() != Some(base_url.as_str());
                self.daemon_instance_id = host_info.daemon_instance_id;
                self.connected_base_url = Some(base_url.clone());

                let has_authoritative_rev = host_info.event_revision > 0;
                let snap_rev_watermark = host_info
                    .event_revision
                    .max(vms.iter().map(|v| v.runtime.revision).max().unwrap_or(0));

                self.connected = true;
                self.host_config_loaded = true;
                self.connection_status = format!("Connected ({base_url})");
                self.host_info = Some(host_info);
                if snapshot_seq == 0 || snapshot_seq >= self.last_host_config_seq {
                    self.last_host_config_seq = self.last_host_config_seq.max(snapshot_seq);
                    self.host_config = host_config;
                    if let Some(d) = self.host_settings_dlg.as_mut() {
                        d.sync_from_config(&self.host_config);
                    }
                }
                self.switches = switches;

                if same_daemon {
                    let existing_by_id: std::collections::HashMap<Uuid, VmSummary> =
                        self.vms.drain(..).map(|v| (v.config.id, v)).collect();
                    let mut merged = Vec::with_capacity(vms.len().max(existing_by_id.len()));
                    let mut seen_ids = std::collections::HashSet::with_capacity(vms.len());
                    for vm in vms {
                        let id = vm.config.id;
                        seen_ids.insert(id);
                        if self.deleted_vms.contains_key(&id) {
                            continue;
                        }
                        let snap_rev = vm.runtime.revision;
                        let known_rev = self.vm_revisions.get(&id).copied().unwrap_or(0);
                        if snap_rev < known_rev
                            && let Some(existing) = existing_by_id.get(&id)
                        {
                            merged.push(existing.clone());
                            continue;
                        }
                        if snap_rev > 0 {
                            self.vm_revisions.insert(id, snap_rev.max(known_rev));
                        }
                        merged.push(vm);
                    }
                    for (id, existing) in existing_by_id {
                        if seen_ids.contains(&id) || self.deleted_vms.contains_key(&id) {
                            continue;
                        }
                        let known_rev = self
                            .vm_revisions
                            .get(&id)
                            .copied()
                            .unwrap_or(existing.runtime.revision);
                        if has_authoritative_rev && known_rev > snap_rev_watermark {
                            merged.push(existing);
                        } else {
                            if has_authoritative_rev {
                                let del_entry = self.deleted_vms.entry(id).or_insert(0);
                                *del_entry = (*del_entry).max(snap_rev_watermark);
                                let rev_entry = self.vm_revisions.entry(id).or_insert(0);
                                *rev_entry = (*rev_entry).max(snap_rev_watermark);
                            } else {
                                self.vm_revisions.remove(&id);
                            }
                            if matches!(&self.logs_cache, Some((cached_id, _)) if *cached_id == id)
                            {
                                self.logs_cache = None;
                            }
                        }
                    }
                    merged.sort_by(|a, b| {
                        a.config
                            .name
                            .to_lowercase()
                            .cmp(&b.config.name.to_lowercase())
                    });
                    self.vms = merged;
                } else {
                    self.vm_revisions.clear();
                    self.deleted_vms.clear();
                    if daemon_changed {
                        self.attach_disk_in_flight.clear();
                        self.mount_iso_in_flight.clear();
                    }
                    self.logs_cache = None;
                    for vm in &vms {
                        if vm.runtime.revision > 0 {
                            self.vm_revisions.insert(vm.config.id, vm.runtime.revision);
                        }
                    }
                    self.vms = vms;
                }

                self.error_banner = None;
                self.status_line = format!("{} virtual machine(s) loaded", self.vms.len());
                if self
                    .selected_vm
                    .is_none_or(|sel| !self.vms.iter().any(|v| v.config.id == sel))
                {
                    self.selected_vm = self.vms.first().map(|v| v.config.id);
                }
                if let Some(dlg_vm_id) = self.vm_settings_dlg.as_ref().map(|d| d.vm_id) {
                    if let Some(vm) = self.vms.iter().find(|v| v.config.id == dlg_vm_id) {
                        Self::sync_vm_settings_from_vm(&mut self.vm_settings_dlg, vm);
                    } else if daemon_changed
                        && let Some(dlg) = self.vm_settings_dlg.as_mut()
                    {
                        if dlg.submitting {
                            dlg.error = Some("VM no longer exists on this daemon.".to_string());
                        } else {
                            self.vm_settings_dlg = None;
                        }
                    }
                }
            }
            GuiUpdate::DaemonDisconnected(reason) => {
                self.connected = false;
                self.connection_status = "Disconnected".to_string();
                self.error_banner = Some(reason);
            }
            GuiUpdate::VmUpdated {
                vm,
                select,
                is_details_fetch,
                daemon_instance_id,
            } => {
                if self.daemon_instance_id.is_some()
                    && daemon_instance_id.is_some()
                    && daemon_instance_id != self.daemon_instance_id
                {
                    return;
                }
                let id = vm.config.id;
                if self.deleted_vms.contains_key(&id) {
                    return;
                }
                let rev = vm.runtime.revision;
                let prev_rev = self.vm_revisions.get(&id).copied().unwrap_or(0);
                if rev < prev_rev {
                    if select && self.vms.iter().any(|v| v.config.id == id) {
                        self.selected_vm = Some(id);
                    }
                    return;
                }
                if let Some(slot) = self.vms.iter_mut().find(|v| v.config.id == id) {
                    if rev > 0 {
                        self.vm_revisions.insert(id, rev);
                    }
                    *slot = vm;
                    Self::sync_vm_settings_from_vm(&mut self.vm_settings_dlg, slot);
                    if select {
                        self.selected_vm = Some(id);
                    }
                } else if !is_details_fetch {
                    if rev > 0 {
                        self.vm_revisions.insert(id, rev);
                    }
                    self.vms.push(vm);
                    self.vms.sort_by(|a, b| {
                        a.config
                            .name
                            .to_lowercase()
                            .cmp(&b.config.name.to_lowercase())
                    });
                    if select || self.selected_vm.is_none() {
                        self.selected_vm = Some(id);
                    }
                }
            }
            GuiUpdate::VmDeleted {
                id,
                revision,
                daemon_instance_id,
            } => {
                if self.daemon_instance_id.is_some()
                    && daemon_instance_id.is_some()
                    && daemon_instance_id != self.daemon_instance_id
                {
                    return;
                }
                let del_entry = self.deleted_vms.entry(id).or_insert(0);
                *del_entry = (*del_entry).max(revision);
                if revision > 0 {
                    let rev_entry = self.vm_revisions.entry(id).or_insert(0);
                    *rev_entry = (*rev_entry).max(revision);
                }
                self.vms.retain(|v| v.config.id != id);
                if matches!(&self.logs_cache, Some((cached_id, _)) if *cached_id == id) {
                    self.logs_cache = None;
                }
                if self.selected_vm == Some(id) {
                    self.selected_vm = self.vms.first().map(|v| v.config.id);
                }
            }
            _ => self.apply_misc_update(upd),
        }
    }

    fn apply_misc_update(&mut self, upd: GuiUpdate) {
        match upd {
            GuiUpdate::VmLogs { id, content } => {
                if self.selected_vm == Some(id) {
                    self.logs_cache = Some((id, content));
                }
            }
                GuiUpdate::DiskCreated(info) | GuiUpdate::DiskInspected(info) => {
                    if let Some(dlg) = self.disk_mgr_dlg.as_mut() {
                        dlg.last_info = Some(info);
                    }
                }
                GuiUpdate::HostConfigUpdated {
                    base_url,
                    host_info,
                    host_config,
                    snapshot_seq,
                } => {
                    if snapshot_seq > 0 && snapshot_seq < self.last_host_config_seq {
                        return;
                    }
                    if snapshot_seq > self.last_host_config_seq {
                        self.last_host_config_seq = snapshot_seq;
                    }
                    if let Some(info) = host_info {
                        let prev_id = self.daemon_instance_id;
                        let daemon_changed = (prev_id.is_some()
                            || info.daemon_instance_id.is_some())
                            && prev_id != info.daemon_instance_id;
                        if daemon_changed {
                            self.vms.clear();
                            self.selected_vm = None;
                            self.vm_revisions.clear();
                            self.deleted_vms.clear();
                            self.attach_disk_in_flight.clear();
                            self.mount_iso_in_flight.clear();
                            self.logs_cache = None;
                            self.backend.send(GuiRequest::RefreshAll);
                        }
                        self.daemon_instance_id = info.daemon_instance_id;
                        self.host_info = Some(info);
                        self.connected_base_url = Some(base_url.clone());
                        if self.connected {
                            self.connection_status = format!("Connected ({base_url})");
                        }
                    }
                    self.host_config_loaded = true;
                    self.host_config = host_config;
                    if let Some(d) = self.host_settings_dlg.as_mut() {
                        d.sync_from_config(&self.host_config);
                    }
                }
                GuiUpdate::DialogFinished { kind, error } => match (kind, error) {
                    (DialogKind::NewVmWizard(wid), None) => {
                        if self
                            .new_vm_wizard
                            .as_ref()
                            .is_some_and(|w| w.wizard_id == wid)
                        {
                            self.new_vm_wizard = None;
                        }
                    }
                    (DialogKind::NewVmWizard(wid), Some(err)) => {
                        if let Some(w) = self.new_vm_wizard.as_mut()
                            && w.wizard_id == wid
                        {
                            w.submitting = false;
                            w.error = Some(err);
                        }
                    }
                    (DialogKind::VmSettings(dialog_id), None) => {
                        if self
                            .vm_settings_dlg
                            .as_ref()
                            .is_some_and(|d| d.dialog_id == dialog_id)
                        {
                            self.vm_settings_dlg = None;
                        }
                    }
                    (DialogKind::VmSettings(dialog_id), Some(err)) => {
                        if let Some(d) = self.vm_settings_dlg.as_mut()
                            && d.dialog_id == dialog_id
                        {
                            d.submitting = false;
                            d.error = Some(err);
                        }
                    }
                    (DialogKind::HostSettings(dialog_id), None) => {
                        if self
                            .host_settings_dlg
                            .as_ref()
                            .is_some_and(|d| d.dialog_id == dialog_id)
                        {
                            self.host_settings_dlg = None;
                        }
                    }
                    (DialogKind::HostSettings(dialog_id), Some(err)) => {
                        if let Some(d) = self.host_settings_dlg.as_mut()
                            && d.dialog_id == dialog_id
                        {
                            d.submitting = false;
                            d.error = Some(err);
                            d.sync_from_config(&self.host_config);
                        }
                    }
                    (DialogKind::DeleteVm(dialog_id), None) => {
                        if self
                            .delete_vm_dlg
                            .as_ref()
                            .is_some_and(|d| d.dialog_id == dialog_id)
                        {
                            self.delete_vm_dlg = None;
                        }
                    }
                    (DialogKind::DeleteVm(dialog_id), Some(err)) => {
                        if let Some(d) = self.delete_vm_dlg.as_mut()
                            && d.dialog_id == dialog_id
                        {
                            d.submitting = false;
                            d.error = Some(err);
                        }
                    }
                },
                GuiUpdate::StorageOpFinished { vm_id, op, op_id } => {
                    let map = match op {
                        controller::StorageOpKind::AttachDisk => &mut self.attach_disk_in_flight,
                        controller::StorageOpKind::MountIso => &mut self.mount_iso_in_flight,
                    };
                    if op_id == 0 || map.get(&vm_id) == Some(&op_id) {
                        map.remove(&vm_id);
                    }
                }
                GuiUpdate::StatusMessage(msg) => {
                    self.status_line = msg;
                    self.error_banner = None;
                }
                GuiUpdate::ErrorMessage(err) => {
                    self.error_banner = Some(err);
                }
                _ => {}
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

impl eframe::App for VeloVmApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render_ui(ctx);
    }
}

impl VeloVmApp {
    fn render_ui(&mut self, ctx: &egui::Context) {
        self.process_updates();

        if self.connected
            && self.detail_tab == DetailTab::ConsoleLogs
            && let Some(sel) = self.selected_vm
        {
            const LOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
            let elapsed = self.last_log_fetch.elapsed();
            if elapsed >= LOG_REFRESH_INTERVAL {
                self.last_log_fetch = Instant::now();
                self.backend.send(GuiRequest::FetchVmLogs { id: sel });
                ctx.request_repaint_after(LOG_REFRESH_INTERVAL);
            } else {
                ctx.request_repaint_after(LOG_REFRESH_INTERVAL - elapsed);
            }
        }

        // Top toolbar
        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("VeloVM Manager").strong().size(16.0));
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
                    self.open_new_vm_wizard();
                }
                if ui.button("💾 Virtual Disk Manager…").clicked() {
                    self.open_disk_manager_dialog();
                }
                let can_edit_host = self.connected && self.host_config_loaded;
                if ui
                    .add_enabled(can_edit_host, egui::Button::new("⚙ Host Settings…"))
                    .on_disabled_hover_text("Connect to velovmd before editing host settings")
                    .clicked()
                {
                    self.open_host_settings_dialog();
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
                    self.open_new_vm_wizard();
                }
                if ui.button("Virtual Hard Disk Manager…").clicked() {
                    self.open_disk_manager_dialog();
                }
                let can_edit_host = self.connected && self.host_config_loaded;
                if ui
                    .add_enabled(can_edit_host, egui::Button::new("Host Settings…"))
                    .on_disabled_hover_text("Connect to velovmd before editing host settings")
                    .clicked()
                {
                    self.open_host_settings_dialog();
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
                    let can_turn_off = is_active || status == VmStatus::Starting;
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
                        .add_enabled(can_turn_off, egui::Button::new("⏹ Turn Off"))
                        .clicked()
                    {
                        self.confirm_action_dlg = Some(ConfirmActionDialog {
                            action: PendingConfirmAction::TurnOff {
                                id,
                                vm_name: vm.config.name.clone(),
                            },
                        });
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
                        self.confirm_action_dlg = Some(ConfirmActionDialog {
                            action: PendingConfirmAction::Reset {
                                id,
                                vm_name: vm.config.name.clone(),
                            },
                        });
                    }
                    ui.separator();
                    if ui.button("⚙ Settings…").clicked()
                        && self
                            .vm_settings_dlg
                            .as_ref()
                            .is_none_or(|d| !d.submitting && d.vm_id != id)
                    {
                        self.vm_settings_dlg =
                            Some(VmSettingsDialog::from_config(&vm.config, status));
                    }
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(RichText::new("🗑 Delete…").color(Color32::LIGHT_RED)),
                        )
                        .clicked()
                        && self
                            .delete_vm_dlg
                            .as_ref()
                            .is_none_or(|d| !d.submitting && d.vm_id != id)
                    {
                        self.delete_vm_dlg =
                            Some(DeleteVmDialog::new(id, vm.config.name.clone()));
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
                                        if self.connected {
                                            self.backend.send(GuiRequest::FetchVmDetails {
                                                id: vm.config.id,
                                            });
                                            if self.detail_tab == DetailTab::ConsoleLogs {
                                                self.backend.send(GuiRequest::FetchVmLogs {
                                                    id: vm.config.id,
                                                });
                                            }
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
                                    ui.label(format_memory_mb(vm.config.memory_mb));
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
                        && self.connected
                    {
                        self.last_log_fetch = Instant::now();
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
        render_confirm_action_dialog(ctx, &mut self.confirm_action_dlg, &self.backend);
        if let Some((target, path)) = render_file_picker_state(ctx, &mut self.file_picker) {
            self.apply_picked_file(target, path);
        }
    }
}

impl VeloVmApp {
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
                    format_memory_mb(vm.config.memory_mb),
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
        if !vm.runtime.orphaned_disks.is_empty() {
            ui.group(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(245, 180, 60),
                    "Warning: Failed hot-add rollback left orphaned disk reservation(s) active until this VM is stopped:",
                );
                for d in &vm.runtime.orphaned_disks {
                    ui.monospace(format!(
                        "LUN {}: [{:?}] {} ({})",
                        d.lun,
                        d.kind,
                        d.path.display(),
                        if d.read_only { "RO" } else { "RW" }
                    ));
                }
            });
            ui.add_space(6.0);
        }
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
                        self.confirm_action_dlg = Some(ConfirmActionDialog {
                            action: PendingConfirmAction::DetachDisk {
                                id,
                                vm_name: vm.config.name.clone(),
                                lun: d.lun,
                                path: d.path.clone(),
                                vm_active: !matches!(vm.runtime.status, VmStatus::Off | VmStatus::Error),
                            },
                        });
                    }
                    ui.end_row();
                }
            });

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Attach Disk:");
            ui.add(egui::TextEdit::singleline(&mut self.attach_disk_input).desired_width(300.0));
            if ui.button("Browse…").clicked() {
                self.file_picker.open(
                    ui.ctx(),
                    FilePickerTarget::AttachDiskPath,
                    &self.attach_disk_input,
                    false,
                    vec!["vhdx", "vhd", "img", "raw"],
                );
            }
            let attach_busy = self.attach_disk_in_flight.contains_key(&id);
            let attach_label = if attach_busy { "Attaching…" } else { "Attach" };
            let can_attach = !attach_busy && !self.attach_disk_input.trim().is_empty();
            if ui
                .add_enabled(can_attach, egui::Button::new(attach_label))
                .clicked()
            {
                let op_id = self.next_storage_op_id;
                self.next_storage_op_id += 1;
                self.attach_disk_in_flight.insert(id, op_id);
                self.backend.send(GuiRequest::AttachDisk {
                    id,
                    op_id,
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
                self.file_picker.open(
                    ui.ctx(),
                    FilePickerTarget::MountIsoPath,
                    &self.mount_iso_input,
                    false,
                    vec!["iso"],
                );
            }
            let mount_busy = self.mount_iso_in_flight.contains_key(&id);
            let mount_label = if mount_busy {
                "Mounting…"
            } else {
                "Mount ISO"
            };
            let can_mount = !mount_busy && !self.mount_iso_input.trim().is_empty();
            if ui
                .add_enabled(can_mount, egui::Button::new(mount_label))
                .clicked()
            {
                let op_id = self.next_storage_op_id;
                self.next_storage_op_id += 1;
                self.mount_iso_in_flight.insert(id, op_id);
                self.backend.send(GuiRequest::MountIso {
                    id,
                    op_id,
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
        if !vm.runtime.orphaned_port_forwards.is_empty() {
            ui.group(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(245, 180, 60),
                    "Warning: Failed hot-add rollback left orphaned port-forward reservation(s) active until this VM is stopped:",
                );
                for p in &vm.runtime.orphaned_port_forwards {
                    let nic_label = vm
                        .config
                        .nics
                        .iter()
                        .find(|n| n.id == p.nic_id)
                        .map(|n| format!("{} ({})", n.name, p.nic_id))
                        .unwrap_or_else(|| p.nic_id.to_string());
                    ui.monospace(format!("NIC {nic_label}: {}", p.forward));
                }
            });
            ui.add_space(6.0);
        }
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
                    NicBackendConfig::Bridge { name } => {
                        ui.label(format!("Backend: Linux Bridge ({name})"));
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
                .add_enabled(can_connect, egui::Button::new("🖥 Launch VeloVM Viewer"))
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
        return run_self_test();
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
        "VeloVM",
        options,
        Box::new(move |cc| Ok(Box::new(VeloVmApp::new(cc, data_dir, daemon_url)))),
    )
    .map_err(|err| anyhow::anyhow!("eframe error: {err}"))
}

fn step_headless_ui(ctx: &egui::Context, app: &mut VeloVmApp, frames: usize) -> egui::FullOutput {
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 820.0),
        )),
        ..Default::default()
    };
    let mut last_output = None;
    for _ in 0..frames.max(1) {
        last_output = Some(ctx.run(raw_input.clone(), |c| app.render_ui(c)));
    }
    last_output.unwrap()
}

fn wait_for_app<F>(
    ctx: &egui::Context,
    app: &mut VeloVmApp,
    timeout: Duration,
    what: &str,
    mut pred: F,
) -> anyhow::Result<()>
where
    F: FnMut(&VeloVmApp) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let _ = step_headless_ui(ctx, app, 1);
        if pred(app) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for {what} (status='{}', error={:?})",
                app.status_line,
                app.error_banner
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn run_self_test() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path().to_path_buf();
    let rt = tokio::runtime::Runtime::new()?;
    let daemon = rt.block_on(velovmd::TestDaemonHandle::spawn(data_dir.clone()))?;

    let ctx = egui::Context::default();
    let mut app = VeloVmApp::new_headless(&ctx, data_dir.clone(), Some(daemon.base_url.clone()));

    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "daemon connection", |a| {
        a.connected && a.host_config_loaded && a.host_info.is_some()
    })?;

    // Render DiskManagerDialog and verify Raw fixed/sparse creation + inspect through the backend.
    let disk_path = tmp.path().join("selftest.img");
    let mut disk_dlg = DiskManagerDialog::new(tmp.path());
    disk_dlg.create_format = DiskFormat::Raw;
    disk_dlg.create_fixed = true;
    disk_dlg.create_path = disk_path.to_string_lossy().into_owned();
    app.disk_mgr_dlg = Some(disk_dlg);
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.backend
        .send(GuiRequest::CreateDisk(velovm_core::models::CreateDiskPayload {
            path: disk_path.clone(),
            size_bytes: 4 * 1024 * 1024,
            format: DiskFormat::Raw,
            fixed: true,
        }));
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "disk creation", |a| {
        a.disk_mgr_dlg
            .as_ref()
            .and_then(|d| d.last_info.as_ref())
            .is_some_and(|i| i.path == disk_path && i.virtual_size_bytes == 4 * 1024 * 1024)
    })?;
    if let Some(dlg) = app.disk_mgr_dlg.as_mut() {
        dlg.tab = 1;
        dlg.inspect_path = disk_path.to_string_lossy().into_owned();
    }
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.backend.send(GuiRequest::InspectDisk(disk_path.clone()));
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "disk inspect", |a| {
        a.disk_mgr_dlg
            .as_ref()
            .and_then(|d| d.last_info.as_ref())
            .is_some_and(|i| i.path == disk_path)
    })?;
    app.disk_mgr_dlg = None;

    // Render every step of NewVmWizard and submit VM creation.
    let mut wiz = NewVmWizard::new(tmp.path());
    assert!(wiz.tpm_enabled, "Win11 preset must default vTPM on");
    wiz.name = "SelfTest-VM".to_string();
    for step in 0..=4 {
        wiz.step = step;
        app.new_vm_wizard = Some(wiz);
        let _ = step_headless_ui(&ctx, &mut app, 1);
        wiz = app.new_vm_wizard.take().unwrap();
    }
    wiz.apply_preset(VmPreset::LinuxDesktop);
    wiz.name = "SelfTest-VM".to_string();
    wiz.disk_mode = dialogs::DiskMode::None;
    let create_payload = wiz.build_create_payload(&app.switches);
    let wizard_id = wiz.wizard_id;
    app.new_vm_wizard = Some(wiz);
    app.backend.send(GuiRequest::CreateVm {
        wizard_id,
        payload: create_payload,
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "VM creation", |a| {
        a.new_vm_wizard.is_none() && a.vms.iter().any(|v| v.config.name == "SelfTest-VM")
    })?;

    let vm = app
        .vms
        .iter()
        .find(|v| v.config.name == "SelfTest-VM")
        .cloned()
        .expect("SelfTest-VM missing");
    let vm_id = vm.config.id;
    app.selected_vm = Some(vm_id);

    // Render every inspector tab and verify idle ConsoleLogs schedules a 2s repaint.
    for tab in [
        DetailTab::Summary,
        DetailTab::Storage,
        DetailTab::Networking,
        DetailTab::ConsoleLogs,
    ] {
        app.detail_tab = tab;
        let out = step_headless_ui(&ctx, &mut app, 2);
        if tab == DetailTab::ConsoleLogs {
            let repaint_delay = out
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|v| v.repaint_delay)
                .unwrap_or(Duration::MAX);
            anyhow::ensure!(
                repaint_delay <= Duration::from_secs(2),
                "ConsoleLogs tab did not schedule idle auto-refresh repaint (delay={repaint_delay:?})"
            );
        }
    }

    // Exercise FilePickerModal navigation via typed dir_input and rendering.
    let sub_dir = tmp.path().join("picker-subdir");
    std::fs::create_dir_all(&sub_dir)?;
    let mut picker = FilePickerModal::open(
        FilePickerTarget::MountIsoPath,
        &tmp.path().to_string_lossy(),
        false,
        vec!["iso"],
    );
    picker.dir_input = sub_dir.to_string_lossy().into_owned();
    picker.navigate_to(sub_dir.clone());
    let picker_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while picker.loading && std::time::Instant::now() < picker_deadline {
        let _ = picker.poll_load();
        if picker.loading {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    anyhow::ensure!(picker.current_dir == sub_dir);
    app.file_picker.set_modal(picker);
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.file_picker.clear();

    // Exercise VmSettingsDialog and UpdateVm.
    let mut settings_dlg = VmSettingsDialog::from_config(&vm.config, vm.runtime.status);
    settings_dlg.description = "Headless UI self-test".to_string();
    settings_dlg.vcpus = 2;
    let update_payload = settings_dlg.build_update_payload(&app.switches);
    let settings_dialog_id = settings_dlg.dialog_id;
    app.vm_settings_dlg = Some(settings_dlg);
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.backend.send(GuiRequest::UpdateVm {
        dialog_id: settings_dialog_id,
        id: vm_id,
        payload: update_payload,
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "VM settings update", |a| {
        a.vm_settings_dlg.is_none()
            && a.vms
                .iter()
                .find(|v| v.config.id == vm_id)
                .is_some_and(|v| v.config.description == "Headless UI self-test")
    })?;

    // Exercise ISO mount and eject.
    let iso_path = tmp.path().join("selftest.iso");
    std::fs::write(&iso_path, vec![0u8; 4096])?;
    app.backend.send(GuiRequest::MountIso {
        id: vm_id,
        op_id: 1,
        payload: MountIsoPayload {
            path: iso_path.clone(),
            lun: None,
        },
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "ISO mount", |a| {
        a.vms
            .iter()
            .find(|v| v.config.id == vm_id)
            .is_some_and(|v| v.config.disks.iter().any(|d| d.kind == DiskKind::Dvd))
    })?;
    app.backend
        .send(GuiRequest::EjectIso { id: vm_id, lun: None });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "ISO eject", |a| {
        a.vms
            .iter()
            .find(|v| v.config.id == vm_id)
            .is_some_and(|v| v.config.disks.iter().all(|d| d.kind != DiskKind::Dvd))
    })?;

    // Exercise PortForward add/remove.
    let nic_id = vm
        .config
        .nics
        .first()
        .map(|n| n.id)
        .ok_or_else(|| anyhow::anyhow!("expected default Consomme NIC"))?;
    let pf = PortForward {
        protocol: IpProto::Udp,
        host_addr: "127.0.0.1".to_string(),
        host_port: 15353,
        guest_port: 53,
    };
    app.backend.send(GuiRequest::AddPortForward {
        id: vm_id,
        nic_id,
        port: pf.clone(),
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "port forward add", |a| {
        a.vms.iter().find(|v| v.config.id == vm_id).is_some_and(|v| {
            v.config.nics.iter().any(|n| {
                matches!(&n.backend, NicBackendConfig::Consomme { port_forwards, .. } if port_forwards.contains(&pf))
            })
        })
    })?;
    app.backend.send(GuiRequest::RemovePortForward {
        id: vm_id,
        nic_id,
        port: pf.clone(),
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "port forward remove", |a| {
        a.vms.iter().find(|v| v.config.id == vm_id).is_some_and(|v| {
            v.config.nics.iter().all(|n| {
                !matches!(&n.backend, NicBackendConfig::Consomme { port_forwards, .. } if port_forwards.contains(&pf))
            })
        })
    })?;

    // Render HostSettingsDialog and verify saving config keeps client connected on daemon's actual URL.
    let host_dlg = HostSettingsDialog::from_config(&app.host_config);
    let host_dialog_id = host_dlg.dialog_id;
    app.host_settings_dlg = Some(host_dlg);
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.backend.send(GuiRequest::SaveHostConfig {
        dialog_id: host_dialog_id,
        patch: HostConfigPatch::default(),
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "host settings save", |a| {
        a.host_settings_dlg.is_none()
    })?;
    anyhow::ensure!(
        app.connection_status.contains(&daemon.base_url),
        "unexpected connection status '{}' (expected '{}')",
        app.connection_status,
        daemon.base_url
    );

    // Render ConfirmActionDialog and DeleteVmDialog, then delete the VM.
    app.confirm_action_dlg = Some(ConfirmActionDialog {
        action: PendingConfirmAction::Reset {
            id: vm_id,
            vm_name: "SelfTest-VM".to_string(),
        },
    });
    let _ = step_headless_ui(&ctx, &mut app, 1);
    app.confirm_action_dlg = None;

    let delete_dlg = DeleteVmDialog::new(vm_id, "SelfTest-VM".to_string());
    let delete_dialog_id = delete_dlg.dialog_id;
    app.delete_vm_dlg = Some(delete_dlg);
    let _ = step_headless_ui(&ctx, &mut app, 2);
    app.backend.send(GuiRequest::DeleteVm {
        dialog_id: delete_dialog_id,
        id: vm_id,
        delete_disks: false,
    });
    wait_for_app(&ctx, &mut app, Duration::from_secs(15), "VM delete", |a| {
        a.delete_vm_dlg.is_none() && !a.vms.iter().any(|v| v.config.id == vm_id)
    })?;

    drop(app);
    rt.block_on(daemon.shutdown());
    println!("SELF-TEST OK (headless egui UI + controller)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use velovm_core::models::{
        HostConfig, HostInfo, VmConfig, VmPreset, VmRuntimeState, VmStatus, VmSummary,
    };

    fn make_test_vm(id: Uuid, name: &str, status: VmStatus, revision: u64) -> VmSummary {
        let mut cfg =
            VmConfig::from_preset(name, VmPreset::Custom, std::path::Path::new("/tmp/vm"));
        cfg.id = id;
        VmSummary {
            config: cfg,
            runtime: VmRuntimeState {
                status,
                revision,
                ..Default::default()
            },
        }
    }

    fn make_host_info(daemon_instance_id: Option<Uuid>) -> HostInfo {
        HostInfo {
            hostname: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_count: 4,
            daemon_version: "0.1.0".into(),
            data_dir: PathBuf::from("/tmp"),
            runtime_dir: PathBuf::from("/tmp"),
            listen_addr: "127.0.0.1:0".into(),
            daemon_instance_id,
            event_revision: 0,
            resolved_paths: Default::default(),
        }
    }

    #[test]
    fn external_vm_update_does_not_hijack_selection_while_gui_create_selects() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_a_id = Uuid::new_v4();
        let vm_b_id = Uuid::new_v4();
        let vm_c_id = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a_id, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 1,
        });
        assert_eq!(app.selected_vm, Some(vm_a_id));

        // External / SSE creation arrives with `select: false`: must not switch selection.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b_id, "Beta", VmStatus::Off, 2),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert_eq!(app.vms.len(), 2);
        assert_eq!(app.selected_vm, Some(vm_a_id));

        // Even if SSE delivers `vm_c` at a newer revision first, the GUI CreateVm response
        // (`select: true`) must still select `vm_c` without regressing its state.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_c_id, "Gamma", VmStatus::Running, 4),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert_eq!(app.selected_vm, Some(vm_a_id));
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_c_id, "Gamma", VmStatus::Off, 3),
            select: true,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert_eq!(app.selected_vm, Some(vm_c_id));
        let vm_c = app.vms.iter().find(|v| v.config.id == vm_c_id).unwrap();
        assert_eq!(vm_c.runtime.status, VmStatus::Running);
        assert_eq!(vm_c.runtime.revision, 4);
    }

    #[test]
    fn late_details_fetch_and_stale_updates_do_not_resurrect_or_regress_vm() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_a_id = Uuid::new_v4();
        let vm_b_id = Uuid::new_v4();
        let unknown_id = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![
                make_test_vm(vm_a_id, "Alpha", VmStatus::Off, 1),
                make_test_vm(vm_b_id, "Beta", VmStatus::Off, 2),
            ],
            snapshot_seq: 2,
        });

        // Stale snapshot (lower snapshot_seq) must be dropped completely.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(unknown_id, "Ghost", VmStatus::Off, 1)],
            snapshot_seq: 1,
        });
        assert_eq!(app.vms.len(), 2);
        assert!(!app.vms.iter().any(|v| v.config.id == unknown_id));

        // Advance `vm_a` to rev 5, then deliver stale rev 3 update and stale rev 4 snapshot.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_a_id, "Alpha", VmStatus::Running, 5),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_a_id, "Alpha", VmStatus::Off, 3),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert_eq!(
            app.vms.iter().find(|v| v.config.id == vm_a_id).unwrap().runtime.status,
            VmStatus::Running
        );

        // Delete `vm_b`, then send late details fetch, late SSE update, and pre-delete snapshot.
        app.apply_update(GuiUpdate::VmDeleted {
            id: vm_b_id,
            revision: 6,
            daemon_instance_id: Some(daemon_id),
        });
        assert!(!app.vms.iter().any(|v| v.config.id == vm_b_id));

        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b_id, "Beta", VmStatus::Off, 2),
            select: false,
            is_details_fetch: true,
            daemon_instance_id: Some(daemon_id),
        });
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(unknown_id, "Unknown", VmStatus::Off, 1),
            select: false,
            is_details_fetch: true,
            daemon_instance_id: Some(daemon_id),
        });
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b_id, "Beta", VmStatus::Off, 2),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![
                make_test_vm(vm_a_id, "Alpha", VmStatus::Off, 4),
                make_test_vm(vm_b_id, "Beta", VmStatus::Off, 2),
            ],
            snapshot_seq: 3,
        });
        assert_eq!(app.vms.len(), 1);
        let vm_a = app.vms.iter().find(|v| v.config.id == vm_a_id).unwrap();
        assert_eq!(vm_a.config.id, vm_a_id);
        assert_eq!(vm_a.runtime.status, VmStatus::Running);
        assert_eq!(vm_a.runtime.revision, 5);
    }

    #[test]
    fn stale_new_vm_wizard_completion_does_not_close_newer_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());

        let old_wiz = NewVmWizard::new(dir.path());
        let old_id = old_wiz.wizard_id;
        let new_wiz = NewVmWizard::new(dir.path());
        let new_id = new_wiz.wizard_id;
        assert_ne!(old_id, new_id);
        app.new_vm_wizard = Some(new_wiz);

        // Completion (success or error) from a cancelled/superseded wizard must not
        // close or mutate the currently open wizard.
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::NewVmWizard(old_id),
            error: Some("stale failure".into()),
        });
        assert!(app.new_vm_wizard.as_ref().is_some_and(|w| w.wizard_id == new_id && w.error.is_none()));

        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::NewVmWizard(old_id),
            error: None,
        });
        assert!(app.new_vm_wizard.as_ref().is_some_and(|w| w.wizard_id == new_id));

        // Matching wizard_id completion closes it cleanly.
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::NewVmWizard(new_id),
            error: None,
        });
        assert!(app.new_vm_wizard.is_none());
    }

    #[test]
    fn storage_in_flight_flags_clear_on_storage_op_finished() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let vm_id = Uuid::new_v4();

        app.attach_disk_in_flight.insert(vm_id, 1);
        app.mount_iso_in_flight.insert(vm_id, 2);
        app.apply_update(GuiUpdate::StorageOpFinished {
            vm_id,
            op: controller::StorageOpKind::AttachDisk,
            op_id: 1,
        });
        assert!(!app.attach_disk_in_flight.contains_key(&vm_id));
        assert!(app.mount_iso_in_flight.contains_key(&vm_id));

        app.apply_update(GuiUpdate::StorageOpFinished {
            vm_id,
            op: controller::StorageOpKind::MountIso,
            op_id: 2,
        });
        assert!(!app.mount_iso_in_flight.contains_key(&vm_id));
    }

    #[test]
    fn ignores_stale_vm_logs_and_clears_deleted_logs_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_a = Uuid::new_v4();
        let vm_b = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![
                make_test_vm(vm_a, "Alpha", VmStatus::Running, 1),
                make_test_vm(vm_b, "Beta", VmStatus::Running, 1),
            ],
            snapshot_seq: 1,
        });
        app.selected_vm = Some(vm_b);

        // Slow log reply for previously selected vm_a must not overwrite or populate logs_cache.
        app.apply_update(GuiUpdate::VmLogs {
            id: vm_a,
            content: "stale alpha logs".into(),
        });
        assert!(app.logs_cache.is_none());

        app.apply_update(GuiUpdate::VmLogs {
            id: vm_b,
            content: "beta logs".into(),
        });
        assert_eq!(app.logs_cache, Some((vm_b, "beta logs".into())));

        // Even after valid logs are cached for vm_b, a late response for vm_a must not overwrite them.
        app.apply_update(GuiUpdate::VmLogs {
            id: vm_a,
            content: "".into(),
        });
        assert_eq!(app.logs_cache, Some((vm_b, "beta logs".into())));

        // Deleting vm_b clears its cached logs.
        app.apply_update(GuiUpdate::VmDeleted {
            id: vm_b,
            revision: 2,
            daemon_instance_id: Some(daemon_id),
        });
        assert!(app.logs_cache.is_none());
    }

    #[test]
    fn disconnected_log_tab_does_not_poll_logs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let vm_id = Uuid::new_v4();
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Running, 1)],
            snapshot_seq: 1,
        });
        app.detail_tab = DetailTab::ConsoleLogs;
        app.connected = false;
        let stale_ts = Instant::now() - Duration::from_secs(10);
        app.last_log_fetch = stale_ts;

        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.render_ui(ctx);
        });
        assert_eq!(
            app.last_log_fetch, stale_ts,
            "disconnected ConsoleLogs tab must not schedule or reset log polling"
        );

        app.connected = true;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.render_ui(ctx);
        });
        assert!(
            app.last_log_fetch > stale_ts,
            "connected ConsoleLogs tab should poll logs when interval elapsed"
        );
    }

    #[test]
    fn stale_snapshot_preserves_just_created_vm_while_fresher_snapshot_drops_unseen_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_a = Uuid::new_v4();
        let vm_b = Uuid::new_v4();

        let mut info1 = make_host_info(Some(daemon_id));
        info1.event_revision = 1;
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: info1,
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 1,
        });

        // Newly created VM arrives over SSE at revision 5.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b, "Beta", VmStatus::Off, 5),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert!(app.vms.iter().any(|v| v.config.id == vm_b));

        // Older snapshot (watermark 2 < Beta's revision 5) omits Beta -> Beta must be preserved.
        let mut stale_info = make_host_info(Some(daemon_id));
        stale_info.event_revision = 2;
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: stale_info,
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 2)],
            snapshot_seq: 2,
        });
        assert!(
            app.vms.iter().any(|v| v.config.id == vm_b),
            "just-created VM with rev 5 must survive stale snapshot at rev 2"
        );

        // Fresher snapshot (watermark 6 >= Beta's revision 5) omits Beta -> Beta was deleted, so drop it.
        app.selected_vm = Some(vm_b);
        app.logs_cache = Some((vm_b, "stale beta logs".into()));
        let mut fresh_info = make_host_info(Some(daemon_id));
        fresh_info.event_revision = 6;
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: fresh_info,
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 6)],
            snapshot_seq: 3,
        });
        assert!(
            !app.vms.iter().any(|v| v.config.id == vm_b),
            "VM omitted from fresher snapshot at rev 6 must be dropped"
        );
        assert!(
            app.deleted_vms.contains_key(&vm_b),
            "VM omitted from fresher snapshot must be recorded in deleted_vms"
        );
        assert!(
            app.logs_cache.is_none(),
            "logs_cache for snapshot-dropped VM must be cleared"
        );

        // Late VmUpdated at prev_rev (or even rev 6) must not resurrect the snapshot-dropped VM.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b, "Beta", VmStatus::Off, 5),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert!(!app.vms.iter().any(|v| v.config.id == vm_b));

        // Stale rev=0 snapshot and stale rev=0 VmUpdated must not overwrite a VM at known_rev > 0.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_a, "Alpha", VmStatus::Running, 7),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_a, "Alpha", VmStatus::Off, 0),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_id),
        });
        assert_eq!(
            app.vms.iter().find(|v| v.config.id == vm_a).unwrap().runtime.status,
            VmStatus::Running
        );
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 0)],
            snapshot_seq: 4,
        });
        assert_eq!(
            app.vms.iter().find(|v| v.config.id == vm_a).unwrap().runtime.status,
            VmStatus::Running
        );
    }

    #[test]
    fn same_daemon_snapshot_preserves_inflight_attach_and_mount_while_new_daemon_clears_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_1 = Uuid::new_v4();
        let daemon_2 = Uuid::new_v4();
        let vm_a = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_1)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Running, 1)],
            snapshot_seq: 1,
        });
        app.attach_disk_in_flight.insert(vm_a, 1);
        app.mount_iso_in_flight.insert(vm_a, 2);

        // Same-daemon snapshot arrives mid-operation -> keep buttons disabled.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_1)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Running, 2)],
            snapshot_seq: 2,
        });
        assert!(app.attach_disk_in_flight.contains_key(&vm_a));
        assert!(app.mount_iso_in_flight.contains_key(&vm_a));

        // Transient DaemonDisconnected on same daemon must keep buttons disabled until worker finishes.
        app.apply_update(GuiUpdate::DaemonDisconnected("transient drop".into()));
        assert!(app.attach_disk_in_flight.contains_key(&vm_a));
        assert!(app.mount_iso_in_flight.contains_key(&vm_a));

        // Different daemon instance -> clear stale in-flight flags.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_2)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Running, 1)],
            snapshot_seq: 3,
        });
        assert!(!app.attach_disk_in_flight.contains_key(&vm_a));
        assert!(!app.mount_iso_in_flight.contains_key(&vm_a));

        // Stale StorageOpFinished with old op_id must not clear a newer op_id.
        app.attach_disk_in_flight.insert(vm_a, 10);
        app.apply_update(GuiUpdate::StorageOpFinished {
            vm_id: vm_a,
            op: controller::StorageOpKind::AttachDisk,
            op_id: 1,
        });
        assert_eq!(app.attach_disk_in_flight.get(&vm_a), Some(&10));
        app.apply_update(GuiUpdate::StorageOpFinished {
            vm_id: vm_a,
            op: controller::StorageOpKind::AttachDisk,
            op_id: 10,
        });
        assert!(!app.attach_disk_in_flight.contains_key(&vm_a));
    }

    #[test]
    fn late_update_from_previous_daemon_instance_does_not_block_new_daemon_lower_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_a = Uuid::new_v4();
        let daemon_b = Uuid::new_v4();
        let vm_id = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_a)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Running, 50)],
            snapshot_seq: 1,
        });

        // Switch to daemon_b (restarted daemon whose revision counter restarted at 1).
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_b)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 2,
        });

        // Late VmUpdated / VmDeleted from daemon_a with high revision 99 must be ignored.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_id, "Alpha", VmStatus::Running, 99),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_a),
        });
        app.apply_update(GuiUpdate::VmDeleted {
            id: vm_id,
            revision: 100,
            daemon_instance_id: Some(daemon_a),
        });
        assert_eq!(app.vms.len(), 1);
        assert_eq!(app.vms[0].runtime.status, VmStatus::Off);

        // Lower-revision update from daemon_b (revision 2) must still apply cleanly.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_id, "Alpha", VmStatus::Running, 2),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_b),
        });
        assert_eq!(app.vms[0].runtime.status, VmStatus::Running);
        assert_eq!(app.vms[0].runtime.revision, 2);
    }

    #[test]
    fn refresh_with_missing_daemon_instance_id_preserves_inflight_flags_on_same_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let vm_id = Uuid::new_v4();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Running, 1)],
            snapshot_seq: 1,
        });
        app.attach_disk_in_flight.insert(vm_id, 1);
        app.mount_iso_in_flight.insert(vm_id, 2);

        // Refresh on same URL while connected with daemon_instance_id == None preserves flags.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Running, 2)],
            snapshot_seq: 2,
        });
        assert!(app.attach_disk_in_flight.contains_key(&vm_id));
        assert!(app.mount_iso_in_flight.contains_key(&vm_id));

        // Switching to a different URL clears flags.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:2".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Running, 1)],
            snapshot_seq: 3,
        });
        assert!(!app.attach_disk_in_flight.contains_key(&vm_id));
        assert!(!app.mount_iso_in_flight.contains_key(&vm_id));
    }

    #[test]
    fn legacy_daemon_without_event_revision_drops_deleted_vm_on_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let vm_a = Uuid::new_v4();
        let vm_b = Uuid::new_v4();

        // Older daemon from main: event_revision == 0, daemon_instance_id == None.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 1,
        });

        // VM B arrives at revision 5, then the live SSE stream drops and B is deleted via CLI.
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_b, "Beta", VmStatus::Off, 5),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: None,
        });
        assert!(app.vms.iter().any(|v| v.config.id == vm_b));

        // Reconnect snapshot from legacy daemon (event_revision == 0) only includes Alpha (rev 1).
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(None),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![make_test_vm(vm_a, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 2,
        });
        assert!(
            !app.vms.iter().any(|v| v.config.id == vm_b),
            "deleted VM omitted from legacy daemon snapshot (event_revision == 0) must not be stranded"
        );
    }

    #[test]
    fn host_config_updated_ignores_out_of_order_and_syncs_restarted_daemon_instance() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_1 = Uuid::new_v4();
        let daemon_2 = Uuid::new_v4();
        let vm_id = Uuid::new_v4();

        let mut cfg_old = HostConfig::default();
        cfg_old.openvmm_bin = Some(PathBuf::from("/tmp/old-openvmm"));
        let mut cfg_new = HostConfig::default();
        cfg_new.openvmm_bin = Some(PathBuf::from("/tmp/new-openvmm"));

        // Newer snapshot (seq=5) arrives before an earlier save response (seq=2).
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_1)),
            host_config: cfg_new.clone(),
            switches: vec![],
            vms: vec![make_test_vm(vm_id, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 5,
        });
        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:1".into(),
            host_info: Some(make_host_info(Some(daemon_1))),
            host_config: cfg_old,
            snapshot_seq: 2,
        });
        assert_eq!(app.host_config.openvmm_bin, cfg_new.openvmm_bin);
        assert_eq!(app.last_vm_snapshot_seq, 5);
        assert_eq!(app.last_host_config_seq, 5);

        // Save response from a restarted daemon (daemon_2, seq=6) updates daemon_instance_id,
        // immediately clears the previous daemon's VM list, and accepts subsequent VmUpdated events
        // from daemon_2.
        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:1".into(),
            host_info: Some(make_host_info(Some(daemon_2))),
            host_config: cfg_new,
            snapshot_seq: 6,
        });
        assert_eq!(app.daemon_instance_id, Some(daemon_2));
        assert!(app.vms.is_empty());
        assert_eq!(app.selected_vm, None);
        app.apply_update(GuiUpdate::VmUpdated {
            vm: make_test_vm(vm_id, "Alpha", VmStatus::Running, 1),
            select: false,
            is_details_fetch: false,
            daemon_instance_id: Some(daemon_2),
        });
        assert_eq!(
            app.vms.iter().find(|v| v.config.id == vm_id).unwrap().runtime.status,
            VmStatus::Running
        );
    }

    #[test]
    fn host_config_updated_without_host_info_does_not_mark_connected() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        assert!(!app.connected);
        let initial_status = app.connection_status.clone();

        let mut cfg = HostConfig::default();
        cfg.openvmm_bin = Some(PathBuf::from("/tmp/updated-openvmm"));

        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:1".into(),
            host_info: None,
            host_config: cfg.clone(),
            snapshot_seq: 1,
        });

        assert!(!app.connected);
        assert_eq!(app.connected_base_url, None);
        assert_eq!(app.connection_status, initial_status);
        assert!(app.host_config_loaded);
        assert_eq!(app.host_config.openvmm_bin, cfg.openvmm_bin);
    }

    #[test]
    fn host_config_updated_does_not_drop_in_flight_vm_snapshot_or_flip_disconnected_banner() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_old_id = Uuid::new_v4();
        let vm_new_id = Uuid::new_v4();

        let mut cfg_before = HostConfig::default();
        cfg_before.openvmm_bin = Some(PathBuf::from("/tmp/before-openvmm"));
        let mut cfg_saved = HostConfig::default();
        cfg_saved.openvmm_bin = Some(PathBuf::from("/tmp/saved-openvmm"));

        // Initial snapshot at seq 1, then disconnect before saving host settings.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: cfg_before.clone(),
            switches: vec![],
            vms: vec![make_test_vm(vm_old_id, "Alpha", VmStatus::Off, 1)],
            snapshot_seq: 1,
        });
        app.apply_update(GuiUpdate::DaemonDisconnected("stream down".into()));
        assert!(!app.connected);

        // SaveHostConfig completes at seq 3 while a resync snapshot (seq 2) is in flight.
        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:1".into(),
            host_info: Some(make_host_info(Some(daemon_id))),
            host_config: cfg_saved.clone(),
            snapshot_seq: 3,
        });
        assert!(
            !app.connected,
            "HostConfigUpdated while SSE is disconnected must not flip connected banner back on"
        );
        assert_eq!(app.connection_status, "Disconnected");

        // In-flight VM snapshot at seq 2 arrives after seq 3 HostConfigUpdated:
        // VM list must still update, while newer host_config from seq 3 is preserved.
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:1".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: cfg_before,
            switches: vec![],
            vms: vec![
                make_test_vm(vm_old_id, "Alpha", VmStatus::Off, 1),
                make_test_vm(vm_new_id, "Beta", VmStatus::Running, 2),
            ],
            snapshot_seq: 2,
        });
        assert!(app.vms.iter().any(|v| v.config.id == vm_new_id));
        assert_eq!(app.host_config.openvmm_bin, cfg_saved.openvmm_bin);
        assert_eq!(app.last_vm_snapshot_seq, 2);
        assert_eq!(app.last_host_config_seq, 3);
    }

    #[test]
    fn failed_host_settings_save_resyncs_dialog_initial_values_from_latest_host_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());

        let initial_cfg = HostConfig {
            listen_addr: "127.0.0.1:47620".into(),
            openvmm_bin: Some(PathBuf::from("/old/openvmm")),
            uefi_firmware_path: Some(PathBuf::from("/old/MSVM.fd")),
            ..Default::default()
        };
        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:47620".into(),
            host_info: None,
            host_config: initial_cfg.clone(),
            snapshot_seq: 1,
        });

        let mut dlg = HostSettingsDialog::from_config(&app.host_config);
        let dialog_id = dlg.dialog_id;
        dlg.openvmm_bin = "/bad/openvmm".into();
        dlg.submitting = true;
        app.host_settings_dlg = Some(dlg);

        // While save is in flight, an external config change arrives and updates app.host_config.
        let external_cfg = HostConfig {
            listen_addr: "127.0.0.1:47620".into(),
            openvmm_bin: Some(PathBuf::from("/old/openvmm")),
            uefi_firmware_path: Some(PathBuf::from("/cli/MSVM.fd")),
            ..Default::default()
        };
        app.apply_update(GuiUpdate::HostConfigUpdated {
            base_url: "http://127.0.0.1:47620".into(),
            host_info: None,
            host_config: external_cfg,
            snapshot_seq: 2,
        });

        // Save fails: dialog clears submitting, shows error, and syncs unedited/initial fields.
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::HostSettings(dialog_id),
            error: Some("invalid openvmm path".into()),
        });

        let dlg = app.host_settings_dlg.as_ref().unwrap();
        assert!(!dlg.submitting);
        assert_eq!(dlg.error.as_deref(), Some("invalid openvmm path"));
        assert_eq!(dlg.openvmm_bin, "/bad/openvmm");
        assert_eq!(dlg.uefi_firmware_path, "/cli/MSVM.fd");
        assert_eq!(dlg.initial_uefi_firmware_path, "/cli/MSVM.fd");
        let patch = dlg.build_patch();
        assert!(patch.uefi_firmware_path.is_none());
        assert_eq!(
            patch.openvmm_bin,
            Some(Some(PathBuf::from("/bad/openvmm")))
        );
    }

    #[test]
    fn daemon_connected_syncs_open_vm_settings_dialog() {
        use velovm_core::models::{FastDeskTransport, NicAttachment};
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let daemon_id = Uuid::new_v4();
        let vm_id = Uuid::new_v4();

        let initial_vm = make_test_vm(vm_id, "Alpha", VmStatus::Running, 1);
        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:47620".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![initial_vm.clone()],
            snapshot_seq: 1,
        });

        let mut dlg = VmSettingsDialog::from_config(&initial_vm.config, initial_vm.runtime.status);
        assert!(!dlg.is_offline);
        dlg.vram_mb = initial_vm.config.fastdesk.vram_mb + 32;
        app.vm_settings_dlg = Some(dlg);

        // VM stops and gains an external fastdesk transport change + NIC while resyncing.
        let mut updated_vm = make_test_vm(vm_id, "Alpha", VmStatus::Off, 2);
        updated_vm.config.fastdesk.transport = FastDeskTransport::HvSock;
        updated_vm.config.nics.push(NicAttachment {
            id: Uuid::new_v4(),
            name: "ext-nic".into(),
            mac_address: "00:15:5d:01:02:03".into(),
            backend: NicBackendConfig::Tap {
                name: "tap9".into(),
            },
        });
        let expected_nics = updated_vm.config.nics.clone();

        app.apply_update(GuiUpdate::DaemonConnected {
            base_url: "http://127.0.0.1:47620".into(),
            host_info: make_host_info(Some(daemon_id)),
            host_config: HostConfig::default(),
            switches: vec![],
            vms: vec![updated_vm],
            snapshot_seq: 2,
        });

        let dlg = app.vm_settings_dlg.as_ref().unwrap();
        assert!(dlg.is_offline);
        assert_eq!(dlg.base_fastdesk.transport, FastDeskTransport::HvSock);
        assert_eq!(dlg.nics, expected_nics);
    }

    #[test]
    fn stale_dialog_finished_does_not_close_or_corrupt_reopened_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());
        let vm_id = Uuid::new_v4();
        let vm = make_test_vm(vm_id, "Reopened", VmStatus::Off, 1);

        // 1. HostSettingsDialog
        let old_host = HostSettingsDialog::from_config(&app.host_config);
        let old_host_id = old_host.dialog_id;
        app.host_settings_dlg = Some(old_host);
        // open_host_settings_dialog must not overwrite an already-open HostSettingsDialog.
        app.host_settings_dlg.as_mut().unwrap().openvmm_bin = "/edited".into();
        app.open_host_settings_dialog();
        assert_eq!(
            app.host_settings_dlg.as_ref().unwrap().openvmm_bin,
            "/edited"
        );
        app.host_settings_dlg = Some(HostSettingsDialog::from_config(&app.host_config));
        let new_host_id = app.host_settings_dlg.as_ref().unwrap().dialog_id;
        assert_ne!(old_host_id, new_host_id);

        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::HostSettings(old_host_id),
            error: Some("stale host error".into()),
        });
        assert!(
            app.host_settings_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_host_id && d.error.is_none())
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::HostSettings(old_host_id),
            error: None,
        });
        assert!(
            app.host_settings_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_host_id)
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::HostSettings(new_host_id),
            error: None,
        });
        assert!(app.host_settings_dlg.is_none());

        // 2. VmSettingsDialog
        let old_vm_id = VmSettingsDialog::from_config(&vm.config, vm.runtime.status).dialog_id;
        let new_vm_dlg = VmSettingsDialog::from_config(&vm.config, vm.runtime.status);
        let new_vm_id = new_vm_dlg.dialog_id;
        assert_ne!(old_vm_id, new_vm_id);
        app.vm_settings_dlg = Some(new_vm_dlg);

        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::VmSettings(old_vm_id),
            error: Some("stale vm settings error".into()),
        });
        assert!(
            app.vm_settings_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_vm_id && d.error.is_none())
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::VmSettings(old_vm_id),
            error: None,
        });
        assert!(
            app.vm_settings_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_vm_id)
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::VmSettings(new_vm_id),
            error: None,
        });
        assert!(app.vm_settings_dlg.is_none());

        // 3. DeleteVmDialog
        let old_del_id = DeleteVmDialog::new(vm_id, "Reopened".into()).dialog_id;
        let new_del_dlg = DeleteVmDialog::new(vm_id, "Reopened".into());
        let new_del_id = new_del_dlg.dialog_id;
        assert_ne!(old_del_id, new_del_id);
        app.delete_vm_dlg = Some(new_del_dlg);

        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::DeleteVm(old_del_id),
            error: Some("stale delete error".into()),
        });
        assert!(
            app.delete_vm_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_del_id && d.error.is_none())
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::DeleteVm(old_del_id),
            error: None,
        });
        assert!(
            app.delete_vm_dlg
                .as_ref()
                .is_some_and(|d| d.dialog_id == new_del_id)
        );
        app.apply_update(GuiUpdate::DialogFinished {
            kind: DialogKind::DeleteVm(new_del_id),
            error: None,
        });
        assert!(app.delete_vm_dlg.is_none());
    }

    #[test]
    fn open_new_vm_wizard_and_disk_manager_do_not_overwrite_open_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());

        app.open_new_vm_wizard();
        let wiz = app.new_vm_wizard.as_mut().unwrap();
        let wiz_id = wiz.wizard_id;
        wiz.name = "My-In-Progress-VM".into();
        wiz.submitting = true;

        app.open_new_vm_wizard();
        let wiz_after = app.new_vm_wizard.as_ref().unwrap();
        assert_eq!(wiz_after.wizard_id, wiz_id);
        assert_eq!(wiz_after.name, "My-In-Progress-VM");
        assert!(wiz_after.submitting);

        app.open_disk_manager_dialog();
        let mgr = app.disk_mgr_dlg.as_mut().unwrap();
        mgr.tab = 1;
        mgr.create_size_gb = 256;
        mgr.inspect_path = "/tmp/existing.vhdx".into();

        app.open_disk_manager_dialog();
        let mgr_after = app.disk_mgr_dlg.as_ref().unwrap();
        assert_eq!(mgr_after.tab, 1);
        assert_eq!(mgr_after.create_size_gb, 256);
        assert_eq!(mgr_after.inspect_path, "/tmp/existing.vhdx");
    }
}
