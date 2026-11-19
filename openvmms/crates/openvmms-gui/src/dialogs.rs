use crate::controller::BackendHandle;
use crate::controller::GuiRequest;
use egui::Color32;
use egui::RichText;
use openvmms_core::models::CreateDiskPayload;
use openvmms_core::models::CreateVmPayload;
use openvmms_core::models::DiskAttachment;
use openvmms_core::models::DiskFormat;
use openvmms_core::models::DiskInfo;
use openvmms_core::models::DiskKind;
use openvmms_core::models::FastDeskSettings;
use openvmms_core::models::FastDeskTransport;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::NicAttachment;
use openvmms_core::models::NicBackendConfig;
use openvmms_core::models::SecureBootTemplateConfig;
use openvmms_core::models::UpdateVmPayload;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmConfig;
use openvmms_core::models::VmPreset;
use openvmms_core::models::VmStatus;
use openvmms_core::models::format_bytes;
use openvmms_core::models::generate_hyperv_mac;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePickerTarget {
    WizardExistingDisk,
    WizardNewDiskPath,
    WizardIso,
    AttachDiskPath,
    MountIsoPath,
    DiskMgrCreatePath,
    DiskMgrInspectPath,
    SettingsFirmwarePath,
    HostOpenVmmBin,
    HostViewerBin,
    HostImgBin,
    HostUefiFirmware,
}

pub struct FilePickerModal {
    pub target: FilePickerTarget,
    pub current_dir: PathBuf,
    pub filename_input: String,
    pub save_mode: bool,
    pub filter_exts: Vec<String>,
    pub entries: Vec<(String, PathBuf, bool)>,
    pub error: Option<String>,
}

impl FilePickerModal {
    pub fn open(
        target: FilePickerTarget,
        initial_path: &str,
        save_mode: bool,
        filter_exts: Vec<&str>,
    ) -> Self {
        let trimmed = initial_path.trim();
        let initial = PathBuf::from(trimmed);
        let (start_dir, fname) = if !trimmed.is_empty() && initial.is_dir() {
            (initial, String::new())
        } else if !trimmed.is_empty()
            && let Some(parent) = initial.parent()
            && parent.is_dir()
        {
            (
                parent.to_path_buf(),
                initial
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        } else {
            let home = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (home, String::new())
        };

        let mut modal = Self {
            target,
            current_dir: start_dir,
            filename_input: fname,
            save_mode,
            filter_exts: filter_exts.into_iter().map(|s| s.to_ascii_lowercase()).collect(),
            entries: Vec::new(),
            error: None,
        };
        modal.reload();
        modal
    }

    pub fn reload(&mut self) {
        self.entries.clear();
        self.error = None;
        let read = match std::fs::read_dir(&self.current_dir) {
            Ok(r) => r,
            Err(err) => {
                self.error = Some(format!("Cannot read {}: {err}", self.current_dir.display()));
                return;
            }
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = path.is_dir();
            if is_dir {
                dirs.push((name, path, true));
            } else {
                if !self.filter_exts.is_empty() {
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .unwrap_or_default();
                    if !self.filter_exts.contains(&ext) {
                        continue;
                    }
                }
                files.push((name, path, false));
            }
        }
        dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        self.entries.extend(dirs);
        self.entries.extend(files);
    }
}

pub enum DiskMode {
    CreateNew,
    UseExisting,
    None,
}

pub struct NewVmWizard {
    pub step: usize,
    pub name: String,
    pub description: String,
    pub preset: VmPreset,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub secure_boot_enabled: bool,
    pub secure_boot_template: SecureBootTemplateConfig,
    pub nested_virt: bool,
    pub fastdesk_enabled: bool,
    pub vram_mb: u32,
    pub use_shm: bool,
    pub transport: FastDeskTransport,
    pub selected_switch_id: String,
    pub disk_mode: DiskMode,
    pub new_disk_path: String,
    pub new_disk_size_gb: u64,
    pub new_disk_format: DiskFormat,
    pub new_disk_fixed: bool,
    pub existing_disk_path: String,
    pub iso_path: String,
    pub error: Option<String>,
}

impl NewVmWizard {
    pub fn new(default_dir: &std::path::Path) -> Self {
        let mut w = Self {
            step: 0,
            name: "Windows-11-VM".to_string(),
            description: String::new(),
            preset: VmPreset::Windows11,
            vcpus: 4,
            memory_mb: 8192,
            secure_boot_enabled: true,
            secure_boot_template: SecureBootTemplateConfig::MicrosoftWindows,
            nested_virt: false,
            fastdesk_enabled: true,
            vram_mb: 32,
            use_shm: true,
            transport: FastDeskTransport::Tcp,
            selected_switch_id: "consomme".to_string(),
            disk_mode: DiskMode::CreateNew,
            new_disk_path: default_dir
                .join("disks")
                .join("Windows-11-VM.vhdx")
                .to_string_lossy()
                .into_owned(),
            new_disk_size_gb: 64,
            new_disk_format: DiskFormat::Vhdx,
            new_disk_fixed: false,
            existing_disk_path: String::new(),
            iso_path: String::new(),
            error: None,
        };
        w.apply_preset(VmPreset::Windows11);
        w
    }

    pub fn apply_preset(&mut self, preset: VmPreset) {
        self.preset = preset;
        match preset {
            VmPreset::Windows11 => {
                self.vcpus = 4;
                self.memory_mb = 8192;
                self.secure_boot_enabled = true;
                self.secure_boot_template = SecureBootTemplateConfig::MicrosoftWindows;
                self.vram_mb = 32;
            }
            VmPreset::LinuxDesktop => {
                self.vcpus = 4;
                self.memory_mb = 4096;
                self.secure_boot_enabled = false;
                self.secure_boot_template = SecureBootTemplateConfig::MicrosoftUefiCa;
                self.vram_mb = 32;
            }
            VmPreset::Custom => {
                self.vcpus = 2;
                self.memory_mb = 4096;
                self.secure_boot_enabled = false;
                self.secure_boot_template = SecureBootTemplateConfig::None;
            }
        }
    }
}

pub struct VmSettingsDialog {
    pub vm_id: Uuid,
    pub is_offline: bool,
    pub name: String,
    pub description: String,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub nested_virt: bool,
    pub secure_boot_enabled: bool,
    pub secure_boot_template: SecureBootTemplateConfig,
    pub uefi_firmware: String,
    pub fastdesk_enabled: bool,
    pub vram_mb: u32,
    pub use_shm: bool,
    pub transport: FastDeskTransport,
    pub selected_switch_id: String,
    pub nics: Vec<NicAttachment>,
}

impl VmSettingsDialog {
    pub fn from_config(cfg: &VmConfig, status: VmStatus) -> Self {
        let selected_switch_id = match cfg.nics.first().map(|n| &n.backend) {
            None => "none".to_string(),
            Some(NicBackendConfig::Consomme { .. }) => "consomme".to_string(),
            Some(NicBackendConfig::VmSwitch { switch_id, .. }) => switch_id.clone(),
            Some(NicBackendConfig::Tap { name }) => name.clone(),
        };
        Self {
            vm_id: cfg.id,
            is_offline: matches!(status, VmStatus::Off | VmStatus::Error),
            name: cfg.name.clone(),
            description: cfg.description.clone(),
            vcpus: cfg.vcpus,
            memory_mb: cfg.memory_mb,
            nested_virt: cfg.nested_virt,
            secure_boot_enabled: cfg.secure_boot_enabled,
            secure_boot_template: cfg.secure_boot_template,
            uefi_firmware: cfg
                .uefi_firmware
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            fastdesk_enabled: cfg.fastdesk.enabled,
            vram_mb: cfg.fastdesk.vram_mb,
            use_shm: cfg.fastdesk.use_shm,
            transport: cfg.fastdesk.transport,
            selected_switch_id,
            nics: cfg.nics.clone(),
        }
    }
}

pub struct DiskManagerDialog {
    pub tab: usize,
    pub create_path: String,
    pub create_size_gb: u64,
    pub create_format: DiskFormat,
    pub create_fixed: bool,
    pub inspect_path: String,
    pub last_info: Option<DiskInfo>,
}

impl DiskManagerDialog {
    pub fn new(default_dir: &std::path::Path) -> Self {
        Self {
            tab: 0,
            create_path: default_dir.join("new-disk.vhdx").to_string_lossy().into_owned(),
            create_size_gb: 64,
            create_format: DiskFormat::Vhdx,
            create_fixed: false,
            inspect_path: String::new(),
            last_info: None,
        }
    }
}

pub struct HostSettingsDialog {
    pub listen_addr: String,
    pub openvmm_bin: String,
    pub fastdesk_viewer_bin: String,
    pub openvmm_img_bin: String,
    pub uefi_firmware_path: String,
    pub base_config: HostConfig,
}

impl HostSettingsDialog {
    pub fn from_config(cfg: &HostConfig) -> Self {
        Self {
            listen_addr: cfg.listen_addr.clone(),
            openvmm_bin: opt_to_string(cfg.openvmm_bin.as_deref()),
            fastdesk_viewer_bin: opt_to_string(cfg.fastdesk_viewer_bin.as_deref()),
            openvmm_img_bin: opt_to_string(cfg.openvmm_img_bin.as_deref()),
            uefi_firmware_path: opt_to_string(cfg.uefi_firmware_path.as_deref()),
            base_config: cfg.clone(),
        }
    }
}

pub struct DeleteVmDialog {
    pub vm_id: Uuid,
    pub vm_name: String,
    pub delete_disks: bool,
}

fn opt_to_string(p: Option<&std::path::Path>) -> String {
    p.map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn string_to_opt(s: &str) -> Option<PathBuf> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(PathBuf::from(t))
    }
}

pub fn build_nics_from_switch_id(
    switch_id: &str,
    switches: &[VirtualSwitchInfo],
    existing_nics: &[NicAttachment],
) -> Vec<NicAttachment> {
    if switch_id == "none" {
        return Vec::new();
    }
    if switch_id == "consomme" {
        if let Some(existing) = existing_nics
            .iter()
            .find(|n| matches!(n.backend, NicBackendConfig::Consomme { .. }))
        {
            return vec![existing.clone()];
        }
        return vec![NicAttachment::default_consomme()];
    }
    let sw = switches.iter().find(|s| s.id == switch_id);
    let mac = existing_nics
        .first()
        .map(|n| n.mac_address.clone())
        .unwrap_or_else(generate_hyperv_mac);
    let backend = match sw.map(|s| s.kind.as_str()) {
        Some("tap") | Some("bridge") => NicBackendConfig::Tap {
            name: switch_id.to_string(),
        },
        _ => NicBackendConfig::VmSwitch {
            switch_id: switch_id.to_string(),
            switch_name: sw.map(|s| s.name.clone()),
        },
    };
    vec![NicAttachment {
        id: existing_nics
            .first()
            .map(|n| n.id)
            .unwrap_or_else(Uuid::new_v4),
        name: sw
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("Switch ({switch_id})")),
        mac_address: mac,
        backend,
    }]
}

pub fn render_file_picker(
    ctx: &egui::Context,
    picker_opt: &mut Option<FilePickerModal>,
) -> Option<(FilePickerTarget, PathBuf)> {
    let mut chosen: Option<(FilePickerTarget, PathBuf)> = None;
    let mut close = false;
    if let Some(picker) = picker_opt.as_mut() {
        egui::Window::new(if picker.save_mode {
            "Choose Output File Path"
        } else {
            "Select File"
        })
        .collapsible(false)
        .resizable(true)
        .default_size([620.0, 440.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("⬆ Up").clicked()
                    && let Some(parent) = picker.current_dir.parent()
                {
                    picker.current_dir = parent.to_path_buf();
                    picker.reload();
                }
                let mut dir_str = picker.current_dir.to_string_lossy().into_owned();
                if ui
                    .add(egui::TextEdit::singleline(&mut dir_str).desired_width(440.0))
                    .lost_focus()
                {
                    let p = PathBuf::from(dir_str.trim());
                    if p.is_dir() {
                        picker.current_dir = p;
                        picker.reload();
                    }
                }
                if ui.button("↻").clicked() {
                    picker.reload();
                }
            });

            if let Some(err) = &picker.error {
                ui.colored_label(Color32::LIGHT_RED, err);
            }

            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("file_picker_scroll")
                .max_height(290.0)
                .show(ui, |ui| {
                    let entries = picker.entries.clone();
                    for (name, path, is_dir) in entries {
                        let label = if is_dir {
                            format!("📁 {name}")
                        } else {
                            format!("📄 {name}")
                        };
                        let selected = !is_dir && picker.filename_input == name;
                        let resp = ui.selectable_label(selected, label);
                        if resp.clicked() {
                            if is_dir {
                                picker.current_dir = path.clone();
                                picker.reload();
                            } else {
                                picker.filename_input = name;
                            }
                        }
                        if resp.double_clicked() && !is_dir {
                            chosen = Some((picker.target, path));
                            close = true;
                        }
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("File:");
                ui.add(
                    egui::TextEdit::singleline(&mut picker.filename_input).desired_width(380.0),
                );
                if ui.button("OK").clicked() && !picker.filename_input.trim().is_empty() {
                    let raw = PathBuf::from(picker.filename_input.trim());
                    let resolved = if raw.is_absolute() {
                        raw
                    } else {
                        picker.current_dir.join(raw)
                    };
                    chosen = Some((picker.target, resolved));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    }
    if close {
        *picker_opt = None;
    }
    chosen
}

pub fn render_new_vm_wizard(
    ctx: &egui::Context,
    wizard_opt: &mut Option<NewVmWizard>,
    switches: &[VirtualSwitchInfo],
    backend: &BackendHandle,
    file_picker: &mut Option<FilePickerModal>,
) {
    let mut close = false;
    if let Some(w) = wizard_opt.as_mut() {
        egui::Window::new("New Virtual Machine Wizard")
            .collapsible(false)
            .resizable(true)
            .default_size([620.0, 480.0])
            .show(ctx, |ui| {
                let steps = [
                    "1. Name & Preset",
                    "2. Compute & Security",
                    "3. FastDesk Display",
                    "4. Networking",
                    "5. Storage & ISO",
                ];
                ui.horizontal_wrapped(|ui| {
                    for (idx, title) in steps.iter().enumerate() {
                        let selected = w.step == idx;
                        if ui.selectable_label(selected, *title).clicked() {
                            w.step = idx;
                        }
                    }
                });
                ui.separator();

                match w.step {
                    0 => {
                        ui.heading("Specify Name and Guest Preset");
                        ui.add_space(6.0);
                        egui::Grid::new("wiz_step0").num_columns(2).spacing([12.0, 8.0]).show(
                            ui,
                            |ui| {
                                ui.label("VM Name:");
                                ui.add(egui::TextEdit::singleline(&mut w.name).desired_width(340.0));
                                ui.end_row();

                                ui.label("Description:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut w.description)
                                        .desired_width(340.0),
                                );
                                ui.end_row();

                                ui.label("Preset:");
                                ui.horizontal(|ui| {
                                    for p in [
                                        VmPreset::Windows11,
                                        VmPreset::LinuxDesktop,
                                        VmPreset::Custom,
                                    ] {
                                        if ui.selectable_label(w.preset == p, p.to_string()).clicked()
                                        {
                                            w.apply_preset(p);
                                        }
                                    }
                                });
                                ui.end_row();
                            },
                        );
                    }
                    1 => {
                        ui.heading("Assign Memory, Processors & Firmware Security");
                        ui.add_space(6.0);
                        egui::Grid::new("wiz_step1").num_columns(2).spacing([12.0, 8.0]).show(
                            ui,
                            |ui| {
                                ui.label("Virtual CPUs:");
                                ui.add(egui::Slider::new(&mut w.vcpus, 1..=64));
                                ui.end_row();

                                ui.label("Startup Memory (MiB):");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut w.memory_mb).range(256..=262_144).speed(256));
                                    for (label, mb) in [("4G", 4096), ("8G", 8192), ("16G", 16384), ("32G", 32768)] {
                                        if ui.button(label).clicked() {
                                            w.memory_mb = mb;
                                        }
                                    }
                                });
                                ui.end_row();

                                ui.label("UEFI Secure Boot:");
                                ui.checkbox(&mut w.secure_boot_enabled, "Enable Secure Boot");
                                ui.end_row();

                                ui.label("Secure Boot Template:");
                                egui::ComboBox::from_id_salt("wiz_sb_tpl")
                                    .selected_text(w.secure_boot_template.to_string())
                                    .show_ui(ui, |ui| {
                                        for tpl in [
                                            SecureBootTemplateConfig::MicrosoftWindows,
                                            SecureBootTemplateConfig::MicrosoftUefiCa,
                                            SecureBootTemplateConfig::None,
                                        ] {
                                            ui.selectable_value(
                                                &mut w.secure_boot_template,
                                                tpl,
                                                tpl.to_string(),
                                            );
                                        }
                                    });
                                ui.end_row();

                                ui.label("Nested Virtualization:");
                                ui.checkbox(&mut w.nested_virt, "Expose hardware virtualization extensions");
                                ui.end_row();
                            },
                        );
                    }
                    2 => {
                        ui.heading("FastDesk 2D Accelerated Display & Input");
                        ui.add_space(6.0);
                        ui.checkbox(&mut w.fastdesk_enabled, "Enable FastDesk Synthetic Video / Keyboard / Mouse");
                        ui.add_enabled_ui(w.fastdesk_enabled, |ui| {
                            egui::Grid::new("wiz_step2").num_columns(2).spacing([12.0, 8.0]).show(
                                ui,
                                |ui| {
                                    ui.label("Video Memory (VRAM MiB):");
                                    ui.horizontal(|ui| {
                                        ui.add(egui::Slider::new(&mut w.vram_mb, 8..=128));
                                        ui.label("(32 MiB supports 4K)");
                                    });
                                    ui.end_row();

                                    ui.label("Zero-Copy Shared VRAM:");
                                    ui.checkbox(&mut w.use_shm, "Use local SHM backing file for zero-copy viewer");
                                    ui.end_row();

                                    ui.label("Transport:");
                                    ui.horizontal(|ui| {
                                        ui.radio_value(&mut w.transport, FastDeskTransport::Tcp, "TCP Loopback");
                                        ui.radio_value(&mut w.transport, FastDeskTransport::HvSock, "AF_HYPERV / AF_VSOCK");
                                    });
                                    ui.end_row();
                                },
                            );
                        });
                    }
                    3 => {
                        ui.heading("Configure Virtual Networking");
                        ui.add_space(6.0);
                        ui.radio_value(&mut w.selected_switch_id, "none".to_string(), "Not Connected");
                        for sw in switches {
                            ui.radio_value(
                                &mut w.selected_switch_id,
                                sw.id.clone(),
                                format!("{} [{}] — {}", sw.name, sw.kind, sw.description),
                            );
                        }
                    }
                    _ => {
                        ui.heading("Connect Virtual Hard Disk & Installer ISO");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut w.disk_mode,
                                DiskMode::CreateNew,
                                "Create a new virtual hard disk",
                            );
                            ui.selectable_value(
                                &mut w.disk_mode,
                                DiskMode::UseExisting,
                                "Use an existing virtual hard disk",
                            );
                            ui.selectable_value(&mut w.disk_mode, DiskMode::None, "Attach later");
                        });
                        ui.add_space(6.0);
                        match w.disk_mode {
                            DiskMode::CreateNew => {
                                egui::Grid::new("wiz_disk_new").num_columns(2).spacing([10.0, 6.0]).show(
                                    ui,
                                    |ui| {
                                        ui.label("Disk Path (optional):");
                                        ui.horizontal(|ui| {
                                            ui.add(egui::TextEdit::singleline(&mut w.new_disk_path).desired_width(300.0));
                                            if ui.button("Browse…").clicked() {
                                                *file_picker = Some(FilePickerModal::open(
                                                    FilePickerTarget::WizardNewDiskPath,
                                                    &w.new_disk_path,
                                                    true,
                                                    vec!["vhdx", "img", "raw"],
                                                ));
                                            }
                                        });
                                        ui.end_row();

                                        ui.label("Size (GiB):");
                                        ui.add(egui::DragValue::new(&mut w.new_disk_size_gb).range(1..=4096));
                                        ui.end_row();

                                        ui.label("Format:");
                                        ui.horizontal(|ui| {
                                            ui.radio_value(&mut w.new_disk_format, DiskFormat::Vhdx, "VHDX");
                                            ui.radio_value(&mut w.new_disk_format, DiskFormat::Raw, "Raw (.img)");
                                            if w.new_disk_format == DiskFormat::Vhdx {
                                                ui.checkbox(&mut w.new_disk_fixed, "Fixed size");
                                            }
                                        });
                                        ui.end_row();
                                    },
                                );
                            }
                            DiskMode::UseExisting => {
                                ui.horizontal(|ui| {
                                    ui.label("Existing Disk:");
                                    ui.add(egui::TextEdit::singleline(&mut w.existing_disk_path).desired_width(320.0));
                                    if ui.button("Browse…").clicked() {
                                        *file_picker = Some(FilePickerModal::open(
                                            FilePickerTarget::WizardExistingDisk,
                                            &w.existing_disk_path,
                                            false,
                                            vec!["vhdx", "vhd", "img", "raw"],
                                        ));
                                    }
                                });
                            }
                            DiskMode::None => {
                                ui.label("You can attach a disk later in Storage & ISO.");
                            }
                        }

                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Installer ISO (optional):");
                            ui.add(egui::TextEdit::singleline(&mut w.iso_path).desired_width(300.0));
                            if ui.button("Browse…").clicked() {
                                *file_picker = Some(FilePickerModal::open(
                                    FilePickerTarget::WizardIso,
                                    &w.iso_path,
                                    false,
                                    vec!["iso"],
                                ));
                            }
                        });
                    }
                }

                if let Some(err) = &w.error {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::LIGHT_RED, err);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w.step == 4 {
                            if ui.button(RichText::new("Finish & Create VM").strong()).clicked() {
                                if w.name.trim().is_empty() {
                                    w.error = Some("VM name cannot be empty".to_string());
                                    return;
                                }
                                let (disks, create_disk) = match w.disk_mode {
                                    DiskMode::CreateNew => (
                                        Vec::new(),
                                        Some(CreateDiskPayload {
                                            path: PathBuf::from(w.new_disk_path.trim()),
                                            size_bytes: w.new_disk_size_gb * 1024 * 1024 * 1024,
                                            format: w.new_disk_format,
                                            fixed: w.new_disk_fixed,
                                        }),
                                    ),
                                    DiskMode::UseExisting => {
                                        let p = PathBuf::from(w.existing_disk_path.trim());
                                        (
                                            vec![DiskAttachment {
                                                lun: 0,
                                                kind: DiskKind::HardDisk,
                                                format: DiskFormat::from_path(&p),
                                                path: p,
                                                read_only: false,
                                            }],
                                            None,
                                        )
                                    }
                                    DiskMode::None => (Vec::new(), None),
                                };
                                let nics = build_nics_from_switch_id(
                                    &w.selected_switch_id,
                                    switches,
                                    &[],
                                );
                                let payload = CreateVmPayload {
                                    name: w.name.trim().to_string(),
                                    description: w.description.clone(),
                                    preset: w.preset,
                                    vcpus: Some(w.vcpus),
                                    memory_mb: Some(w.memory_mb),
                                    nested_virt: Some(w.nested_virt),
                                    uefi_firmware: None,
                                    secure_boot_enabled: Some(w.secure_boot_enabled),
                                    secure_boot_template: Some(w.secure_boot_template),
                                    fastdesk: Some(FastDeskSettings {
                                        enabled: w.fastdesk_enabled,
                                        vram_mb: w.vram_mb,
                                        use_shm: w.use_shm,
                                        transport: w.transport,
                                        listen_addr: "127.0.0.1:0".to_string(),
                                        vsock_port: (w.transport == FastDeskTransport::HvSock)
                                            .then_some(5901),
                                    }),
                                    disks,
                                    create_disk,
                                    iso_path: string_to_opt(&w.iso_path),
                                    nics: Some(nics),
                                };
                                backend.send(GuiRequest::CreateVm(payload));
                                close = true;
                            }
                        } else if ui.button("Next >").clicked() {
                            w.step += 1;
                        }
                        if w.step > 0 && ui.button("< Back").clicked() {
                            w.step -= 1;
                        }
                    });
                });
            });
    }
    if close {
        *wizard_opt = None;
    }
}

impl PartialEq for DiskMode {
    fn eq(&self, other: &Self) -> bool {
        core::mem::discriminant(self) == core::mem::discriminant(other)
    }
}

pub fn render_vm_settings_dialog(
    ctx: &egui::Context,
    dlg_opt: &mut Option<VmSettingsDialog>,
    switches: &[VirtualSwitchInfo],
    backend: &BackendHandle,
    file_picker: &mut Option<FilePickerModal>,
) {
    let mut close = false;
    if let Some(d) = dlg_opt.as_mut() {
        egui::Window::new(format!("Settings — {}", d.name))
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 440.0])
            .show(ctx, |ui| {
                if !d.is_offline {
                    ui.colored_label(
                        Color32::from_rgb(240, 190, 70),
                        "Hardware settings are read-only while VM is active. Turn off VM to modify CPU, RAM, or firmware.",
                    );
                    ui.separator();
                }

                egui::Grid::new("vm_settings_grid").num_columns(2).spacing([12.0, 8.0]).show(
                    ui,
                    |ui| {
                        ui.label("Name:");
                        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(300.0));
                        ui.end_row();

                        ui.label("Description:");
                        ui.add(egui::TextEdit::singleline(&mut d.description).desired_width(300.0));
                        ui.end_row();
                    },
                );

                ui.add_enabled_ui(d.is_offline, |ui| {
                    ui.separator();
                    egui::Grid::new("vm_settings_hw").num_columns(2).spacing([12.0, 8.0]).show(
                        ui,
                        |ui| {
                            ui.label("Virtual CPUs:");
                            ui.add(egui::Slider::new(&mut d.vcpus, 1..=64));
                            ui.end_row();

                            ui.label("Memory (MiB):");
                            ui.add(egui::DragValue::new(&mut d.memory_mb).range(256..=262_144).speed(256));
                            ui.end_row();

                            ui.label("Secure Boot:");
                            ui.checkbox(&mut d.secure_boot_enabled, "Enable UEFI Secure Boot");
                            ui.end_row();

                            ui.label("SB Template:");
                            egui::ComboBox::from_id_salt("dlg_sb_tpl")
                                .selected_text(d.secure_boot_template.to_string())
                                .show_ui(ui, |ui| {
                                    for tpl in [
                                        SecureBootTemplateConfig::MicrosoftWindows,
                                        SecureBootTemplateConfig::MicrosoftUefiCa,
                                        SecureBootTemplateConfig::None,
                                    ] {
                                        ui.selectable_value(
                                            &mut d.secure_boot_template,
                                            tpl,
                                            tpl.to_string(),
                                        );
                                    }
                                });
                            ui.end_row();

                            ui.label("Nested Virt:");
                            ui.checkbox(&mut d.nested_virt, "Expose hardware virtualization");
                            ui.end_row();

                            ui.label("UEFI Firmware:");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut d.uefi_firmware).desired_width(240.0));
                                if ui.button("Browse…").clicked() {
                                    *file_picker = Some(FilePickerModal::open(
                                        FilePickerTarget::SettingsFirmwarePath,
                                        &d.uefi_firmware,
                                        false,
                                        vec!["fd", "bin"],
                                    ));
                                }
                            });
                            ui.end_row();

                            ui.label("FastDesk Display:");
                            ui.checkbox(&mut d.fastdesk_enabled, "Enabled");
                            ui.end_row();

                            ui.label("VRAM (MiB):");
                            ui.add(egui::Slider::new(&mut d.vram_mb, 8..=128));
                            ui.end_row();

                            ui.label("Shared VRAM (SHM):");
                            ui.checkbox(&mut d.use_shm, "Zero-copy SHM");
                            ui.end_row();

                            ui.label("Network Adapter:");
                            egui::ComboBox::from_id_salt("dlg_net_sw")
                                .selected_text(&d.selected_switch_id)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut d.selected_switch_id,
                                        "none".to_string(),
                                        "Not Connected",
                                    );
                                    for sw in switches {
                                        ui.selectable_value(
                                            &mut d.selected_switch_id,
                                            sw.id.clone(),
                                            format!("{} ({})", sw.name, sw.kind),
                                        );
                                    }
                                });
                            ui.end_row();
                        },
                    );
                });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save Changes").clicked() {
                        let payload = if d.is_offline {
                            let nics = build_nics_from_switch_id(
                                &d.selected_switch_id,
                                switches,
                                &d.nics,
                            );
                            UpdateVmPayload {
                                name: Some(d.name.trim().to_string()),
                                description: Some(d.description.clone()),
                                vcpus: Some(d.vcpus),
                                memory_mb: Some(d.memory_mb),
                                nested_virt: Some(d.nested_virt),
                                uefi_firmware: Some(string_to_opt(&d.uefi_firmware)),
                                secure_boot_enabled: Some(d.secure_boot_enabled),
                                secure_boot_template: Some(d.secure_boot_template),
                                fastdesk: Some(FastDeskSettings {
                                    enabled: d.fastdesk_enabled,
                                    vram_mb: d.vram_mb,
                                    use_shm: d.use_shm,
                                    transport: d.transport,
                                    listen_addr: "127.0.0.1:0".to_string(),
                                    vsock_port: (d.transport == FastDeskTransport::HvSock)
                                        .then_some(5901),
                                }),
                                nics: Some(nics),
                                ..Default::default()
                            }
                        } else {
                            UpdateVmPayload {
                                name: Some(d.name.trim().to_string()),
                                description: Some(d.description.clone()),
                                ..Default::default()
                            }
                        };
                        backend.send(GuiRequest::UpdateVm {
                            id: d.vm_id,
                            payload,
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }
    if close {
        *dlg_opt = None;
    }
}

pub fn render_disk_manager_dialog(
    ctx: &egui::Context,
    dlg_opt: &mut Option<DiskManagerDialog>,
    backend: &BackendHandle,
    file_picker: &mut Option<FilePickerModal>,
) {
    let mut close = false;
    if let Some(d) = dlg_opt.as_mut() {
        egui::Window::new("Virtual Hard Disk Manager")
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 360.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut d.tab, 0, "Create New Disk");
                    ui.selectable_value(&mut d.tab, 1, "Inspect Disk");
                });
                ui.separator();

                if d.tab == 0 {
                    egui::Grid::new("disk_mgr_create").num_columns(2).spacing([10.0, 8.0]).show(
                        ui,
                        |ui| {
                            ui.label("Output Path:");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut d.create_path).desired_width(320.0));
                                if ui.button("Browse…").clicked() {
                                    *file_picker = Some(FilePickerModal::open(
                                        FilePickerTarget::DiskMgrCreatePath,
                                        &d.create_path,
                                        true,
                                        vec!["vhdx", "img", "raw"],
                                    ));
                                }
                            });
                            ui.end_row();

                            ui.label("Format:");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut d.create_format, DiskFormat::Vhdx, "VHDX");
                                ui.radio_value(&mut d.create_format, DiskFormat::Raw, "Raw (.img)");
                                if d.create_format == DiskFormat::Vhdx {
                                    ui.checkbox(&mut d.create_fixed, "Fixed allocation");
                                }
                            });
                            ui.end_row();

                            ui.label("Size (GiB):");
                            ui.add(egui::DragValue::new(&mut d.create_size_gb).range(1..=4096));
                            ui.end_row();
                        },
                    );
                    ui.add_space(10.0);
                    if ui.button("Create Virtual Disk").clicked() {
                        backend.send(GuiRequest::CreateDisk(CreateDiskPayload {
                            path: PathBuf::from(d.create_path.trim()),
                            size_bytes: d.create_size_gb * 1024 * 1024 * 1024,
                            format: d.create_format,
                            fixed: d.create_fixed,
                        }));
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Disk Path:");
                        ui.add(egui::TextEdit::singleline(&mut d.inspect_path).desired_width(320.0));
                        if ui.button("Browse…").clicked() {
                            *file_picker = Some(FilePickerModal::open(
                                FilePickerTarget::DiskMgrInspectPath,
                                &d.inspect_path,
                                false,
                                vec!["vhdx", "vhd", "img", "raw", "iso"],
                            ));
                        }
                        if ui.button("Inspect").clicked() && !d.inspect_path.trim().is_empty() {
                            backend.send(GuiRequest::InspectDisk(PathBuf::from(
                                d.inspect_path.trim(),
                            )));
                        }
                    });
                }

                if let Some(info) = &d.last_info {
                    ui.separator();
                    ui.label(RichText::new("Disk Properties").strong());
                    egui::Grid::new("disk_info_grid").num_columns(2).spacing([12.0, 4.0]).show(
                        ui,
                        |ui| {
                            ui.label("Path:");
                            ui.label(info.path.display().to_string());
                            ui.end_row();
                            ui.label("Format / Type:");
                            ui.label(format!("{} ({})", info.format, info.disk_type));
                            ui.end_row();
                            ui.label("Virtual Size:");
                            ui.label(format_bytes(info.virtual_size_bytes));
                            ui.end_row();
                            ui.label("File Size on Disk:");
                            ui.label(format_bytes(info.file_size_bytes));
                            ui.end_row();
                        },
                    );
                }

                ui.separator();
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
    }
    if close {
        *dlg_opt = None;
    }
}

pub fn render_host_settings_dialog(
    ctx: &egui::Context,
    dlg_opt: &mut Option<HostSettingsDialog>,
    host_info: Option<&HostInfo>,
    backend: &BackendHandle,
    file_picker: &mut Option<FilePickerModal>,
) {
    let mut close = false;
    if let Some(d) = dlg_opt.as_mut() {
        egui::Window::new("Host & Binary Settings")
            .collapsible(false)
            .resizable(true)
            .default_size([640.0, 380.0])
            .show(ctx, |ui| {
                if let Some(info) = host_info {
                    ui.label(format!(
                        "Host: {} ({} {})  |  Data Dir: {}",
                        info.hostname,
                        info.os,
                        info.arch,
                        info.data_dir.display()
                    ));
                    ui.separator();
                }

                egui::Grid::new("host_cfg_grid").num_columns(2).spacing([10.0, 8.0]).show(
                    ui,
                    |ui| {
                        ui.label("Daemon Listen Addr:");
                        ui.add(egui::TextEdit::singleline(&mut d.listen_addr).desired_width(240.0));
                        ui.end_row();

                        path_row(
                            ui,
                            "openvmm Binary:",
                            &mut d.openvmm_bin,
                            FilePickerTarget::HostOpenVmmBin,
                            file_picker,
                        );
                        path_row(
                            ui,
                            "fastdesk-viewer Binary:",
                            &mut d.fastdesk_viewer_bin,
                            FilePickerTarget::HostViewerBin,
                            file_picker,
                        );
                        path_row(
                            ui,
                            "openvmm-img Binary:",
                            &mut d.openvmm_img_bin,
                            FilePickerTarget::HostImgBin,
                            file_picker,
                        );
                        path_row(
                            ui,
                            "UEFI Firmware (MSVM.fd):",
                            &mut d.uefi_firmware_path,
                            FilePickerTarget::HostUefiFirmware,
                            file_picker,
                        );
                    },
                );

                if let Some(info) = host_info {
                    ui.separator();
                    ui.label(RichText::new("Active Discovered Paths").strong());
                    ui.label(format!(
                        "openvmm: {} | viewer: {} | img: {} | UEFI: {}",
                        opt_status(info.resolved_paths.openvmm_bin.as_deref()),
                        opt_status(info.resolved_paths.fastdesk_viewer_bin.as_deref()),
                        opt_status(info.resolved_paths.openvmm_img_bin.as_deref()),
                        opt_status(info.resolved_paths.uefi_firmware_path.as_deref()),
                    ));
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save Host Settings").clicked() {
                        let mut cfg = d.base_config.clone();
                        cfg.listen_addr = d.listen_addr.trim().to_string();
                        cfg.openvmm_bin = string_to_opt(&d.openvmm_bin);
                        cfg.fastdesk_viewer_bin = string_to_opt(&d.fastdesk_viewer_bin);
                        cfg.openvmm_img_bin = string_to_opt(&d.openvmm_img_bin);
                        cfg.uefi_firmware_path = string_to_opt(&d.uefi_firmware_path);
                        backend.send(GuiRequest::SaveHostConfig(cfg));
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }
    if close {
        *dlg_opt = None;
    }
}

fn path_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    target: FilePickerTarget,
    file_picker: &mut Option<FilePickerModal>,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(360.0));
        if ui.button("Browse…").clicked() {
            *file_picker = Some(FilePickerModal::open(target, value, false, vec![]));
        }
    });
    ui.end_row();
}

fn opt_status(p: Option<&std::path::Path>) -> String {
    p.map(|x| x.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .unwrap_or_else(|| "missing".to_string())
}

pub fn render_delete_vm_dialog(
    ctx: &egui::Context,
    dlg_opt: &mut Option<DeleteVmDialog>,
    backend: &BackendHandle,
) {
    let mut close = false;
    if let Some(d) = dlg_opt.as_mut() {
        egui::Window::new("Delete Virtual Machine")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Are you sure you want to delete virtual machine '{}'?",
                    d.vm_name
                ));
                ui.checkbox(
                    &mut d.delete_disks,
                    "Also delete writable virtual hard disk files attached to this VM",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("Delete").color(Color32::LIGHT_RED))
                        .clicked()
                    {
                        backend.send(GuiRequest::DeleteVm {
                            id: d.vm_id,
                            delete_disks: d.delete_disks,
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }
    if close {
        *dlg_opt = None;
    }
}
